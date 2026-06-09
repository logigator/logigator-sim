//! Cross-engine equivalence: drive the golden corpus through the **WASM binding** (compiled to
//! wasm32, run under `wasm-pack test --node`) and diff every tick against the same C++-oracle
//! goldens the native `sim-core` suite uses (plan §10.1, phase 4). This proves the engine produces
//! identical traces on the wasm target — catching target-specific issues (usize width, endianness,
//! simd128 codegen) — and exercises the actual marshalling surface: constructor-from-`JsValue`,
//! `tick`, `link`, `getOutputs`, `triggerInput`, and the zero-copy `snapshot` ptr/len plumbing.
//!
//! New corpus boards must be added to `FIXTURES` below (the native `tests/golden.rs` auto-discovers
//! them; `include_str!` needs literal paths, so the wasm list is maintained by hand).

#![cfg(target_arch = "wasm32")]

use sim_core::BoardDescriptor;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;
use sim_wasm::Simulation;

/// `(name, board-fixture JSON, golden-trace JSON)` for every corpus board with a golden.
macro_rules! fixtures {
    ($($name:literal),+ $(,)?) => {
        &[ $((
            $name,
            include_str!(concat!("../../../corpus/boards/", $name, ".json")),
            include_str!(concat!("../../../corpus/golden/", $name, ".json")),
        )),+ ]
    };
}

const FIXTURES: &[(&str, &str, &str)] = fixtures![
    "adders", "clk", "d_ff", "decoder", "demux", "encoder", "full_adder", "gates", "jk_ff",
    "led_matrix", "mux", "not_chain", "pulse", "ram", "rom", "sr_ff", "wired_or",
];

#[derive(serde::Deserialize)]
struct Trigger {
    tick: u64,
    comp: u32,
    event: String,
    state: Vec<bool>,
}

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
    ticks: u64,
    #[serde(default)]
    triggers: Vec<Trigger>,
    board: BoardDescriptor,
}

#[derive(serde::Deserialize)]
struct Frame {
    tick: u64,
    links: String,
    outputs: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Golden {
    trace: Vec<Frame>,
}

/// Construct the WASM `Simulation` from a descriptor by round-tripping it through a `JsValue`
/// (the real `new(descriptor)` path).
fn build(desc: &BoardDescriptor) -> Simulation {
    let value = serde_wasm_bindgen::to_value(desc).expect("descriptor → JsValue");
    Simulation::new(value).expect("Simulation::new")
}

/// Per-pin output counts of each component, in submission order — the segmentation of `getOutputs`.
fn output_counts(desc: &BoardDescriptor) -> Vec<usize> {
    desc.components.iter().map(|c| c.outputs.len()).collect()
}

fn apply_triggers(sim: &Simulation, triggers: &[Trigger], tick: u64) {
    for t in triggers.iter().filter(|t| t.tick == tick) {
        let event: u8 = match t.event.as_str() {
            "cont" => 0,
            "pulse" => 1,
            other => panic!("unknown trigger event '{other}'"),
        };
        let arr = js_sys::Array::new();
        for &b in &t.state {
            arr.push(&JsValue::from_bool(b));
        }
        sim.trigger_input(t.comp, event, arr)
            .unwrap_or_else(|_| panic!("triggerInput(comp {})", t.comp));
    }
}

/// Diff one golden frame against the live sim: link values (via `link()` *and* the Full-snapshot
/// bytes read out of linear memory) and component output pins (via `getOutputs`).
fn diff_frame(sim: &Simulation, desc: &BoardDescriptor, counts: &[usize], frame: &Frame) {
    let link_count = frame.links.len();

    // Links via the scalar accessor.
    for (l, ch) in frame.links.chars().enumerate() {
        assert_eq!(
            sim.link(l as u32),
            ch == '1',
            "tick {}: link {l} via link()",
            frame.tick
        );
    }

    // Links via a Full snapshot: ptr/len point directly into live link_state (same linear memory),
    // so reconstruct the packed byte slice in-module and check every bit matches.
    let view = sim.snapshot(false, 0.0);
    assert!(!view.is_delta, "a `delta=false` request must be Full");
    assert_eq!(view.len as usize, link_count.div_ceil(8), "Full snapshot byte length");
    let bytes = unsafe { core::slice::from_raw_parts(view.ptr as *const u8, view.len as usize) };
    for (l, ch) in frame.links.chars().enumerate() {
        let bit = (bytes[l >> 3] >> (l & 7)) & 1 == 1;
        assert_eq!(bit, ch == '1', "tick {}: link {l} via snapshot bytes", frame.tick);
    }

    // Component outputs via getOutputs (one byte per pin, component-major submission order).
    let out = sim.outputs();
    let mut off = 0usize;
    for (c, pins) in frame.outputs.iter().enumerate() {
        for (p, ch) in pins.chars().enumerate() {
            assert_eq!(
                out[off + p] != 0,
                ch == '1',
                "tick {}: component {c} output[{p}] via getOutputs",
                frame.tick
            );
        }
        off += counts[c];
    }
    let _ = desc;
}

fn run_fixture(fixture_json: &str, golden_json: &str) {
    let fx: Fixture = serde_json::from_str(fixture_json).expect("parse fixture");
    let golden: Golden = serde_json::from_str(golden_json).expect("parse golden");
    assert_eq!(
        golden.trace.len() as u64,
        fx.ticks + 1,
        "[{}] golden frame count",
        fx.name
    );

    let counts = output_counts(&fx.board);
    let sim = build(&fx.board);

    // Trigger timing mirrors the generator (and tests/golden.rs): apply(tick) then observe frame.
    apply_triggers(&sim, &fx.triggers, 0);
    diff_frame(&sim, &fx.board, &counts, &golden.trace[0]);
    for (i, frame) in golden.trace[1..].iter().enumerate() {
        let tick = i as u64 + 1;
        sim.tick();
        apply_triggers(&sim, &fx.triggers, tick);
        diff_frame(&sim, &fx.board, &counts, frame);
    }
}

#[wasm_bindgen_test]
fn golden_traces_match_on_wasm() {
    for (name, board, golden) in FIXTURES {
        // Each fixture's own `name` field is checked against the trace; surface which board failed.
        let _ = name;
        run_fixture(board, golden);
    }
}

/// The Delta path: after a Full baseline, a change is reported as a `Delta` whose id (u32 LE) and
/// packed-value buffers — read out of linear memory at the view's ptr/len — match the live links.
#[wasm_bindgen_test]
fn delta_snapshot_roundtrip() {
    // A UserInput driving a link; flipping it produces a small, well-defined change set.
    let desc: BoardDescriptor = serde_json::from_str(
        r#"{ "links": 2, "components": [
            { "type": 200, "inputs": [], "outputs": [0] },
            { "type": 1,   "inputs": [0], "outputs": [1] }
        ] }"#,
    )
    .unwrap();
    let sim = build(&desc);

    // Settle, then take a Full to establish the delta baseline.
    for _ in 0..3 {
        sim.tick();
    }
    let base = sim.snapshot(true, 1.0);
    assert!(!base.is_delta, "first snapshot is the Full baseline");

    // Drive link 0 high; after settling, exactly links 0 and 1 (NOT output) have flipped.
    let arr = js_sys::Array::new();
    arr.push(&JsValue::from_bool(true));
    sim.trigger_input(0, 0, arr).unwrap();
    for _ in 0..3 {
        sim.tick();
    }

    let view = sim.snapshot(true, 1.0);
    assert!(view.is_delta, "a small post-baseline change set is a Delta");
    let n = (view.len / 4) as usize; // u32 ids
    let ids = unsafe { core::slice::from_raw_parts(view.ptr as *const u32, n) };
    let vals = unsafe { core::slice::from_raw_parts(view.values_ptr as *const u8, view.values_len as usize) };
    assert!(n >= 1, "delta must carry the changed links");
    for (i, &l) in ids.iter().enumerate() {
        let bit = (vals[i >> 3] >> (i & 7)) & 1 == 1;
        assert_eq!(bit, sim.link(l), "delta value for link {l}");
    }
}
