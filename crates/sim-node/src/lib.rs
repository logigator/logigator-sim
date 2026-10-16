//! Node.js native-addon surface for the Logigator simulation engine.
//!
//! A thin `napi-rs` shim over [`sim_core::Simulation`]. The engine is owned by a dedicated worker
//! thread ([`worker`]); the JS object holds only a command channel + lock-free [`worker::Shared`]
//! status, so `getStatus`/`linkCount`/`componentCount` never block on a running sim and state reads
//! during a run are served coherently at a tick boundary (the copy-and-resume model).
//!
//! `submitInput` never waits for the worker either: which components are inputs and how wide
//! never changes after compile, so the handle validates a request on the JS thread against its own
//! copy, sends it as one command and returns. `inputProgress` reads per-input event counters the
//! worker publishes.
//!
//! Component state follows the same split: `stateVersions` reads versions the worker publishes,
//! `stateLength` and the checks of `writeState` come from the descriptor (a write is then queued
//! like an input request), and `readState` / `readRecords` are copied on the worker at a tick
//! boundary, as `snapshot` is.

mod input;
mod worker;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use napi::bindgen_prelude::{
    Array, Buffer, Float64Array, Object, Uint8ArraySlice, Uint32Array, Uint32ArraySlice,
};
use napi::{Env, JsDeferred};
use napi_derive::napi;
use sim_core::{SimError, Simulation as CoreSim, StateLayout};

use input::InputRequest;
use worker::{
    BytesResolver, Command, RecordsResolver, Shared, SnapResolver, UnitResolver, core_err,
};

/// Lifecycle of a [`Simulation`], exported so consumers can name the numeric `getStatus().state`.
/// Mirrors [`sim_core::SimState`]; the discriminants are the frozen wire contract.
#[napi]
pub enum SimState {
    Uninitialized = 0,
    Stopped = 1,
    Running = 2,
    Stopping = 3,
}

impl From<u8> for SimState {
    fn from(v: u8) -> Self {
        match v {
            0 => SimState::Uninitialized,
            2 => SimState::Running,
            3 => SimState::Stopping,
            _ => SimState::Stopped,
        }
    }
}

/// One timed step of a `UserInput`'s pins (`submitInput`). Declares the TypeScript shape only:
/// requests are parsed by hand ([`input::InputRequest`]) so each bad entry gets its own error.
#[napi(object, object_from_js = false, object_to_js = false)]
pub struct InputFrame {
    /// Exactly one entry per output pin: `true`/`false` drive the pin's logical value, `null`
    /// keeps the value the pin shows when the frame is applied.
    #[napi(ts_type = "(boolean | null)[]")]
    pub state: Vec<Option<bool>>,
    /// Minimum ticks on the pins before the input's next frame replaces it; integer ≥ 1, default 1.
    pub hold: Option<u32>,
    /// `false` (default): an event, never dropped or merged by the engine. `true`: merges into the
    /// input's last pending frame when that one is coalescing too.
    pub coalesce: Option<bool>,
}

/// Frames for one `UserInput` in a `submitInput` request. Declares the TypeScript shape only.
#[napi(object, object_from_js = false, object_to_js = false)]
pub struct InputWrite {
    /// Component id of a `UserInput`.
    pub comp: u32,
    pub frames: Vec<InputFrame>,
    /// Drop the input's pending frames and end any running hold first; default `false`.
    pub replace: Option<bool>,
}

/// One component as it crosses from JS (`{ type, inputs, outputs, ops?, negInputs?,
/// negOutputs? }`). Mirrors the public `BoardDescriptor` JS shape; napi requires binding-local
/// object types. Negated-pin indices stay `u32` at the JS boundary (uniform with `inputs`/`outputs`,
/// guaranteed napi support) and are narrowed to `u16` in [`BoardDescriptor::into_core`].
#[napi(object)]
pub struct ComponentDescriptor {
    #[napi(js_name = "type")]
    pub ty: u32,
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub ops: Option<Vec<u32>>,
    pub neg_inputs: Option<Vec<u32>>,
    pub neg_outputs: Option<Vec<u32>>,
}

/// A board description (`{ links, components }`).
#[napi(object)]
pub struct BoardDescriptor {
    pub links: u32,
    pub components: Vec<ComponentDescriptor>,
}

/// Narrow JS-side `u32` negated-pin indices to the core `u16`, erroring on any > 65535 (the same
/// coarse range check serde enforces on the JSON/wasm path; `Board::compile` then validates against
/// the component's actual arity).
fn narrow_pins(v: Option<Vec<u32>>, field: &str) -> napi::Result<Vec<u16>> {
    v.unwrap_or_default()
        .into_iter()
        .map(|p| {
            u16::try_from(p).map_err(|_| {
                napi::Error::from_reason(format!("{field} pin index {p} out of range (max 65535)"))
            })
        })
        .collect()
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
                    neg_inputs: narrow_pins(c.neg_inputs, "negInputs")?,
                    neg_outputs: narrow_pins(c.neg_outputs, "negOutputs")?,
                })
            })
            .collect::<napi::Result<Vec<_>>>()?;
        Ok(sim_core::BoardDescriptor {
            link_count: self.links,
            components,
        })
    }
}

/// How a run should terminate.
#[napi(object)]
pub struct RunConfig {
    pub ticks: Option<f64>,
    pub ms: Option<f64>,
}

/// Run status as serialized to JS; `state` is a [`SimState`].
#[napi(object)]
pub struct JsStatus {
    pub state: SimState,
    pub tick: f64,
    pub speed: u32,
    pub link_count: u32,
    pub component_count: u32,
}

/// A coherent snapshot copied out at a tick boundary. `Full`:
/// [`links`](Self::links) is the packed `link_state` bitset (byte `l>>3`, bit `l&7`). `Delta`:
/// [`ids`](Self::ids) are the changed link ids (`u32` LE) and [`values`](Self::values) their packed
/// bits (bit `i` ↔ `ids[i]`). The `Buffer`s own their bytes (copied off the live state), so the sim
/// resumes immediately after the copy — the consumer parses them at its own pace.
#[napi(object)]
pub struct JsSnapshot {
    pub tick: f64,
    pub is_delta: bool,
    pub links: Option<Buffer>,
    pub ids: Option<Buffer>,
    pub values: Option<Buffer>,
}

/// Records read with `readRecords`: records `first … first + values.length` of `count`.
#[napi(object, object_from_js = false)]
pub struct Records {
    /// Records the recorder has ever written.
    pub count: f64,
    /// Index of `values[0]`; greater than the `since` asked for when records were overwritten
    /// before the read.
    pub first: f64,
    pub values: Uint32Array,
    /// The tick of each record, for a recorder with the tick flag only.
    pub ticks: Option<Float64Array>,
}

/// Parse a run config into `(ticks, timeout)`; a missing `ticks` means unbounded (`u64::MAX`).
fn parse_run(cfg: Option<RunConfig>) -> (u64, Option<Duration>) {
    let rc = match cfg {
        Some(c) => sim_core::RunConfig::from_float_bounds(c.ticks, c.ms),
        None => sim_core::RunConfig::default(),
    };
    (rc.ticks, rc.timeout)
}

/// The board's `UserInput`s as the JS thread sees them: fixed at compile except `submitted`.
struct Inputs {
    /// Input index of each component (input `i` is the `i`-th `UserInput`), `NOT_AN_INPUT` for
    /// every other component.
    index_of: Box<[u32]>,
    /// Output pin count per input.
    width: Box<[u32]>,
    /// Event frames submitted per input, wrapping. Pending = `submitted - Shared::retired`.
    submitted: Box<[u32]>,
}

const NOT_AN_INPUT: u32 = u32::MAX;

impl Inputs {
    fn new(desc: &sim_core::BoardDescriptor) -> Self {
        let mut index_of = vec![NOT_AN_INPUT; desc.components.len()];
        let mut width = Vec::new();
        for (c, comp) in desc.components.iter().enumerate() {
            if comp.ty == sim_core::CompType::UserInput {
                index_of[c] = width.len() as u32;
                width.push(comp.outputs.len() as u32);
            }
        }
        Inputs {
            index_of: index_of.into_boxed_slice(),
            submitted: vec![0; width.len()].into_boxed_slice(),
            width: width.into_boxed_slice(),
        }
    }

    fn index(&self, comp: u32) -> Option<usize> {
        match self.index_of.get(comp as usize) {
            Some(&i) if i != NOT_AN_INPUT => Some(i as usize),
            _ => None,
        }
    }
}

/// What the JS thread knows of the board's component state, all fixed at compile: each
/// component's layout and the slot of its published version.
struct States {
    layout: Box<[StateLayout]>,
    /// Slot in `Shared::versions` per component, `NO_VERSION` for a component without state.
    slot: Box<[u32]>,
}

const NO_VERSION: u32 = u32::MAX;

impl States {
    fn new(desc: &sim_core::BoardDescriptor) -> Self {
        let layout: Box<[StateLayout]> = desc.components.iter().map(StateLayout::of).collect();
        let mut next = 0;
        let slot = layout
            .iter()
            .map(|l| {
                if l.is_versioned() {
                    next += 1;
                    next - 1
                } else {
                    NO_VERSION
                }
            })
            .collect();
        States { layout, slot }
    }

    /// Components with a version, in slot order: what the worker publishes.
    fn versioned(&self) -> Box<[u32]> {
        (0..self.slot.len() as u32)
            .filter(|&c| self.slot[c as usize] != NO_VERSION)
            .collect()
    }

    fn layout(&self, comp: u32) -> napi::Result<StateLayout> {
        self.layout
            .get(comp as usize)
            .copied()
            .ok_or_else(|| core_err(SimError::ComponentOutOfRange(comp)))
    }
}

/// A single owned simulation. `destroy()` (or GC → `Drop`) stops and joins the worker.
#[napi]
pub struct Simulation {
    /// `None` only after `destroy()`. Dropping it disconnects the channel so the worker exits.
    tx: Option<Sender<Command>>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    inputs: Inputs,
    states: States,
}

impl Simulation {
    /// Compile `desc`, spawn the worker thread that owns the simulation and seed the published
    /// status from it.
    fn spawn(desc: &sim_core::BoardDescriptor) -> napi::Result<Self> {
        let sim = CoreSim::from_descriptor(desc).map_err(core_err)?;
        let inputs = Inputs::new(desc);
        let states = States::new(desc);
        let versioned = states.versioned();
        let status = sim.status();
        let shared = Arc::new(Shared {
            state: AtomicU8::new(worker::STOPPED),
            tick: AtomicU64::new(status.tick),
            speed: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            link_count: status.link_count,
            component_count: status.component_count,
            retired: (0..inputs.width.len()).map(|_| AtomicU32::new(0)).collect(),
            versions: versioned.iter().map(|_| AtomicU32::new(0)).collect(),
        });
        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        let join = std::thread::Builder::new()
            .name("sim-node-worker".into())
            .spawn(move || worker::run_worker(sim, sh, rx, versioned))
            .map_err(core_err)?;
        Ok(Simulation {
            tx: Some(tx),
            shared,
            join: Some(join),
            inputs,
            states,
        })
    }

    /// The live command sender, or a clean error if the simulation was already `destroy()`-ed.
    /// (Never panic on a post-destroy call from JS — under `panic = "abort"` that would take down
    /// the whole Node process.)
    fn sender(&self) -> napi::Result<&Sender<Command>> {
        self.tx
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("simulation has been destroyed"))
    }

    /// Send a command and block the calling (JS) thread on its oneshot reply. The worker serves it
    /// at the next tick boundary; when stopped that is immediate.
    fn request<T>(&self, build: impl FnOnce(Sender<T>) -> Command) -> napi::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        self.sender()?
            .send(build(rtx))
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        rrx.recv()
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))
    }
}

#[napi]
impl Simulation {
    /// Build from a `BoardDescriptor` object (`{ links, components }`).
    #[napi(constructor)]
    pub fn new(board: BoardDescriptor) -> napi::Result<Self> {
        Simulation::spawn(&board.into_core()?)
    }

    /// Build from a compact `.lgb` binary board.
    #[napi(factory, js_name = "fromBinary")]
    pub fn from_binary(buf: Buffer) -> napi::Result<Self> {
        Simulation::spawn(&sim_core::codec::decode_board(&buf).map_err(core_err)?)
    }

    /// Build from a JSON `BoardDescriptor` string (the debug path).
    #[napi(factory, js_name = "fromJson")]
    pub fn from_json(json: String) -> napi::Result<Self> {
        let desc: sim_core::BoardDescriptor = serde_json::from_str(&json).map_err(core_err)?;
        Simulation::spawn(&desc)
    }

    /// One deterministic step.
    #[napi]
    pub fn tick(&self) -> napi::Result<()> {
        self.request(Command::Tick)
    }

    /// Blocking run-to-completion. **Requires** a finite `ticks` or
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
    /// interruptible simulation); the worker ticks in batches and serves `snapshot`/`submitInput`
    /// between them, so the Node event loop and in-run state reads stay responsive.
    #[napi(js_name = "runAsync", ts_return_type = "Promise<void>")]
    pub fn run_async<'env>(
        &self,
        env: &'env Env,
        config: Option<RunConfig>,
    ) -> napi::Result<Object<'env>> {
        let (ticks, timeout) = parse_run(config);
        let (deferred, promise): (JsDeferred<(), UnitResolver>, Object<'env>) =
            env.create_deferred()?;
        self.sender()?
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

    /// Typed run status, read lock-free — never blocks on the running sim.
    #[napi(js_name = "getStatus")]
    pub fn status(&self) -> JsStatus {
        JsStatus {
            state: self.shared.state.load(Relaxed).into(),
            tick: self.shared.tick.load(Relaxed) as f64,
            speed: self.shared.speed.load(Relaxed),
            link_count: self.shared.link_count,
            component_count: self.shared.component_count,
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

    /// Powered value of a single link (coherent only when stopped). Range-checked: an
    /// out-of-range id is an error, never a panic (under release `panic = "abort"` a panic would
    /// abort the process — `sim_core`'s `get` only `debug_assert!`s the bound).
    #[napi]
    pub fn link(&self, id: u32) -> napi::Result<bool> {
        if id >= self.shared.link_count {
            return Err(napi::Error::from_reason(format!(
                "link id {id} out of range (link_count = {})",
                self.shared.link_count
            )));
        }
        self.request(|reply| Command::Link { id, reply })
    }

    /// Coherent tick-boundary snapshot. Returns a `Promise<JsSnapshot>`; the worker
    /// produces the copy at the next tick boundary (where `link_state` is coherent) and resolves it,
    /// so the sim is never read concurrently with its writers. `delta` opts into delta snapshots;
    /// `threshold` is the changed-fraction at which a delta falls back to a `Full`.
    #[napi(js_name = "snapshot", ts_return_type = "Promise<JsSnapshot>")]
    pub fn snapshot<'env>(
        &self,
        env: &'env Env,
        delta: bool,
        threshold: f64,
    ) -> napi::Result<Object<'env>> {
        let (deferred, promise): (JsDeferred<JsSnapshot, SnapResolver>, Object<'env>) =
            env.create_deferred()?;
        self.sender()?
            .send(Command::Snapshot {
                delta,
                threshold: threshold as f32,
                deferred,
            })
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        Ok(promise)
    }

    /// One byte (`0`/`1`) per output pin, component-major in submission order.
    #[napi(js_name = "getOutputs")]
    pub fn outputs(&self) -> napi::Result<Buffer> {
        Ok(self.request(Command::Outputs)?.into())
    }

    /// Queue frames on one or more `UserInput`s. The request is validated as a whole on this
    /// thread — nothing is queued on any error — then handed to the worker without waiting; every
    /// write in it lands at the same tick boundary, and a later `tick()` still sees it.
    #[napi(js_name = "submitInput")]
    pub fn submit_input(
        &mut self,
        #[napi(ts_arg_type = "InputWrite[]")] writes: Array,
    ) -> napi::Result<()> {
        let req = InputRequest::parse(&writes)?;
        let inputs = &self.inputs;
        let retired = &self.shared.retired;
        // `retired` lags the worker, so this pending count can only over-report: the limit check
        // may refuse early but never lets a queue exceed it, and the worker never refuses.
        let events = req.with_writes(|writes| {
            sim_core::validate_input_writes(writes, |comp| {
                let i = inputs.index(comp)?;
                Some(sim_core::InputInfo {
                    width: inputs.width[i],
                    pending_events: inputs.submitted[i].wrapping_sub(retired[i].load(Relaxed)),
                })
            })
            .map_err(core_err)?;
            Ok::<_, napi::Error>(
                writes
                    .iter()
                    .map(|w| (inputs.index(w.comp).unwrap(), w.event_count() as u32))
                    .collect::<Vec<_>>(),
            )
        })?;
        self.sender()?
            .send(Command::SubmitInput {
                req,
                events: events.clone(),
            })
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        for (i, n) in events {
            self.inputs.submitted[i] = self.inputs.submitted[i].wrapping_add(n);
        }
        Ok(())
    }

    /// Fill `out[i]` with the number of input `i`'s event frames not yet applied (input `i` is the
    /// `i`-th `UserInput` of the descriptor). Counts a request from the moment `submitInput`
    /// returns, even before the worker has picked it up, and is exact once `tick()` / `run()`
    /// returns or `runAsync()` resolves; during a `runAsync` it lags by at most one batch, always
    /// high. Allocates nothing.
    #[napi(js_name = "inputProgress")]
    pub fn input_progress(
        &self,
        #[napi(ts_arg_type = "Uint32Array")] mut out: Uint32ArraySlice,
    ) -> napi::Result<()> {
        // SAFETY: a zero-copy view of the caller's array, written synchronously on the JS thread,
        // so no JS code runs (and could touch the buffer) while the slice is alive.
        let out = unsafe { out.as_mut() };
        let n = self.inputs.width.len();
        if out.len() < n {
            return Err(napi::Error::from_reason(format!(
                "inputProgress: out has {} entries, the board has {n} inputs",
                out.len()
            )));
        }
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = self.inputs.submitted[i].wrapping_sub(self.shared.retired[i].load(Relaxed));
        }
        Ok(())
    }

    /// Fill `out[i]` with the state version of component `compIds[i]`: a number that changes
    /// whenever the component's state does, 0 for a type without state. A new simulation (including
    /// one rebuilt from the same board) starts every version at 0. Read from versions the worker
    /// publishes after every batch and every command, so it never blocks: exact once `tick()` /
    /// `run()` returns or `runAsync()` resolves, at most one batch behind during a `runAsync`.
    /// Allocates nothing. Throws if a component id is out of range or `out` is shorter than
    /// `compIds`.
    #[napi(js_name = "stateVersions")]
    pub fn state_versions(
        &self,
        #[napi(ts_arg_type = "Uint32Array")] comp_ids: Uint32ArraySlice,
        #[napi(ts_arg_type = "Uint32Array")] mut out: Uint32ArraySlice,
    ) -> napi::Result<()> {
        // SAFETY: as in `input_progress`; `comp_ids` is only read.
        let out = unsafe { out.as_mut() };
        if out.len() < comp_ids.len() {
            return Err(napi::Error::from_reason(format!(
                "stateVersions: out has {} entries for {} components",
                out.len(),
                comp_ids.len()
            )));
        }
        for (o, &c) in out.iter_mut().zip(comp_ids.iter()) {
            *o = match self.states.slot.get(c as usize) {
                None => return Err(core_err(SimError::ComponentOutOfRange(c))),
                Some(&NO_VERSION) => 0,
                Some(&slot) => self.shared.versions[slot as usize].load(Relaxed),
            };
        }
        Ok(())
    }

    /// Size of the component's byte state (a RAM's memory); 0 for a type without byte state.
    #[napi(js_name = "stateLength")]
    pub fn state_length(&self, comp_id: u32) -> napi::Result<u32> {
        Ok(self.states.layout(comp_id)?.byte_len() as u32)
    }

    /// A copy of `length` bytes of the component's byte state from byte `offset`, taken at the next
    /// tick boundary. Rejects if the component id is out of range or `offset + length` exceeds
    /// `stateLength`, which is any non-empty range on a type without byte state.
    #[napi(js_name = "readState", ts_return_type = "Promise<Buffer>")]
    pub fn read_state<'env>(
        &self,
        env: &'env Env,
        comp_id: u32,
        offset: u32,
        length: u32,
    ) -> napi::Result<Object<'env>> {
        let (deferred, promise): (JsDeferred<Buffer, BytesResolver>, Object<'env>) =
            env.create_deferred()?;
        self.sender()?
            .send(Command::ReadState {
                comp: comp_id,
                offset: offset as usize,
                len: length as usize,
                deferred,
            })
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        Ok(promise)
    }

    /// Overwrite the component's byte state from byte `offset` with `bytes` (RAM only). Checked on
    /// this thread against the descriptor, then queued without waiting, in order with
    /// `submitInput`: it lands at the next tick boundary, no later than an input request submitted
    /// after it.
    #[napi(js_name = "writeState")]
    pub fn write_state(
        &self,
        comp_id: u32,
        offset: u32,
        #[napi(ts_arg_type = "Uint8Array")] bytes: Uint8ArraySlice,
    ) -> napi::Result<()> {
        self.states
            .layout(comp_id)?
            .check_write(comp_id, offset as usize, bytes.len())
            .map_err(core_err)?;
        self.sender()?
            .send(Command::WriteState {
                comp: comp_id,
                offset: offset as usize,
                bytes: bytes.to_vec(),
            })
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))
    }

    /// A recorder's records from index `since` on, oldest first, at most `max` of them (default:
    /// all it still holds), copied at the next tick boundary together with its count.
    #[napi(js_name = "readRecords", ts_return_type = "Promise<Records>")]
    pub fn read_records<'env>(
        &self,
        env: &'env Env,
        comp_id: u32,
        since: f64,
        max: Option<u32>,
    ) -> napi::Result<Object<'env>> {
        let (deferred, promise): (JsDeferred<Records, RecordsResolver>, Object<'env>) =
            env.create_deferred()?;
        // The largest record index a JS number holds exactly (`Number.MAX_SAFE_INTEGER`).
        if !(since >= 0.0 && since.fract() == 0.0 && since <= 9_007_199_254_740_991.0) {
            deferred.reject(napi::Error::from_reason(
                "readRecords: since must be a non-negative integer",
            ));
            return Ok(promise);
        }
        self.sender()?
            .send(Command::ReadRecords {
                comp: comp_id,
                since: since as u64,
                max,
                deferred,
            })
            .map_err(|_| napi::Error::from_reason("simulation worker terminated"))?;
        Ok(promise)
    }

    /// Stop the run and join the worker thread deterministically (GC → `Drop` is the
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
