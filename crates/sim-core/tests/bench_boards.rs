//! Sanity for the synthetic bench boards: each board under `corpus/bench/` loads,
//! runs the requested ticks, and is *actually active* — its links keep flipping, observed through
//! non-empty snapshot deltas. Without this, a generator regression (a ring that settles, a clock
//! wired dead) would silently turn the benchmark suite into a no-op measurement.

use sim_core::{
    BoardDescriptor, InputFrame, InputWrite, RunConfig, SimError, Simulation, SnapshotConfig,
};
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
    #[serde(default, rename = "inputWrites")]
    input_writes: Vec<Request>,
    board: BoardDescriptor,
}

/// One `inputWrites` request (the JS `submitInput` shape).
#[derive(serde::Deserialize)]
struct Request {
    tick: u64,
    writes: Vec<Write>,
}

#[derive(serde::Deserialize)]
struct Write {
    comp: u32,
    frames: Vec<Frame>,
    #[serde(default)]
    replace: bool,
}

#[derive(serde::Deserialize)]
struct Frame {
    state: Vec<Option<bool>>,
    hold: u32,
    #[serde(default)]
    coalesce: bool,
}

impl Request {
    fn submit(&self, sim: &mut Simulation) -> Result<(), SimError> {
        let frames: Vec<Vec<InputFrame<'_>>> = self
            .writes
            .iter()
            .map(|w| {
                w.frames
                    .iter()
                    .map(|f| InputFrame {
                        state: &f.state,
                        hold: f.hold,
                        coalesce: f.coalesce,
                    })
                    .collect()
            })
            .collect();
        let writes: Vec<InputWrite<'_>> = self
            .writes
            .iter()
            .zip(&frames)
            .map(|(w, frames)| InputWrite {
                comp: w.comp,
                replace: w.replace,
                frames,
            })
            .collect();
        sim.submit_input(&writes)
    }
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
    ("input_paste", 64),
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
    let mut sim =
        Simulation::from_descriptor(&fx.board).unwrap_or_else(|e| panic!("building {name}: {e}"));
    // The harnesses queue the tick-0 input before timing the run; do the same.
    for r in fx.input_writes.iter().filter(|r| r.tick == 0) {
        r.submit(&mut sim)
            .unwrap_or_else(|e| panic!("{name}: inputWrites refused: {e}"));
    }
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
    sim.run(cfg(64)).unwrap();
    sim.input_progress(&mut progress);
    assert_eq!(
        progress[0],
        submitted - 1,
        "first frame applied, the rest pending"
    );
}
