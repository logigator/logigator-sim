//! Binary adder kernels: half adder (10) and full adder (11).
//!
//! Both are pure combinational functions of their inputs — no per-tick state, so trivially
//! idempotent under reorder/re-execution within a tick. Each mirrors the old engine
//! kernel (`src/components/{half_addr,full_addr}.h`): output pin 0 is the sum bit, pin 1 the carry.

use super::{Kernel, TickCtx};

/// Half adder (type 10): `sum = a ^ b`, `carry = a & b`. Two inputs, two outputs.
pub(crate) struct HalfAdder;

impl Kernel for HalfAdder {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let a = ctx.input(ins[0]);
            let b = ctx.input(ins[1]);
            ctx.set_output(ctx.output_at(c, 0), a ^ b);
            ctx.set_output(ctx.output_at(c, 1), a & b);
        }
    }
}

/// Full adder (type 11): `sum = a ^ b ^ cin`, `carry = (a & b) | ((a ^ b) & cin)`. Three inputs
/// (a, b, cin), two outputs (sum, carry). The carry mirrors C++ operator precedence exactly:
/// `^` binds tighter than `&&`, so the second term groups as `(a ^ b) && cin` (`full_addr.h:16`).
pub(crate) struct FullAdder;

impl Kernel for FullAdder {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let a = ctx.input(ins[0]);
            let b = ctx.input(ins[1]);
            let cin = ctx.input(ins[2]);
            ctx.set_output(ctx.output_at(c, 0), a ^ b ^ cin);
            ctx.set_output(ctx.output_at(c, 1), (a & b) | ((a ^ b) & cin));
        }
    }
}
