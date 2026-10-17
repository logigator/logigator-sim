//! Cross-engine equivalence: drive the golden corpus through the **WASM binding** (compiled to
//! wasm32, run under `wasm-pack test --node`) and diff every tick against the same goldens the
//! native `sim-core` suite uses. This proves the engine produces identical traces on the wasm
//! target — catching target-specific issues (usize width, endianness, simd128 codegen) — and
//! exercises the actual marshalling surface: constructor-from-`JsValue`, `tick`, `link`,
//! `getOutputs`, `submitInput`, `inputProgress`, the component-state methods, and the zero-copy
//! `snapshot` ptr/len plumbing.
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
    "ram_legacy",
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
    #[serde(default)]
    invalid: Vec<u32>,
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
/// bytes read out of linear memory), the snapshot's invalid links and component output pins (via
/// `getOutputs`).
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
    let invalid = unsafe {
        core::slice::from_raw_parts(
            view.invalid_ptr as *const u32,
            view.invalid_len as usize / 4,
        )
    };
    assert_eq!(invalid, frame.invalid, "tick {}: invalid links", frame.tick);

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

/// The constructor hands ids to the engine as they are, and the engine's own validation rejects
/// them: an id whose low 16 bits name a real type or pin (`Not`, pin 0) is not narrowed onto it.
/// The Node suite asserts the same messages.
#[wasm_bindgen_test]
fn constructor_leaves_validation_to_the_engine() {
    let not = |extra: &str| {
        js(&format!(
            r#"{{ "links": 2, "components": [{{ "type": 1, "inputs": [0], "outputs": [1]{extra} }}] }}"#
        ))
    };
    for (extra, expected) in [
        (
            r#", "type": 65537"#,
            "component 0: unknown component type id 65537",
        ),
        (
            r#", "negInputs": [65536]"#,
            "component 0: negated input index 65536 out of range",
        ),
        (r#", "inputs": [2]"#, "component 0: link id 2 out of range"),
    ] {
        let err = message(Simulation::new(not(extra)).err().expect(extra));
        assert!(
            err.contains(expected),
            "{err:?} should contain {expected:?}"
        );
    }
}

/// What the constructor itself checks is the shape: objects, arrays, and integers that fit a
/// `u32`, never coerced (`"1"`, `0.5`, `-1` and a hole are refused, not read as numbers). Absent and
/// `null` optional lists are empty.
#[wasm_bindgen_test]
fn constructor_checks_the_shape() {
    let board = |comp: &str| js(&format!(r#"{{ "links": 2, "components": [{comp}] }}"#));
    Simulation::new(board(
        r#"{ "type": 1, "inputs": [0], "outputs": [1], "ops": null, "negInputs": [] }"#,
    ))
    .expect("null and empty optional lists");
    for (comp, expected) in [
        (
            r#"{ "type": 1, "outputs": [1] }"#,
            "components[0].inputs is missing",
        ),
        (
            r#"{ "type": 1, "inputs": [0.5], "outputs": [1] }"#,
            "components[0].inputs must be an array of u32",
        ),
        (
            r#"{ "type": 1, "inputs": [-1], "outputs": [1] }"#,
            "components[0].inputs must be an array of u32",
        ),
        (
            r#"{ "type": 1, "inputs": ["0"], "outputs": [1] }"#,
            "components[0].inputs must be an array of u32",
        ),
        (
            r#"{ "type": 1, "inputs": 0, "outputs": [1] }"#,
            "components[0].inputs must be an array of u32",
        ),
        (
            r#"{ "type": "1", "inputs": [0], "outputs": [1] }"#,
            "components[0].type must be a u32",
        ),
        (r#"[1, [0], [1]]"#, "components[0] must be an object"),
    ] {
        let err = message(Simulation::new(board(comp)).err().expect(comp));
        assert!(
            err.contains(expected),
            "{comp}: {err:?} should contain {expected:?}"
        );
    }
    let hole =
        js_sys::eval(r#"({ links: 2, components: [{ type: 1, inputs: [,], outputs: [1] }] })"#)
            .unwrap();
    let err = message(Simulation::new(hole).err().unwrap());
    assert!(
        err.contains("components[0].inputs must be an array of u32"),
        "{err}"
    );
    let err = message(
        Simulation::new(js(r#"{ "links": -1, "components": [] }"#))
            .err()
            .unwrap(),
    );
    assert!(err.contains("links must be a u32"), "{err}");
}

/// `getStatus()` is built by hand, so pin its keys and values to the `SimStatus` contract.
#[wasm_bindgen_test]
fn status_carries_the_contract_fields() {
    let sim = build(
        &serde_json::from_str(
            r#"{ "links": 2, "components": [{ "type": 1, "inputs": [0], "outputs": [1] }] }"#,
        )
        .unwrap(),
    );
    sim.run(js(r#"{ "ticks": 3 }"#)).unwrap();
    let status = sim.status().unwrap();
    let field = |k: &str| js_sys::Reflect::get(&status, &k.into()).unwrap().as_f64();
    assert_eq!(field("state"), Some(1.0), "Stopped");
    assert_eq!(field("tick"), Some(3.0));
    assert_eq!(field("linkCount"), Some(2.0));
    assert_eq!(field("componentCount"), Some(1.0));
    assert!(field("speed").is_some());
}

/// A blocking `run()` with no effective bound is refused, not started: `{}` and bounds that lower to
/// unbounded (`1e999` parses to `Infinity`) alike — the latter used to trap converting the timeout.
#[wasm_bindgen_test]
fn run_refuses_unbounded_configs() {
    let sim = build(
        &serde_json::from_str(
            r#"{ "links": 1, "components": [{ "type": 1, "inputs": [0], "outputs": [0] }] }"#,
        )
        .unwrap(),
    );
    for cfg in ["{}", r#"{ "ms": 1e999 }"#, r#"{ "ticks": 1e999 }"#] {
        let err = message(sim.run(js(cfg)).unwrap_err());
        assert!(err.contains("finite"), "{cfg}: {err}");
    }
    sim.run(js(r#"{ "ticks": 1e999, "ms": 1 }"#))
        .expect("an infinite tick count is bounded by a finite ms");
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

/// Component state through the binding: a RAM written with `writeState` and read back with
/// `readState`, a recorder's records (with ticks) from `readRecords`, the versions `stateVersions`
/// fills in, and the errors each method reports.
#[wasm_bindgen_test]
fn component_state_round_trips() {
    // Input 0 drives data links 0–2 and a trigger (link 3). Component 1 records them (capacity 4,
    // with ticks); component 2 is a RAM of two 2-bit words (address = link 0, data = links 1–2,
    // write-enable = link 4, never driven, clock = link 3) outputting on links 5–6.
    let desc: BoardDescriptor = serde_json::from_str(
        r#"{ "links": 7, "components": [
            { "type": 200, "inputs": [], "outputs": [0, 1, 2, 3] },
            { "type": 201, "inputs": [0, 1, 2, 3], "outputs": [], "ops": [4, 1] },
            { "type": 17, "inputs": [0, 1, 2, 4, 3], "outputs": [5, 6] }
        ] }"#,
    )
    .unwrap();
    let sim = build(&desc);
    let comps = [0u32, 1, 2];
    let versions = |sim: &Simulation| {
        let mut out = [9u32; 3];
        sim.state_versions(&comps, &mut out).unwrap();
        out
    };
    assert_eq!(versions(&sim), [0, 0, 0]);
    assert_eq!(
        comps.map(|c| sim.state_length(c).unwrap()),
        [0, 0, 1],
        "only the RAM has bytes: 2 words × 2 bits"
    );

    // Word 0 = 0b10, word 1 = 0b11; the padding nibble is dropped.
    sim.write_state(2, 0, &[0xFE]).unwrap();
    assert_eq!(sim.read_state(2, 0, 1).unwrap(), [0b1110]);
    let ram_version = versions(&sim)[2];
    assert_ne!(ram_version, 0, "the write moved the RAM's version");
    assert!(sim.read_state(1, 0, 0).unwrap().is_empty());
    let err = message(sim.read_state(2, 0, 2).unwrap_err());
    assert!(
        err.contains("state range 0+2 exceeds its 1 state bytes"),
        "{err}"
    );
    let err = message(sim.read_state(2, u32::MAX, u32::MAX).unwrap_err());
    assert!(err.contains("exceeds"), "{err}");
    let err = message(sim.write_state(1, 0, &[]).unwrap_err());
    assert!(err.contains("read-only"), "{err}");

    // Five trigger pulses with data 5, 2, 7, 1, 6: the recorder keeps the last four, and each pulse
    // reads the RAM word at address `data & 1`.
    let frames: Vec<String> = [5u32, 2, 7, 1, 6]
        .iter()
        .flat_map(|d| {
            let pins = |trigger: bool| {
                let p: Vec<String> = (0..3)
                    .map(|b| (d >> b & 1 == 1).to_string())
                    .chain([trigger.to_string()])
                    .collect();
                format!(r#"{{ "state": [{}] }}"#, p.join(","))
            };
            [pins(true), pins(false)]
        })
        .collect();
    sim.submit_input(js(&format!(
        r#"[{{ "comp": 0, "frames": [{}] }}]"#,
        frames.join(",")
    )))
    .unwrap();
    for _ in 0..12 {
        sim.tick();
    }
    assert_eq!(&sim.outputs()[4..], [0, 1], "the last pulse read word 0");

    let get = |v: &JsValue, k: &str| js_sys::Reflect::get(v, &k.into()).unwrap();
    let r = sim.read_records(1, 0.0, None).unwrap();
    assert_eq!(get(&r, "count").as_f64(), Some(5.0));
    assert_eq!(
        get(&r, "first").as_f64(),
        Some(1.0),
        "record 0 was overwritten"
    );
    let values = js_sys::Uint32Array::from(get(&r, "values")).to_vec();
    assert_eq!(values, [2, 7, 1, 6]);
    let ticks = js_sys::Float64Array::from(get(&r, "ticks")).to_vec();
    assert_eq!(ticks, [3.0, 5.0, 7.0, 9.0], "pulse k rises in tick 2k + 1");
    assert_eq!(versions(&sim), [0, 5, ram_version], "reads moved nothing");

    let r = sim.read_records(1, 3.0, Some(1)).unwrap();
    assert_eq!(get(&r, "first").as_f64(), Some(3.0));
    assert_eq!(js_sys::Uint32Array::from(get(&r, "values")).to_vec(), [1]);

    for (comp, since, expected) in [
        (1, 6.0, "since 6 is past its 5 records"),
        (1, -1.0, "since must be a non-negative integer"),
        (1, 0.5, "since must be a non-negative integer"),
        (0, 0.0, "component 0 is not a recorder"),
        (7, 0.0, "component 7 out of range"),
    ] {
        let err = message(sim.read_records(comp, since, None).unwrap_err());
        assert!(
            err.contains(expected),
            "{err:?} should contain {expected:?}"
        );
    }
    let err = message(sim.state_versions(&[0, 3], &mut [0; 2]).unwrap_err());
    assert!(err.contains("component 3 out of range"), "{err}");
    assert!(
        sim.state_versions(&[0, 1], &mut [0; 1]).is_err(),
        "out too short"
    );
}
