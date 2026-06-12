//! `sim-core` — the Logigator logic-circuit simulation engine.
//!
//! A circuit is modelled as **links** carrying a boolean `powered` value, driven by component
//! **outputs** under wired-OR (bus) semantics, advanced by a change-driven, double-buffered tick
//! loop. State lives in cache-friendly struct-of-arrays with dense `u32` ids and CSR adjacency;
//! component dispatch is a generated `enum` + `match` over per-type dirty queues.
//!
//! The `component_table!` macro wires the **full component set** — gates (NOT/AND/OR/XOR/DELAY),
//! adders, ROM, the edge-clocked D/JK/SR flip-flops + RAM + LED matrix, DEC/ENC/MUX/DEMUX, the
//! input-gated CLK, the per-component-seeded RNG, and UserInput — over SoA `Board::compile` + the
//! change-driven `tick()`, with the `.lgb` binary board [`codec`] and the coherent tick-boundary
//! [`snapshot`] machinery (full / delta). The engine is single-threaded; an earlier
//! adaptive parallel driver was removed after profiling showed it a net loss for every realistic
//! board size. The SIMD kernels handle wide-fan-in gate reductions.

mod bitset;
mod board;
pub mod codec;
mod components;
mod error;
#[cfg(test)]
mod proptests;
mod scratch;
mod sim;
mod simd;
mod snapshot;
mod tick;
mod types;

pub use bitset::BitSet;
pub use board::{Board, BoardBuilder, BoardDescriptor, ComponentDescriptor};
pub use error::{Result, SimError};
pub use sim::{RunConfig, Simulation, Status};
pub use snapshot::{SnapshotConfig, SnapshotInfo};
pub use types::{CompType, InputEvent, SimState};
