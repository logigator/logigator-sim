//! `sim-core` — the Logigator logic-circuit simulation engine.
//!
//! A circuit is modelled as **links** carrying a boolean `powered` value, driven by component
//! **outputs** under wired-OR (bus) semantics, advanced by a change-driven, double-buffered tick
//! loop. State lives in cache-friendly struct-of-arrays with dense `u32` ids and CSR adjacency;
//! component dispatch is a generated `enum` + `match` over per-type dirty queues.
//!
//! The `component_table!` macro wires the **full component set** — gates (NOT/AND/OR/XOR/DELAY),
//! adders, ROM, the edge-clocked D/JK/SR flip-flops + RAM + LED matrix, DEC/ENC/MUX/DEMUX, the
//! input-gated CLK, the per-component-seeded RNG, UserInput (fed by per-input timed frame
//! queues, see [`Simulation::submit_input`]) and the recorder (its captures read back through the
//! versioned component-state API, see [`Simulation::read_records`]) — over SoA `Board::compile` + the change-driven
//! `tick()`, with the `.lgb` binary board [`codec`] and the coherent tick-boundary
//! [`snapshot`] machinery (full / delta). The engine is single-threaded; an earlier
//! adaptive parallel driver was removed after profiling showed it a net loss for every realistic
//! board size. The `reduce` module handles wide-fan-in gate reductions.

mod bitset;
mod board;
pub mod codec;
mod components;
mod error;
mod input;
#[cfg(test)]
mod proptests;
mod reduce;
mod scratch;
mod sim;
mod snapshot;
mod state;
mod tick;
mod types;

pub use bitset::BitSet;
pub use board::{
    Board, BoardBuilder, BoardDescriptor, CLK_PULSE, CLK_SQUARE, ComponentDescriptor,
    RAM_ASYNC_READ, RAM_CLOCKED_READ,
};
pub use error::{Result, SimError};
pub use input::{InputFrame, InputInfo, InputWrite, MAX_PENDING_EVENTS, validate_input_writes};
pub use sim::{RunConfig, Simulation, Status};
pub use snapshot::{SnapshotConfig, SnapshotInfo};
pub use state::{MAX_RECORDER_CAPACITY, RECORD_TICKS, Records, StateLayout};
pub use types::{CompType, SimState};
