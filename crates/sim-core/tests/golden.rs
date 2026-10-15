//! Tick-exact golden-trace test.
//!
//! For every board fixture in `corpus/boards/`, replay its `inputWrites` through the engine and diff
//! every tick against the committed trace in `corpus/golden/`: the packed link bitset and every
//! component's output pins. A mismatch reports the first divergent tick and the offending
//! link/component.
//!
//! The goldens are this engine's own output (`just gen-golden` runs `sim trace` over every board), so
//! the test pins per-tick behavior to the commit that last regenerated them. A failure is either a
//! regression or an intended behavior change; for the latter, regenerate and review the golden diff
//! in the same commit.

mod common;

use common::{Fixture, corpus_dir};
use sim_core::Simulation;
use std::path::{Path, PathBuf};

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

/// Compare the live simulation against one golden frame; return a human-readable diff on mismatch.
fn diff_frame(sim: &Simulation, frame: &Frame) -> Result<(), String> {
    for (link, ch) in frame.links.chars().enumerate() {
        let expected = ch == '1';
        let got = sim.link(link as u32);
        if got != expected {
            return Err(format!(
                "tick {}: link {link} = {got}, expected {expected} (links {})",
                frame.tick, frame.links
            ));
        }
    }
    for (comp, pins) in frame.outputs.iter().enumerate() {
        for (pin, ch) in pins.chars().enumerate() {
            let expected = ch == '1';
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
    let fixture = Fixture::load(fixture_path);
    let name = &fixture.name;

    let golden_path = corpus_dir().join("golden").join(format!("{name}.json"));
    let golden: Golden =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).map_err(|e| {
            format!(
                "reading {}: {e} (regenerate with `just gen-golden`)",
                golden_path.display()
            )
        })?)
        .map_err(|e| format!("parsing {}: {e}", golden_path.display()))?;

    if golden.trace.len() as u64 != fixture.ticks + 1 {
        return Err(format!(
            "{name}: golden has {} frames, expected ticks+1 = {}",
            golden.trace.len(),
            fixture.ticks + 1
        ));
    }
    if let Some(r) = fixture
        .input_writes
        .iter()
        .find(|r| r.tick >= fixture.ticks)
    {
        return Err(format!(
            "{name}: inputWrites at tick {} would apply past the {}-tick trace",
            r.tick, fixture.ticks
        ));
    }

    let mut sim = Simulation::from_descriptor(&fixture.board)
        .map_err(|e| format!("{name}: compile/new failed: {e}"))?;

    // The same order `sim trace` records in: step, submit what is due, then observe.
    for (tick, frame) in golden.trace.iter().enumerate() {
        let tick = tick as u64;
        if tick > 0 {
            sim.tick();
        }
        fixture
            .submit_due(&mut sim, tick)
            .map_err(|e| format!("{name}: inputWrites at tick {tick}: {e}"))?;
        diff_frame(&sim, frame).map_err(|e| format!("[{name}] {e}"))?;
    }
    Ok(())
}

#[test]
fn golden_traces_match() {
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

    let failures: Vec<String> = files.iter().filter_map(|f| run_fixture(f).err()).collect();
    assert!(
        failures.is_empty(),
        "{} fixtures, {} failed:\n{}",
        files.len(),
        failures.len(),
        failures.join("\n")
    );
}
