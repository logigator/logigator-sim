//! `sim-core` — the Logigator logic-circuit simulation engine.
//!
//! A circuit is modelled as **links** carrying a boolean `powered` value, driven by component
//! **outputs** under wired-OR (bus) semantics, advanced by a change-driven, double-buffered tick
//! loop. State lives in cache-friendly struct-of-arrays with dense `u32` ids and CSR adjacency;
//! component dispatch is a generated `enum` + `match` over per-type dirty queues (plan §1.2/§1.6).
//!
//! Phases 1–2 are complete: `BitSet`, SoA `Board::compile`, the single-threaded `tick()`, and the
//! `component_table!` macro wiring the **full component set** — gates (NOT/AND/OR/XOR/DELAY),
//! adders, ROM, the edge-clocked D/JK/SR flip-flops + RAM + LED matrix, DEC/ENC/MUX/DEMUX, the
//! input-gated CLK, the per-component-seeded RNG, and UserInput. The CLI/WASM/Node surfaces,
//! adaptive multithreading, and SIMD land in later phases.

mod bitset;
mod board;
mod components;
mod error;
mod scratch;
mod sim;
mod tick;
mod types;

pub use bitset::BitSet;
pub use board::{Board, BoardBuilder, BoardDescriptor, ComponentDescriptor};
pub use error::{Result, SimError};
pub use sim::{RunConfig, Simulation, Status};
pub use types::{CompType, InputEvent, SimState};
