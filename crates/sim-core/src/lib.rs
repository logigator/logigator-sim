//! `sim-core` — the Logigator logic-circuit simulation engine.
//!
//! A circuit is modelled as **links** carrying a boolean `powered` value, driven by component
//! **outputs** under wired-OR (bus) semantics, advanced by a change-driven, double-buffered tick
//! loop. State lives in cache-friendly struct-of-arrays with dense `u32` ids and CSR adjacency;
//! component dispatch is a generated `enum` + `match` over per-type dirty queues (plan §1.2/§1.6).
//!
//! This is the phase-1 skeleton: `BitSet`, SoA `Board::compile`, the single-threaded `tick()`,
//! and the `component_table!` macro wiring NOT/AND/OR/XOR/DELAY + UserInput. Remaining component
//! types, the CLI/WASM/Node surfaces, adaptive multithreading, and SIMD land in later phases.

mod bitset;

pub use bitset::BitSet;
