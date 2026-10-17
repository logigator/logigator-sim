//! Board/fixture loading and the shared tick driver.
//!
//! A board file is one of: a JSON [`BoardDescriptor`] (`{components}`), a JSON **corpus
//! fixture** wrapper (`{name?, ticks?, inputWrites?, board}` — the shape under
//! `corpus/boards/` and `corpus/bench/`), or a `.lgb` binary. `run`/`trace` accept any of these; the
//! fixture wrapper additionally carries timed input ([`TimedInput`]) and a default tick count.
//! Format is auto-detected from the extension (`.lgb` → binary, else JSON) unless overridden.

use sim_core::{BoardDescriptor, InputFrame, InputWrite, Simulation};
use std::path::Path;
use std::time::{Duration, Instant};

/// Board file encoding.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Format {
    Json,
    Bin,
}

/// One `submitInput` request from a fixture's `inputWrites`, in the JS API's shape.
#[derive(serde::Deserialize)]
pub struct InputRequest {
    /// Submitted once the simulation has run this many ticks, so its first frames apply in tick
    /// `tick + 1`.
    pub tick: u64,
    writes: Vec<WriteJson>,
}

#[derive(serde::Deserialize)]
struct WriteJson {
    comp: u32,
    frames: Vec<FrameJson>,
    #[serde(default)]
    replace: bool,
}

#[derive(serde::Deserialize)]
struct FrameJson {
    state: Vec<Option<bool>>,
    #[serde(default = "one")]
    hold: u32,
    #[serde(default)]
    coalesce: bool,
}

fn one() -> u32 {
    1
}

impl InputRequest {
    fn submit(&self, sim: &mut Simulation) -> Result<(), String> {
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
            .map_err(|e| format!("inputWrites at tick {}: {e}", self.tick))
    }
}

/// A fixture's timed input: its `inputWrites` requests, each submitted at exactly its tick count.
#[derive(Default)]
pub struct TimedInput {
    pub requests: Vec<InputRequest>,
}

impl TimedInput {
    /// Submit the requests due once the simulation has run `tick` ticks.
    pub fn submit_due(&self, sim: &mut Simulation, tick: u64) -> Result<(), String> {
        for r in self.requests.iter().filter(|r| r.tick == tick) {
            r.submit(sim)?;
        }
        Ok(())
    }

    /// The last tick count a request is due at, or `None` if there is no input.
    pub fn last_due(&self) -> Option<u64> {
        self.requests.iter().map(|r| r.tick).max()
    }
}

/// The corpus fixture wrapper around a board.
#[derive(serde::Deserialize)]
struct Fixture {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ticks: Option<u64>,
    #[serde(default, rename = "inputWrites")]
    input_writes: Vec<InputRequest>,
    board: BoardDescriptor,
}

/// A loaded board plus anything a fixture wrapper supplied.
pub struct Loaded {
    /// Display name (fixture `name`, else the file stem).
    pub name: String,
    pub desc: BoardDescriptor,
    /// Default tick budget from a fixture, if any (a CLI `--ticks` overrides it).
    pub ticks: Option<u64>,
    /// Timed input from a fixture (empty for a bare board / `.lgb`).
    pub input: TimedInput,
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
                input: TimedInput::default(),
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
                    input: TimedInput {
                        requests: fx.input_writes,
                    },
                })
            } else {
                let desc: BoardDescriptor = serde_json::from_value(value)
                    .map_err(|e| format!("parsing {}: {e}", path.display()))?;
                Ok(Loaded {
                    name: stem,
                    desc,
                    ticks: None,
                    input: TimedInput::default(),
                })
            }
        }
    }
}

/// Drive the simulation, submitting timed input ([`TimedInput::submit_due`]) and invoking
/// `on_frame(sim, tick)` once per observed tick — at tick 0 and after every step. Stops at
/// `max_ticks`, at `timeout`, or immediately if neither bound is set. Returns the number of ticks
/// advanced.
pub fn drive(
    sim: &mut Simulation,
    max_ticks: Option<u64>,
    timeout: Option<Duration>,
    input: &TimedInput,
    mut on_frame: impl FnMut(&Simulation, u64) -> Result<(), String>,
) -> Result<u64, String> {
    let start = Instant::now();
    input.submit_due(sim, 0)?;
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
        input.submit_due(sim, t)?;
        on_frame(sim, t)?;
    }
    Ok(t)
}
