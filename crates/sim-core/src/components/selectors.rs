//! Selector kernels: decoder (18), encoder (19), multiplexer (20), demultiplexer (21).
//!
//! ENC and MUX are purely combinational. DEC and DEMUX carry a one-element `sel`-latch (the
//! currently-driven output index) so duplicate/reordered computes converge (§5.3a): the C++
//! `out[prev]=false; out[index]=...; prev=index` write-then-clear is *not* reorder-safe, so we
//! load `sel` **once**, guard the clear-of-old on `index != sel`, and never clear an output we are
//! about to set. Inputs come from the frozen `link_state`, so every compute of a component in a
//! tick derives the same index — making the guarded form bit-identical to C++'s settled state
//! while staying idempotent under double-compute.

use super::{Kernel, TickCtx};

/// Decoder (type 18): one-hot. `index = Σ inputs[i] << i`; drive `out[index]` high, all others low.
/// Outputs number `2^inputCount`. Init drives `out[0]` (`dec.h::init`); `sel` starts at 0.
pub(crate) struct Decoder;

impl Kernel for Decoder {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        ctx.set_output(ctx.first_output(c), true);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let mut index = 0u32;
            for (i, &l) in ins.iter().enumerate() {
                index |= (ctx.input(l) as u32) << i;
            }
            let prev = ctx.sel(c); // load ONCE (a re-load could observe another compute's store)
            if index != prev {
                ctx.set_output(ctx.output_at(c, prev), false);
                ctx.set_output(ctx.output_at(c, index), true);
                ctx.set_sel(c, index);
            }
        }
    }
}

/// Encoder (type 19): `value =` highest powered input index scanning `inputCount-1 .. 1` (0 if none
/// above input 0), output as binary across `outputCount` pins. Inputs number `2^outputCount`.
pub(crate) struct Encoder;

impl Kernel for Encoder {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let out_count = ctx.output_count(c);
            let mut value = 0u32;
            for i in (1..ins.len()).rev() {
                if ctx.input(ins[i]) {
                    value = i as u32;
                    break;
                }
            }
            for i in 0..out_count {
                ctx.set_output(ctx.output_at(c, i), (value >> i) & 1 != 0);
            }
        }
    }
}

/// Multiplexer (type 20): `index = Σ inputs[i] << i` over the first `selectBits` inputs (config.a);
/// output the data input `inputs[selectBits + index]`. Inputs number `2^selectBits + selectBits`.
pub(crate) struct Mux;

impl Kernel for Mux {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let sel_bits = ctx.config(c).a as usize;
            let mut index = 0usize;
            for (i, &l) in ins[..sel_bits].iter().enumerate() {
                index |= (ctx.input(l) as usize) << i;
            }
            let v = ctx.input(ins[sel_bits + index]);
            ctx.set_output(ctx.first_output(c), v);
        }
    }
}

/// Demultiplexer (type 21): `index = Σ inputs[i] << (i-1)` over the select inputs `inputs[1..]`;
/// route the data input `inputs[0]` to `out[index]`, all others low. Outputs number
/// `2^(inputCount-1)`. Same `sel`-latch discipline as DEC; the data set is always applied (it may
/// have flipped while the index held), the old-output clear only on an index change.
pub(crate) struct Demux;

impl Kernel for Demux {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let mut index = 0u32;
            for (i, &l) in ins.iter().enumerate().skip(1) {
                index |= (ctx.input(l) as u32) << (i - 1);
            }
            let data = ctx.input(ins[0]);
            let prev = ctx.sel(c); // load ONCE
            if index != prev {
                ctx.set_output(ctx.output_at(c, prev), false);
                ctx.set_sel(c, index);
            }
            ctx.set_output(ctx.output_at(c, index), data);
        }
    }
}
