//! Flip-flop kernels: D (13), JK (14), SR (15). All rising-edge-triggered on input pin 1
//! (the clock / enable), with output pin 0 = Q and pin 1 = Q̄, seeded Q̄=high at init
//! (`{d,jk,sr}_ff.h::init`).
//!
//! Each shares the `edge_prev` latch: act only on `clk && !prev`, then re-latch
//! `prev = clk` **unconditionally** — so a falling edge resets it and a duplicate/reordered compute
//! in the same tick (first one sets `prev`, the rest see `clk && !prev == false`) is a no-op. This
//! is what makes the self-referential JK toggle idempotent under double-compute.
//!
//! D and JK match the old engine bit-for-bit. **SR deliberately diverges**: the old `sr_ff.h` is
//! *level*-sensitive (acts whenever enable is held high, no `prevClk`); making it rising-edge here
//! is the intentional consistency choice. SR boards are verified against the C++ oracle only where
//! the two agree — i.e. enable held high spans over which S/R do not change (see corpus `sr_ff`).

use super::{Kernel, TickCtx};

/// D flip-flop (type 13): on a rising clock (input 1), latch `Q = D` (input 0), `Q̄ = !D`.
pub(crate) struct DFf;

impl Kernel for DFf {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        ctx.set_output(ctx.output_at(c, 1), true);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let clk = ctx.input(ins[1]);
            if clk && !ctx.edge_prev(c) {
                let d = ctx.input(ins[0]);
                ctx.set_output(ctx.output_at(c, 0), d);
                ctx.set_output(ctx.output_at(c, 1), !d);
            }
            ctx.set_edge_prev(c, clk);
        }
    }
}

/// JK flip-flop (type 14): on a rising clock (input 1), with J=input 0, K=input 2: J&K toggles both
/// outputs, J alone sets (Q=1), K alone resets (Q=0), neither holds. Reads its own outputs to
/// toggle — idempotent under double-compute thanks to the `edge_prev` guard.
pub(crate) struct JkFf;

impl Kernel for JkFf {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        ctx.set_output(ctx.output_at(c, 1), true);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let clk = ctx.input(ins[1]);
            if clk && !ctx.edge_prev(c) {
                let (j, k) = (ctx.input(ins[0]), ctx.input(ins[2]));
                let (o0, o1) = (ctx.output_at(c, 0), ctx.output_at(c, 1));
                if j && k {
                    ctx.set_output(o0, !ctx.output(o0));
                    ctx.set_output(o1, !ctx.output(o1));
                } else if j {
                    ctx.set_output(o0, true);
                    ctx.set_output(o1, false);
                } else if k {
                    ctx.set_output(o0, false);
                    ctx.set_output(o1, true);
                }
            }
            ctx.set_edge_prev(c, clk);
        }
    }
}

/// SR flip-flop (type 15): on a rising **enable** (input 1), with S=input 0, R=input 2: S sets
/// (Q=1), else R resets (Q=0). **Rising-edge — a deliberate divergence** from the old level-sensitive latch.
pub(crate) struct SrFf;

impl Kernel for SrFf {
    #[inline]
    fn init(c: u32, ctx: &mut TickCtx<'_>) {
        ctx.set_output(ctx.output_at(c, 1), true);
    }

    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let ins = ctx.inputs(c);
            let enable = ctx.input(ins[1]);
            if enable && !ctx.edge_prev(c) {
                if ctx.input(ins[0]) {
                    ctx.set_output(ctx.output_at(c, 0), true);
                    ctx.set_output(ctx.output_at(c, 1), false);
                } else if ctx.input(ins[2]) {
                    ctx.set_output(ctx.output_at(c, 0), false);
                    ctx.set_output(ctx.output_at(c, 1), true);
                }
            }
            ctx.set_edge_prev(c, enable);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, InputEvent, Simulation};

    /// Documents the deliberate SR divergence: the new SR is **rising-edge** triggered, so changing S/R
    /// while the enable is *held* high does not re-latch — whereas the old level-sensitive
    /// `sr_ff.h` would. (The `sr_ff` corpus board stays in the agreement region; this is the part
    /// that intentionally differs and is therefore checked here, not against the C++ oracle.)
    #[test]
    fn sr_ff_is_rising_edge_not_level_sensitive() {
        let mut b = BoardBuilder::new(5);
        b.component(CompType::UserInput, &[], &[0, 1, 2], &[]); // S, enable, R
        b.component(CompType::SrFf, &[0, 1, 2], &[3, 4], &[]); // Q=link3, Qbar=link4
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        let settle = |s: &mut Simulation| (0..5).for_each(|_| s.tick());

        // Rising edge of enable with S=1 → set Q.
        sim.trigger_input(0, InputEvent::Cont, &[true, true, false])
            .unwrap();
        settle(&mut sim);
        assert!(sim.output(1, 0), "Q set on the enable rising edge");

        // Enable held HIGH, now drive S=0, R=1. A level latch resets here; the edge latch holds.
        sim.trigger_input(0, InputEvent::Cont, &[false, true, true])
            .unwrap();
        settle(&mut sim);
        assert!(
            sim.output(1, 0),
            "rising-edge SR holds Q while enable stays high (no re-trigger)"
        );
        assert!(!sim.output(1, 1), "Qbar stays low");
    }
}
