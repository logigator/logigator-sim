//! Selector kernels: decoder (18), encoder (19), multiplexer (20), demultiplexer (21).
//!
//! ENC and MUX are purely combinational. DEC and DEMUX carry a one-element `sel`-latch (the
//! currently-driven output index) so duplicate/reordered computes converge: a plain
//! `out[prev]=false; out[index]=...; prev=index` write-then-clear is *not* reorder-safe, so we
//! load `sel` **once**, guard the clear-of-old on `index != sel`, and never clear an output we are
//! about to set. Inputs come from the frozen `link_state`, so every compute of a component in a
//! tick derives the same index — the settled state is the same as a single compute's, and the
//! kernel stays idempotent under double-compute.

use super::{Kernel, TickCtx};

/// Decoder (type 18): one-hot. `index = Σ inputs[i] << i`; drive `out[index]` high, all others low.
/// Outputs number `2^inputCount`. Init drives `out[0]`; `sel` starts at 0.
pub(crate) struct Decoder;

impl Kernel for Decoder {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        ctx.set_output(ctx.first_output(c), true);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let in_count = ctx.inputs(c).len();
            let mut index = 0u32;
            for i in 0..in_count {
                index |= (ctx.input_at(c, i as u32) as u32) << i;
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
            let in_count = ctx.inputs(c).len();
            let out_count = ctx.output_count(c);
            let mut value = 0u32;
            for i in (1..in_count).rev() {
                if ctx.input_at(c, i as u32) {
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

/// Multiplexer (type 20): `index = Σ inputs[i] << i` over the first `selectBits` inputs (config.a,
/// derived from the input count); output the data input `inputs[selectBits + index]`. Inputs
/// number `selectBits + 2^selectBits`.
pub(crate) struct Mux;

impl Kernel for Mux {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let sel_bits = ctx.config(c).a as usize;
            let mut index = 0usize;
            for i in 0..sel_bits {
                index |= (ctx.input_at(c, i as u32) as usize) << i;
            }
            let v = ctx.input_at(c, (sel_bits + index) as u32);
            ctx.set_output(ctx.first_output(c), v);
        }
    }
}

/// Demultiplexer (type 21): `index = Σ inputs[i] << i` over the select inputs `inputs[..s]`, where
/// `s = inputCount - 1`; route the data input `inputs[s]` to `out[index]`, all others low. Select
/// bits come first, as on the MUX. Outputs number `2^s`. Same `sel`-latch discipline as DEC; the data set is always applied (it may
/// have flipped while the index held), the old-output clear only on an index change.
pub(crate) struct Demux;

impl Kernel for Demux {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let sel_bits = ctx.inputs(c).len() as u32 - 1;
            let mut index = 0u32;
            for i in 0..sel_bits {
                index |= (ctx.input_at(c, i) as u32) << i;
            }
            let data = ctx.input_at(c, sel_bits);
            let prev = ctx.sel(c); // load ONCE
            if index != prev {
                ctx.set_output(ctx.output_at(c, prev), false);
                ctx.set_sel(c, index);
            }
            ctx.set_output(ctx.output_at(c, index), data);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, Simulation};

    fn mux(inputs: u32) -> crate::Result<Simulation> {
        let mut b = BoardBuilder::new(inputs + 1);
        b.component(
            CompType::Mux,
            &(0..inputs).collect::<Vec<_>>(),
            &[inputs],
            &[],
        );
        Simulation::from_descriptor(&b.finish())
    }

    /// The input count `s + 2^s` fixes the select width, so only those counts compile.
    #[test]
    fn mux_select_width_comes_from_the_input_count() {
        let ok: Vec<u32> = (3..40).filter(|&n| mux(n).is_ok()).collect();
        assert_eq!(ok, [3, 6, 11, 20, 37]);
    }
}
