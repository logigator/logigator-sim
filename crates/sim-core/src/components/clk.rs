//! CLK kernel (type 6): a free-running clock.
//!
//! A CLK has two halves. The **period toggle** — count up to `speed` (= `config.a`) then flip the
//! output, the inverse the next tick — is driven by the simulation's between-tick section over the
//! subscribed clocks (see [`crate::tick`]).
//!
//! This kernel is the other half: the **enable-input gating**. It runs in
//! the compute phase when the single enable input flips. A *high* enable freezes the clock
//! (unsubscribe and force the output low); a *low* enable runs it (subscribe). Every CLK starts
//! subscribed (seeded by the simulation). Idempotent under double-compute: it reads the frozen
//! enable input and converges on the subscribe bit / a single output flip.

use super::{Kernel, TickCtx};

/// CLK (type 6): subscribe/unsubscribe the period toggle based on the enable input.
pub(crate) struct Clk;

impl Kernel for Clk {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let enable = ctx.input_at(c, 0);
            if enable {
                // Enable high → freeze: unsubscribe and force the output low.
                if ctx.clk_subscribed(c) {
                    ctx.set_clk_subscribed(c, false);
                    let o0 = ctx.first_output(c);
                    if ctx.output(o0) {
                        ctx.set_output(o0, false);
                    }
                }
            } else if !ctx.clk_subscribed(c) {
                // Enable low → run: re-subscribe to the period toggle.
                ctx.set_clk_subscribed(c, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, Simulation};

    /// The CLK's output over `ticks` ticks, one char per tick.
    fn wave(sim: &mut Simulation, ticks: usize) -> String {
        (0..ticks)
            .map(|_| {
                sim.tick();
                if sim.output(0, 0) { '1' } else { '0' }
            })
            .collect()
    }

    /// A clock halted part-way through its low period restarts from the beginning of the cycle:
    /// the wave after the restart is the same wherever the halt landed.
    #[test]
    fn restart_after_halt_begins_a_full_cycle() {
        let after_restart = |halt_at: usize| {
            let mut b = BoardBuilder::new(2);
            b.component(CompType::Clk, &[0], &[1], &[4]);
            b.component(CompType::UserInput, &[], &[0], &[]);
            let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
            wave(&mut sim, halt_at);
            sim.set_input(1, &[true]).unwrap();
            wave(&mut sim, 3);
            sim.set_input(1, &[false]).unwrap();
            wave(&mut sim, 12)
        };
        let reference = after_restart(1);
        assert!(reference.contains('1'), "the clock runs again: {reference}");
        for halt_at in 2..6 {
            assert_eq!(
                after_restart(halt_at),
                reference,
                "halted after {halt_at} ticks"
            );
        }
    }

    /// A period past `i32::MAX` is a long period, not a negative one that fires every tick.
    #[test]
    fn huge_period_stays_low() {
        let mut b = BoardBuilder::new(2);
        b.component(CompType::Clk, &[0], &[1], &[(1 << 31) + 1]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        assert_eq!(wave(&mut sim, 8), "00000000");
    }
}
