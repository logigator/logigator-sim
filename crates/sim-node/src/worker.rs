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
//! A `runAsync` run ticks in **batches** of [`BATCH`] each; between batches the worker drains any
//! queued commands (snapshot / state reads / submitInput / a single tick), so in-run state retrieval
//! is served at a coherent boundary without competing for a lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::{Buffer, Float64Array, Uint32Array};
use napi::{Env, JsDeferred, Result as NapiResult};
use sim_core::{RunConfig, SimError, Simulation as CoreSim, SnapshotConfig, StateLayout};

use crate::input::InputRequest;
use crate::{JsSnapshot, Records};

/// Wall-clock length of a `runAsync` batch. A `stop()`, snapshot or state read waits at most about
/// this long, on any board; the publishing and command drain between batches cost well under 1%.
const BATCH: Duration = Duration::from_millis(1);

/// Resolver for the `runAsync` promise: a boxed closure so the `JsDeferred`'s `Resolver` type is
/// nameable in [`Command`] (the closure itself is built on this thread at resolve time).
pub type UnitResolver = Box<dyn FnOnce(Env) -> NapiResult<()> + Send>;
/// Resolver for the `snapshot` promise (builds the JS `Buffer`s on the JS thread from the bytes the
/// worker copied at the boundary).
pub type SnapResolver = Box<dyn FnOnce(Env) -> NapiResult<JsSnapshot> + Send>;
/// Resolver for the `readState` promise.
pub type BytesResolver = Box<dyn FnOnce(Env) -> NapiResult<Buffer> + Send>;
/// Resolver for the `readRecords` promise.
pub type RecordsResolver = Box<dyn FnOnce(Env) -> NapiResult<Records> + Send>;

/// Lock-free status the worker republishes at each tick boundary; the JS-thread getters
/// (`getStatus`/`linkCount`/`componentCount`) read it directly so they never block on the running
/// sim. `link_count`/`component_count` are immutable after compile.
pub struct Shared {
    /// A `runAsync` is in flight.
    pub running: AtomicBool,
    pub tick: AtomicU64,
    /// `stop()` calls so far, wrapping. A `runAsync` run carries the count read when it was called
    /// and ends once the count moves, checked each batch. A flag the worker cleared on starting the
    /// run would lose a `stop()` made before the worker dequeued it, leaving the run going forever.
    pub stops: AtomicU32,
    pub link_count: u32,
    pub component_count: u32,
    /// Event frames retired per input (applied, or dropped by a `replace`), wrapping. Republished
    /// after every batch and every command; the JS thread subtracts it from its own `submitted`
    /// count, so progress never reads 0 for a request still in the channel.
    pub retired: Box<[AtomicU32]>,
    /// State version of every component that has one, in slot order (see `States` in the
    /// binding). Republished with `retired`.
    pub versions: Box<[AtomicU32]>,
}

/// A request to the simulation thread. Each sync reply rides a fresh oneshot `mpsc` channel the JS
/// thread blocks on; the async `RunAsync` carries a `JsDeferred` resolved when the run ends.
pub enum Command {
    /// One deterministic step.
    Tick(Sender<()>),
    /// Blocking run-to-completion on the sim thread; the JS thread is parked on `reply`. `Err` if a background run is already in progress.
    RunBlocking {
        ticks: u64,
        timeout: Option<Duration>,
        reply: Sender<NapiResult<()>>,
    },
    /// Background run; the promise resolves when the bound is reached or `stop()` interrupts it.
    RunAsync {
        ticks: u64,
        timeout: Option<Duration>,
        /// `Shared::stops` when `runAsync` was called.
        stops: u32,
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
    /// Copy `len` bytes of a component's byte state from `offset`.
    ReadState {
        comp: u32,
        offset: usize,
        len: usize,
        deferred: JsDeferred<Buffer, BytesResolver>,
    },
    /// Overwrite a component's byte state; the JS thread already checked it against the layout.
    WriteState {
        comp: u32,
        offset: usize,
        bytes: Vec<u8>,
    },
    /// Copy a recorder's records from `since` on, at most `max` (default: its capacity).
    ReadRecords {
        comp: u32,
        since: u64,
        max: Option<u32>,
        deferred: JsDeferred<Records, RecordsResolver>,
    },
}

/// State of an in-flight `runAsync`, owned by the worker between batches.
struct Active {
    deferred: JsDeferred<(), UnitResolver>,
    /// The tick the run started on; it runs at most `ticks` (`u64::MAX` ⇒ unbounded).
    start_tick: u64,
    ticks: u64,
    timeout: Option<Duration>,
    /// `Shared::stops` when `runAsync` was called; the run ends once it moves.
    stops: u32,
    start: Instant,
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
///
/// `versioned` lists the components with a state version, in `Shared::versions` slot order.
pub fn run_worker(
    mut sim: CoreSim,
    shared: Arc<Shared>,
    rx: Receiver<Command>,
    versioned: Box<[u32]>,
) {
    let mut active: Option<Active> = None;
    let mut published = Published {
        inputs: InputCounters::new(shared.retired.len()),
        versioned,
    };
    loop {
        if active.is_some() {
            advance_batch(&mut sim, &shared, &mut active, &mut published);
            // Drain anything queued, at this coherent boundary.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => handle(cmd, &mut sim, &shared, &mut active, &mut published),
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
                Ok(cmd) => handle(cmd, &mut sim, &shared, &mut active, &mut published),
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

/// What the worker publishes into [`Shared`] for the JS thread's non-blocking reads: the input
/// progress counters and the state versions.
///
/// Published before the worker answers anything that ticks, submits or writes state, so once such
/// a call returns on the JS thread both are exact; during a `runAsync` they are republished after
/// every batch.
struct Published {
    inputs: InputCounters,
    /// Components with a state version, in `Shared::versions` slot order.
    versioned: Box<[u32]>,
}

impl Published {
    fn publish(&mut self, sim: &CoreSim, shared: &Shared) {
        self.inputs.publish(sim, shared);
        for (v, &c) in shared.versions.iter().zip(&self.versioned) {
            // Every listed component is in range, so this never falls back to 0.
            v.store(sim.state_version(c).unwrap_or(0), Relaxed);
        }
    }
}

/// Tick one batch of an active run, republish status, and resolve the promise if the run ended.
fn advance_batch(
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    active: &mut Option<Active>,
    published: &mut Published,
) {
    let a = active
        .take()
        .expect("advance_batch called with no active run");

    let ended = step_batch(sim, shared, &a);
    shared.tick.store(sim.tick_count(), Relaxed);
    published.publish(sim, shared);

    if ended {
        shared.running.store(false, Relaxed);
        resolve_run(a.deferred);
    } else {
        *active = Some(a);
    }
}

/// End the active run if `stop()` has been called since it started. A stop that lands after a
/// batch's last check is otherwise seen only by the next batch, after the commands queued behind it,
/// so a `runAsync()` or `run()` sent straight after the `stop()` would find the run still going.
fn end_if_stopped(shared: &Shared, active: &mut Option<Active>) {
    if let Some(a) = active.take_if(|a| shared.stops.load(Relaxed) != a.stops) {
        shared.running.store(false, Relaxed);
        resolve_run(a.deferred);
    }
}

/// Tick one batch of the run, clamped to what remains of its bounds; returns whether the run ended
/// (stopped, ticks spent or timed out).
fn step_batch(sim: &mut CoreSim, shared: &Shared, a: &Active) -> bool {
    let ended = |sim: &CoreSim| {
        shared.stops.load(Relaxed) != a.stops
            || sim.tick_count() - a.start_tick >= a.ticks
            || a.timeout.is_some_and(|t| a.start.elapsed() >= t)
    };
    if ended(sim) {
        return true;
    }
    let budget = match a.timeout {
        Some(t) => t.saturating_sub(a.start.elapsed()).min(BATCH),
        None => BATCH,
    };
    sim.run(RunConfig {
        ticks: a.ticks - (sim.tick_count() - a.start_tick),
        timeout: Some(budget),
    });
    ended(sim)
}

/// Serve one command at a tick boundary (`sim` is at rest here, so reads are coherent).
fn handle(
    cmd: Command,
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    active: &mut Option<Active>,
    published: &mut Published,
) {
    match cmd {
        Command::Tick(reply) => {
            sim.tick();
            shared.tick.store(sim.tick_count(), Relaxed);
            published.publish(sim, shared);
            let _ = reply.send(());
        }
        Command::RunBlocking {
            ticks,
            timeout,
            reply,
        } => {
            end_if_stopped(shared, active);
            let r = if active.is_some() {
                Err(napi::Error::from_reason(
                    "a background run is in progress; stop() it first",
                ))
            } else {
                // Inline: the JS thread is parked on the reply, so nothing can arrive mid-run.
                sim.run(RunConfig { ticks, timeout });
                shared.tick.store(sim.tick_count(), Relaxed);
                Ok(())
            };
            published.publish(sim, shared);
            let _ = reply.send(r);
        }
        Command::RunAsync {
            ticks,
            timeout,
            stops,
            deferred,
        } => {
            end_if_stopped(shared, active);
            if active.is_some() {
                deferred.reject(napi::Error::from_reason("a run is already in progress"));
            } else {
                shared.running.store(true, Relaxed);
                *active = Some(Active {
                    deferred,
                    start_tick: sim.tick_count(),
                    ticks,
                    timeout,
                    stops,
                    start: Instant::now(),
                });
            }
        }
        Command::SubmitInput { req, events } => {
            // Validated on the JS thread against a pending count that never under-reports, so
            // this cannot fail: the request is queued whole.
            let r = req.with_writes(|writes| sim.submit_input(writes));
            debug_assert!(r.is_ok(), "pre-validated input request refused: {r:?}");
            published.inputs.receive(events);
            published.publish(sim, shared);
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
            let invalid = sim.snapshot_invalid().to_vec();
            deferred.resolve(Box::new(move |_| {
                Ok(JsSnapshot {
                    tick,
                    is_delta,
                    links: links.map(Into::into),
                    ids: ids.map(Into::into),
                    values: values.map(Into::into),
                    invalid: Uint32Array::new(invalid),
                })
            }));
        }
        Command::ReadState {
            comp,
            offset,
            len,
            deferred,
        } => match read_state(sim, comp, offset, len) {
            Ok(bytes) => deferred.resolve(Box::new(move |_| Ok(bytes.into()))),
            Err(e) => deferred.reject(core_err(e)),
        },
        Command::WriteState {
            comp,
            offset,
            bytes,
        } => {
            let r = sim.write_state(comp, offset, &bytes);
            debug_assert!(r.is_ok(), "pre-validated state write refused: {r:?}");
            published.publish(sim, shared);
        }
        Command::ReadRecords {
            comp,
            since,
            max,
            deferred,
        } => match read_records(sim, comp, since, max) {
            Ok((r, values, ticks)) => deferred.resolve(Box::new(move |_| {
                Ok(Records {
                    count: r.count as f64,
                    first: r.first as f64,
                    values: Uint32Array::new(values),
                    ticks: ticks
                        .map(|t| Float64Array::new(t.into_iter().map(|t| t as f64).collect())),
                })
            })),
            Err(e) => deferred.reject(core_err(e)),
        },
    }
}

/// Copy `len` bytes of `comp`'s byte state from `offset`, checking the range before allocating.
fn read_state(sim: &CoreSim, comp: u32, offset: usize, len: usize) -> Result<Vec<u8>, SimError> {
    sim.state_layout(comp)?.check_read(comp, offset, len)?;
    let mut out = vec![0; len];
    sim.read_state(comp, offset, &mut out)?;
    Ok(out)
}

/// Copy recorder `comp`'s records from `since` on, at most `max` (default: all it holds), with
/// their ticks if it records them. Returns `(records, values, ticks)`.
type RecordsParts = (sim_core::Records, Vec<u32>, Option<Vec<u64>>);
fn read_records(
    sim: &CoreSim,
    comp: u32,
    since: u64,
    max: Option<u32>,
) -> Result<RecordsParts, SimError> {
    let StateLayout::Records { capacity, ticks } = sim.state_layout(comp)? else {
        return Err(SimError::NotARecorder(comp));
    };
    let n = max.map_or(capacity, |m| m.min(capacity)) as usize;
    let mut values = vec![0; n];
    let mut ticks = ticks.then(|| vec![0; n]);
    let r = sim.read_records(comp, since, &mut values, ticks.as_deref_mut())?;
    values.truncate(r.len);
    if let Some(t) = &mut ticks {
        t.truncate(r.len);
    }
    Ok((r, values, ticks))
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
