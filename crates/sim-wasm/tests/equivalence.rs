//! Cross-engine equivalence: drive the golden corpus through the **WASM binding** (compiled to
//! wasm32, run under `wasm-pack test --node`) and diff every tick against the same goldens the
//! native `sim-core` suite uses. This proves the engine produces identical traces on the wasm
//! target — catching target-specific issues (usize width, endianness, simd128 codegen) — and
//! exercises the actual marshalling surface: constructor-from-`JsValue`, `tick`, `link`,
//! `getOutputs`, `submitInput`, `inputProgress`, and the zero-copy `snapshot` ptr/len plumbing.
//!
//! A fixture's `inputWrites` requests are passed to `submitInput` as-is, each once the simulation has
//! run its `tick` ticks, in the order `sim trace` records the goldens: step, submit, observe.
//!
//! New corpus boards must be added to `FIXTURES` below (the native `tests/golden.rs` auto-discovers
//! them; `include_str!` needs literal paths, so the wasm list is maintained by hand).

#![cfg(target_arch = "wasm32")]

use sim_core::BoardDescriptor;
use sim_wasm::Simulation;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

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
    "adders",
    "clk",
    "d_ff",
    "decoder",
    "demux",
    "encoder",
    "full_adder",
    "gates",
    "jk_ff",
    "led_matrix",
    "mux",
    "not_chain",
    "pulse",
    "ram",
    "rng",
    "rom",
    "sr_ff",
    "wired_or",
];

/// One `inputWrites` request; `writes` is the `submitInput` argument, kept as raw JSON.
#[derive(serde::Deserialize)]
struct Request {
    tick: u64,
    writes: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
    ticks: u64,
    #[serde(default, rename = "inputWrites")]
    input_writes: Vec<Request>,
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

/// Parse a JS value from JSON.
fn js(json: &str) -> JsValue {
    js_sys::JSON::parse(json).expect("valid JSON")
}

/// The message of a `JsError` returned by the binding.
fn message(e: wasm_bindgen::JsError) -> String {
    js_sys::Error::from(JsValue::from(e)).message().into()
}

/// Submit the requests due once the simulation has run `tick` ticks.
fn submit_due(sim: &Simulation, requests: &[Request], tick: u64) {
    for r in requests.iter().filter(|r| r.tick == tick) {
        sim.submit_input(js(&r.writes.to_string()))
            .unwrap_or_else(|e| panic!("submitInput at tick {tick}: {}", message(e)));
    }
}

/// Diff one golden frame against the live sim: link values (via `link()` *and* the Full-snapshot
/// bytes read out of linear memory) and component output pins (via `getOutputs`).
fn diff_frame(sim: &Simulation, counts: &[usize], frame: &Frame) {
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
    assert_eq!(
        view.len as usize,
        link_count.div_ceil(8),
        "Full snapshot byte length"
    );
    let bytes = unsafe { core::slice::from_raw_parts(view.ptr as *const u8, view.len as usize) };
    for (l, ch) in frame.links.chars().enumerate() {
        let bit = (bytes[l >> 3] >> (l & 7)) & 1 == 1;
        assert_eq!(
            bit,
            ch == '1',
            "tick {}: link {l} via snapshot bytes",
            frame.tick
        );
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

    for (tick, frame) in golden.trace.iter().enumerate() {
        let tick = tick as u64;
        if tick > 0 {
            sim.tick();
        }
        submit_due(&sim, &fx.input_writes, tick);
        diff_frame(&sim, &counts, frame);
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

/// Round-trips a board carrying
/// `negInputs`/`negOutputs` through the wasm `serde` marshalling and checks the hand-derived
/// truth table — a NAND (negated output) and a NOR (both inputs negated). The Node suite asserts the
/// same board, so the two bindings agree.
#[wasm_bindgen_test]
fn negated_ports_marshal_and_compute() {
    let desc: BoardDescriptor = serde_json::from_str(
        r#"{ "links": 4, "components": [
            { "type": 200, "inputs": [], "outputs": [0] },
            { "type": 200, "inputs": [], "outputs": [1] },
            { "type": 2, "inputs": [0, 1], "outputs": [2], "negOutputs": [0] },
            { "type": 2, "inputs": [0, 1], "outputs": [3], "negInputs": [0, 1] }
        ] }"#,
    )
    .unwrap();
    let sim = build(&desc);

    let drive = |comp: u32, v: bool| {
        let writes = js(&format!(
            r#"[{{ "comp": {comp}, "frames": [{{ "state": [{v}], "coalesce": true }}] }}]"#
        ));
        sim.submit_input(writes).unwrap();
    };
    let check = |a: bool, b: bool, nand: bool, nor: bool| {
        drive(0, a);
        drive(1, b);
        for _ in 0..8 {
            sim.tick();
        }
        assert_eq!(sim.link(2), nand, "NAND a={a} b={b}");
        assert_eq!(sim.link(3), nor, "NOR a={a} b={b}");
    };
    check(false, false, true, true);
    check(true, false, true, false);
    check(false, true, true, false);
    check(true, true, false, false);
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
    sim.submit_input(js(r#"[{ "comp": 0, "frames": [{ "state": [true] }] }]"#))
        .unwrap();
    for _ in 0..3 {
        sim.tick();
    }

    let view = sim.snapshot(true, 1.0);
    assert!(view.is_delta, "a small post-baseline change set is a Delta");
    let n = (view.len / 4) as usize; // u32 ids
    let ids = unsafe { core::slice::from_raw_parts(view.ptr as *const u32, n) };
    let vals = unsafe {
        core::slice::from_raw_parts(view.values_ptr as *const u8, view.values_len as usize)
    };
    assert!(n >= 1, "delta must carry the changed links");
    for (i, &l) in ids.iter().enumerate() {
        let bit = (vals[i >> 3] >> (i & 7)) & 1 == 1;
        assert_eq!(bit, sim.link(l), "delta value for link {l}");
    }
}

/// The JS-only input checks — a `state` entry that is not `true`/`false`/`null` (including a hole
/// or `undefined`) and a non-integer `hold` — refuse the whole request; `inputProgress` reports the
/// pending events into a caller-owned buffer and drains as frames apply.
#[wasm_bindgen_test]
fn submit_input_validates_and_reports_progress() {
    let desc: BoardDescriptor = serde_json::from_str(
        r#"{ "links": 3, "components": [
            { "type": 200, "inputs": [], "outputs": [0, 1] },
            { "type": 200, "inputs": [], "outputs": [2] }
        ] }"#,
    )
    .unwrap();
    let sim = build(&desc);
    let good = r#"{ "comp": 1, "frames": [{ "state": [true] }] }"#;
    let refused = |second: JsValue, expected: &str| {
        let writes = js_sys::Array::of2(&js(good), &second);
        let err = message(sim.submit_input(writes.into()).unwrap_err());
        assert!(
            err.contains(expected),
            "{err:?} should contain {expected:?}"
        );
        let mut progress = [9u32; 2];
        sim.input_progress(&mut progress).unwrap();
        assert_eq!(progress, [0, 0], "nothing queued after: {expected}");
    };
    refused(
        js(r#"{ "comp": 0, "frames": [{ "state": [true, 0] }] }"#),
        "input 0 frame 0: state[1] must be true, false or null",
    );
    // A hole at index 0 and an explicit `undefined` at index 1.
    let state = js_sys::Array::new_with_length(2);
    js_sys::Reflect::set(&state, &JsValue::from(1), &JsValue::TRUE).unwrap();
    let with_state = |state: &js_sys::Array| {
        let frame = js_sys::Object::new();
        js_sys::Reflect::set(&frame, &"state".into(), state).unwrap();
        let write = js(r#"{ "comp": 0 }"#);
        js_sys::Reflect::set(&write, &"frames".into(), &js_sys::Array::of1(&frame)).unwrap();
        write
    };
    refused(with_state(&state), "state[0] must be true, false or null");
    let state = js_sys::Array::of2(&JsValue::TRUE, &JsValue::UNDEFINED);
    refused(with_state(&state), "state[1] must be true, false or null");
    refused(
        js(r#"{ "comp": 0, "frames": [{ "state": [true, true], "hold": 1.5 }] }"#),
        "input 0 frame 0: hold must be",
    );
    refused(
        js(r#"{ "comp": 0, "frames": [{ "state": [true] }] }"#),
        "state has 1 entries, the input has 2 pins",
    );

    sim.submit_input(js(r#"[
        { "comp": 0, "frames": [{ "state": [true, null], "hold": 2 }, { "state": [null, true] }] },
        { "comp": 1, "frames": [{ "state": [true], "coalesce": true }] }
    ]"#))
        .unwrap();
    let mut progress = [0u32; 2];
    sim.input_progress(&mut progress).unwrap();
    assert_eq!(progress, [2, 0], "coalescing frames are not counted");
    sim.tick();
    sim.input_progress(&mut progress).unwrap();
    assert_eq!(progress, [1, 0]);
    assert_eq!(sim.outputs(), [1, 0, 1]);
    sim.tick();
    sim.tick();
    sim.input_progress(&mut progress).unwrap();
    assert_eq!(progress, [0, 0]);
    assert_eq!(sim.outputs(), [1, 1, 1], "null kept pin 0");
    assert!(sim.input_progress(&mut [0u32; 1]).is_err(), "out too short");
}
