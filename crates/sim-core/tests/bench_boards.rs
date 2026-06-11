//! Sanity for the synthetic bench boards: each board under `corpus/bench/` loads,
//! runs the requested ticks, and is *actually active* — its links keep flipping, observed through
//! non-empty snapshot deltas. Without this, a generator regression (a ring that settles, a clock
//! wired dead) would silently turn the benchmark suite into a no-op measurement.

use sim_core::{BoardDescriptor, RunConfig, Simulation, SnapshotConfig};
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
    board: BoardDescriptor,
}

fn bench_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/bench")
}

/// All bench boards with the tick budget the sanity run uses (small for the 200k-component boards —
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
];

fn load(name: &str) -> Simulation {
    let path = bench_dir().join(format!("{name}.json"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "reading {} (run corpus/tools/gen-bench.mjs?): {e}",
            path.display()
        )
    });
    let fx: Fixture = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
    assert_eq!(fx.name, name, "fixture name matches its file");
    Simulation::from_descriptor(&fx.board).unwrap_or_else(|e| panic!("building {name}: {e}"))
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
        sim.run(cfg(ticks)).unwrap();
        assert_eq!(sim.tick_count(), ticks, "{name}: ran the full tick budget");

        // Establish the delta baseline, then observe a few more ticks.
        assert!(
            !sim.snapshot(delta_cfg()).is_delta,
            "{name}: first poll is Full"
        );
        sim.run(cfg(8)).unwrap();
        let info = sim.snapshot(delta_cfg());
        assert!(info.is_delta, "{name}: second poll is a Delta");
        assert!(
            info.changed > 0,
            "{name}: board went quiet — no link flipped in 8 ticks"
        );
    }
}
