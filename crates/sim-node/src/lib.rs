//! Node.js native-addon surface for the Logigator simulation engine (plan §7.4).
//!
//! A thin `napi-rs` shim over [`sim_core::Simulation`]. The engine is owned by a dedicated worker
//! thread ([`worker`]); the JS object holds only a command channel + lock-free [`worker::Shared`]
//! status, so `getStatus`/`linkCount`/`componentCount` never block on a running sim and state reads
//! during a run are served coherently at a tick boundary (the copy-and-resume model, plan §6.4).
//!
//! This phase ships the synchronous surface (construct / `tick` / blocking `run` / `stop` / status /
//! `link` / `getOutputs` / `triggerInput`); `runAsync` and the coherent `snapshot` handoff are added
//! on top of the same machinery in the following commits.

mod worker;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use napi::bindgen_prelude::{Buffer, Object};
use napi::{Env, JsDeferred};
use napi_derive::napi;
use sim_core::Simulation as CoreSim;

use worker::{Command, Shared, UnitResolver, core_err};

/// One component as it crosses from JS (`{ type, inputs, outputs, ops? }`, plan §7.4). Mirrors the
/// public `BoardDescriptor` JS shape; napi requires binding-local object types.
#[napi(object)]
pub struct ComponentDescriptor {
    #[napi(js_name = "type")]
    pub ty: u32,
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub ops: Option<Vec<u32>>,
}

/// A board description (`{ links, components }`, plan §7.4).
#[napi(object)]
pub struct BoardDescriptor {
    pub links: u32,
    pub components: Vec<ComponentDescriptor>,
}

impl BoardDescriptor {
    /// Lower the JS shape to the core descriptor, validating each `type` against [`sim_core::CompType`].
    fn into_core(self) -> napi::Result<sim_core::BoardDescriptor> {
        let components = self
            .components
            .into_iter()
            .map(|c| {
                let raw = u16::try_from(c.ty).map_err(|_| {
                    napi::Error::from_reason(format!("component type {} out of range", c.ty))
                })?;
                let ty = sim_core::CompType::try_from(raw).map_err(core_err)?;
                Ok(sim_core::ComponentDescriptor {
                    ty,
                    inputs: c.inputs,
                    outputs: c.outputs,
                    ops: c.ops.unwrap_or_default(),
                })
            })
            .collect::<napi::Result<Vec<_>>>()?;
        Ok(sim_core::BoardDescriptor {
            link_count: self.links,
            components,
        })
    }
}

/// How a run should terminate (plan §7.4). `threads` is accepted for API parity and ignored until
/// the adaptive parallel driver lands in plan phase 6.
#[napi(object)]
pub struct RunConfig {
    pub ticks: Option<f64>,
    pub ms: Option<f64>,
    pub threads: Option<u32>,
}

/// Run status as serialized to JS (plan §7.4); `state` is the numeric [`sim_core::SimState`] and
/// `parallel` is always `false` until phase 6.
#[napi(object)]
pub struct JsStatus {
    pub state: u32,
    pub tick: f64,
    pub speed: u32,
    pub link_count: u32,
    pub component_count: u32,
    pub parallel: bool,
}

/// Parse a run config into `(ticks, timeout)`; a missing `ticks` means unbounded (`u64::MAX`).
fn parse_run(cfg: Option<RunConfig>) -> (u64, Option<Duration>) {
    let cfg = cfg.unwrap_or(RunConfig {
        ticks: None,
        ms: None,
        threads: None,
    });
    let ticks = cfg.ticks.map(|t| t.max(0.0) as u64).unwrap_or(u64::MAX);
    let timeout = cfg.ms.map(|m| Duration::from_secs_f64(m.max(0.0) / 1000.0));
    (ticks, timeout)
}

/// A single owned simulation (plan D12). `destroy()` (or GC → `Drop`) stops and joins the worker.
#[napi]
pub struct Simulation {
    /// `None` only after `destroy()`. Dropping it disconnects the channel so the worker exits.
    tx: Option<Sender<Command>>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl Simulation {
    /// Spawn the worker thread that owns `sim` and seed the published status from it.
    fn spawn(sim: CoreSim) -> napi::Result<Self> {
        let status = sim.status();
        let shared = Arc::new(Shared {
            state: AtomicU8::new(worker::STOPPED),
            tick: AtomicU64::new(status.tick),
            speed: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            link_count: status.link_count,
            component_count: status.component_count,
        });
        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        let join = std::thread::Builder::new()
            .name("sim-node-worker".into())
            .spawn(move || worker::run_worker(sim, sh, rx))
            .map_err(core_err)?;
        Ok(Simulation {
            tx: Some(tx),
            shared,
            join: Some(join),
        })
    }

    /// The live command sender (panics only if called after `destroy()` — not reachable from JS,
    /// which loses its handle on destroy).
    fn tx(&self) -> &Sender<Command> {
        self.tx.as_ref().expect("simulation already destroyed")
    }

    /// Send a command and block the calling (JS) thread on its oneshot reply. The worker serves it
    /// at the next tick boundary; when stopped that is immediate.
    fn request<T>(&self, build: impl FnOnce(Sender<T>) -> Command) -> napi::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        self.tx()
            .send(build(rtx))
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        rrx.recv()
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))
    }
}

#[napi]
impl Simulation {
    /// Build from a `BoardDescriptor` object (`{ links, components }`, plan §7.4).
    #[napi(constructor)]
    pub fn new(board: BoardDescriptor) -> napi::Result<Self> {
        let desc = board.into_core()?;
        Simulation::spawn(CoreSim::from_descriptor(&desc).map_err(core_err)?)
    }

    /// Build from a compact `.lgb` binary board (plan §7.4).
    #[napi(factory, js_name = "fromBinary")]
    pub fn from_binary(buf: Buffer) -> napi::Result<Self> {
        let desc = sim_core::codec::decode_board(&buf).map_err(core_err)?;
        Simulation::spawn(CoreSim::from_descriptor(&desc).map_err(core_err)?)
    }

    /// Build from a JSON `BoardDescriptor` string (the debug path, plan §7.4).
    #[napi(factory, js_name = "fromJson")]
    pub fn from_json(json: String) -> napi::Result<Self> {
        let desc: sim_core::BoardDescriptor = serde_json::from_str(&json).map_err(core_err)?;
        Simulation::spawn(CoreSim::from_descriptor(&desc).map_err(core_err)?)
    }

    /// One deterministic step.
    #[napi]
    pub fn tick(&self) -> napi::Result<()> {
        self.request(Command::Tick)
    }

    /// Blocking run-to-completion (the old `synchronized: true`). **Requires** a finite `ticks` or
    /// `ms`; an unbounded blocking run would park the Node event loop forever — use `runAsync`.
    #[napi]
    pub fn run(&self, config: Option<RunConfig>) -> napi::Result<()> {
        let (ticks, timeout) = parse_run(config);
        if ticks == u64::MAX && timeout.is_none() {
            return Err(napi::Error::from_reason(
                "run() needs a finite `ticks` or `ms` bound; an unbounded run would park the event loop — use runAsync()",
            ));
        }
        self.request(|reply| Command::RunBlocking {
            ticks,
            timeout,
            reply,
        })?
    }

    /// Background run: returns a `Promise` that resolves when the bound (`ticks`/`ms`) is reached or
    /// `stop()` interrupts it. An unbounded `runAsync` is allowed (and is the way to drive a live,
    /// interruptible simulation); the worker ticks in batches and serves `snapshot`/`triggerInput`
    /// between them, so the Node event loop and in-run state reads stay responsive (plan §7.4).
    #[napi(js_name = "runAsync")]
    pub fn run_async<'env>(
        &self,
        env: &'env Env,
        config: Option<RunConfig>,
    ) -> napi::Result<Object<'env>> {
        let (ticks, timeout) = parse_run(config);
        let (deferred, promise): (JsDeferred<(), UnitResolver>, Object<'env>) =
            env.create_deferred()?;
        self.tx()
            .send(Command::RunAsync {
                ticks,
                timeout,
                deferred,
            })
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        Ok(promise)
    }

    /// Cooperatively interrupt a `runAsync` run at the next batch boundary.
    #[napi]
    pub fn stop(&self) {
        self.shared.stop.store(true, Relaxed);
    }

    /// Typed run status (plan §7.4), read lock-free — never blocks on the running sim.
    #[napi(js_name = "getStatus")]
    pub fn status(&self) -> JsStatus {
        JsStatus {
            state: self.shared.state.load(Relaxed) as u32,
            tick: self.shared.tick.load(Relaxed) as f64,
            speed: self.shared.speed.load(Relaxed),
            link_count: self.shared.link_count,
            component_count: self.shared.component_count,
            parallel: false,
        }
    }

    #[napi(js_name = "linkCount")]
    pub fn link_count(&self) -> u32 {
        self.shared.link_count
    }

    #[napi(js_name = "componentCount")]
    pub fn component_count(&self) -> u32 {
        self.shared.component_count
    }

    /// Powered value of a single link (coherent only when stopped, plan §7.4).
    #[napi]
    pub fn link(&self, id: u32) -> napi::Result<bool> {
        self.request(|reply| Command::Link { id, reply })
    }

    /// One byte (`0`/`1`) per output pin, component-major in submission order (plan §7.3/§7.4).
    #[napi(js_name = "getOutputs")]
    pub fn outputs(&self) -> napi::Result<Buffer> {
        Ok(self.request(Command::Outputs)?.into())
    }

    /// Apply external input to a `UserInput` at a tick boundary (`event`: `0` = Cont, `1` = Pulse).
    #[napi(js_name = "triggerInput")]
    pub fn trigger_input(&self, comp_id: u32, event: u32, state: Vec<bool>) -> napi::Result<()> {
        self.request(|reply| Command::Trigger {
            comp: comp_id,
            event: event as u8,
            state,
            reply,
        })?
    }

    /// Stop the run and join the worker thread deterministically (plan §7.7; GC → `Drop` is the
    /// safety net). Idempotent.
    #[napi]
    pub fn destroy(&mut self) {
        self.shared.stop.store(true, Relaxed);
        drop(self.tx.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Simulation {
    fn drop(&mut self) {
        self.destroy();
    }
}
