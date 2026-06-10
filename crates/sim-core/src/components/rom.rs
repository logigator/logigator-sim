//! ROM kernel (type 12): a combinational lookup table (`src/components/rom.h`).
//!
//! The input bits form an address `position = Σ inputs[i] << i`; the output word at that address is
//! `outputCount` consecutive bits of the immutable data blob starting at bit `position*outputCount`.
//! Purely combinational (no per-tick state) → idempotent under reorder/re-execution (invariant I3).
//! The data blob lives in `Board::rom_data`; this ROM's slice starts at byte `config(c).a`.

use super::{Kernel, TickCtx};

/// ROM (type 12): address-in → data-word-out lookup over a compiled byte blob.
pub(crate) struct Rom;

impl Kernel for Rom {
    /// Power-on state replays `compute` with all inputs low (`rom.h::init`), seeding the word at
    /// address 0.
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_, false>) {
        Self::compute_batch(&[c], ctx);
    }

    #[inline]
    fn compute_batch<const PAR: bool>(dirty: &[u32], ctx: &mut TickCtx<'_, PAR>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let out_count = ctx.output_count(c) as usize;
            let off = ctx.config(c).a as usize;
            let data = ctx.rom_data();

            let mut position = 0usize;
            for (i, &l) in ins.iter().enumerate() {
                position |= (ctx.input(l) as usize) << i;
            }
            position *= out_count;

            for i in 0..out_count {
                let bitpos = position + i;
                let bit = (data[off + bitpos / 8] >> (bitpos % 8)) & 1 != 0;
                ctx.set_output(ctx.output_at(c, i as u32), bit);
            }
        }
    }
}
