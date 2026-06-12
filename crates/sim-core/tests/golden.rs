//! Tick-exact golden-trace test.
//!
//! For every board fixture in `corpus/boards/`, replay the matching per-tick golden trace
//! (generated from the published C++ engine — see `corpus/tools/`) through the Rust engine and diff
//! every tick: the packed link bitset and every component's output pins. A mismatch reports the
//! first divergent tick and the offending link/component.
//!
//! The corpus covers the full component set. The two deliberate divergences are handled outside this
//! oracle diff: RNG (time-seeded in C++, non-reproducible) has no corpus board and is verified by a
//! Rust property test (`components::rng`); the SR flip-flop's `sr_ff` board stays inside the region
//! where the new rising-edge latch agrees with the old level-sensitive one (enable pulsed, S/R
//! stable while held high), and the divergent behavior is pinned by a unit test
//! (`components::flipflops`).

use sim_core::{BoardDescriptor, InputEvent, Simulation};
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

/// Map string event names to [`InputEvent`].
fn parse_event(name: &str) -> Option<InputEvent> {
    match name {
        "cont" => Some(InputEvent::Cont),
        "pulse" => Some(InputEvent::Pulse),
        _ => None,
    }
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

    // Group triggers by tick.
    let mut triggers_by_tick: Vec<Vec<&Trigger>> = vec![Vec::new(); (fixture.ticks + 1) as usize];
    for t in &fixture.triggers {
        triggers_by_tick
            .get_mut(t.tick as usize)
            .ok_or_else(|| {
                format!(
                    "{}: trigger tick {} exceeds ticks {}",
                    fixture.name, t.tick, fixture.ticks
                )
            })?
            .push(t);
    }

    let mut sim = Simulation::from_descriptor(&fixture.board)
        .map_err(|e| format!("{}: compile/new failed: {e}", fixture.name))?;

    // Trigger timing mirrors the generator (`corpus/tools/gen-one.mjs`) exactly: a trigger
    // scheduled for tick T is applied immediately *before* frame T is captured — i.e. after the
    // step that advances the engine to tick T (and a tick-0 trigger before the initial frame). So
    // frame T reflects trigger T's output pins immediately; their link propagation appears in
    // frame T+1. Getting this ordering wrong shifts every post-trigger frame by one tick.
    let apply = |sim: &mut Simulation, tick: u64| -> Result<(), String> {
        if let Some(pending) = triggers_by_tick.get(tick as usize) {
            for t in pending {
                let event = parse_event(&t.event).ok_or_else(|| {
                    format!("{}: unknown trigger event '{}'", fixture.name, t.event)
                })?;
                sim.trigger_input(t.comp, event, &t.state)
                    .map_err(|e| format!("{}: trigger_input failed: {e}", fixture.name))?;
            }
        }
        Ok(())
    };

    // trigger(0) → frame 0, then for each tick: step → trigger(tick) → frame(tick).
    apply(&mut sim, 0)?;
    diff_frame(&sim, &golden.trace[0]).map_err(|e| format!("[{}] {e}", fixture.name))?;
    for (tick_idx, frame) in golden.trace[1..].iter().enumerate() {
        let tick = (tick_idx as u64) + 1;
        sim.tick();
        apply(&mut sim, tick)?;
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
