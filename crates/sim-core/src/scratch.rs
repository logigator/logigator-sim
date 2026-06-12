//! Per-component mutable scratch for stateful kernels (plan §5.2, §5.3a).
//!
//! Every field is interior-mutable (atomics / [`BitSet`](crate::bitset::BitSet)) so a kernel can
//! mutate it through the shared `&Scratch` held by [`TickCtx`](crate::components::TickCtx) while
//! the dirty-word buffer is borrowed mutably. Everything uses plain relaxed load/store — which
//! lowers to a plain `mov`, so the atomic *type* costs nothing on the hot path (§1.3a, I7).
//!
//! Slots are sized by component count and indexed by component id; only the components of the
//! relevant type ever touch a given field (e.g. only DEC/DEMUX use `sel`).

use crate::bitset::BitSet;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering::Relaxed};

/// Fixed board seed for the per-component RNG (type 16). Fixed (not time-based) so RNG output is
/// **reproducible** run-to-run — the whole point of the §0 RNG rework (D7/§8.4).
const BOARD_SEED: u64 = 0x1234_5678_9ABC_DEF0;

/// SplitMix64 — a fast, well-distributed 64-bit mixing function. Used both to derive each RNG's
/// per-component seed and (in the kernel) to draw its per-tick word.
#[inline]
pub(crate) fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

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
    /// One bit per component: whether a CLK (6) is subscribed to the between-tick period toggle.
    /// Toggled by the CLK's own enable input in the compute phase (`clk.h::outputChange`): a high
    /// enable freezes the clock (unsubscribe), a low enable runs it (subscribe). Seeded by the
    /// simulation to high for every CLK at construction.
    clk_subscribed: BitSet,
    /// Per-component RNG (16) seed `splitmix64(BOARD_SEED ^ public_id)`. The kernel draws a pure
    /// function of `(seed, tick)`, so no per-tick latch is needed: re-execution within a tick
    /// recomputes the same bits (idempotent, thread-count-independent) — this consciously
    /// collapses the plan's `rng_state { seed, last_tick }` to seed-only. The slot is indexed by
    /// the *internal* (type-bucketed) component id like every other scratch field, but the seed
    /// keys on the **public** (submission-order) id — the D13/D17 reproducibility invariant: an
    /// RNG's output stream must not change when the internal layout does (pinned by
    /// `rng_bytes_are_pinned`).
    rng_seed: Box<[u64]>,
}

impl Scratch {
    /// Allocate zeroed scratch for a board with `ram_bytes` total RAM backing-store bytes;
    /// `int2pub` (one entry per component) sizes the per-component slots and keys the RNG seeds
    /// on public ids.
    pub(crate) fn new(int2pub: &[u32], ram_bytes: u32) -> Self {
        let comp_count = int2pub.len() as u32;
        let n = int2pub.len();
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
            clk_subscribed: BitSet::new(comp_count),
            rng_seed: int2pub
                .iter()
                .map(|&pub_id| splitmix64(BOARD_SEED ^ pub_id as u64))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    /// Per-component RNG seed (see field docs for the D13/D17 reproducibility invariant).
    #[inline]
    pub(crate) fn rng_seed(&self, c: u32) -> u64 {
        self.rng_seed[c as usize]
    }

    /// Whether CLK component `c` is subscribed to the period toggle.
    #[inline]
    pub(crate) fn clk_subscribed(&self, c: u32) -> bool {
        self.clk_subscribed.get(c)
    }

    /// Set CLK component `c`'s subscription state (plain load/store — the tick is single-threaded).
    #[inline]
    pub(crate) fn set_clk_subscribed(&self, c: u32, v: bool) {
        self.clk_subscribed.set(c, v);
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

    /// Latch the clock/enable level of edge-clocked component `c` (called every compute; plain
    /// load/store — the tick is single-threaded).
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
