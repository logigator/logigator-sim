//! `sim run` — advance a board for a fixed tick/time budget, then optionally dump its state
//! (plan §7.5).

use crate::CliResult;
use crate::load::{self, Format};
use sim_core::{BoardDescriptor, RunConfig, Simulation};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Final-state dump encoding.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum DumpFormat {
    /// `{ tick, links: [bool…], components: [[bool…]…] }` (mirrors the old `getBoard`).
    Json,
    /// Raw `ceil(links/8)`-byte packed link bitset.
    Bin,
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// Board file: a JSON `BoardDescriptor` / corpus fixture, or a `.lgb` binary.
    pub board: PathBuf,
    /// Maximum ticks to run (falls back to a corpus fixture's `ticks`).
    #[arg(long)]
    pub ticks: Option<u64>,
    /// Wall-clock budget in milliseconds.
    #[arg(long)]
    pub ms: Option<u64>,
    /// Worker threads. `1` (default) is single-threaded; `> 1` engages the adaptive parallel
    /// driver (plan §8). Ignored for fixtures with triggers scheduled after tick 0 (those are
    /// single-stepped so the triggers land on the right tick).
    #[arg(long, default_value_t = 1)]
    pub threads: usize,
    /// Override input-format detection (default: `.lgb` → bin, otherwise json).
    #[arg(long, value_enum)]
    pub format: Option<Format>,
    /// Write the final state to this file (else print a one-line summary to stderr).
    #[arg(long)]
    pub dump: Option<PathBuf>,
    /// Encoding for `--dump`.
    #[arg(long = "dump-format", value_enum, default_value_t = DumpFormat::Json)]
    pub dump_format: DumpFormat,
}

/// The JSON `--dump` shape (plan §7.5): the same `links`/`components` layout the old `getBoard`
/// returned, with `components[i]` carrying component `i`'s output-pin values.
#[derive(serde::Serialize)]
struct StateDump {
    tick: u64,
    links: Vec<bool>,
    components: Vec<Vec<bool>>,
}

pub fn run(args: RunArgs) -> CliResult {
    let loaded = load::load(&args.board, args.format)?;
    let ticks = args.ticks.or(loaded.ticks);
    let timeout = args.ms.map(Duration::from_millis);
    if ticks.is_none() && timeout.is_none() {
        return Err(
            "specify a run bound: --ticks N and/or --ms N (the engine has no quiescence stop)"
                .into(),
        );
    }
    let mut sim =
        Simulation::from_descriptor(&loaded.desc).map_err(|e| format!("{}: {e}", loaded.name))?;

    // `sim run` only observes the *final* state, so when no trigger fires after tick 0 we can hand
    // the whole run to the adaptive parallel driver in one shot. Timed triggers need single-stepping
    // (each must land on its exact tick), so those fall back to the per-tick driver.
    let has_late_triggers = loaded.triggers.iter().any(|t| t.tick > 0);
    let ran = if args.threads > 1 && !has_late_triggers {
        load::apply_triggers(&mut sim, &loaded.triggers, 0)?;
        let cfg = RunConfig {
            ticks: ticks.unwrap_or(u64::MAX),
            timeout,
            threads: args.threads,
            ..RunConfig::default()
        };
        sim.run(cfg).map_err(|e| e.to_string())?;
        sim.tick_count()
    } else {
        if args.threads > 1 {
            eprintln!(
                "note: --threads {} ignored — timed triggers require single-stepping",
                args.threads
            );
        }
        load::drive(&mut sim, ticks, timeout, &loaded.triggers, |_, _| Ok(()))?
    };

    match &args.dump {
        Some(path) => {
            dump(&sim, &loaded.desc, args.dump_format, path)?;
            eprintln!("ran {ran} ticks; dumped state → {}", path.display());
        }
        None => {
            let powered = (0..loaded.desc.link_count).filter(|&l| sim.link(l)).count();
            eprintln!(
                "ran {ran} ticks; {powered}/{} links powered",
                loaded.desc.link_count
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn dump(
    sim: &Simulation,
    desc: &BoardDescriptor,
    fmt: DumpFormat,
    path: &PathBuf,
) -> Result<(), String> {
    match fmt {
        DumpFormat::Bin => {
            std::fs::write(path, sim.link_bytes())
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
        }
        DumpFormat::Json => {
            // Public component ids == submission order (D17), so the descriptor's component index
            // and output-array length address the same component/pins as the engine.
            let links = (0..desc.link_count).map(|l| sim.link(l)).collect();
            let components = desc
                .components
                .iter()
                .enumerate()
                .map(|(c, cd)| {
                    (0..cd.outputs.len())
                        .map(|p| sim.output(c as u32, p))
                        .collect()
                })
                .collect();
            let out = StateDump {
                tick: sim.tick_count(),
                links,
                components,
            };
            let json =
                serde_json::to_string_pretty(&out).map_err(|e| format!("serializing dump: {e}"))?;
            std::fs::write(path, json).map_err(|e| format!("writing {}: {e}", path.display()))?;
        }
    }
    Ok(())
}
