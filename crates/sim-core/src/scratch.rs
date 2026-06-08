//! Per-component mutable scratch for stateful kernels (plan §5.2, §5.3a).
//!
//! Every field is interior-mutable (atomics / [`BitSet`](crate::bitset::BitSet)) so a kernel can
//! mutate it through the shared `&Scratch` held by [`TickCtx`](crate::components::TickCtx) while
//! `write_buf` is borrowed mutably. The single-threaded path uses plain relaxed load/store — which
//! lowers to a plain `mov`, so the atomic *type* costs nothing on the hot path (§1.3a, I7); the
//! atomic *RMW* forms are reserved for the multi-threaded driver (phase 6).
//!
//! Slots are sized by component count and indexed by component id; only the components of the
//! relevant type ever touch a given field (e.g. only DEC/DEMUX use `sel`).

use crate::bitset::BitSet;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// Mutable per-component scratch, owned by the [`Simulation`](crate::Simulation).
pub(crate) struct Scratch {
    /// DEC/DEMUX currently-selected output index — the idempotent `sel`-latch (§5.3a). Seeded to 0
    /// (DEC drives `out[0]` at init; DEMUX starts all-low with `sel = 0`).
    sel: Box<[AtomicU32]>,
    /// One bit per component: the previous clock/enable-input level of an edge-clocked component
    /// (D/JK/SR flip-flops, and later RAM/LED matrix). All rising-edge: a kernel acts on
    /// `clk && !prev` then re-latches `prev = clk` **unconditionally** every compute, so a falling
    /// edge resets it and duplicate computes in one tick converge (§5.3a). Starts all-low.
    edge_prev: BitSet,
}

impl Scratch {
    /// Allocate zeroed scratch for a board with `comp_count` components.
    pub(crate) fn new(comp_count: u32) -> Self {
        let n = comp_count as usize;
        Scratch {
            sel: (0..n)
                .map(|_| AtomicU32::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            edge_prev: BitSet::new(comp_count),
        }
    }

    /// Previous clock/enable level of edge-clocked component `c`.
    #[inline]
    pub(crate) fn edge_prev(&self, c: u32) -> bool {
        self.edge_prev.get(c)
    }

    /// Latch the clock/enable level of edge-clocked component `c` (called every compute).
    #[inline]
    pub(crate) fn set_edge_prev(&self, c: u32, v: bool) {
        self.edge_prev.set(c, v);
    }

    /// Currently-selected output index of DEC/DEMUX component `c`.
    #[inline]
    pub(crate) fn sel(&self, c: u32) -> u32 {
        self.sel[c as usize].load(Relaxed)
    }

    /// Latch the selected output index of DEC/DEMUX component `c`.
    #[inline]
    pub(crate) fn set_sel(&self, c: u32, v: u32) {
        self.sel[c as usize].store(v, Relaxed);
    }
}
