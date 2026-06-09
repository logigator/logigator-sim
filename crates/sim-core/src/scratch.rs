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
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering::Relaxed};

/// Mutable per-component scratch, owned by the [`Simulation`](crate::Simulation).
pub(crate) struct Scratch {
    /// DEC/DEMUX currently-selected output index — the idempotent `sel`-latch (§5.3a). Seeded to 0
    /// (DEC drives `out[0]` at init; DEMUX starts all-low with `sel = 0`).
    sel: Box<[AtomicU32]>,
    /// One bit per component: the previous clock/enable-input level of an edge-clocked component
    /// (D/JK/SR flip-flops, RAM, LED matrix). All rising-edge: a kernel acts on `clk && !prev` then
    /// re-latches `prev = clk` **unconditionally** every compute, so a falling edge resets it and
    /// duplicate computes in one tick converge (§5.3a). Starts all-low.
    edge_prev: BitSet,
    /// RAM (17) backing store: all RAMs' byte-addressed memory concatenated; a RAM's region starts
    /// at byte `config.a`. Atomic-typed so a double-compute's identical same-value writes don't
    /// race on the MT path (§5.3a); plain load/store on the ST path. Starts zeroed.
    mem: Box<[AtomicU8]>,
}

impl Scratch {
    /// Allocate zeroed scratch for a board with `comp_count` components and `ram_bytes` total RAM
    /// backing-store bytes.
    pub(crate) fn new(comp_count: u32, ram_bytes: u32) -> Self {
        let n = comp_count as usize;
        Scratch {
            sel: (0..n)
                .map(|_| AtomicU32::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            edge_prev: BitSet::new(comp_count),
            mem: (0..ram_bytes)
                .map(|_| AtomicU8::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    /// Read bit `bit` of the RAM backing store at byte `byte`.
    #[inline]
    pub(crate) fn mem_bit(&self, byte: usize, bit: u32) -> bool {
        self.mem[byte].load(Relaxed) & (1 << bit) != 0
    }

    /// Write bit `bit` of the RAM backing store at byte `byte`.
    #[inline]
    pub(crate) fn set_mem_bit(&self, byte: usize, bit: u32, v: bool) {
        let cur = self.mem[byte].load(Relaxed);
        let mask = 1u8 << bit;
        self.mem[byte].store(if v { cur | mask } else { cur & !mask }, Relaxed);
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
