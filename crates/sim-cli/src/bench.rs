//! `sim bench` — measure end-to-end tick throughput.
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
    /// Tick-bound mode: run exactly this many ticks per repeat (a determinism escape hatch). When
    /// omitted, the bench is time-bound by `--ms`.
    #[arg(long)]
    pub ticks: Option<u64>,
    /// Time-bound window per repeat, in milliseconds (the default mode; ignored when `--ticks` is
    /// set). Throughput is the ticks actually executed over the measured wall-clock.
    #[arg(long, default_value_t = 1000.0)]
    pub ms: f64,
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
    // Time-bound by default (count the ticks that land in `--ms`); `--ticks` forces the old
    // fixed-tick mode for a deterministic step count.
    let cfg = match args.ticks {
        Some(ticks) => RunConfig {
            ticks,
            timeout: None,
        },
        None => RunConfig::from_float_bounds(None, Some(args.ms)),
    };

    match args.ticks {
        Some(ticks) => eprintln!(
            "benching {} — {} ticks × {} repeats",
            loaded.name,
            group(ticks),
            args.repeat
        ),
        None => eprintln!(
            "benching {} — {} ms window × {} repeats",
            loaded.name, args.ms, args.repeat
        ),
    }

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
        // A fresh sim starts at tick 0, so `tick_count()` is exactly the ticks this repeat ran.
        let ticks_run = args.ticks.unwrap_or_else(|| sim.tick_count());

        let tps = ticks_run as f64 / secs.max(1e-12);
        best_tps = best_tps.max(tps);
        sum_tps += tps;
        eprintln!(
            "  run {r}: {:.3} ms, {} ticks → {} ticks/s",
            secs * 1e3,
            group(ticks_run),
            group(tps as u64)
        );
    }

    println!(
        "{}: best {} ticks/s, mean {} ticks/s ({} repeats)",
        loaded.name,
        group(best_tps as u64),
        group((sum_tps / args.repeat as f64) as u64),
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
