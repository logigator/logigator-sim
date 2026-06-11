//! Combinational gate kernels: NOT, AND, OR, XOR, DELAY (plan phase 1).
//!
//! All are pure functions of their inputs and therefore idempotent under reorder/re-execution
//! within a tick (invariant I3) — they carry no per-tick state. Each mirrors the corresponding old
//! engine kernel (`src/components/{not,and,or,xor,delay}.h`).

use super::{Kernel, TickCtx};
use crate::simd;

/// NOT (type 1): `out = !in`. Power-on output is high (`not.h:init` sets `out=true`).
pub(crate) struct Not;

impl Kernel for Not {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        let o = ctx.first_output(c);
        ctx.set_output(o, true);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let v = !ctx.input(ctx.inputs(c)[0]);
            let o = ctx.first_output(c);
            ctx.set_output(o, v);
        }
    }
}

/// AND (type 2): `out = in[0] && in[1] && …` (wired-AND of all inputs).
pub(crate) struct And;

impl Kernel for And {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let v = simd::and_inputs(ctx.inputs(c), ctx.link_state());
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
            let v = simd::or_inputs(ctx.inputs(c), ctx.link_state());
            let o = ctx.first_output(c);
            ctx.set_output(o, v);
        }
    }
}

/// XOR (type 4): `out = (popcount(inputs) is odd)` (matches `xor.h`'s `sum % 2`).
pub(crate) struct Xor;

impl Kernel for Xor {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let odd = simd::xor_inputs(ctx.inputs(c), ctx.link_state());
            let o = ctx.first_output(c);
            ctx.set_output(o, odd);
        }
    }
}

/// DELAY (type 5): `out = in`. A one-tick buffer (state lives in the tick scheduling, not the
/// kernel) — `delay.h` simply copies the input through.
pub(crate) struct Delay;

impl Kernel for Delay {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let v = ctx.input(ctx.inputs(c)[0]);
            let o = ctx.first_output(c);
            ctx.set_output(o, v);
        }
    }
}
