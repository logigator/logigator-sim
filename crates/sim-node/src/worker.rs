//! The dedicated simulation thread and its command channel.
//!
//! `sim_core::Simulation` is **owned by one thread** and never shared behind a lock — every access
//! goes through a [`Command`] sent over an `mpsc` channel and is served at a tick boundary, where
//! `link_state` is coherent. The JS-thread getters that must never block on a running sim read
//! [`Shared`] (lock-free atomics the worker republishes each boundary) directly instead of sending a
//! command. Async methods (`runAsync`, `snapshot`) hand the worker a `napi` [`JsDeferred`] it resolves
//! from this thread when the result is ready — so no libuv pool thread is occupied for the run's
//! duration.
//!
//! A `runAsync` run ticks in **batches**; between batches the worker drains any queued commands
//! (snapshot / submitInput / a single tick / stop via the flag), so in-run state retrieval is
//! served at a coherent boundary without competing for a lock. The engine is
//! single-threaded; the batch loop calls `tick()` directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use napi::{Env, JsDeferred, Result as NapiResult};
use sim_core::{Simulation as CoreSim, SnapshotConfig};

use crate::JsSnapshot;
use crate::input::InputRequest;

/// `SimState::Stopped as u8` — published when no run is active.
pub const STOPPED: u8 = sim_core::SimState::Stopped as u8;
/// `SimState::Running as u8` — published while a `runAsync`/`run` is in flight.
pub const RUNNING: u8 = sim_core::SimState::Running as u8;

/// Ticks per `runAsync` batch between command drains. Large enough to amortize the per-batch
/// bookkeeping, small enough to keep snapshot / stop latency low (analogous to the WASM batch).
/// Prime, not a round 4096: snapshots are served at batch boundaries, so a round (even) stride
/// would alias a period-2 oscillator onto one phase; an odd prime stride steps through every phase.
const BATCH: u64 = 4093;

/// Resolver for the `runAsync` promise: a boxed closure so the `JsDeferred`'s `Resolver` type is
/// nameable in [`Command`] (the closure itself is built on this thread at resolve time).
pub type UnitResolver = Box<dyn FnOnce(Env) -> NapiResult<()> + Send>;
/// Resolver for the `snapshot` promise (builds the JS `Buffer`s on the JS thread from the bytes the
/// worker copied at the boundary).
pub type SnapResolver = Box<dyn FnOnce(Env) -> NapiResult<JsSnapshot> + Send>;

/// Lock-free status the worker republishes at each tick boundary; the JS-thread getters
/// (`getStatus`/`linkCount`/`componentCount`) read it directly so they never block on the running
/// sim. `link_count`/`component_count` are immutable after compile.
pub struct Shared {
    pub state: AtomicU8,
    pub tick: AtomicU64,
    pub speed: AtomicU32,
    /// Cooperative stop flag for an in-flight `runAsync`; the run loop checks it each tick.
    pub stop: AtomicBool,
    pub link_count: u32,
    pub component_count: u32,
    /// Event frames retired per input (applied, or dropped by a `replace`), wrapping. Republished
    /// after every batch and every command; the JS thread subtracts it from its own `submitted`
    /// count, so progress never reads 0 for a request still in the channel.
    pub retired: Box<[AtomicU32]>,
}

/// A request to the simulation thread. Each sync reply rides a fresh oneshot `mpsc` channel the JS
/// thread blocks on; the async `RunAsync` carries a `JsDeferred` resolved when the run ends.
pub enum Command {
    /// One deterministic step.
    Tick(Sender<()>),
    /// Blocking run-to-completion on the sim thread; the JS thread is parked on `reply` (the old
    /// `synchronized: true`). `Err` if a background run is already in progress.
    RunBlocking {
        ticks: u64,
        timeout: Option<Duration>,
        reply: Sender<NapiResult<()>>,
    },
    /// Background run; the promise resolves when the bound is reached or `stop()` interrupts it.
    RunAsync {
        ticks: u64,
        timeout: Option<Duration>,
        deferred: JsDeferred<(), UnitResolver>,
    },
    /// Queue an input request the JS thread already validated, at this tick boundary. `events`
    /// lists `(input index, event frames)` per write, for the retired counters.
    SubmitInput {
        req: InputRequest,
        events: Vec<(usize, u32)>,
    },
    /// Coherent single-link read (coherent only when stopped).
    Link { id: u32, reply: Sender<bool> },
    /// One byte per output pin, component-major (`getOutputs`).
    Outputs(Sender<Vec<u8>>),
    /// Coherent tick-boundary snapshot; the promise resolves with the copied bytes.
    Snapshot {
        delta: bool,
        threshold: f32,
        deferred: JsDeferred<JsSnapshot, SnapResolver>,
    },
}

/// State of an in-flight `runAsync`, owned by the worker between batches.
struct Active {
    deferred: JsDeferred<(), UnitResolver>,
    /// Ticks executed this run; the bound is `ticks` (`u64::MAX` ⇒ unbounded).
    done: u64,
    ticks: u64,
    timeout: Option<Duration>,
    start: Instant,
    /// Window base for the rolling ticks/sec speed published each batch.
    window_start: Instant,
    window_tick: u64,
}

/// Map any `Display` (e.g. `SimError`) to a `napi::Error`.
pub fn core_err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Resolve the `runAsync` promise with `undefined`.
fn resolve_run(deferred: JsDeferred<(), UnitResolver>) {
    deferred.resolve(Box::new(|_| Ok(())));
}

/// The simulation thread. Idle ⇒ block on the next command. Running ⇒ tick a batch, republish
/// status, then drain queued commands at the boundary. Returns when every `Sender` is dropped (the
/// owning `Simulation` was destroyed / GC'd) so the join in `Drop` completes.
pub fn run_worker(mut sim: CoreSim, shared: Arc<Shared>, rx: Receiver<Command>) {
    let mut active: Option<Active> = None;
    let mut inputs = InputCounters::new(shared.retired.len());
    loop {
        if active.is_some() {
            advance_batch(&mut sim, &shared, &mut active, &mut inputs);
            // Drain anything queued, at this coherent boundary.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => handle(cmd, &mut sim, &shared, &mut active, &mut inputs),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if let Some(a) = active.take() {
                            resolve_run(a.deferred);
                        }
                        return;
                    }
                }
            }
        } else {
            match rx.recv() {
                Ok(cmd) => handle(cmd, &mut sim, &shared, &mut active, &mut inputs),
                Err(_) => return, // owner dropped, no run in flight
            }
        }
    }
}

/// The worker's side of the input progress counters.
///
/// Published before the worker answers anything that ticks or submits (a `tick()` / `run()` reply,
/// a resolved `runAsync`), so once such a call returns on the JS thread `inputProgress` is exact;
/// during a `runAsync` it is republished after every batch.
struct InputCounters {
    /// Event frames received per input, wrapping.
    received: Vec<u32>,
    /// Scratch for `input_progress`.
    pending: Vec<u32>,
    /// Whether the published counters may be out of date: a request arrived, or the last publish
    /// still saw event frames pending (ticks may have applied them since). Without either, no
    /// event frame can have been applied or dropped, so publishing would store the same values.
    stale: bool,
}

impl InputCounters {
    fn new(inputs: usize) -> Self {
        InputCounters {
            received: vec![0; inputs],
            pending: vec![0; inputs],
            stale: false,
        }
    }

    /// Count a request's event frames, `(input index, frames)` per write.
    fn receive(&mut self, events: Vec<(usize, u32)>) {
        for (i, n) in events {
            self.received[i] = self.received[i].wrapping_add(n);
        }
        self.stale = true;
    }

    /// Publish `retired = received - pending` per input, if it can have changed.
    fn publish(&mut self, sim: &CoreSim, shared: &Shared) {
        if !self.stale {
            return;
        }
        sim.input_progress(&mut self.pending);
        for ((r, &got), &pending) in shared.retired.iter().zip(&self.received).zip(&self.pending) {
            r.store(got.wrapping_sub(pending), Relaxed);
        }
        self.stale = self.pending.iter().any(|&p| p > 0);
    }
}

/// Tick one batch of an active run, republish status, and resolve the promise if the run ended.
fn advance_batch(
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    active: &mut Option<Active>,
    inputs: &mut InputCounters,
) {
    let mut a = active
        .take()
        .expect("advance_batch called with no active run");

    let ended = step_batch(sim, shared, &mut a);

    let now = Instant::now();
    let dt = now.duration_since(a.window_start).as_secs_f64();
    if dt > 0.0 {
        let dticks = a.done - a.window_tick;
        shared.speed.store((dticks as f64 / dt) as u32, Relaxed);
    }
    a.window_start = now;
    a.window_tick = a.done;
    shared.tick.store(sim.tick_count(), Relaxed);
    inputs.publish(sim, shared);

    if ended {
        shared.speed.store(0, Relaxed);
        shared.state.store(STOPPED, Relaxed);
        resolve_run(a.deferred);
    } else {
        shared.state.store(RUNNING, Relaxed);
        *active = Some(a);
    }
}

/// Tick up to `BATCH` steps; returns whether the run ended. The per-tick stop flag (a relaxed
/// atomic load, ~1 ns) and tick budget are checked every tick so stop latency stays low; the
/// timeout's `Instant::elapsed` (a `clock_gettime`) is checked once per batch instead — a run may
/// overshoot its deadline by up to one batch, the same granularity the batch already imposes on
/// stop/snapshot servicing.
fn step_batch(sim: &mut CoreSim, shared: &Arc<Shared>, a: &mut Active) -> bool {
    if a.timeout.is_some_and(|t| a.start.elapsed() >= t) {
        return true;
    }
    for _ in 0..BATCH {
        if shared.stop.load(Relaxed) || a.done >= a.ticks {
            return true;
        }
        sim.tick();
        a.done += 1;
    }
    false
}

/// Serve one command at a tick boundary (`sim` is at rest here, so reads are coherent).
fn handle(
    cmd: Command,
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    active: &mut Option<Active>,
    inputs: &mut InputCounters,
) {
    match cmd {
        Command::Tick(reply) => {
            sim.tick();
            shared.tick.store(sim.tick_count(), Relaxed);
            inputs.publish(sim, shared);
            let _ = reply.send(());
        }
        Command::RunBlocking {
            ticks,
            timeout,
            reply,
        } => {
            let r = if active.is_some() {
                Err(napi::Error::from_reason(
                    "a background run is in progress; stop() it first",
                ))
            } else {
                run_blocking(sim, shared, ticks, timeout)
            };
            inputs.publish(sim, shared);
            let _ = reply.send(r);
        }
        Command::RunAsync {
            ticks,
            timeout,
            deferred,
        } => {
            if active.is_some() {
                deferred.reject(napi::Error::from_reason("a run is already in progress"));
            } else {
                shared.stop.store(false, Relaxed);
                shared.state.store(RUNNING, Relaxed);
                let now = Instant::now();
                *active = Some(Active {
                    deferred,
                    done: 0,
                    ticks,
                    timeout,
                    start: now,
                    window_start: now,
                    window_tick: 0,
                });
            }
        }
        Command::SubmitInput { req, events } => {
            // Validated on the JS thread against a pending count that never under-reports, so
            // this cannot fail: the request is queued whole.
            let r = req.with_writes(|writes| sim.submit_input(writes));
            debug_assert!(r.is_ok(), "pre-validated input request refused: {r:?}");
            inputs.receive(events);
            inputs.publish(sim, shared);
        }
        Command::Link { id, reply } => {
            let _ = reply.send(sim.link(id));
        }
        Command::Outputs(reply) => {
            let _ = reply.send(sim.output_bytes());
        }
        Command::Snapshot {
            delta,
            threshold,
            deferred,
        } => {
            // Copy the snapshot bytes out here (at the boundary); the sim resumes immediately. The
            // JS-thread resolver wraps the owned `Vec`s into `Buffer`s — copy-and-resume.
            let (tick, is_delta, links, ids, values) = snapshot_parts(sim, delta, threshold);
            deferred.resolve(Box::new(move |_| {
                Ok(JsSnapshot {
                    tick,
                    is_delta,
                    links: links.map(Into::into),
                    ids: ids.map(Into::into),
                    values: values.map(Into::into),
                })
            }));
        }
    }
}

/// Produce a snapshot's owned byte buffers at a tick boundary. `Full` → packed
/// `link_state` bytes in `links`; `Delta` → changed ids (`u32` LE) in `ids` and packed values in
/// `values`. Returns `(tick, is_delta, links, ids, values)`.
type SnapParts = (f64, bool, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);
fn snapshot_parts(sim: &mut CoreSim, delta: bool, threshold: f32) -> SnapParts {
    let info = sim.snapshot(SnapshotConfig {
        delta,
        delta_threshold: threshold,
    });
    if info.is_delta {
        let mut ids = Vec::with_capacity(sim.snapshot_ids().len() * 4);
        for &id in sim.snapshot_ids() {
            ids.extend_from_slice(&id.to_le_bytes());
        }
        let values = sim.snapshot_values().to_vec();
        (info.tick as f64, true, None, Some(ids), Some(values))
    } else {
        (info.tick as f64, false, Some(sim.link_bytes()), None, None)
    }
}

/// Run-to-completion inline on the sim thread (the JS thread is parked on the reply, so no command —
/// `stop()` included — can arrive mid-run). The timeout is checked once per `BATCH` ticks so an
/// idle board doesn't pay a `clock_gettime` per tick (the run may overshoot by up to one batch).
fn run_blocking(
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    ticks: u64,
    timeout: Option<Duration>,
) -> NapiResult<()> {
    shared.stop.store(false, Relaxed);
    shared.state.store(RUNNING, Relaxed);
    let start = Instant::now();
    let start_tick = sim.tick_count();
    let mut done = 0u64;
    'outer: while done < ticks {
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            break;
        }
        for _ in 0..BATCH {
            if done >= ticks || shared.stop.load(Relaxed) {
                break 'outer;
            }
            sim.tick();
            done += 1;
        }
    }
    let done = sim.tick_count() - start_tick;
    let dt = start.elapsed().as_secs_f64();
    shared.speed.store(
        if dt > 0.0 {
            (done as f64 / dt) as u32
        } else {
            0
        },
        Relaxed,
    );
    shared.tick.store(sim.tick_count(), Relaxed);
    shared.state.store(STOPPED, Relaxed);
    Ok(())
}
