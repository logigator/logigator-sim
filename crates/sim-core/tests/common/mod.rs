//! The corpus fixture shape shared by `corpus/boards/` and `corpus/bench/`:
//! `{name, ticks, inputWrites?, board}`, with `inputWrites` in the JS `submitInput` shape.

// Each test crate compiles its own copy of this module and uses a different subset of it.
#![allow(dead_code)]

use sim_core::{BoardDescriptor, InputFrame, InputWrite, SimError, Simulation};
use std::path::{Path, PathBuf};

#[derive(serde::Deserialize)]
pub struct Fixture {
    pub name: String,
    pub ticks: u64,
    #[serde(default, rename = "inputWrites")]
    pub input_writes: Vec<Request>,
    pub board: BoardDescriptor,
}

impl Fixture {
    pub fn load(path: &Path) -> Fixture {
        let bytes =
            std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
    }

    /// Submit the requests due once the simulation has run `tick` ticks.
    pub fn submit_due(&self, sim: &mut Simulation, tick: u64) -> Result<(), SimError> {
        for r in self.input_writes.iter().filter(|r| r.tick == tick) {
            r.submit(sim)?;
        }
        Ok(())
    }
}

/// One `inputWrites` request: submitted once the simulation has run `tick` ticks, so its first
/// frames apply in tick `tick + 1`.
#[derive(serde::Deserialize)]
pub struct Request {
    pub tick: u64,
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
    #[serde(default = "one")]
    hold: u32,
    #[serde(default)]
    coalesce: bool,
}

fn one() -> u32 {
    1
}

impl Request {
    pub fn submit(&self, sim: &mut Simulation) -> Result<(), SimError> {
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

pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}
