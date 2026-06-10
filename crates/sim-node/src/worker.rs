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
//! Phase 6 swaps the single-thread batch loop here for the adaptive rayon driver but keeps this
//! command / `Deferred` surface unchanged (plan §11, §7.4).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use napi::Result as NapiResult;
use sim_core::{InputEvent, Simulation as CoreSim};

/// `SimState::Stopped as u8` (plan §7.1) — the lifecycle value published when no run is active.
pub const STOPPED: u8 = sim_core::SimState::Stopped as u8;

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

/// A request to the simulation thread. Each reply rides a fresh oneshot `mpsc` channel (sync calls)
/// the JS thread blocks on, served at the next tick boundary.
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

/// Map any `Display` (e.g. `SimError`) to a `napi::Error`.
pub fn core_err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// The simulation thread: owns `sim`, serves commands until every `Sender` is dropped (the owning
/// `Simulation` was destroyed / GC'd), then returns so the join in `Drop` completes.
pub fn run_worker(mut sim: CoreSim, shared: Arc<Shared>, rx: Receiver<Command>) {
    while let Ok(cmd) = rx.recv() {
        handle(cmd, &mut sim, &shared);
    }
}

/// Serve one command at a tick boundary (`sim` is at rest here, so reads are coherent).
fn handle(cmd: Command, sim: &mut CoreSim, shared: &Arc<Shared>) {
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
            let _ = reply.send(run_blocking(sim, shared, ticks, timeout));
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
    shared
        .state
        .store(sim_core::SimState::Running as u8, Relaxed);
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
