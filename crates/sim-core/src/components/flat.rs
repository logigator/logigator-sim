//! Flat-record kernels for the fixed-arity hot types.
//!
//! The generic path reaches a component's wiring through three CSR lookups (`comp_in_off` →
//! `comp_inputs`, `comp_out_off` → `output_link`), each a dependent load on the read → compute →
//! drive chain. For the types below the shape is fixed, so `Board::compile` lowers every instance
//! into one record holding its input link words, its first output id and its output link words
//! (negate flags packed into the top bit, as in the CSR). The consumer CSR then names the record
//! by its index within the kind's array instead of the component id, and the kernel reads
//! everything it needs from that one record.
//!
//! Each kind has its own compute queue, placed after the generic per-type queues
//! ([`Kind::queue_index`]). AND/OR/XOR take a record only with exactly two inputs; wider gates stay on
//! the generic path. These kernels are the only per-tick compute of NOT, DELAY, the adders and the
//! D flip-flop; init hooks still run through the type kernels (by component id).

use super::{N_TYPES, TickCtx};
use crate::CompType;

/// A single-input, single-output gate (NOT, DELAY).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(crate) struct Rec1 {
    pub inp: u32,
    pub oid: u32,
    pub out: u32,
}

/// A two-input, single-output gate (AND, OR, XOR with two inputs).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(crate) struct Rec2 {
    pub inp: [u32; 2],
    pub oid: u32,
    pub out: u32,
}

/// A two-output component: the adders (a half adder leaves `inp[2]` unused) and the D flip-flop
/// (`inp[2]` holds its component id, which keys its `edge_prev` latch). Output pin `i` is output
/// id `oid + i`, driving `out[i]`.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(crate) struct Rec3 {
    pub inp: [u32; 3],
    pub oid: u32,
    pub out: [u32; 2],
}

/// The flat record kinds, in queue order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    Not,
    Delay,
    And2,
    Or2,
    Xor2,
    HalfAdder,
    FullAdder,
    DFf,
}

/// Number of flat record kinds.
pub(crate) const N_FLAT: usize = 8;

/// Number of compute queues: the generic per-type queues, then one per flat kind.
pub(crate) const N_QUEUES: usize = N_TYPES + N_FLAT;
// The simulation tracks its non-empty queues in a `u32` mask, one bit per queue.
const _: () = assert!(N_QUEUES <= 32);

impl Kind {
    const ALL: [Kind; N_FLAT] = [
        Kind::Not,
        Kind::Delay,
        Kind::And2,
        Kind::Or2,
        Kind::Xor2,
        Kind::HalfAdder,
        Kind::FullAdder,
        Kind::DFf,
    ];

    /// The flat kind a component of type `ty` with `ins` inputs compiles to, if any.
    pub(crate) fn of(ty: CompType, ins: usize) -> Option<Kind> {
        Some(match ty {
            CompType::Not => Kind::Not,
            CompType::Delay => Kind::Delay,
            CompType::And if ins == 2 => Kind::And2,
            CompType::Or if ins == 2 => Kind::Or2,
            CompType::Xor if ins == 2 => Kind::Xor2,
            CompType::HalfAdder => Kind::HalfAdder,
            CompType::FullAdder => Kind::FullAdder,
            CompType::DFf => Kind::DFf,
            _ => return None,
        })
    }

    /// The kind whose compute queue is `qi` (`qi >= N_TYPES`).
    #[inline]
    pub(crate) fn from_queue(qi: usize) -> Kind {
        Kind::ALL[qi - N_TYPES]
    }

    /// This kind's compute queue.
    #[inline]
    pub(crate) const fn queue_index(self) -> usize {
        N_TYPES + self as usize
    }
}

/// The flat records of a board, one array per record shape. A record's index within its array is
/// what the consumer CSR and the kind's compute queue hold. `Rec1` serves NOT and DELAY, `Rec2` the
/// two-input gates and `Rec3` the adders and the D flip-flop, each kind in its own array.
#[derive(Debug, Default)]
pub(crate) struct FlatRecords {
    pub r1: [Box<[Rec1]>; 2],
    pub r2: [Box<[Rec2]>; 3],
    pub r3: [Box<[Rec3]>; 3],
}

impl FlatRecords {
    /// Number of records of `kind`.
    #[cfg(debug_assertions)]
    pub(crate) fn len(&self, kind: Kind) -> usize {
        match kind {
            Kind::Not | Kind::Delay => self.r1[kind as usize].len(),
            Kind::And2 | Kind::Or2 | Kind::Xor2 => self.r2[kind as usize - 2].len(),
            Kind::HalfAdder | Kind::FullAdder | Kind::DFf => self.r3[kind as usize - 5].len(),
        }
    }

    /// Every link word (input and output) a record of `kind` at `i` holds; the D flip-flop's
    /// component-id slot is left out.
    #[cfg(debug_assertions)]
    pub(crate) fn link_words(&self, kind: Kind, i: usize) -> Vec<u32> {
        match kind {
            Kind::Not | Kind::Delay => {
                let r = self.r1[kind as usize][i];
                vec![r.inp, r.out]
            }
            Kind::And2 | Kind::Or2 | Kind::Xor2 => {
                let r = self.r2[kind as usize - 2][i];
                vec![r.inp[0], r.inp[1], r.out]
            }
            Kind::HalfAdder => {
                let r = self.r3[0][i];
                vec![r.inp[0], r.inp[1], r.out[0], r.out[1]]
            }
            Kind::FullAdder => {
                let r = self.r3[1][i];
                vec![r.inp[0], r.inp[1], r.inp[2], r.out[0], r.out[1]]
            }
            Kind::DFf => {
                let r = self.r3[2][i];
                vec![r.inp[0], r.inp[1], r.out[0], r.out[1]]
            }
        }
    }
}

/// Fetch record `i` of a kind's array without a bounds check.
///
/// # Safety
///
/// `i` is below `recs.len()`: every index in a flat kind's queue came from the consumer CSR or
/// [`Board::comp_entry`](crate::board::Board), which hold only indices into that kind's array (a
/// board invariant).
#[inline(always)]
unsafe fn rec<T: Copy>(recs: &[T], i: u32) -> T {
    debug_assert!((i as usize) < recs.len());
    unsafe { *recs.get_unchecked(i as usize) }
}

/// Run one flat kind's compute over its dirty queue of record indices.
pub(crate) fn dispatch_compute(kind: Kind, dirty: &[u32], ctx: &mut TickCtx<'_>) {
    let recs = &ctx.board.flat;
    // SAFETY (every `rec`, `read` and `set_output_raw` below): queue entries index the kind's record array, and a
    // record's link words are below `link_count` with the negate bit masked (board invariants).
    match kind {
        Kind::Not => {
            let rs = &recs.r1[0];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let v = !unsafe { ctx.read(r.inp) };
                unsafe { ctx.set_output_raw(r.oid, r.out, v) };
            }
        }
        Kind::Delay => {
            let rs = &recs.r1[1];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let v = unsafe { ctx.read(r.inp) };
                unsafe { ctx.set_output_raw(r.oid, r.out, v) };
            }
        }
        Kind::And2 => {
            let rs = &recs.r2[0];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let v = unsafe { ctx.read(r.inp[0]) & ctx.read(r.inp[1]) };
                unsafe { ctx.set_output_raw(r.oid, r.out, v) };
            }
        }
        Kind::Or2 => {
            let rs = &recs.r2[1];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let v = unsafe { ctx.read(r.inp[0]) | ctx.read(r.inp[1]) };
                unsafe { ctx.set_output_raw(r.oid, r.out, v) };
            }
        }
        Kind::Xor2 => {
            let rs = &recs.r2[2];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let v = unsafe { ctx.read(r.inp[0]) ^ ctx.read(r.inp[1]) };
                unsafe { ctx.set_output_raw(r.oid, r.out, v) };
            }
        }
        Kind::HalfAdder => {
            let rs = &recs.r3[0];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let (a, b) = unsafe { (ctx.read(r.inp[0]), ctx.read(r.inp[1])) };
                unsafe { ctx.set_output_raw(r.oid, r.out[0], a ^ b) };
                unsafe { ctx.set_output_raw(r.oid + 1, r.out[1], a & b) };
            }
        }
        Kind::FullAdder => {
            let rs = &recs.r3[1];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let (a, b, cin) =
                    unsafe { (ctx.read(r.inp[0]), ctx.read(r.inp[1]), ctx.read(r.inp[2])) };
                unsafe { ctx.set_output_raw(r.oid, r.out[0], a ^ b ^ cin) };
                unsafe { ctx.set_output_raw(r.oid + 1, r.out[1], (a & b) | ((a ^ b) & cin)) };
            }
        }
        Kind::DFf => {
            // Rising-edge latch, as the JK and SR flip-flops: act on `clk && !prev`, then re-latch
            // `prev = clk` unconditionally so duplicate computes converge.
            let rs = &recs.r3[2];
            for &i in dirty {
                let r = unsafe { rec(rs, i) };
                let c = r.inp[2];
                let clk = unsafe { ctx.read(r.inp[1]) };
                if clk && !ctx.edge_prev(c) {
                    let d = unsafe { ctx.read(r.inp[0]) };
                    unsafe { ctx.set_output_raw(r.oid, r.out[0], d) };
                    unsafe { ctx.set_output_raw(r.oid + 1, r.out[1], !d) };
                }
                ctx.set_edge_prev(c, clk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;
    use crate::CompType;
    use crate::components::arity;

    /// The types whose type kernel has no compute (it would hit `unreachable!`) must lower to a flat
    /// kind at every input count their arity allows, or a component of theirs would queue on the
    /// generic path.
    #[test]
    fn types_without_generic_compute_always_lower() {
        let flat_only = [
            CompType::Not,
            CompType::Delay,
            CompType::HalfAdder,
            CompType::FullAdder,
            CompType::DFf,
        ];
        for ty in flat_only {
            let a = arity(ty);
            assert!(a.in_min == a.in_max, "{ty:?} has a variable input count");
            assert!(Kind::of(ty, a.in_min).is_some(), "{ty:?} does not lower");
        }
    }
}
