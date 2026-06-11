//! `sim-core` — the Logigator logic-circuit simulation engine.
//!
//! A circuit is modelled as **links** carrying a boolean `powered` value, driven by component
//! **outputs** under wired-OR (bus) semantics, advanced by a change-driven, double-buffered tick
//! loop. State lives in cache-friendly struct-of-arrays with dense `u32` ids and CSR adjacency;
//! component dispatch is a generated `enum` + `match` over per-type dirty queues (plan §1.2/§1.6).
//!
//! The `component_table!` macro wires the **full component set** — gates (NOT/AND/OR/XOR/DELAY),
//! adders, ROM, the edge-clocked D/JK/SR flip-flops + RAM + LED matrix, DEC/ENC/MUX/DEMUX, the
//! input-gated CLK, the per-component-seeded RNG, and UserInput — over SoA `Board::compile` + the
//! change-driven `tick()`, with the `.lgb` binary board [`codec`] and the coherent tick-boundary
//! [`snapshot`] machinery (full / delta, plan §6.4). With the `threads` feature the [`driver`]
//! module adds the **adaptive parallel** run loop (plan §8): single-threaded by default, sharding a
//! phase across a rayon pool only when its per-tick frontier exceeds `par_threshold`, with per-tick
//! state bit-identical to single-threaded (§8.6). The SIMD kernels land in a later phase.

mod bitset;
mod board;
pub mod codec;
mod components;
#[cfg(feature = "threads")]
mod driver;
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
