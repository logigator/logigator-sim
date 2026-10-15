//! Tick-exact golden-trace test.
//!
//! For every board fixture in `corpus/boards/`, replay the matching per-tick golden trace
//! (generated from the published C++ engine — see `corpus/tools/`) through the Rust engine and diff
//! every tick: the packed link bitset and every component's output pins. A mismatch reports the
//! first divergent tick and the offending link/component.
//!
//! The fixtures' `cont`/`pulse` triggers reach the engine as input frames ([`submit_trigger`]).
//! This engine drives an input's wires one tick after submission, the C++ engine two, so a trigger
//! scheduled for tick T is submitted one boundary later (after the step to T + 1) and lands on the
//! same tick as the oracle's. The oracle shows a `UserInput`'s pins ahead of its wires, this engine
//! shows both together, so `UserInput` pins are not compared; their wires carry the same
//! information and are.
//!
//! The corpus covers the full component set. The two deliberate divergences are handled outside this
//! oracle diff: RNG (time-seeded in C++, non-reproducible) has no corpus board and is verified by a
//! Rust property test (`components::rng`); the SR flip-flop's `sr_ff` board stays inside the region
//! where the new rising-edge latch agrees with the old level-sensitive one (enable pulsed, S/R
//! stable while held high), and the divergent behavior is pinned by a unit test
//! (`components::flipflops`).

use sim_core::{BoardDescriptor, CompType, InputFrame, InputWrite, Simulation};
use std::path::{Path, PathBuf};

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

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

/// Submit a corpus trigger as input frames: `cont` is one coalescing frame (two between the same
/// two ticks merge, so the last one wins, as with the old call), `pulse` two events, the second
/// returning every pin to 0.
fn submit_trigger(sim: &mut Simulation, t: &Trigger) -> Result<(), String> {
    let set: Vec<Option<bool>> = t.state.iter().map(|&v| Some(v)).collect();
    let zeros = vec![Some(false); t.state.len()];
    let frame = |state, coalesce| InputFrame {
        state,
        hold: 1,
        coalesce,
    };
    let frames = match t.event.as_str() {
        "cont" => vec![frame(&set, true)],
        "pulse" => vec![frame(&set, false), frame(&zeros, false)],
        other => return Err(format!("unknown trigger event '{other}'")),
    };
    sim.submit_input(&[InputWrite {
        comp: t.comp,
        replace: false,
        frames: &frames,
    }])
    .map_err(|e| format!("submit_input(comp {}): {e}", t.comp))
}

/// Compare the live simulation against one golden frame; return a human-readable diff on mismatch.
fn diff_frame(sim: &Simulation, board: &BoardDescriptor, frame: &Frame) -> Result<(), String> {
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
        if board.components[comp].ty == CompType::UserInput {
            continue;
        }
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

    // Group triggers by the tick after whose step they are submitted: one boundary after their
    // scheduled tick (see the module docs). A trigger on the last tick would land past the trace.
    let mut triggers_by_tick: Vec<Vec<&Trigger>> = vec![Vec::new(); (fixture.ticks + 1) as usize];
    for t in &fixture.triggers {
        if t.tick > fixture.ticks {
            return Err(format!(
                "{}: trigger tick {} exceeds ticks {}",
                fixture.name, t.tick, fixture.ticks
            ));
        }
        if let Some(slot) = triggers_by_tick.get_mut(t.tick as usize + 1) {
            slot.push(t);
        }
    }

    let mut sim = Simulation::from_descriptor(&fixture.board)
        .map_err(|e| format!("{}: compile/new failed: {e}", fixture.name))?;

    diff_frame(&sim, &fixture.board, &golden.trace[0])
        .map_err(|e| format!("[{}] {e}", fixture.name))?;
    for (tick_idx, frame) in golden.trace[1..].iter().enumerate() {
        let tick = (tick_idx as u64) + 1;
        sim.tick();
        for t in &triggers_by_tick[tick as usize] {
            submit_trigger(&mut sim, t).map_err(|e| format!("{}: {e}", fixture.name))?;
        }
        diff_frame(&sim, &fixture.board, frame).map_err(|e| format!("[{}] {e}", fixture.name))?;
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
