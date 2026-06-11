//! The dedicated simulation thread and its command channel (plan §7.4, the "RunHandle-lite").
//!
//! `sim_core::Simulation` is **owned by one thread** and never shared behind a lock — every access
//! goes through a [`Command`] sent over an `mpsc` channel and is served at a tick boundary, where
//! `link_state` is coherent. The JS-thread getters that must never block on a running sim read
//! [`Shared`] (lock-free atomics the worker republishes each boundary) directly instead of sending a
//! command. Async methods (`runAsync`, `snapshot`) hand the worker a `napi` [`JsDeferred`] it resolves
//! from this thread when the result is ready — so no libuv pool thread is occupied for the run's
//! duration (plan §4.3 async note).
//!
//! A `runAsync` run ticks in **batches**; between batches the worker drains any queued commands
//! (snapshot / triggerInput / a single tick / stop via the flag), so in-run state retrieval is
//! served at a coherent boundary without competing for a lock (advisor). When `cfg.threads > 1` the
//! batch ticks go through the engine's adaptive parallel driver (`tick_adaptive`), handed the one
//! per-run rayon pool the worker built at the start of the run; the driver installs that pool only
//! on the ticks that actually parallelize, so small/ST ticks stay plain. The command / `Deferred`
//! surface is unchanged (plan §8/§7.4) and results are bit-identical to single-threaded (§8.6).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use napi::{Env, JsDeferred, Result as NapiResult};
use sim_core::{InputEvent, RunConfig, Simulation as CoreSim, SnapshotConfig};

use crate::JsSnapshot;

/// `SimState::Stopped as u8` (plan §7.1) — published when no run is active.
pub const STOPPED: u8 = sim_core::SimState::Stopped as u8;
/// `SimState::Running as u8` — published while a `runAsync`/`run` is in flight.
pub const RUNNING: u8 = sim_core::SimState::Running as u8;

/// Ticks per `runAsync` batch between command drains. Large enough to amortize the per-batch
/// bookkeeping, small enough to keep snapshot / stop latency low (analogous to the WASM batch).
const BATCH: u64 = 4096;

/// Per-phase frontier above which a tick parallelizes (plan §8.1). The JS `RunConfig` exposes only
/// `threads`, so the worker uses the engine's default threshold.
fn par_threshold() -> usize {
    RunConfig::default().par_threshold
}

/// Resolver for the `runAsync` promise: a boxed closure so the `JsDeferred`'s `Resolver` type is
/// nameable in [`Command`] (the closure itself is built on this thread at resolve time).
pub type UnitResolver = Box<dyn FnOnce(Env) -> NapiResult<()> + Send>;
/// Resolver for the `snapshot` promise (builds the JS `Buffer`s on the JS thread from the bytes the
/// worker copied at the boundary).
pub type SnapResolver = Box<dyn FnOnce(Env) -> NapiResult<JsSnapshot> + Send>;

/// Lock-free status the worker republishes at each tick boundary; the JS-thread getters
/// (`getStatus`/`linkCount`/`componentCount`) read it directly so they never block on the running
/// sim (advisor / plan §7.4). `link_count`/`component_count` are immutable after compile.
pub struct Shared {
    pub state: AtomicU8,
    pub tick: AtomicU64,
    pub speed: AtomicU32,
    /// Cooperative stop flag for an in-flight `runAsync`; the run loop checks it each tick.
    pub stop: AtomicBool,
    /// Whether the most recent batch/run took the parallel path (plan §7.2 `Status.parallel`).
    pub parallel: AtomicBool,
    pub link_count: u32,
    pub component_count: u32,
}

/// A request to the simulation thread. Each sync reply rides a fresh oneshot `mpsc` channel the JS
/// thread blocks on; the async `RunAsync` carries a `JsDeferred` resolved when the run ends.
pub enum Command {
    /// One deterministic step.
    Tick(Sender<()>),
    /// Blocking run-to-completion on the sim thread; the JS thread is parked on `reply` (the old
    /// `synchronized: true`). `Err` if a background run is already in progress. `threads > 1` engages
    /// the adaptive parallel driver (plan §8).
    RunBlocking {
        ticks: u64,
        timeout: Option<Duration>,
        threads: usize,
        reply: Sender<NapiResult<()>>,
    },
    /// Background run; the promise resolves when the bound is reached or `stop()` interrupts it.
    /// `threads > 1` engages the adaptive parallel driver across the batched run (plan §8).
    RunAsync {
        ticks: u64,
        timeout: Option<Duration>,
        threads: usize,
        deferred: JsDeferred<(), UnitResolver>,
    },
    /// Apply external input to a `UserInput` at this tick boundary.
    Trigger {
        comp: u32,
        event: u8,
        state: Vec<bool>,
        reply: Sender<NapiResult<()>>,
    },
    /// Coherent single-link read (coherent only when stopped, plan §7.4).
    Link { id: u32, reply: Sender<bool> },
    /// One byte per output pin, component-major (plan §7.3 `getOutputs`).
    Outputs(Sender<Vec<u8>>),
    /// Coherent tick-boundary snapshot; the promise resolves with the copied bytes (plan §6.4).
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
    /// The per-run rayon pool (built once, reused for every batch), `None` for a single-threaded
    /// run. The worker installs it around the batch so the parallel driver uses it (no per-batch
    /// pool build) and is capped at `cfg.threads`.
    pool: Option<rayon::ThreadPool>,
    /// Per-phase parallelize threshold passed to `tick_adaptive` (plan §8.1).
    threshold: usize,
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
    loop {
        if active.is_some() {
            advance_batch(&mut sim, &shared, &mut active);
            // Drain anything queued, at this coherent boundary.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => handle(cmd, &mut sim, &shared, &mut active),
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
                Ok(cmd) => handle(cmd, &mut sim, &shared, &mut active),
                Err(_) => return, // owner dropped, no run in flight
            }
        }
    }
}

/// Tick one batch of an active run, republish status, and resolve the promise if the run ended.
/// A parallel run (its `pool` is `Some`) installs the per-run pool around the whole batch, so the
/// adaptive driver shares it across every tick (the pool is built once, in the `RunAsync` handler).
fn advance_batch(sim: &mut CoreSim, shared: &Arc<Shared>, active: &mut Option<Active>) {
    let mut a = active
        .take()
        .expect("advance_batch called with no active run");

    let ended = match a.pool.take() {
        Some(pool) => {
            let ended = step_batch(sim, shared, &mut a, Some(&pool));
            a.pool = Some(pool);
            ended
        }
        None => step_batch(sim, shared, &mut a, None),
    };
    shared.parallel.store(sim.status().parallel, Relaxed);

    let now = Instant::now();
    let dt = now.duration_since(a.window_start).as_secs_f64();
    if dt > 0.0 {
        let dticks = a.done - a.window_tick;
        shared.speed.store((dticks as f64 / dt) as u32, Relaxed);
    }
    a.window_start = now;
    a.window_tick = a.done;
    shared.tick.store(sim.tick_count(), Relaxed);

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
/// stop/snapshot servicing. With a `pool`, steps go through the adaptive driver (which installs the
/// pool only on the ticks that actually parallelize); without one they are plain single-threaded
/// ticks.
fn step_batch(
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    a: &mut Active,
    pool: Option<&rayon::ThreadPool>,
) -> bool {
    if a.timeout.is_some_and(|t| a.start.elapsed() >= t) {
        return true;
    }
    for _ in 0..BATCH {
        if shared.stop.load(Relaxed) || a.done >= a.ticks {
            return true;
        }
        match pool {
            Some(p) => sim.tick_adaptive(p, a.threshold),
            None => sim.tick(),
        }
        a.done += 1;
    }
    false
}

/// Serve one command at a tick boundary (`sim` is at rest here, so reads are coherent).
fn handle(cmd: Command, sim: &mut CoreSim, shared: &Arc<Shared>, active: &mut Option<Active>) {
    match cmd {
        Command::Tick(reply) => {
            sim.tick();
            shared.tick.store(sim.tick_count(), Relaxed);
            let _ = reply.send(());
        }
        Command::RunBlocking {
            ticks,
            timeout,
            threads,
            reply,
        } => {
            let r = if active.is_some() {
                Err(napi::Error::from_reason(
                    "a background run is in progress; stop() it first",
                ))
            } else {
                run_blocking(sim, shared, ticks, timeout, threads)
            };
            let _ = reply.send(r);
        }
        Command::RunAsync {
            ticks,
            timeout,
            threads,
            deferred,
        } => {
            if active.is_some() {
                deferred.reject(napi::Error::from_reason("a run is already in progress"));
            } else {
                // Build the per-run pool once (reused for every batch); reject the promise rather
                // than abort if the OS can't give us the threads.
                let pool = if threads > 1 {
                    match rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
                        Ok(p) => Some(p),
                        Err(e) => {
                            deferred.reject(core_err(e));
                            return;
                        }
                    }
                } else {
                    None
                };
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
                    pool,
                    threshold: par_threshold(),
                });
            }
        }
        Command::Trigger {
            comp,
            event,
            state,
            reply,
        } => {
            let r = InputEvent::try_from(event)
                .map_err(|v| {
                    napi::Error::from_reason(format!(
                        "invalid input event {v} (want 0=cont, 1=pulse)"
                    ))
                })
                .and_then(|ev| sim.trigger_input(comp, ev, &state).map_err(core_err));
            let _ = reply.send(r);
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
            // JS-thread resolver wraps the owned `Vec`s into `Buffer`s — copy-and-resume (§6.4).
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

/// Produce a snapshot's owned byte buffers at a tick boundary (plan §6.4). `Full` → packed
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
/// `stop()` included — can arrive mid-run). With `threads > 1` the whole run goes to the adaptive
/// driver in one `sim.run` call (one pool build); otherwise it is the single-threaded tick loop.
fn run_blocking(
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    ticks: u64,
    timeout: Option<Duration>,
    threads: usize,
) -> NapiResult<()> {
    shared.stop.store(false, Relaxed);
    shared.state.store(RUNNING, Relaxed);
    let start = Instant::now();
    let start_tick = sim.tick_count();
    if threads > 1 {
        sim.run(RunConfig {
            ticks,
            timeout,
            threads,
            par_threshold: par_threshold(),
        })
        .map_err(core_err)?;
    } else {
        let mut done = 0u64;
        while done < ticks {
            if shared.stop.load(Relaxed) {
                break;
            }
            if timeout.is_some_and(|t| start.elapsed() >= t) {
                break;
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
    shared.parallel.store(sim.status().parallel, Relaxed);
    shared.tick.store(sim.tick_count(), Relaxed);
    shared.state.store(STOPPED, Relaxed);
    Ok(())
}
