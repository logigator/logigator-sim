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
//! served at a coherent boundary without competing for a lock (advisor). Phase 6 swaps the
//! single-thread batch loop for the adaptive rayon driver but keeps this command / `Deferred`
//! surface unchanged (plan §11, §7.4).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use napi::{Env, JsDeferred, Result as NapiResult};
use sim_core::{InputEvent, Simulation as CoreSim};

/// `SimState::Stopped as u8` (plan §7.1) — published when no run is active.
pub const STOPPED: u8 = sim_core::SimState::Stopped as u8;
/// `SimState::Running as u8` — published while a `runAsync`/`run` is in flight.
pub const RUNNING: u8 = sim_core::SimState::Running as u8;

/// Ticks per `runAsync` batch between command drains. Large enough to amortize the per-batch
/// bookkeeping, small enough to keep snapshot / stop latency low (analogous to the WASM batch).
const BATCH: u64 = 4096;

/// Resolver for the `runAsync` promise: a boxed closure so the `JsDeferred`'s `Resolver` type is
/// nameable in [`Command`] (the closure itself is built on this thread at resolve time).
pub type UnitResolver = Box<dyn FnOnce(Env) -> NapiResult<()> + Send>;

/// Lock-free status the worker republishes at each tick boundary; the JS-thread getters
/// (`getStatus`/`linkCount`/`componentCount`) read it directly so they never block on the running
/// sim (advisor / plan §7.4). `link_count`/`component_count` are immutable after compile.
pub struct Shared {
    pub state: AtomicU8,
    pub tick: AtomicU64,
    pub speed: AtomicU32,
    /// Cooperative stop flag for an in-flight `runAsync`; the run loop checks it each tick.
    pub stop: AtomicBool,
    pub link_count: u32,
    pub component_count: u32,
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
fn advance_batch(sim: &mut CoreSim, shared: &Arc<Shared>, active: &mut Option<Active>) {
    let a = active
        .as_mut()
        .expect("advance_batch called with no active run");
    let mut ended = false;
    for _ in 0..BATCH {
        if shared.stop.load(Relaxed) || a.done >= a.ticks {
            ended = true;
            break;
        }
        if a.timeout.is_some_and(|t| a.start.elapsed() >= t) {
            ended = true;
            break;
        }
        sim.tick();
        a.done += 1;
    }

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
        let a = active.take().expect("active checked above");
        shared.speed.store(0, Relaxed);
        shared.state.store(STOPPED, Relaxed);
        resolve_run(a.deferred);
    } else {
        shared.state.store(RUNNING, Relaxed);
    }
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
            reply,
        } => {
            let r = if active.is_some() {
                Err(napi::Error::from_reason(
                    "a background run is in progress; stop() it first",
                ))
            } else {
                run_blocking(sim, shared, ticks, timeout)
            };
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
    }
}

/// Run-to-completion inline on the sim thread (the JS thread is parked on the reply). Single-threaded
/// until the phase-6 driver lands; `threads`/`par_threshold` from the config are ignored for now.
fn run_blocking(
    sim: &mut CoreSim,
    shared: &Arc<Shared>,
    ticks: u64,
    timeout: Option<Duration>,
) -> NapiResult<()> {
    shared.stop.store(false, Relaxed);
    shared.state.store(RUNNING, Relaxed);
    let start = Instant::now();
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
