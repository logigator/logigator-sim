//! RAM kernel (type 17): edge-clocked addressable read/write memory (`src/components/ram.h`).
//!
//! Inputs are laid out `[address(addressSize), data(wordSize), write-enable, clock]`; outputs are
//! the `wordSize` data pins. On a **rising clock** (last input), the word at
//! `position = address * wordSize` is either written from the data inputs (when write-enable is
//! high, also echoed to the outputs) or read out to the outputs. The clock edge is latched in the
//! shared `edge_prev` bit, so the write/read happens once per edge and is idempotent under
//! double-compute (§5.3a); the backing store is atomic-typed so racing same-value writes are sound.
//! `addressSize = inputs - wordSize - 2`; the region starts at byte `config(c).a`.

use super::{Kernel, TickCtx};

/// RAM (type 17): synchronous read/write memory.
pub(crate) struct Ram;

impl Kernel for Ram {
    /// Power-on replays `compute` (`ram.h::init`); the clock starts low so it is a no-op.
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        Self::compute_batch(&[c], ctx);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let in_count = ins.len();
            let word_size = ctx.output_count(c) as usize;
            let addr_size = in_count - word_size - 2;
            let clk = ctx.input(ins[in_count - 1]);
            if clk && !ctx.edge_prev(c) {
                let base = ctx.config(c).a as usize;
                let mut position = 0usize;
                for (i, &l) in ins[..addr_size].iter().enumerate() {
                    position |= (ctx.input(l) as usize) << i;
                }
                position *= word_size;

                let write = ctx.input(ins[in_count - 2]);
                for i in 0..word_size {
                    let v = if write {
                        let d = ctx.input(ins[addr_size + i]);
                        ctx.ram_set(base, position + i, d);
                        d
                    } else {
                        ctx.ram_get(base, position + i)
                    };
                    ctx.set_output(ctx.output_at(c, i as u32), v);
                }
            }
            ctx.set_edge_prev(c, clk);
        }
    }
}
