//! Tick-exact golden-trace test (plan §10.1, D14).
//!
//! For every board fixture in `corpus/boards/`, replay the matching per-tick golden trace
//! (generated from the published C++ engine — see `corpus/tools/`) through the Rust engine and diff
//! every tick: the packed link bitset and every component's output pins. A mismatch reports the
//! first divergent tick and the offending link/component.
//!
//! Phase 1 covers combinational boards plus `UserInput` (Cont + one-shot Pulse). The two §0
//! divergences (RNG, SR-FF) do not occur here and need no exemption yet.

use sim_core::{BoardDescriptor, InputEvent, Simulation};
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize)]
struct Trigger {
    comp: u32,
    event: u8,
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
    outputs: Vec<Vec<bool>>,
}

#[derive(serde::Deserialize)]
struct Golden {
    trace: Vec<Frame>,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

/// Compare the live simulation against one golden frame; return a human-readable diff on mismatch.
fn diff_frame(sim: &Simulation, frame: &Frame) -> Result<(), String> {
    let bits: Vec<bool> = frame.links.chars().map(|c| c == '1').collect();
    for (link, &expected) in bits.iter().enumerate() {
        let got = sim.link(link as u32);
        if got != expected {
            return Err(format!(
                "tick {}: link {link} = {got}, expected {expected} (links {})",
                frame.tick, frame.links
            ));
        }
    }
    for (comp, pins) in frame.outputs.iter().enumerate() {
        for (pin, &expected) in pins.iter().enumerate() {
            let got = sim.output(comp as u32, pin);
            if got != expected {
                return Err(format!(
                    "tick {}: component {comp} output[{pin}] = {got}, expected {expected}",
                    frame.tick
                ));
            }
        }
    }
    Ok(())
}

fn run_fixture(fixture_path: &Path) -> Result<(), String> {
    let fixture: Fixture =
        serde_json::from_str(&std::fs::read_to_string(fixture_path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("parsing {}: {e}", fixture_path.display()))?;

    let golden_path = corpus_dir()
        .join("golden")
        .join(format!("{}.json", fixture.name));
    let golden: Golden =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).map_err(|e| {
            format!(
                "reading {}: {e} (regenerate with corpus/tools)",
                golden_path.display()
            )
        })?)
        .map_err(|e| format!("parsing {}: {e}", golden_path.display()))?;

    assert_eq!(
        golden.trace.len() as u64,
        fixture.ticks + 1,
        "{}: golden has {} frames, expected ticks+1 = {}",
        fixture.name,
        golden.trace.len(),
        fixture.ticks + 1
    );

    let mut sim = Simulation::from_descriptor(&fixture.board)
        .map_err(|e| format!("{}: compile/new failed: {e}", fixture.name))?;
    for t in &fixture.triggers {
        let event = InputEvent::try_from(t.event)
            .map_err(|v| format!("{}: bad trigger event {v}", fixture.name))?;
        sim.trigger_input(t.comp, event, &t.state)
            .map_err(|e| format!("{}: trigger_input failed: {e}", fixture.name))?;
    }

    // Frame 0 is the post-init / post-trigger state, then one frame per tick.
    diff_frame(&sim, &golden.trace[0]).map_err(|e| format!("[{}] {e}", fixture.name))?;
    for frame in &golden.trace[1..] {
        sim.tick();
        diff_frame(&sim, frame).map_err(|e| format!("[{}] {e}", fixture.name))?;
    }
    Ok(())
}

#[test]
fn golden_traces_match_cpp_oracle() {
    let boards = corpus_dir().join("boards");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&boards)
        .unwrap_or_else(|e| panic!("reading {}: {e}", boards.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no board fixtures found in {}",
        boards.display()
    );

    let mut failures = Vec::new();
    let mut ran = 0;
    for f in &files {
        ran += 1;
        if let Err(e) = run_fixture(f) {
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "{ran} fixtures, {} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("golden: {ran} fixtures matched the C++ oracle tick-for-tick");
}
