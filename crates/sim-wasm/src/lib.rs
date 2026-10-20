//! WebAssembly surface for the Logigator simulation engine.
//!
//! A thin `wasm-bindgen` shim over [`sim_core::Simulation`]. WASM is **single-threaded** — JS drives
//! the ticks — so every read is inherently coherent and snapshots are zero-copy: a `Full` snapshot
//! returns a pointer **directly into the live `link_state`** (valid until the next tick), and a
//! `Delta` points at the engine's accumulated changed-id / value buffers. Nothing is copied across
//! the wasm↔JS boundary for state read-out.
//!
//! The simulation is held behind `Rc<RefCell<…>>` so the cooperative `runAsync` future can drive it
//! across event-loop yields while `stop()` (a shared counter) interrupts it from the outside. All
//! methods take `&self` and borrow internally; in single-threaded JS the `RefCell` never contends.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use sim_core::{RunConfig, SnapshotConfig};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};
use web_time::Instant;

mod board;
mod input;
mod js;

/// Yield to the JS **event loop** (a macrotask, not just a microtask) so the page can render and
/// process input between tick batches.
///
/// A `MessageChannel` message rather than `setTimeout(_, 0)`: browsers clamp a timer nested more
/// than five deep to 4 ms, and every yield is scheduled from the previous one's callback, so a timer
/// would idle the thread 4 ms per batch. A message task has no clamp and still queues behind the
/// messages already posted to the worker. One channel serves every simulation; its waiters resolve
/// in post order. Node takes `setImmediate` instead, because a `MessagePort` with a handler keeps
/// the process alive.
#[wasm_bindgen(inline_js = r#"
const inNode = typeof process === 'object' && process !== null
  && typeof process.versions?.node === 'string' && typeof setImmediate === 'function';
let channel = null;
const waiters = [];
export function __yield_to_event_loop() {
  return new Promise(function (resolve) {
    if (inNode) {
      setImmediate(resolve);
    } else if (typeof MessageChannel === 'function') {
      if (channel === null) {
        channel = new MessageChannel();
        channel.port1.onmessage = function () { waiters.shift()(); };
      }
      waiters.push(resolve);
      channel.port2.postMessage(null);
    } else {
      setTimeout(resolve, 0);
    }
  });
}
"#)]
extern "C" {
    fn __yield_to_event_loop() -> js_sys::Promise;
}

/// Wall-clock length of a `runAsync` batch: well under one 60 Hz frame, so the thread can render
/// and handle messages between batches, and long enough that the yield's cost is negligible.
const BATCH: Duration = Duration::from_millis(5);

/// The `runAsync` loop: tick a batch, yield, until the run's bound is reached or `stop()` is
/// called.
async fn drive(
    inner: &RefCell<sim_core::Simulation>,
    run: &RunState,
    stops: u32,
    rc: RunConfig,
) -> Result<JsValue, JsValue> {
    let start = Instant::now();
    // Measure progress from the tick the run begins on. The engine's tick counter is monotonic, so
    // an unbounded `rc.ticks` (u64::MAX) simply never trips the budget test.
    let start_tick = inner.borrow().tick_count();
    let done = || inner.borrow().tick_count() - start_tick;

    // Stopped, ticks exhausted, or timed out.
    let finished = || {
        run.stops.get() != stops
            || done() >= rc.ticks
            || rc.timeout.is_some_and(|t| start.elapsed() >= t)
    };

    while !finished() {
        // Clamped to whatever remains of the overall ms bound, so the last batch can't overshoot
        // the deadline by a batch.
        let budget = match rc.timeout {
            Some(t) => t.saturating_sub(start.elapsed()).min(BATCH),
            None => BATCH,
        };
        let ticks = rc.ticks - done();
        inner.borrow_mut().run_slice(RunConfig {
            ticks,
            timeout: Some(budget),
        });
        // Re-check before yielding so a finished/stopped run resolves immediately, without
        // depending on a trailing event-loop turn. Only a run that has more work to do yields.
        if finished() {
            break;
        }
        JsFuture::from(__yield_to_event_loop()).await?;
    }
    Ok(JsValue::UNDEFINED)
}

/// Build a `JsError` from anything `Display` (e.g. [`sim_core::SimError`]).
fn js_err<E: std::fmt::Display>(e: E) -> JsError {
    JsError::new(&e.to_string())
}

/// TypeScript type declarations: named types for every `any`/`unknown` that would otherwise appear
/// in the generated `.d.ts`. The interface merging rules allow `interface Simulation` here to
/// augment the generated `class Simulation` with the methods that carry `skip_typescript`.
#[wasm_bindgen(typescript_custom_section)]
const TS_EXTRA: &str = r#"
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
    /** From the `runAsync` call until its promise settles. */
    running: boolean;
    tick: number;
    linkCount: number;
    componentCount: number;
}

/** Descriptor for one component in a {@link BoardDescriptor}. `negInputs` / `negOutputs`
 * list the pin indices (into `inputs` / `outputs`) that read or drive the inverted value. The
 * fields after them are per-type parameters, each taken by one type only. */
export interface ComponentDescriptor {
    type: number;
    inputs?: number[];
    outputs?: number[];
    negInputs?: number[];
    negOutputs?: number[];
    /** CLK (required): ticks low per cycle. */
    period?: number;
    /** CLK: the waveform, default `"square"`. */
    mode?: "pulse" | "square";
    /** RAM: when the outputs pick up a word, default `"async"`. */
    read?: "async" | "clocked";
    /** ROM: its contents, bit-packed words; zero-filled past the end. */
    data?: Uint8Array | number[];
    /** Recorder (required): the records kept. */
    capacity?: number;
    /** Recorder: also record each record's tick, default `false`. */
    recordTicks?: boolean;
    /** Addressable register (required): the data-bus width. */
    dataBus?: number;
}

/** Board description passed to the {@link Simulation} constructor or factory methods. */
export interface BoardDescriptor {
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
/// `id[i]`). Either way, [`invalid_ptr`](Self::invalid_ptr)/[`invalid_len`](Self::invalid_len) are
/// the ids (`u32` LE) of every invalid link, ascending. A view is valid until the next `tick()`/`run()`/allocating call — re-acquire it after
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
    #[wasm_bindgen(js_name = invalidPtr)]
    pub invalid_ptr: u32,
    /// Byte length of the invalid link ids.
    #[wasm_bindgen(js_name = invalidLen)]
    pub invalid_len: u32,
}

/// A single owned simulation. `destroy()`/`free()` drops it.
#[wasm_bindgen]
pub struct Simulation {
    inner: Rc<RefCell<sim_core::Simulation>>,
    run: Rc<RunState>,
}

/// Run status shared with an in-flight `runAsync`.
#[derive(Default)]
struct RunState {
    /// Bumped by every `stop()`. A `runAsync` ends once this moves past the value it started at, so
    /// a stopped run still waiting out its yield is not revived by the run that replaces it.
    stops: Cell<u32>,
    /// The `stops` value the newest `runAsync` started at, until its promise settles.
    current: Cell<Option<u32>>,
}

impl RunState {
    /// A `runAsync` is in flight and has not been stopped.
    fn live(&self) -> bool {
        self.current.get() == Some(self.stops.get())
    }
}

impl Simulation {
    fn wrap(sim: sim_core::Simulation) -> Simulation {
        Simulation {
            inner: Rc::new(RefCell::new(sim)),
            run: Rc::default(),
        }
    }
}

#[wasm_bindgen]
impl Simulation {
    /// Build from a `BoardDescriptor` object (`{ components }`).
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "BoardDescriptor")] descriptor: JsValue,
    ) -> Result<Simulation, JsError> {
        let desc = board::descriptor(&descriptor)?;
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
        let cfg = board::run_config(&config)?;
        // Checked on the lowered config so `Infinity` (which lowers to unbounded) is refused too.
        if cfg.is_unbounded() {
            return Err(JsError::new(
                "run() needs a finite `ticks` or `ms` bound; an unbounded run would freeze the tab — use runAsync()",
            ));
        }
        if self.run.live() {
            return Err(JsError::new(
                "a background run is in progress; stop() it first",
            ));
        }
        self.inner.borrow_mut().run(cfg);
        Ok(())
    }

    /// Cooperative run: ticks in time-sliced batches, yielding to the JS event loop between them so
    /// the page stays responsive. Resolves when the bound is reached or `stop()` is called. An
    /// unbounded `runAsync` is allowed (it yields), and is the way to drive a live, interruptible
    /// simulation. Rejects while another `runAsync` on this simulation is in flight and not
    /// stopped; after `stop()`, a new run may start straight away.
    ///
    /// Each batch runs for a fixed wall-clock slice rather than a fixed tick count, so the yield
    /// cadence — and thus frame pacing and `stop()` latency — stays constant regardless of board
    /// size: a tiny board ticks millions of times per slice, a heavy board only a few.
    #[wasm_bindgen(js_name = runAsync, skip_typescript)]
    pub fn run_async(&self, config: JsValue) -> Result<js_sys::Promise, JsError> {
        let rc = board::run_config(&config)?;
        if self.run.live() {
            return Ok(js_sys::Promise::reject(
                &JsError::new("a run is already in progress").into(),
            ));
        }
        let inner = self.inner.clone();
        let run = self.run.clone();
        let stops = run.stops.get();
        run.current.set(Some(stops));

        Ok(future_to_promise(async move {
            let result = drive(&inner, &run, stops, rc).await;
            // A run that was stopped and then replaced leaves the status to the run after it.
            if run.current.get() == Some(stops) {
                run.current.set(None);
            }
            result
        }))
    }

    /// Interrupt a `runAsync` run at the next batch boundary.
    pub fn stop(&self) {
        self.run.stops.set(self.run.stops.get().wrapping_add(1));
    }

    /// Typed run status.
    #[wasm_bindgen(js_name = getStatus, skip_typescript)]
    pub fn status(&self) -> Result<JsValue, JsError> {
        // The keys match Node's `getStatus()` (`SimStatus`).
        let s = self.inner.borrow().status();
        let obj = js_sys::Object::new();
        js::set(&obj, "running", &self.run.current.get().is_some().into())?;
        js::set(&obj, "tick", &(s.tick as f64).into())?;
        js::set(&obj, "linkCount", &s.link_count.into())?;
        js::set(&obj, "componentCount", &s.component_count.into())?;
        Ok(obj.into())
    }

    /// Powered value of a single link (always coherent — single-threaded). Throws for an id past
    /// the last link.
    pub fn link(&self, id: u32) -> Result<bool, JsError> {
        let sim = self.inner.borrow();
        let count = sim.status().link_count;
        if id >= count {
            return Err(js_err(sim_core::SimError::LinkOutOfRange { id, count }));
        }
        Ok(sim.link(id))
    }

    /// Coherent zero-copy snapshot. `Full` → `ptr`/`len` directly into the live
    /// `link_state`; `Delta` → the accumulated changed-id/value buffers. See [`SnapshotView`].
    pub fn snapshot(&self, delta: bool, threshold: f32) -> SnapshotView {
        let mut sim = self.inner.borrow_mut();
        let info = sim.snapshot(SnapshotConfig {
            delta,
            delta_threshold: threshold,
        });
        let invalid = sim.snapshot_invalid();
        let (invalid_ptr, invalid_len) = (
            invalid.as_ptr() as usize as u32,
            core::mem::size_of_val(invalid) as u32,
        );
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
                invalid_ptr,
                invalid_len,
            }
        } else {
            let words = sim.link_words();
            SnapshotView {
                is_delta: false,
                tick: info.tick as f64,
                ptr: words.as_ptr() as usize as u32,
                len: sim.status().link_count.div_ceil(8), // packed bytes
                values_ptr: 0,
                values_len: 0,
                invalid_ptr,
                invalid_len,
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
        let req = input::parse(&writes)?;
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
        self.inner
            .borrow()
            .read_state(comp_id, offset as usize, length as usize)
            .map_err(js_err)
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
        let since = sim_core::exact_index(since)
            .ok_or_else(|| JsError::new("readRecords: since must be a non-negative integer"))?;
        let max = max.map_or(usize::MAX, |m| m as usize);
        let r = self
            .inner
            .borrow()
            .read_records(comp_id, since, max)
            .map_err(js_err)?;

        let obj = js_sys::Object::new();
        js::set(&obj, "count", &(r.count as f64).into())?;
        js::set(&obj, "first", &(r.first as f64).into())?;
        js::set(&obj, "values", &js_sys::Uint32Array::from(&r.values[..]))?;
        if let Some(ticks) = r.ticks {
            let ticks: Vec<f64> = ticks.into_iter().map(|t| t as f64).collect();
            js::set(&obj, "ticks", &js_sys::Float64Array::from(&ticks[..]))?;
        }
        Ok(obj.into())
    }

    /// Free the simulation (alias for the generated `free()`); consumes the handle.
    pub fn destroy(self) {}
}

/// A `runAsync` holds its own references to the simulation, so freeing the handle (`destroy()`,
/// `free()` or the GC) stops it; otherwise it would tick on forever.
impl Drop for Simulation {
    fn drop(&mut self) {
        self.stop();
    }
}
