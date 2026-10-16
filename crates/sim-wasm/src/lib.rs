//! WebAssembly surface for the Logigator simulation engine.
//!
//! A thin `wasm-bindgen` shim over [`sim_core::Simulation`]. WASM is **single-threaded** — JS drives
//! the ticks — so every read is inherently coherent and snapshots are zero-copy: a `Full` snapshot
//! returns a pointer **directly into the live `link_state`** (valid until the next tick), and a
//! `Delta` points at the engine's accumulated changed-id / value buffers. Nothing is copied across
//! the wasm↔JS boundary for state read-out.
//!
//! The simulation is held behind `Rc<RefCell<…>>` so the cooperative `runAsync` future can drive it
//! across event-loop yields while `stop()` (a shared flag) interrupts it from the outside. All
//! methods take `&self` and borrow internally; in single-threaded JS the `RefCell` never contends.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use sim_core::{SnapshotConfig, StateLayout};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};

mod input;

use input::InputRequest;

/// Yield to the JS **event loop** (a macrotask, not just a microtask) so the page can render and
/// process input between tick batches. `setTimeout(_, 0)` is available in browsers, workers, and
/// Node alike — unlike `window`, which a worker/Node lacks.
#[wasm_bindgen(inline_js = "export function __yield_to_event_loop() { \
    return new Promise(function (resolve) { setTimeout(resolve, 0); }); }")]
extern "C" {
    fn __yield_to_event_loop() -> js_sys::Promise;
}

/// `Number.MAX_SAFE_INTEGER`: the largest record index a JS number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Build a `JsError` from anything `Display` (e.g. [`sim_core::SimError`], a serde error).
fn js_err<E: std::fmt::Display>(e: E) -> JsError {
    JsError::new(&e.to_string())
}

/// `RunConfig` as it crosses from JS.
#[derive(serde::Deserialize, Default)]
struct WasmRunConfig {
    #[serde(default)]
    ticks: Option<f64>,
    #[serde(default)]
    ms: Option<f64>,
}

impl WasmRunConfig {
    /// Parse a JS config value; `undefined`/`null` is an empty config.
    fn parse(value: JsValue) -> Result<Self, serde_wasm_bindgen::Error> {
        if value.is_undefined() || value.is_null() {
            Ok(WasmRunConfig::default())
        } else {
            serde_wasm_bindgen::from_value(value)
        }
    }

    /// Lower to the core config (clamping shared with the Node binding).
    fn to_core(&self) -> sim_core::RunConfig {
        sim_core::RunConfig::from_float_bounds(self.ticks, self.ms)
    }
}

/// Typed `Status` as serialized to JS; `state` is the numeric [`sim_core::SimState`]. Fields are
/// camelCased to match the Node binding's `getStatus()` shape (the shared `SimStatus` contract).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WasmStatus {
    state: u8,
    tick: f64,
    speed: u32,
    link_count: u32,
    component_count: u32,
}

/// TypeScript type declarations: named types for every `any`/`unknown` that would otherwise appear
/// in the generated `.d.ts`. The interface merging rules allow `interface Simulation` here to
/// augment the generated `class Simulation` with the methods that carry `skip_typescript`.
#[wasm_bindgen(typescript_custom_section)]
const TS_EXTRA: &str = r#"
export declare namespace SimState {
    const Uninitialized: 0;
    const Stopped: 1;
    const Running: 2;
    const Stopping: 3;
}
export type SimState = (typeof SimState)[keyof typeof SimState];

/** One timed step of a `UserInput`'s pins, in a {@link InputWrite}. */
export interface InputFrame {
    /** Exactly one entry per output pin: `true`/`false` drive the pin's logical value, `null`
     * keeps the value the pin shows when the frame is applied. */
    state: (boolean | null)[];
    /** Minimum ticks on the pins before the input's next frame replaces it; integer ≥ 1, default 1. */
    hold?: number;
    /** `false` (default): an event, never dropped or merged by the engine. `true`: merges into the
     * input's last pending frame when that one is coalescing too. */
    coalesce?: boolean;
}

/** Frames for one `UserInput`, passed to {@link Simulation.submitInput}. */
export interface InputWrite {
    /** Component id of a `UserInput`. */
    comp: number;
    frames: InputFrame[];
    /** Drop the input's pending frames and end any running hold first; default `false`. */
    replace?: boolean;
}

/** Snapshot of simulation status returned by {@link Simulation.getStatus}. */
export interface SimStatus {
    state: SimState;
    tick: number;
    speed: number;
    linkCount: number;
    componentCount: number;
}

/** Descriptor for one component in a {@link BoardDescriptor}. `negInputs` / `negOutputs`
 * list the pin indices (into `inputs` / `outputs`) that read or drive the inverted value. */
export interface ComponentDescriptor {
    type: number;
    inputs: number[];
    outputs: number[];
    ops?: number[];
    negInputs?: number[];
    negOutputs?: number[];
}

/** Board description passed to the {@link Simulation} constructor or factory methods. */
export interface BoardDescriptor {
    links: number;
    components: ComponentDescriptor[];
}

/** Optional tick/time bounds for {@link Simulation.run} and {@link Simulation.runAsync}. */
export interface RunConfig {
    /** Maximum number of ticks to execute. */
    ticks?: number;
    /** Wall-clock time limit in milliseconds. */
    ms?: number;
}

/** Records read with {@link Simulation.readRecords}: records `first … first + values.length` of
 * `count`. */
export interface Records {
    /** Records the recorder has ever written. */
    count: number;
    /** Index of `values[0]`; greater than the `since` asked for when records were overwritten
     * before the read. */
    first: number;
    values: Uint32Array;
    /** The tick of each record, for a recorder with the tick flag only. */
    ticks?: Float64Array;
}

export interface Simulation {
    getStatus(): SimStatus;
    run(config?: RunConfig | null): void;
    runAsync(config?: RunConfig | null): Promise<void>;
    readRecords(compId: number, since: number, max?: number | null): Records;
}
"#;

/// Metadata for one snapshot; the bytes themselves stay in linear memory.
///
/// `Full`: [`ptr`](Self::ptr)/[`len`](Self::len) point at the packed `link_state` bits (byte `l>>3`,
/// bit `l&7`). `Delta`: `ptr`/`len` are the changed link ids (`u32` LE) and
/// [`values_ptr`](Self::values_ptr)/[`values_len`](Self::values_len) the packed values (bit `i` ↔
/// `id[i]`). A view is valid until the next `tick()`/`run()`/allocating call — re-acquire it after
/// memory growth detaches the JS buffer.
#[wasm_bindgen]
pub struct SnapshotView {
    #[wasm_bindgen(js_name = isDelta)]
    pub is_delta: bool,
    pub tick: f64,
    pub ptr: u32,
    pub len: u32,
    #[wasm_bindgen(js_name = valuesPtr)]
    pub values_ptr: u32,
    #[wasm_bindgen(js_name = valuesLen)]
    pub values_len: u32,
}

/// A single owned simulation. `destroy()`/`free()` drops it.
#[wasm_bindgen]
pub struct Simulation {
    inner: Rc<RefCell<sim_core::Simulation>>,
    /// Cooperative stop flag for an in-flight `runAsync`.
    stop: Rc<Cell<bool>>,
}

impl Simulation {
    fn wrap(sim: sim_core::Simulation) -> Simulation {
        Simulation {
            inner: Rc::new(RefCell::new(sim)),
            stop: Rc::new(Cell::new(false)),
        }
    }
}

#[wasm_bindgen]
impl Simulation {
    /// Build from a `BoardDescriptor` object (`{ links, components }`).
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "BoardDescriptor")] descriptor: JsValue,
    ) -> Result<Simulation, JsError> {
        let desc: sim_core::BoardDescriptor =
            serde_wasm_bindgen::from_value(descriptor).map_err(js_err)?;
        Ok(Simulation::wrap(
            sim_core::Simulation::from_descriptor(&desc).map_err(js_err)?,
        ))
    }

    /// Build from a compact `.lgb` binary board (one heap copy, deserialized inside wasm).
    #[wasm_bindgen(js_name = fromBinary)]
    pub fn from_binary(board_bin: &[u8]) -> Result<Simulation, JsError> {
        let desc = sim_core::codec::decode_board(board_bin).map_err(js_err)?;
        Ok(Simulation::wrap(
            sim_core::Simulation::from_descriptor(&desc).map_err(js_err)?,
        ))
    }

    /// One deterministic step.
    pub fn tick(&self) {
        self.inner.borrow_mut().tick();
    }

    /// Blocking run-to-completion. **Requires** a finite `ticks` or `ms` — an unbounded run would
    /// freeze the tab and `stop()` (which only acts between batches) could not interrupt it; use
    /// `runAsync` for that.
    #[wasm_bindgen(skip_typescript)]
    pub fn run(&self, config: JsValue) -> Result<(), JsError> {
        let cfg = WasmRunConfig::parse(config).map_err(js_err)?.to_core();
        // Checked on the lowered config so `Infinity` (which lowers to unbounded) is refused too.
        if cfg.is_unbounded() {
            return Err(JsError::new(
                "run() needs a finite `ticks` or `ms` bound; an unbounded run would freeze the tab — use runAsync()",
            ));
        }
        self.stop.set(false);
        self.inner.borrow_mut().run(cfg).map_err(js_err)
    }

    /// Cooperative run: ticks in time-sliced batches, yielding to the JS event loop between them so
    /// the page stays responsive. Resolves when the bound is reached or `stop()` is called. An
    /// unbounded `runAsync` is allowed (it yields), and is the way to drive a live, interruptible
    /// simulation.
    ///
    /// Each batch runs for a fixed wall-clock slice rather than a fixed tick count, so the yield
    /// cadence — and thus frame pacing and `stop()` latency — stays constant regardless of board
    /// size: a tiny board ticks millions of times per slice, a heavy board only a few.
    #[wasm_bindgen(js_name = runAsync, skip_typescript)]
    pub fn run_async(&self, config: JsValue) -> js_sys::Promise {
        // Wall-clock budget per batch. A frame slice (well under one 60 Hz frame) leaves the JS
        // thread time to render and process input between batches while keeping per-batch overhead
        // negligible. The core run loop samples the clock every CHECK_EVERY ticks but shrinks its
        // final window toward the deadline, so a batch tracks this slice closely even when a single
        // tick is heavy. Do not set lower than 4ms, as this is the minimum `setTimeout` will wait
        // and lowering this would lower the effective clock speed.
        const FRAME_BUDGET_MS: f64 = 5.0;

        let inner = self.inner.clone();
        let stop = self.stop.clone();
        stop.set(false);

        future_to_promise(async move {
            let cfg = WasmRunConfig::parse(config).map_err(|e| JsValue::from(js_err(e)))?;
            let rc = cfg.to_core();
            let timeout_ms = rc.timeout.map(|t| t.as_secs_f64() * 1000.0);
            let start = js_sys::Date::now();
            // Measure progress from the tick the run begins on. The engine's tick counter is
            // monotonic, so an unbounded `rc.ticks` (u64::MAX) simply never trips the budget test.
            let start_tick = inner.borrow().tick_count();

            // Stopped, ticks exhausted, or timed out.
            let finished = |done: u64| {
                stop.get()
                    || done >= rc.ticks
                    || timeout_ms.is_some_and(|ms| js_sys::Date::now() - start >= ms)
            };

            loop {
                let done = inner.borrow().tick_count() - start_tick;
                if finished(done) {
                    break;
                }

                // Slice the next batch to a frame, clamped to whatever remains of the overall ms
                // bound so the final batch can't overshoot the deadline by a frame. The core run
                // loop returns when either this slice elapses or the remaining ticks are spent.
                let budget_ms = match timeout_ms {
                    Some(ms) => (ms - (js_sys::Date::now() - start)).clamp(0.0, FRAME_BUDGET_MS),
                    None => FRAME_BUDGET_MS,
                };
                {
                    let mut sim = inner.borrow_mut();
                    let batch = sim_core::RunConfig::from_float_bounds(
                        Some((rc.ticks - done) as f64),
                        Some(budget_ms),
                    );
                    sim.run(batch).map_err(|e| JsValue::from(js_err(e)))?;
                } // drop the borrow before awaiting (no &mut held across a yield)

                // Re-check before yielding so a finished/stopped run resolves immediately, without
                // depending on a trailing event-loop turn (a timer firing). Only a run that has
                // more work to do yields.
                let done = inner.borrow().tick_count() - start_tick;
                if finished(done) {
                    break;
                }
                JsFuture::from(__yield_to_event_loop()).await?;
            }
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Interrupt a `runAsync` run at the next batch boundary.
    pub fn stop(&self) {
        self.stop.set(true);
    }

    /// Typed run status.
    #[wasm_bindgen(js_name = getStatus, skip_typescript)]
    pub fn status(&self) -> Result<JsValue, JsError> {
        let s = self.inner.borrow().status();
        let out = WasmStatus {
            state: s.state as u8,
            tick: s.tick as f64,
            speed: s.speed,
            link_count: s.link_count,
            component_count: s.component_count,
        };
        serde_wasm_bindgen::to_value(&out).map_err(js_err)
    }

    #[wasm_bindgen(js_name = linkCount)]
    pub fn link_count(&self) -> u32 {
        self.inner.borrow().status().link_count
    }

    #[wasm_bindgen(js_name = componentCount)]
    pub fn component_count(&self) -> u32 {
        self.inner.borrow().status().component_count
    }

    /// Powered value of a single link (always coherent — single-threaded).
    pub fn link(&self, id: u32) -> bool {
        self.inner.borrow().link(id)
    }

    /// Coherent zero-copy snapshot. `Full` → `ptr`/`len` directly into the live
    /// `link_state`; `Delta` → the accumulated changed-id/value buffers. See [`SnapshotView`].
    pub fn snapshot(&self, delta: bool, threshold: f32) -> SnapshotView {
        let mut sim = self.inner.borrow_mut();
        let info = sim.snapshot(SnapshotConfig {
            delta,
            delta_threshold: threshold,
        });
        if info.is_delta {
            let ids = sim.snapshot_ids();
            let vals = sim.snapshot_values();
            SnapshotView {
                is_delta: true,
                tick: info.tick as f64,
                ptr: ids.as_ptr() as usize as u32,
                len: core::mem::size_of_val(ids) as u32,
                values_ptr: vals.as_ptr() as usize as u32,
                values_len: vals.len() as u32,
            }
        } else {
            let words = sim.link_words();
            SnapshotView {
                is_delta: false,
                tick: info.tick as f64,
                ptr: words.as_ptr() as usize as u32,
                len: (info.changed.div_ceil(8)) as u32, // ceil(link_count / 8) packed bytes
                values_ptr: 0,
                values_len: 0,
            }
        }
    }

    /// One byte (`0`/`1`) per output pin, component-major in submission order.
    #[wasm_bindgen(js_name = getOutputs)]
    pub fn outputs(&self) -> Vec<u8> {
        self.inner.borrow().output_bytes()
    }

    /// Queue frames on one or more `UserInput`s. The request is validated as a whole; on any error
    /// nothing is queued. A frame submitted to an idle input is on its pins and wires after the next
    /// tick.
    #[wasm_bindgen(js_name = submitInput)]
    pub fn submit_input(
        &self,
        #[wasm_bindgen(unchecked_param_type = "InputWrite[]")] writes: JsValue,
    ) -> Result<(), JsError> {
        let req = InputRequest::parse(&writes)?;
        let mut sim = self.inner.borrow_mut();
        req.with_writes(|writes| sim.submit_input(writes))
            .map_err(js_err)
    }

    /// Fill `out[i]` with the number of input `i`'s event frames not yet applied (input `i` is the
    /// `i`-th `UserInput` of the descriptor). `out` needs at least one entry per input; reuse it
    /// across calls.
    #[wasm_bindgen(js_name = inputProgress)]
    pub fn input_progress(&self, out: &mut [u32]) -> Result<(), JsError> {
        let sim = self.inner.borrow();
        let n = sim.input_count() as usize;
        if out.len() < n {
            return Err(JsError::new(&format!(
                "inputProgress: out has {} entries, the board has {n} inputs",
                out.len()
            )));
        }
        sim.input_progress(out);
        Ok(())
    }

    /// Fill `out[i]` with the state version of component `compIds[i]`: a number that changes
    /// whenever the component's state does, 0 for a type without state. A new simulation (including
    /// one rebuilt from the same board) starts every version at 0. Poll it to re-read only what
    /// moved; reuse both arrays across calls. Throws if a component id is out of range or `out` is
    /// shorter than `compIds`.
    #[wasm_bindgen(js_name = stateVersions)]
    pub fn state_versions(&self, comp_ids: &[u32], out: &mut [u32]) -> Result<(), JsError> {
        if out.len() < comp_ids.len() {
            return Err(JsError::new(&format!(
                "stateVersions: out has {} entries for {} components",
                out.len(),
                comp_ids.len()
            )));
        }
        let sim = self.inner.borrow();
        for (o, &c) in out.iter_mut().zip(comp_ids) {
            *o = sim.state_version(c).map_err(js_err)?;
        }
        Ok(())
    }

    /// Size of the component's byte state (a RAM's memory); 0 for a type without byte state.
    #[wasm_bindgen(js_name = stateLength)]
    pub fn state_length(&self, comp_id: u32) -> Result<u32, JsError> {
        let len = self.inner.borrow().state_len(comp_id).map_err(js_err)?;
        Ok(len as u32)
    }

    /// A copy of `length` bytes of the component's byte state from byte `offset`. Throws if the
    /// component id is out of range or `offset + length` exceeds `stateLength`, which is any
    /// non-empty range on a type without byte state.
    #[wasm_bindgen(js_name = readState)]
    pub fn read_state(&self, comp_id: u32, offset: u32, length: u32) -> Result<Vec<u8>, JsError> {
        let sim = self.inner.borrow();
        let (offset, length) = (offset as usize, length as usize);
        // Check before allocating, so a bad length is an error rather than a huge allocation.
        let layout = sim.state_layout(comp_id).map_err(js_err)?;
        layout.check_read(comp_id, offset, length).map_err(js_err)?;
        let mut out = vec![0; length];
        sim.read_state(comp_id, offset, &mut out).map_err(js_err)?;
        Ok(out)
    }

    /// Overwrite the component's byte state from byte `offset` with `bytes` (RAM only).
    #[wasm_bindgen(js_name = writeState)]
    pub fn write_state(&self, comp_id: u32, offset: u32, bytes: &[u8]) -> Result<(), JsError> {
        self.inner
            .borrow_mut()
            .write_state(comp_id, offset as usize, bytes)
            .map_err(js_err)
    }

    /// A recorder's records from index `since` on, oldest first, at most `max` of them (default:
    /// all it still holds).
    #[wasm_bindgen(js_name = readRecords, skip_typescript)]
    pub fn read_records(
        &self,
        comp_id: u32,
        since: f64,
        max: Option<u32>,
    ) -> Result<JsValue, JsError> {
        if !(since >= 0.0 && since.fract() == 0.0 && since <= MAX_SAFE_INTEGER) {
            return Err(JsError::new(
                "readRecords: since must be a non-negative integer",
            ));
        }
        let sim = self.inner.borrow();
        let (capacity, has_ticks) = match sim.state_layout(comp_id).map_err(js_err)? {
            StateLayout::Records { capacity, ticks } => (capacity, ticks),
            _ => return Err(js_err(sim_core::SimError::NotARecorder(comp_id))),
        };
        let n = max.map_or(capacity, |m| m.min(capacity)) as usize;
        let mut values = vec![0u32; n];
        let mut ticks = has_ticks.then(|| vec![0u64; n]);
        let r = sim
            .read_records(comp_id, since as u64, &mut values, ticks.as_deref_mut())
            .map_err(js_err)?;

        let obj = js_sys::Object::new();
        let set = |k: &str, v: &JsValue| {
            js_sys::Reflect::set(&obj, &JsValue::from_str(k), v)
                .map_err(|_| JsError::new("readRecords: building the result"))
        };
        set("count", &JsValue::from_f64(r.count as f64))?;
        set("first", &JsValue::from_f64(r.first as f64))?;
        set(
            "values",
            &js_sys::Uint32Array::from(&values[..r.len]).into(),
        )?;
        if let Some(ticks) = ticks {
            let ticks: Vec<f64> = ticks[..r.len].iter().map(|&t| t as f64).collect();
            set("ticks", &js_sys::Float64Array::from(&ticks[..]).into())?;
        }
        Ok(obj.into())
    }

    /// Free the simulation (alias for the generated `free()`); consumes the handle.
    pub fn destroy(self) {}
}
