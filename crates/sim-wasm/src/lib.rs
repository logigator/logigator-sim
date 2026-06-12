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

use sim_core::{InputEvent, SnapshotConfig};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};

/// Yield to the JS **event loop** (a macrotask, not just a microtask) so the page can render and
/// process input between tick batches. `setTimeout(_, 0)` is available in browsers, workers, and
/// Node alike — unlike `window`, which a worker/Node lacks.
#[wasm_bindgen(inline_js = "export function __yield_to_event_loop() { \
    return new Promise(function (resolve) { setTimeout(resolve, 0); }); }")]
extern "C" {
    fn __yield_to_event_loop() -> js_sys::Promise;
}

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

/// Typed `Status` as serialized to JS; `state` is the numeric [`sim_core::SimState`].
#[derive(serde::Serialize)]
struct WasmStatus {
    state: u8,
    tick: f64,
    speed: u32,
    link_count: u32,
    component_count: u32,
}

/// Metadata for one snapshot; the bytes themselves stay in linear memory.
///
/// `Full`: [`ptr`](Self::ptr)/[`len`](Self::len) point at the packed `link_state` bits (byte `l>>3`,
/// bit `l&7`). `Delta`: `ptr`/`len` are the changed link ids (`u32` LE) and
/// [`values_ptr`](Self::values_ptr)/[`values_len`](Self::values_len) the packed values (bit `i` ↔
/// `id[i]`). A view is valid until the next `tick()`/`run()`/allocating call — re-acquire it after
/// memory growth detaches the JS buffer.
#[wasm_bindgen]
pub struct SnapshotView {
    pub is_delta: bool,
    pub tick: f64,
    pub ptr: u32,
    pub len: u32,
    pub values_ptr: u32,
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
    pub fn new(descriptor: JsValue) -> Result<Simulation, JsError> {
        let desc: sim_core::BoardDescriptor =
            serde_wasm_bindgen::from_value(descriptor).map_err(js_err)?;
        Ok(Simulation::wrap(
            sim_core::Simulation::from_descriptor(&desc).map_err(js_err)?,
        ))
    }

    /// Build from a JSON `BoardDescriptor` string (the debug path).
    #[wasm_bindgen(js_name = fromJson)]
    pub fn from_json(json: &str) -> Result<Simulation, JsError> {
        let desc: sim_core::BoardDescriptor = serde_json::from_str(json).map_err(js_err)?;
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
    pub fn run(&self, config: JsValue) -> Result<(), JsError> {
        let cfg = WasmRunConfig::parse(config).map_err(js_err)?;
        if cfg.ticks.is_none() && cfg.ms.is_none() {
            return Err(JsError::new(
                "run() needs a finite `ticks` or `ms` bound; an unbounded run would freeze the tab — use runAsync()",
            ));
        }
        self.stop.set(false);
        self.inner.borrow_mut().run(cfg.to_core()).map_err(js_err)
    }

    /// Cooperative run: ticks in batches, yielding to the JS event loop between them so the page
    /// stays responsive. Resolves when the bound is reached or `stop()` is called. An unbounded
    /// `runAsync` is allowed (it yields), and is the way to drive a live, interruptible simulation.
    #[wasm_bindgen(js_name = runAsync)]
    pub fn run_async(&self, config: JsValue) -> js_sys::Promise {
        // Ticks between event-loop yields. Large enough to amortize the setTimeout turn, small
        // enough to keep stop() latency and frame pacing reasonable.
        const BATCH: u64 = 4096;

        let inner = self.inner.clone();
        let stop = self.stop.clone();
        stop.set(false);

        future_to_promise(async move {
            let cfg = WasmRunConfig::parse(config).map_err(|e| JsValue::from(js_err(e)))?;
            let rc = cfg.to_core();
            let timeout_ms = rc.timeout.map(|t| t.as_secs_f64() * 1000.0);
            let start = js_sys::Date::now();
            let mut done: u64 = 0;

            // Stopped, ticks exhausted, or timed out.
            let finished = |done: u64| {
                stop.get()
                    || done >= rc.ticks
                    || timeout_ms.is_some_and(|ms| js_sys::Date::now() - start >= ms)
            };

            loop {
                if finished(done) {
                    break;
                }

                let batch = (rc.ticks - done).min(BATCH);
                {
                    let mut sim = inner.borrow_mut();
                    for _ in 0..batch {
                        if stop.get() {
                            break;
                        }
                        sim.tick();
                        done += 1;
                    }
                } // drop the borrow before awaiting (no &mut held across a yield)

                // Re-check before yielding so a finished/stopped run resolves immediately, without
                // depending on a trailing event-loop turn (a timer firing). Only a run that has
                // more work to do yields.
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
    #[wasm_bindgen(js_name = getStatus)]
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

    /// Apply external input to a `UserInput` at a tick boundary (`event`: `0` = Cont, `1` = Pulse).
    /// `state` is a JS array of per-pin values (each coerced by truthiness, so `boolean[]` works).
    /// wasm-bindgen has no native bool-vector marshalling, hence the `Array`.
    #[wasm_bindgen(js_name = triggerInput)]
    pub fn trigger_input(
        &self,
        comp_id: u32,
        event: u8,
        state: js_sys::Array,
    ) -> Result<(), JsError> {
        let ev = InputEvent::try_from(event).map_err(|v| {
            JsError::new(&format!("invalid input event {v} (want 0=cont, 1=pulse)"))
        })?;
        let bits: Vec<bool> = state.iter().map(|v| v.is_truthy()).collect();
        self.inner
            .borrow_mut()
            .trigger_input(comp_id, ev, &bits)
            .map_err(js_err)
    }

    /// Free the simulation (alias for the generated `free()`); consumes the handle.
    pub fn destroy(self) {}
}
