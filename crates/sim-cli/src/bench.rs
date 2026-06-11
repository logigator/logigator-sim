//! `sim bench` — measure end-to-end tick throughput (plan §7.5, §10.2).
//!
//! Each repeat rebuilds the simulation from the board (fresh power-on init), applies any tick-0
//! fixture triggers so the board actually does work, then times `Simulation::run` over `--ticks`
//! steps. We report the best and mean ticks/second — "best" being the least noisy estimate of the
//! engine's real throughput. This times the shipped `run()` path (including its per-tick speed
//! sampling), not a bare tick loop.

use crate::CliResult;
use crate::load::{self, Format};
use sim_core::{RunConfig, Simulation};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[derive(clap::Args)]
pub struct BenchArgs {
    /// Board file: a JSON `BoardDescriptor` / corpus fixture, or a `.lgb` binary.
    pub board: PathBuf,
    /// Ticks per repeat.
    #[arg(long, default_value_t = 1_000_000)]
    pub ticks: u64,
    /// Number of timed repeats.
    #[arg(long, default_value_t = 5)]
    pub repeat: u32,
    /// Override input-format detection (default: `.lgb` → bin, otherwise json).
    #[arg(long, value_enum)]
    pub format: Option<Format>,
}

pub fn bench(args: BenchArgs) -> CliResult {
    if args.repeat == 0 {
        return Err("--repeat must be at least 1".into());
    }
    let loaded = load::load(&args.board, args.format)?;
    let cfg = RunConfig {
        ticks: args.ticks,
        timeout: None,
    };

    eprintln!(
        "benching {} — {} ticks × {} repeats",
        loaded.name,
        group(args.ticks),
        args.repeat
    );

    let mut best_tps = 0.0_f64;
    let mut sum_tps = 0.0_f64;
    for r in 1..=args.repeat {
        let mut sim = Simulation::from_descriptor(&loaded.desc)
            .map_err(|e| format!("{}: {e}", loaded.name))?;
        // Apply a tick-0 kick (corpus fixtures latch their inputs at tick 0); timed triggers at
        // later ticks are intentionally skipped — bench measures steady throughput, not a scenario.
        load::apply_triggers(&mut sim, &loaded.triggers, 0)?;

        let start = Instant::now();
        sim.run(cfg).map_err(|e| e.to_string())?;
        let secs = start.elapsed().as_secs_f64();

        let tps = args.ticks as f64 / secs.max(1e-12);
        best_tps = best_tps.max(tps);
        sum_tps += tps;
        eprintln!(
            "  run {r}: {:.3} ms → {} ticks/s",
            secs * 1e3,
            group(tps as u64)
        );
    }

    println!(
        "{}: best {} ticks/s, mean {} ticks/s ({} ticks × {} repeats)",
        loaded.name,
        group(best_tps as u64),
        group((sum_tps / args.repeat as f64) as u64),
        group(args.ticks),
        args.repeat
    );
    Ok(ExitCode::SUCCESS)
}

/// Group a number into thousands with `_` separators (e.g. `1_000_000`) for readable throughput.
fn group(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push('_');
        }
        out.push(b as char);
    }
    out
}
