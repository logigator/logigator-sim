//! Recorder kernel (type 201): captures a data bus on a trigger edge.
//!
//! Inputs are `[data(1..=32), trigger]`; no outputs. On a **rising trigger** (last input) it appends
//! `Σ dataᵢ << i` to its ring, read from the frozen `link_state` like every input, so it samples
//! what a D flip-flop clocked by the same signal latches. The edge is latched in the shared
//! `edge_prev` bit, so a duplicate compute in one tick records once. The ring and its count live in
//! the scratch arenas at the recorder's index `config(c).a`.

use super::{Kernel, TickCtx};

/// Recorder (type 201): append the data bus to a ring on each rising trigger.
pub(crate) struct Recorder;

impl Kernel for Recorder {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let in_count = ctx.inputs(c).len() as u32;
            let trigger = ctx.input_at(c, in_count - 1);
            if trigger && !ctx.edge_prev(c) {
                let mut value = 0u32;
                for i in 0..in_count - 1 {
                    value |= (ctx.input_at(c, i) as u32) << i;
                }
                ctx.record(c, value);
            }
            ctx.set_edge_prev(c, trigger);
        }
    }
}
