//! `sim trace` — dump a per-tick trace (tick, packed link bitset, per-component output pins) in
//! the golden-corpus format.
//!
//! NOTE: this emits **this Rust engine's** trace, not the C++ oracle's. The authoritative
//! `corpus/golden/` traces are generated from the published C++ engine via `corpus/tools/`; use
//! `trace` for inspection or to author *new* fixtures, never to regenerate the goldens the
//! `golden` test diffs against (that would make the test self-referential).

use crate::CliResult;
use crate::load::{self, Format};
use sim_core::{BoardDescriptor, Simulation};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct TraceArgs {
    /// Board file: a JSON `BoardDescriptor` / corpus fixture, or a `.lgb` binary.
    pub board: PathBuf,
    /// Ticks to trace (falls back to a corpus fixture's `ticks`). Emits `ticks + 1` frames.
    #[arg(long)]
    pub ticks: Option<u64>,
    /// Write the trace JSON here (else print to stdout).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Override input-format detection (default: `.lgb` → bin, otherwise json).
    #[arg(long, value_enum)]
    pub format: Option<Format>,
}

/// One trace frame — the golden-corpus shape consumed by `tests/golden.rs`.
#[derive(serde::Serialize)]
struct Frame {
    tick: u64,
    /// Link bitset as a `'0'`/`'1'` string, link 0 first.
    links: String,
    /// Per-component output-pin strings (`outputs[c]` = component `c`'s pins).
    outputs: Vec<String>,
}

#[derive(serde::Serialize)]
struct Trace {
    name: String,
    ticks: u64,
    trace: Vec<Frame>,
}

/// Capture the current state into a golden frame. Public component ids == submission order,
/// so the descriptor index/output-array length address the same component/pins as the engine.
fn frame(sim: &Simulation, desc: &BoardDescriptor, tick: u64) -> Frame {
    let bit = |b: bool| if b { '1' } else { '0' };
    let links = (0..desc.link_count).map(|l| bit(sim.link(l))).collect();
    let outputs = desc
        .components
        .iter()
        .enumerate()
        .map(|(c, cd)| {
            (0..cd.outputs.len())
                .map(|p| bit(sim.output(c as u32, p)))
                .collect()
        })
        .collect();
    Frame {
        tick,
        links,
        outputs,
    }
}

pub fn trace(args: TraceArgs) -> CliResult {
    let loaded = load::load(&args.board, args.format)?;
    let ticks = args
        .ticks
        .or(loaded.ticks)
        .ok_or("specify --ticks N (the board carries no fixture default)")?;

    let mut sim =
        Simulation::from_descriptor(&loaded.desc).map_err(|e| format!("{}: {e}", loaded.name))?;
    let mut frames = Vec::with_capacity((ticks + 1) as usize);
    load::drive(
        &mut sim,
        Some(ticks),
        None,
        &loaded.triggers,
        |sim, tick| {
            frames.push(frame(sim, &loaded.desc, tick));
            Ok(())
        },
    )?;

    let out = Trace {
        name: loaded.name,
        ticks,
        trace: frames,
    };
    let json = serde_json::to_string_pretty(&out).map_err(|e| format!("serializing trace: {e}"))?;
    match &args.out {
        Some(path) => {
            std::fs::write(path, json).map_err(|e| format!("writing {}: {e}", path.display()))?;
            eprintln!("traced {ticks} ticks → {}", path.display());
        }
        None => println!("{json}"),
    }
    Ok(ExitCode::SUCCESS)
}
