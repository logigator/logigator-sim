//! Combinational gate kernels: NOT, AND, OR, XOR, DELAY.
//!
//! All are pure functions of their inputs and therefore idempotent under reorder/re-execution
//! within a tick — they carry no per-tick state. NOT and DELAY, and two-input AND/OR/XOR, compute
//! from their flat records ([`super::flat`]); here are NOT's power-on init and the wide-gate
//! AND/OR/XOR reductions.

use super::{Kernel, TickCtx};
use crate::reduce;

/// NOT (type 1): `out = !in`. Power-on output is high.
pub(crate) struct Not;

impl Kernel for Not {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        let o = ctx.first_output(c);
        ctx.set_output(o, true);
    }
}

/// AND (type 2): `out = in[0] && in[1] && …` (wired-AND of all effective inputs). This kernel runs
/// the gates with more than two inputs.
pub(crate) struct And;

impl Kernel for And {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            // SAFETY: a component's input slice holds in-range link ids (a board invariant).
            let v = unsafe { reduce::and_inputs(ctx.inputs(c), ctx.link_state()) };
            let o = ctx.first_output(c);
            ctx.set_output(o, v);
        }
    }
}

/// OR (type 3): `out = in[0] || in[1] || …`.
pub(crate) struct Or;

impl Kernel for Or {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            // SAFETY: as for AND.
            let v = unsafe { reduce::or_inputs(ctx.inputs(c), ctx.link_state()) };
            let o = ctx.first_output(c);
            ctx.set_output(o, v);
        }
    }
}

/// XOR (type 4): `out = (popcount(effective inputs) is odd)`.
pub(crate) struct Xor;

impl Kernel for Xor {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            // SAFETY: as for AND.
            let odd = unsafe { reduce::xor_inputs(ctx.inputs(c), ctx.link_state()) };
            let o = ctx.first_output(c);
            ctx.set_output(o, odd);
        }
    }
}

/// DELAY (type 5): `out = in`. A one-tick buffer (state lives in the tick scheduling, not the
/// kernel); its flat kernel copies the input through.
pub(crate) struct Delay;

impl Kernel for Delay {}
