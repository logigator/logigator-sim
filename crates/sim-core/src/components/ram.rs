//! RAM kernel (type 17): addressable read/write memory with a clocked write.
//!
//! Inputs are laid out `[address(addressSize), data(wordSize), write-enable, clock]`; outputs are
//! the `wordSize` data pins. On a **rising clock** (last input) with write-enable high, the data
//! inputs are stored at the word `position = address * wordSize`. The outputs show the addressed
//! word: in the default mode they follow the address (and the memory) like any combinational part,
//! so a write shows on them too. In the legacy clocked-read mode (`read: "clocked"`) they
//! change only on a rising clock, which either writes (and echoes the written word) or reads.
//!
//! The clock edge is latched in the shared `edge_prev` bit, so a write happens once per edge and the
//! kernel is idempotent under double-compute. `addressSize = inputs - wordSize - 2`; the region
//! starts at byte `config(c).a`. A write that changes a stored bit advances the RAM's state version
//! (slot `config(c).b`).

use super::{Kernel, TickCtx};

/// RAM (type 17): memory with a clocked write and an asynchronous (or, legacy, clocked) read.
pub(crate) struct Ram;

impl Kernel for Ram {
    /// Power-on replays `compute`: the clock starts low, so it only shows the (zero) word at
    /// address 0, which is a no-op.
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        Self::compute_batch(&[c], ctx);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let in_count = ctx.inputs(c).len();
            let word_size = ctx.output_count(c) as usize;
            let addr_size = in_count - word_size - 2;
            let clk = ctx.input_at(c, (in_count - 1) as u32);
            let edge = clk && !ctx.edge_prev(c);
            ctx.set_edge_prev(c, clk);
            if !edge && ctx.ram_clocked_read(c) {
                continue;
            }
            let base = ctx.config(c).a as usize;
            let mut position = 0usize;
            for i in 0..addr_size {
                position |= (ctx.input_at(c, i as u32) as usize) << i;
            }
            position *= word_size;

            let write = edge && ctx.input_at(c, (in_count - 2) as u32);
            let mut changed = false;
            for i in 0..word_size {
                let v = if write {
                    let d = ctx.input_at(c, (addr_size + i) as u32);
                    changed |= ctx.ram_set(base, position + i, d);
                    d
                } else {
                    ctx.ram_get(base, position + i)
                };
                ctx.set_output(ctx.output_at(c, i as u32), v);
            }
            if changed {
                ctx.bump_ram_version(c);
            }
        }
    }
}
