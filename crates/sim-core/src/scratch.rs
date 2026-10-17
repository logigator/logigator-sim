//! Per-component mutable scratch for stateful kernels.
//!
//! Every field is interior-mutable (atomics / [`BitSet`](crate::bitset::BitSet)) so a kernel can
//! mutate it through the shared `&Scratch` held by [`TickCtx`](crate::components::TickCtx) while
//! `write_buf` is borrowed mutably. The tick is single-threaded, so every access is a relaxed
//! load/store — which lowers to a plain `mov`, so the atomic *type* costs nothing on the hot path.
//!
//! Slots are sized by component count and indexed by component id; only the components of the
//! relevant type ever touch a given field (e.g. only DEC/DEMUX use `sel`).

use crate::bitset::BitSet;
use crate::board::{Board, RecorderConfig};
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering::Relaxed};

/// Fixed board seed for the per-component RNG (type 16). Fixed (not time-based) so RNG output is
/// **reproducible** run-to-run.
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
    /// DEC/DEMUX currently-selected output index — the idempotent `sel`-latch. Seeded to 0
    /// (DEC drives `out[0]` at init; DEMUX starts all-low with `sel = 0`).
    sel: Box<[AtomicU32]>,
    /// One bit per component: the previous clock/enable-input level of an edge-clocked component
    /// (D/JK/SR flip-flops, RAM, LED matrix). All rising-edge: a kernel acts on `clk && !prev` then
    /// re-latches `prev = clk` **unconditionally** every compute, so a falling edge resets it and
    /// duplicate computes in one tick converge. Starts all-low.
    edge_prev: BitSet,
    /// RAM (17) backing store: all RAMs' byte-addressed memory concatenated; a RAM's region starts
    /// at byte `config.a`. Starts zeroed.
    mem: Box<[AtomicU8]>,
    /// RAM (17) state versions, one per RAM at its `config.b`: bumped (wrapping) by every write
    /// that changes a stored bit. Starts at 0.
    ram_version: Box<[AtomicU32]>,
    /// Recorder (201) value rings, concatenated; a recorder's ring starts at its
    /// [`RecorderConfig::values`]. Record `i` sits in slot `i % capacity`.
    rec_values: Box<[AtomicU32]>,
    /// Recorder tick rings, laid out like `rec_values`, for the recorders that record ticks.
    rec_ticks: Box<[AtomicU64]>,
    /// Records ever written per recorder, at its `config.a`. Never reset.
    rec_count: Box<[AtomicU64]>,
    /// One bit per component: whether a CLK (6) is subscribed to the between-tick period toggle.
    /// Toggled by the CLK's own halt input in the compute phase: a high
    /// halt freezes the clock (unsubscribe), a low halt runs it (subscribe). Seeded by the
    /// simulation to high for every CLK at construction.
    clk_subscribed: BitSet,
    /// Per-component RNG (16) seed `splitmix64(BOARD_SEED ^ id)`. The kernel draws a pure function
    /// of `(seed, tick)`, so no per-tick latch is needed: re-execution within a tick recomputes the
    /// same bits (idempotent). `id` is the component id, which today is the stable submission-order
    /// id; any future internal renumbering MUST keep keying this on the public id (via a
    /// translation table) or every RNG's reproducible output silently changes.
    rng_seed: Box<[u64]>,
    /// One bit per component: whether an SR flip-flop (15) holds the invalid state left by a clock
    /// edge with S and R both high. Its outputs are invalid while it is set.
    invalid: BitSet,
    /// Per link: how many of its drivers are outputs of a component in the invalid state.
    invalid_drivers: Box<[AtomicU16]>,
}

/// `n` zeroed atomics.
fn zeroed<T: Default>(n: usize) -> Box<[T]> {
    (0..n).map(|_| T::default()).collect()
}

impl Scratch {
    /// Allocate zeroed scratch for `board`.
    pub(crate) fn new(board: &Board) -> Self {
        let comp_count = board.comp_count;
        Scratch {
            sel: zeroed(comp_count as usize),
            edge_prev: BitSet::new(comp_count),
            mem: zeroed(board.ram_bytes as usize),
            ram_version: zeroed(board.ram_count as usize),
            rec_values: zeroed(board.rec_values),
            rec_ticks: zeroed(board.rec_ticks),
            rec_count: zeroed(board.recorders.len()),
            clk_subscribed: BitSet::new(comp_count),
            rng_seed: (0..comp_count as u64)
                .map(|id| splitmix64(BOARD_SEED ^ id))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            invalid: BitSet::new(comp_count),
            invalid_drivers: zeroed(board.link_count as usize),
        }
    }

    /// Per-component RNG seed (see field docs for the reproducibility invariant).
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

    /// Write bit `bit` of the RAM backing store at byte `byte`; returns whether the bit changed.
    #[inline]
    pub(crate) fn set_mem_bit(&self, byte: usize, bit: u32, v: bool) -> bool {
        let cur = self.mem[byte].load(Relaxed);
        let mask = 1u8 << bit;
        let new = if v { cur | mask } else { cur & !mask };
        self.mem[byte].store(new, Relaxed);
        new != cur
    }

    /// The RAM backing store bytes `range`.
    #[inline]
    pub(crate) fn mem(&self, range: core::ops::Range<usize>) -> &[AtomicU8] {
        &self.mem[range]
    }

    /// State version of the RAM with index `ram` (its `config.b`).
    #[inline]
    pub(crate) fn ram_version(&self, ram: u32) -> u32 {
        self.ram_version[ram as usize].load(Relaxed)
    }

    /// Advance the state version of the RAM with index `ram`, wrapping.
    #[inline]
    pub(crate) fn bump_ram_version(&self, ram: u32) {
        let v = &self.ram_version[ram as usize];
        v.store(v.load(Relaxed).wrapping_add(1), Relaxed);
    }

    /// Records ever written by the recorder with index `rec` (its `config.a`).
    #[inline]
    pub(crate) fn rec_count(&self, rec: u32) -> u64 {
        self.rec_count[rec as usize].load(Relaxed)
    }

    /// Append a record to the recorder with index `rec`, overwriting its oldest once the ring is
    /// full. `tick` is stored only if the recorder records ticks.
    #[inline]
    pub(crate) fn record(&self, rec: u32, cfg: &RecorderConfig, value: u32, tick: u64) {
        let count = &self.rec_count[rec as usize];
        let n = count.load(Relaxed);
        let slot = (n % cfg.capacity as u64) as usize;
        self.rec_values[cfg.values + slot].store(value, Relaxed);
        if let Some(ticks) = cfg.ticks {
            self.rec_ticks[ticks + slot].store(tick, Relaxed);
        }
        count.store(n + 1, Relaxed);
    }

    /// Record `i` of a recorder (`i` must be among its last `capacity`): its value and, if it
    /// records ticks, its tick.
    #[inline]
    pub(crate) fn record_at(&self, cfg: &RecorderConfig, i: u64) -> (u32, Option<u64>) {
        let slot = (i % cfg.capacity as u64) as usize;
        let value = self.rec_values[cfg.values + slot].load(Relaxed);
        let tick = cfg.ticks.map(|t| self.rec_ticks[t + slot].load(Relaxed));
        (value, tick)
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

    /// Whether component `c` is in the invalid state.
    #[inline]
    pub(crate) fn invalid(&self, c: u32) -> bool {
        self.invalid.get(c)
    }

    /// Set component `c`'s invalid state.
    #[inline]
    pub(crate) fn set_invalid(&self, c: u32, v: bool) {
        self.invalid.set(c, v);
    }

    /// Number of `link`'s drivers in the invalid state.
    #[inline]
    pub(crate) fn invalid_drivers(&self, link: u32) -> u16 {
        self.invalid_drivers[link as usize].load(Relaxed)
    }

    /// Count one of `link`'s drivers into (`v`) or out of the invalid state.
    #[inline]
    pub(crate) fn add_invalid_driver(&self, link: u32, v: bool) {
        let n = &self.invalid_drivers[link as usize];
        let cur = n.load(Relaxed);
        n.store(if v { cur + 1 } else { cur - 1 }, Relaxed);
    }
}
