//! Sanity for the synthetic bench boards: each board under `corpus/bench/` loads,
//! runs the requested ticks, and is *actually active* — its links keep flipping, observed through
//! non-empty snapshot deltas. Without this, a generator regression (a ring that settles, a clock
//! wired dead) would silently turn the benchmark suite into a no-op measurement.

mod common;

use common::{Fixture, corpus_dir};
use sim_core::{RunConfig, Simulation, SnapshotConfig};

/// All bench boards with the tick budget the sanity run uses (small for the ~200k-component boards —
/// these tests also run unoptimized).
const BOARDS: &[(&str, u64)] = &[
    ("small_idle", 64),
    ("small_active", 64),
    ("medium_idle", 64),
    ("medium_active", 64),
    ("large_idle", 8),
    ("large_active", 8),
    ("fanout", 32),
    ("correlated", 64),
    ("input_paste", 64),
    ("bus", 64),
    ("generic", 64),
    ("negated", 64),
    // cpu's pulse clock fires every 17 ticks and the board is quiet in between; the window after
    // 16 ticks holds the second pulse.
    ("large_cpu", 16),
];

fn load(name: &str) -> Simulation {
    let fx = Fixture::load(&corpus_dir().join("bench").join(format!("{name}.json")));
    assert_eq!(fx.name, name, "fixture name matches its file");
    let mut sim =
        Simulation::from_descriptor(&fx.board).unwrap_or_else(|e| panic!("building {name}: {e}"));
    // The harnesses queue the tick-0 input before timing the run; do the same.
    fx.submit_due(&mut sim, 0)
        .unwrap_or_else(|e| panic!("{name}: inputWrites refused: {e}"));
    sim
}

fn cfg(ticks: u64) -> RunConfig {
    RunConfig {
        ticks,
        timeout: None,
    }
}

/// Snapshot-delta config that never falls back to `Full` on size, so `changed` counts real flips.
fn delta_cfg() -> SnapshotConfig {
    SnapshotConfig {
        delta: true,
        delta_threshold: 1.0,
    }
}

/// Every bench board loads, runs exactly its tick budget, and keeps flipping links: a fresh
/// snapshot window over the last few ticks must report a non-empty delta. (Even the idle boards
/// have a clock-driven active corner — a benchmark of a settled board measures nothing.)
#[test]
fn bench_boards_load_run_and_stay_active() {
    for &(name, ticks) in BOARDS {
        let mut sim = load(name);
        sim.run(cfg(ticks));
        assert_eq!(sim.tick_count(), ticks, "{name}: ran the full tick budget");

        // Establish the delta baseline, then observe a few more ticks.
        assert!(
            !sim.snapshot(delta_cfg()).is_delta,
            "{name}: first poll is Full"
        );
        sim.run(cfg(8));
        let info = sim.snapshot(delta_cfg());
        assert!(info.is_delta, "{name}: second poll is a Delta");
        assert!(
            !sim.snapshot_ids().is_empty(),
            "{name}: board went quiet — no link flipped in 8 ticks"
        );
    }
}

/// `input_paste` measures the per-tick cost of an input with frames pending, so its paste must
/// start applying on the first tick and still be pending long after: one frame applied after the
/// sanity run, every other one still queued.
#[test]
fn input_paste_stays_pending() {
    let mut sim = load("input_paste");
    let mut progress = vec![0; sim.input_count() as usize];
    sim.input_progress(&mut progress);
    let submitted = progress[0];
    assert!(submitted > 1, "the paste is queued");
    sim.run(cfg(64));
    sim.input_progress(&mut progress);
    assert_eq!(
        progress[0],
        submitted - 1,
        "first frame applied, the rest pending"
    );
}
