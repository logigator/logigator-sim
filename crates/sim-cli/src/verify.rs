//! `sim verify` — check a fixture's final state against an expected snapshot (plan §7.5). This
//! replaces the old `test.js`: each fixture is the wrapper shape `{ ticks, expected, inputTriggers,
//! threads?, board }` (see the old `tests/*.json`), *not* a bare board.
//!
//! Each `inputTriggers` entry is a component index whose outputs are all latched high (`Cont`,
//! state = all-`true`) before the run — exactly what `test.js` did. The board is then run
//! single-threaded for the fixture's `ticks`/`ms`, and the final `links` + per-component output
//! pins are compared to `expected`. Exit code is `1` on any mismatch.

use crate::CliResult;
use sim_core::{BoardDescriptor, InputEvent, RunConfig, Simulation};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(clap::Args)]
pub struct VerifyArgs {
    /// One or more fixtures in the `tests/*.json` shape (`{ticks, expected, inputTriggers, board}`).
    #[arg(required = true, num_args = 1..)]
    pub fixtures: Vec<PathBuf>,
}

#[derive(serde::Deserialize)]
struct Expected {
    links: Vec<bool>,
    /// `components[i]` = component `i`'s expected output-pin values.
    components: Vec<Vec<bool>>,
}

#[derive(serde::Deserialize)]
struct Fixture {
    #[serde(default)]
    ticks: Option<u64>,
    #[serde(default)]
    ms: Option<u64>,
    #[serde(default)]
    threads: Option<usize>,
    #[serde(default, rename = "inputTriggers")]
    input_triggers: Vec<u32>,
    expected: Expected,
    board: BoardDescriptor,
}

pub fn verify(args: VerifyArgs) -> CliResult {
    let mut any_failed = false;
    for path in &args.fixtures {
        let mismatches = verify_one(path)?;
        if mismatches.is_empty() {
            println!("{}: passed", path.display());
        } else {
            any_failed = true;
            eprintln!("{}: FAILED", path.display());
            for m in &mismatches {
                eprintln!("  {m}");
            }
        }
    }
    Ok(if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Run one fixture and return the list of state mismatches (empty = passed). `Err` is reserved for
/// hard errors (bad JSON, an out-of-range trigger/expected index) — those surface as exit 2.
fn verify_one(path: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let fx: Fixture =
        serde_json::from_slice(&bytes).map_err(|e| format!("parsing {}: {e}", path.display()))?;

    if fx.ticks.is_none() && fx.ms.is_none() {
        return Err("fixture sets neither `ticks` nor `ms` (the run would never terminate)".into());
    }
    // Guard the expected dimensions against the board so the comparison can't index out of range.
    if fx.expected.components.len() > fx.board.components.len() {
        return Err(format!(
            "expected lists {} components, board has {}",
            fx.expected.components.len(),
            fx.board.components.len()
        ));
    }
    if fx.expected.links.len() > fx.board.link_count as usize {
        return Err(format!(
            "expected lists {} links, board has {}",
            fx.expected.links.len(),
            fx.board.link_count
        ));
    }
    // Per-component pin guard: `sim.output(i, j)` addresses `comp_out_off[i] + j`, so an over-long
    // expected row would read into the next component. Each row may list *fewer* pins (only those
    // are checked) but never more than the component drives.
    for (i, comp) in fx.expected.components.iter().enumerate() {
        let pins = fx.board.components[i].outputs.len();
        if comp.len() > pins {
            return Err(format!(
                "expected component[{i}] lists {} pins, board component drives {pins}",
                comp.len()
            ));
        }
    }

    let mut sim = Simulation::from_descriptor(&fx.board).map_err(|e| e.to_string())?;

    // Latch every triggered input high (Cont), matching test.js's `outputs.map(() => true)`.
    for &idx in &fx.input_triggers {
        let pins = fx
            .board
            .components
            .get(idx as usize)
            .ok_or_else(|| format!("inputTriggers index {idx} out of range"))?
            .outputs
            .len();
        sim.trigger_input(idx, InputEvent::Cont, &vec![true; pins])
            .map_err(|e| format!("trigger_input(comp {idx}): {e}"))?;
    }

    let cfg = RunConfig {
        ticks: fx.ticks.unwrap_or(u64::MAX),
        timeout: fx.ms.map(Duration::from_millis),
        threads: fx.threads.unwrap_or(1),
        ..RunConfig::default()
    };
    sim.run(cfg).map_err(|e| e.to_string())?;

    let mut mismatches = Vec::new();
    for (i, comp) in fx.expected.components.iter().enumerate() {
        // `expected` may list fewer pins than the component has; only the listed pins are checked.
        for (j, &exp) in comp.iter().enumerate() {
            let got = sim.output(i as u32, j);
            if got != exp {
                mismatches.push(format!("component[{i}][{j}]: expected {exp}, got {got}"));
            }
        }
    }
    for (i, &exp) in fx.expected.links.iter().enumerate() {
        let got = sim.link(i as u32);
        if got != exp {
            mismatches.push(format!("link[{i}]: expected {exp}, got {got}"));
        }
    }
    Ok(mismatches)
}
