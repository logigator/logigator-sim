//! Board/fixture loading and the shared tick driver.
//!
//! A board file is one of: a JSON [`BoardDescriptor`] (`{links, components}`), a JSON **corpus
//! fixture** wrapper (`{name?, ticks?, triggers?, board}` — the shape under `corpus/boards/`), or a
//! `.lgb` binary. `run`/`trace` accept any of these; the fixture wrapper additionally carries timed
//! `triggers` and a default tick count. Format is auto-detected from the extension (`.lgb` → binary,
//! else JSON) unless overridden.

use sim_core::{BoardDescriptor, InputEvent, Simulation};
use std::path::Path;
use std::time::{Duration, Instant};

/// Board file encoding.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Format {
    Json,
    Bin,
}

/// One timed trigger from a corpus fixture (`gen-one.mjs` / `tests/golden.rs` shape).
#[derive(serde::Deserialize)]
pub struct Trigger {
    pub tick: u64,
    pub comp: u32,
    pub event: String,
    pub state: Vec<bool>,
}

/// The corpus fixture wrapper around a board.
#[derive(serde::Deserialize)]
struct Fixture {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ticks: Option<u64>,
    #[serde(default)]
    triggers: Vec<Trigger>,
    board: BoardDescriptor,
}

/// A loaded board plus anything a fixture wrapper supplied.
pub struct Loaded {
    /// Display name (fixture `name`, else the file stem).
    pub name: String,
    pub desc: BoardDescriptor,
    /// Default tick budget from a fixture, if any (a CLI `--ticks` overrides it).
    pub ticks: Option<u64>,
    /// Timed triggers from a fixture (empty for a bare board / `.lgb`).
    pub triggers: Vec<Trigger>,
}

fn detect(path: &Path) -> Format {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("lgb"))
    {
        Format::Bin
    } else {
        Format::Json
    }
}

/// Read and parse a board file. JSON is probed for a top-level `board` key to tell a fixture
/// wrapper from a bare descriptor, so each reports its own parse error rather than a misleading one.
pub fn load(path: &Path, format: Option<Format>) -> Result<Loaded, String> {
    let fmt = format.unwrap_or_else(|| detect(path));
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("board")
        .to_string();

    match fmt {
        Format::Bin => {
            let desc = sim_core::codec::decode_board(&bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Loaded {
                name: stem,
                desc,
                ticks: None,
                triggers: Vec::new(),
            })
        }
        Format::Json => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("parsing {}: {e}", path.display()))?;
            if value.get("board").is_some() {
                let fx: Fixture = serde_json::from_value(value)
                    .map_err(|e| format!("parsing fixture {}: {e}", path.display()))?;
                Ok(Loaded {
                    name: fx.name.unwrap_or(stem),
                    desc: fx.board,
                    ticks: fx.ticks,
                    triggers: fx.triggers,
                })
            } else {
                let desc: BoardDescriptor = serde_json::from_value(value)
                    .map_err(|e| format!("parsing {}: {e}", path.display()))?;
                Ok(Loaded {
                    name: stem,
                    desc,
                    ticks: None,
                    triggers: Vec::new(),
                })
            }
        }
    }
}

/// Apply every trigger scheduled for `tick`. Mirrors `corpus/tools/gen-one.mjs`: a trigger for
/// tick T is applied immediately before frame T is observed.
pub fn apply_triggers(sim: &mut Simulation, triggers: &[Trigger], tick: u64) -> Result<(), String> {
    for t in triggers.iter().filter(|t| t.tick == tick) {
        let event = match t.event.as_str() {
            "cont" => InputEvent::Cont,
            "pulse" => InputEvent::Pulse,
            other => {
                return Err(format!(
                    "unknown trigger event '{other}' (want 'cont' or 'pulse')"
                ));
            }
        };
        sim.trigger_input(t.comp, event, &t.state)
            .map_err(|e| format!("trigger_input(comp {}): {e}", t.comp))?;
    }
    Ok(())
}

/// Drive the simulation, applying timed triggers and invoking `on_frame(sim, tick)` once per
/// observed tick — at tick 0 (after tick-0 triggers) and after every step. Stops at `max_ticks`,
/// at `timeout`, or immediately if neither bound is set. Returns the number of ticks advanced.
///
/// The trigger/frame ordering matches the golden generator exactly (`apply(t)` then observe frame
/// `t`), so a `trace` reproduces a corpus golden tick-for-tick.
pub fn drive(
    sim: &mut Simulation,
    max_ticks: Option<u64>,
    timeout: Option<Duration>,
    triggers: &[Trigger],
    mut on_frame: impl FnMut(&Simulation, u64) -> Result<(), String>,
) -> Result<u64, String> {
    let start = Instant::now();
    apply_triggers(sim, triggers, 0)?;
    on_frame(sim, 0)?;
    let mut t = 0u64;
    loop {
        if max_ticks.is_some_and(|m| t >= m) {
            break;
        }
        if timeout.is_some_and(|to| start.elapsed() >= to) {
            break;
        }
        if max_ticks.is_none() && timeout.is_none() {
            break;
        }
        sim.tick();
        t += 1;
        apply_triggers(sim, triggers, t)?;
        on_frame(sim, t)?;
    }
    Ok(t)
}
