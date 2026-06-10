//! Cross-engine equivalence: single-threaded ≡ forced-parallel, tick for tick, over the whole
//! corpus (plan §10.1 pt 4, the §8.6 determinism guarantee tested rather than assumed). This is the
//! MT coverage of the *full* component set — the corpus carries DEC/DEMUX/MUX/RAM/ROM/CLK/RNG/LED,
//! which the random-board property test (`proptests`) deliberately leaves out (their data/ops/2ⁿ
//! arity makes them awkward to generate).
//!
//! Only built with the `threads` feature; without it `run(threads > 1)` falls through to the
//! single-threaded loop and the comparison would be vacuous. `par_threshold = 1` forces the
//! parallel path on every non-empty phase so even the small corpus boards actually shard (advisor).
#![cfg(feature = "threads")]

use sim_core::{BoardDescriptor, InputEvent, RunConfig, Simulation};
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

fn corpus_boards() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/boards")
}

fn parse_event(name: &str) -> InputEvent {
    match name {
        "cont" => InputEvent::Cont,
        "pulse" => InputEvent::Pulse,
        other => panic!("unknown trigger event '{other}'"),
    }
}

/// Apply every trigger scheduled for `tick` (same timing as the golden generator / `golden.rs`).
fn apply(sim: &mut Simulation, triggers: &[Trigger], tick: u64) {
    for t in triggers.iter().filter(|t| t.tick == tick) {
        sim.trigger_input(t.comp, parse_event(&t.event), &t.state)
            .expect("trigger_input");
    }
}

/// Full settled state — packed link bits + per-pin output bytes.
fn state(sim: &Simulation) -> (Vec<u8>, Vec<u8>) {
    (sim.link_bytes(), sim.output_bytes())
}

/// Drive one corpus board single-threaded and forced-parallel in lockstep, comparing after every
/// tick (matching `golden.rs`'s trigger timing: apply(0) before the initial frame, then for each
/// tick step → apply → compare).
fn run_board(path: &Path) -> Result<(), String> {
    let fixture: Fixture = serde_json::from_str(
        &std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("parse {}: {e}", path.display()))?;

    let mut st = Simulation::from_descriptor(&fixture.board).map_err(|e| e.to_string())?;
    let mut mt = Simulation::from_descriptor(&fixture.board).map_err(|e| e.to_string())?;

    // The MT sim takes one adaptive tick per `run` call (a fresh pool each time — fine for a small
    // test board; the point is to exercise the parallel phases, not throughput).
    let par = RunConfig {
        ticks: 1,
        timeout: None,
        par_threshold: 1,
        threads: 4,
    };
    let step_mt = |mt: &mut Simulation| mt.run(par).expect("mt run");

    apply(&mut st, &fixture.triggers, 0);
    apply(&mut mt, &fixture.triggers, 0);
    if state(&st) != state(&mt) {
        return Err(format!("[{}] diverged at tick 0", fixture.name));
    }
    // A *quiescent* tick (no link flips) legitimately takes the ST path even at threshold 1 — there
    // is nothing to shard. So we don't require every tick to be parallel, only that the board
    // exercised the parallel path at least once (otherwise the comparison is vacuous).
    let mut saw_parallel = false;
    for tick in 1..=fixture.ticks {
        st.tick();
        step_mt(&mut mt);
        apply(&mut st, &fixture.triggers, tick);
        apply(&mut mt, &fixture.triggers, tick);
        saw_parallel |= mt.status().parallel;
        if state(&st) != state(&mt) {
            return Err(format!("[{}] diverged at tick {tick}", fixture.name));
        }
    }
    if fixture.ticks > 0 && !saw_parallel {
        return Err(format!(
            "[{}] never took the parallel path — the ST≡MT comparison was vacuous",
            fixture.name
        ));
    }
    Ok(())
}

#[test]
fn corpus_single_threaded_equals_parallel() {
    let dir = corpus_boards();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus boards in {}", dir.display());

    let mut failures = Vec::new();
    for f in &files {
        if let Err(e) = run_board(f) {
            failures.push(e);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} corpus boards diverged ST vs MT:\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
    eprintln!(
        "ST≡MT verified tick-for-tick on {} corpus boards",
        files.len()
    );
}
