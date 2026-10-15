//! Board/fixture loading and the shared tick driver.
//!
//! A board file is one of: a JSON [`BoardDescriptor`] (`{links, components}`), a JSON **corpus
//! fixture** wrapper (`{name?, ticks?, triggers?, inputWrites?, board}` — the shape under
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

/// One timed trigger from a corpus fixture (`gen-one.mjs` / `tests/golden.rs` shape).
#[derive(serde::Deserialize)]
pub struct Trigger {
    pub tick: u64,
    pub comp: u32,
    pub event: String,
    pub state: Vec<bool>,
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

/// A fixture's timed input: corpus `triggers` (the C++-oracle format, see [`submit_trigger`]) and
/// `inputWrites` (requests in the engine's own format, submitted at exactly their tick count).
#[derive(Default)]
pub struct TimedInput {
    pub triggers: Vec<Trigger>,
    pub writes: Vec<InputRequest>,
}

impl TimedInput {
    /// Submit everything due once the simulation has run `tick` ticks: the triggers scheduled for
    /// `tick - 1` (see below) and the input requests for `tick`.
    ///
    /// A fixture trigger scheduled for tick T is submitted one boundary late, after the step to
    /// T + 1, so its frames are applied in tick T + 2. That is the tick the C++ engine the corpus
    /// goldens come from drove such a write onto its wire (its inputs took two ticks; this
    /// engine's take one), so the links of a `trace` still match the goldens.
    pub fn submit_due(&self, sim: &mut Simulation, tick: u64) -> Result<(), String> {
        for t in self.triggers.iter().filter(|t| t.tick + 1 == tick) {
            submit_trigger(sim, t)?;
        }
        for r in self.writes.iter().filter(|r| r.tick == tick) {
            r.submit(sim)?;
        }
        Ok(())
    }

    /// The last tick count anything is due at ([`TimedInput::submit_due`]), or `None` if there is
    /// no input.
    pub fn last_due(&self) -> Option<u64> {
        let triggers = self.triggers.iter().map(|t| t.tick + 1);
        triggers.chain(self.writes.iter().map(|r| r.tick)).max()
    }

    /// Submit the input meant for the start of the run right away, without the trigger shift: the
    /// tick-0 triggers and requests. `bench` uses this as its kick.
    pub fn submit_start(&self, sim: &mut Simulation) -> Result<(), String> {
        for t in self.triggers.iter().filter(|t| t.tick == 0) {
            submit_trigger(sim, t)?;
        }
        for r in self.writes.iter().filter(|r| r.tick == 0) {
            r.submit(sim)?;
        }
        Ok(())
    }
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
                        triggers: fx.triggers,
                        writes: fx.input_writes,
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

/// Submit one fixture trigger as input frames: `cont` is one coalescing frame (two between the
/// same two ticks merge, so the last one wins), `pulse` two events, the second returning every pin
/// to 0.
pub fn submit_trigger(sim: &mut Simulation, t: &Trigger) -> Result<(), String> {
    let set: Vec<Option<bool>> = t.state.iter().map(|&v| Some(v)).collect();
    let zeros = vec![Some(false); t.state.len()];
    let frame = |state, coalesce| InputFrame {
        state,
        hold: 1,
        coalesce,
    };
    let frames = match t.event.as_str() {
        "cont" => vec![frame(&set, true)],
        "pulse" => vec![frame(&set, false), frame(&zeros, false)],
        other => {
            return Err(format!(
                "unknown trigger event '{other}' (want 'cont' or 'pulse')"
            ));
        }
    };
    sim.submit_input(&[InputWrite {
        comp: t.comp,
        replace: false,
        frames: &frames,
    }])
    .map_err(|e| format!("submit_input(comp {}): {e}", t.comp))
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
