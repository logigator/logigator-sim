//! Component kernels and the generated dispatch (plan §5.3).
//!
//! Dispatch is an `enum` + `match` over per-type dirty queues, fed by a single declarative table
//! ([`component_table!`]): one row per type yields the arity rule, the power-on init hook, and the
//! per-tick compute call. Adding a type is one row plus a [`Kernel`] impl. The match is exhaustive
//! over [`CompType`], so a new variant without a table row fails to compile — table and wire enum
//! cannot drift.
//!
//! Phase 1 wires the combinational core (NOT/AND/OR/XOR/DELAY) and `UserInput`. The remaining types
//! land in plan phase 2.

mod adders;
mod flipflops;
mod gates;
mod ram;
mod rom;
mod selectors;
mod user_input;

use crate::CompType;
use crate::bitset::BitSet;
use crate::board::{Board, CompConfig};
use crate::scratch::Scratch;
use core::sync::atomic::{AtomicU16, Ordering::Relaxed};

pub(crate) use adders::{FullAdder, HalfAdder};
pub(crate) use flipflops::{DFf, JkFf, SrFf};
pub(crate) use gates::{And, Delay, Not, Or, Xor};
pub(crate) use ram::Ram;
pub(crate) use rom::Rom;
pub(crate) use selectors::{Decoder, Demux, Encoder, Mux};
pub(crate) use user_input::UserInput;

/// Sentinel upper bound for "unbounded" arity (e.g. an N-input AND).
const INF: usize = usize::MAX;

/// Permitted input/output/ops counts for a component type (plan §6.1 step 2 validation).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Arity {
    pub in_min: usize,
    pub in_max: usize,
    pub out_min: usize,
    pub out_max: usize,
    pub ops_min: usize,
    pub ops_max: usize,
}

impl Arity {
    /// Whether the given counts satisfy this arity.
    #[inline]
    pub(crate) fn accepts(&self, ins: usize, outs: usize, ops: usize) -> bool {
        (self.in_min..=self.in_max).contains(&ins)
            && (self.out_min..=self.out_max).contains(&outs)
            && (self.ops_min..=self.ops_max).contains(&ops)
    }
}

/// Compute-phase context handed to every [`Kernel`] (plan §5.3).
///
/// Reads come from the **frozen** `link_state` (invariant I1: `link_state` never changes during
/// compute). Writes go only through [`set_output`](TickCtx::set_output), which — on a real flip —
/// toggles `output_state`, applies `±1` to the driven link's `driver_count` (the incremental
/// wired-OR count, D3/I2), and schedules that link into `write_buf` for the next read phase.
/// `set_output` never touches `link_state` (invariant I1/D4).
pub(crate) struct TickCtx<'a> {
    /// Immutable topology + compiled config (CSR adjacency, output→link map, ROM data).
    board: &'a Board,
    // State.
    link_state: &'a BitSet,        // frozen snapshot compute() reads
    output_state: &'a BitSet,      // each output pin's own value
    driver_count: &'a [AtomicU16], // # of currently-powered drivers per link
    scratch: &'a Scratch,          // interior-mutable per-component scratch (sel-latch, …)
    write_buf: &'a mut Vec<u32>,   // links whose net value may change next tick
}

impl<'a> TickCtx<'a> {
    /// Borrow the pieces needed for a compute phase. `link_state` must be the frozen snapshot.
    pub(crate) fn new(
        board: &'a Board,
        link_state: &'a BitSet,
        output_state: &'a BitSet,
        driver_count: &'a [AtomicU16],
        scratch: &'a Scratch,
        write_buf: &'a mut Vec<u32>,
    ) -> Self {
        TickCtx {
            board,
            link_state,
            output_state,
            driver_count,
            scratch,
            write_buf,
        }
    }

    /// Input link ids of component `c`. The returned slice is tied to the topology lifetime, not to
    /// `&self`, so a kernel can read its inputs and then call `set_output` without a borrow clash.
    #[inline]
    pub(crate) fn inputs(&self, c: u32) -> &'a [u32] {
        let c = c as usize;
        let (off, inputs) = (&self.board.comp_in_off, &self.board.comp_inputs);
        &inputs[off[c] as usize..off[c + 1] as usize]
    }

    /// The first output id of `c` — the common case for single-output components.
    #[inline]
    pub(crate) fn first_output(&self, c: u32) -> u32 {
        self.board.comp_out_off[c as usize]
    }

    /// The id of output pin `pin` of component `c` (`pin` must be in range for the type's arity).
    #[inline]
    pub(crate) fn output_at(&self, c: u32, pin: u32) -> u32 {
        self.board.comp_out_off[c as usize] + pin
    }

    /// Number of output pins of component `c`.
    #[inline]
    pub(crate) fn output_count(&self, c: u32) -> u32 {
        let c = c as usize;
        self.board.comp_out_off[c + 1] - self.board.comp_out_off[c]
    }

    /// Compiled configuration of component `c` (see [`CompConfig`]).
    #[inline]
    pub(crate) fn config(&self, c: u32) -> CompConfig {
        self.board.config(c)
    }

    /// The immutable ROM (type 12) data pool; a ROM's blob starts at byte `config(c).a`.
    #[inline]
    pub(crate) fn rom_data(&self) -> &'a [u8] {
        &self.board.rom_data
    }

    /// Currently-selected output index of DEC/DEMUX component `c` (the §5.3a `sel`-latch). Takes
    /// `&self` (interior-mutable scratch) so a kernel can latch it alongside a `set_output`.
    #[inline]
    pub(crate) fn sel(&self, c: u32) -> u32 {
        self.scratch.sel(c)
    }

    /// Latch the selected output index of DEC/DEMUX component `c`.
    #[inline]
    pub(crate) fn set_sel(&self, c: u32, v: u32) {
        self.scratch.set_sel(c, v);
    }

    /// Read an input link's powered value from the frozen snapshot.
    #[inline]
    pub(crate) fn input(&self, link: u32) -> bool {
        self.link_state.get(link)
    }

    /// Read output pin `oid`'s own current value (e.g. the JK flip-flop toggles its own outputs).
    #[inline]
    pub(crate) fn output(&self, oid: u32) -> bool {
        self.output_state.get(oid)
    }

    /// Previous clock/enable level of edge-clocked component `c` (the §5.3a rising-edge latch).
    #[inline]
    pub(crate) fn edge_prev(&self, c: u32) -> bool {
        self.scratch.edge_prev(c)
    }

    /// Latch the clock/enable level of edge-clocked component `c`. Call **unconditionally** each
    /// compute (a falling edge must reset it, or the component fires once and never again).
    #[inline]
    pub(crate) fn set_edge_prev(&self, c: u32, v: bool) {
        self.scratch.set_edge_prev(c, v);
    }

    /// Read bit position `p` of a RAM's backing store, whose region starts at byte `base`
    /// (`config(c).a`). `p` is a bit index within the region (`p = position + i` in `ram.h`).
    #[inline]
    pub(crate) fn ram_get(&self, base: usize, p: usize) -> bool {
        self.scratch.mem_bit(base + (p >> 3), (p & 7) as u32)
    }

    /// Write bit position `p` of a RAM's backing store (region at byte `base`).
    #[inline]
    pub(crate) fn ram_set(&self, base: usize, p: usize, v: bool) {
        self.scratch.set_mem_bit(base + (p >> 3), (p & 7) as u32, v);
    }

    /// Drive output pin `oid` to `v`. Only a *real* flip mutates state (matches the old
    /// `Output::setPowered`): it toggles `output_state`, applies `±1` to the driven link's
    /// `driver_count`, and pushes the link onto `write_buf`. Repeated/idempotent writes of the
    /// same value are no-ops.
    #[inline]
    pub(crate) fn set_output(&mut self, oid: u32, v: bool) {
        if self.output_state.get(oid) == v {
            return;
        }
        self.output_state.set(oid, v);
        let link = self.board.output_link[oid as usize] as usize;
        let dc = &self.driver_count[link];
        let cur = dc.load(Relaxed);
        // Single-threaded load/store (the parallel driver uses fetch_add here, phase 6).
        dc.store(if v { cur + 1 } else { cur - 1 }, Relaxed);
        self.write_buf.push(link as u32);
    }
}

/// One component-type kernel. Stateless gates implement only `compute_batch`; types whose power-on
/// state is non-zero (e.g. NOT) also override `init`.
pub(crate) trait Kernel {
    /// Replay this type's power-on initialization through the same flip→count→push path the runtime
    /// uses (plan §6.1 step 4). Default: seed nothing.
    #[inline]
    fn init(_c: u32, _ctx: &mut TickCtx<'_>) {}

    /// Recompute every component id in `dirty` from the frozen `link_state`, writing results via
    /// `set_output`. `dirty` may contain a component more than once in a tick; kernels must be
    /// idempotent under that (invariant I3 — trivially true for the pure gates).
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>);
}

/// The single source of truth for arity + dispatch (plan §5.3). One row per implemented type.
macro_rules! component_table {
    ($($variant:ident => $kernel:ident
        ($imin:expr, $imax:expr) ($omin:expr, $omax:expr) ($pmin:expr, $pmax:expr);)+) => {
        /// Every implemented type, in dispatch order; indexes the per-type compute queues.
        pub(crate) const ALL_TYPES: &[CompType] = &[$(CompType::$variant),+];

        /// Number of implemented component types (= number of per-type compute queues).
        pub(crate) const N_TYPES: usize = ALL_TYPES.len();

        /// Dense queue index `0..N_TYPES` for a type (precomputed per component at build time).
        pub(crate) const fn type_index(ty: CompType) -> usize {
            let mut i = 0;
            while i < N_TYPES {
                if ALL_TYPES[i] as u16 == ty as u16 {
                    return i;
                }
                i += 1;
            }
            panic!("CompType is not in the component_table!")
        }

        /// Inverse of [`type_index`].
        #[inline]
        pub(crate) const fn type_from_index(i: usize) -> CompType {
            ALL_TYPES[i]
        }

        /// Arity rule for a component type.
        pub(crate) const fn arity(ty: CompType) -> Arity {
            match ty {
                $(CompType::$variant => Arity {
                    in_min: $imin, in_max: $imax,
                    out_min: $omin, out_max: $omax,
                    ops_min: $pmin, ops_max: $pmax,
                },)+
            }
        }

        /// Seed a component's power-on state (init phase).
        pub(crate) fn dispatch_init(ty: CompType, c: u32, ctx: &mut TickCtx<'_>) {
            match ty {
                $(CompType::$variant => <$kernel as Kernel>::init(c, ctx),)+
            }
        }

        /// Run one type's per-tick compute over its dirty queue.
        pub(crate) fn dispatch_compute(ty: CompType, dirty: &[u32], ctx: &mut TickCtx<'_>) {
            match ty {
                $(CompType::$variant => <$kernel as Kernel>::compute_batch(dirty, ctx),)+
            }
        }
    };
}

component_table! {
    //  wire variant   kernel       inputs       outputs      ops
    Not       => Not       (1, 1)     (1, 1)     (0, 0);
    And       => And       (2, INF)   (1, 1)     (0, 0);
    Or        => Or        (2, INF)   (1, 1)     (0, 0);
    Xor       => Xor       (2, INF)   (1, 1)     (0, 0);
    Delay     => Delay     (1, 1)     (1, 1)     (0, 0);
    HalfAdder => HalfAdder (2, 2)     (2, 2)     (0, 0);
    FullAdder => FullAdder (3, 3)     (2, 2)     (0, 0);
    Rom       => Rom       (1, 16)    (1, 64)    (0, INF);
    DFf       => DFf       (2, 2)     (2, 2)     (0, 0);
    JkFf      => JkFf      (3, 3)     (2, 2)     (0, 0);
    SrFf      => SrFf      (3, 3)     (2, 2)     (0, 0);
    Ram       => Ram       (3, INF)   (1, INF)   (0, 0);
    Decoder   => Decoder   (1, 16)    (2, INF)   (0, 0);
    Encoder   => Encoder   (2, INF)   (1, 16)    (0, 0);
    Mux       => Mux       (3, INF)   (1, 1)     (1, 1);
    Demux     => Demux     (2, INF)   (2, INF)   (0, 0);
    UserInput => UserInput (0, 0)     (1, INF)   (0, 0);
}
