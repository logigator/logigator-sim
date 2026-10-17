//! CLK kernel (type 6): a free-running clock.
//!
//! A CLK has two halves. The **period toggle** — stay low for `config.a` ticks, then high for
//! `config.b` (1 for a pulse, `config.a` for a square wave), and repeat — is driven by the
//! simulation's between-tick section over the subscribed clocks (see [`crate::tick`]).
//!
//! This kernel is the other half: the **halt-input gating**. It runs in
//! the compute phase when the single halt input flips. A *high* halt freezes the clock
//! (unsubscribe and force the output low); a *low* halt runs it (subscribe). Every CLK starts
//! subscribed (seeded by the simulation). Idempotent under double-compute: it reads the frozen
//! halt input and converges on the subscribe bit / a single output flip.

use super::{Kernel, TickCtx};

/// CLK (type 6): subscribe/unsubscribe the period toggle based on the halt input.
pub(crate) struct Clk;

impl Kernel for Clk {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            let halt = ctx.input_at(c, 0);
            if halt {
                // Halt high → freeze: unsubscribe and force the output low.
                if ctx.clk_subscribed(c) {
                    ctx.set_clk_subscribed(c, false);
                    let o0 = ctx.first_output(c);
                    if ctx.output(o0) {
                        ctx.set_output(o0, false);
                    }
                }
            } else if !ctx.clk_subscribed(c) {
                // Halt low → run: re-subscribe to the period toggle.
                ctx.set_clk_subscribed(c, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, ClkMode, CompType, ComponentDescriptor, Simulation};

    /// A CLK on halt link 0 and output link 1.
    fn clk(period: u32, mode: Option<ClkMode>) -> ComponentDescriptor {
        ComponentDescriptor {
            period: Some(period),
            mode,
            ..ComponentDescriptor::new(CompType::Clk, &[0], &[1])
        }
    }

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
            let mut b = BoardBuilder::new();
            b.add(clk(4, None));
            b.component(CompType::UserInput, &[], &[0]);
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

    fn clock(period: u32, mode: Option<ClkMode>) -> Simulation {
        let mut b = BoardBuilder::new();
        b.add(clk(period, mode));
        Simulation::from_descriptor(&b.finish()).unwrap()
    }

    /// Square mode (the default) spends `period` ticks at each level; pulse mode stays low
    /// `period` ticks then high for one. Period 1 is the same wave in both modes.
    #[test]
    fn pulse_and_square_waves() {
        assert_eq!(wave(&mut clock(3, None), 12), "001110001110");
        assert_eq!(
            wave(&mut clock(3, Some(ClkMode::Square)), 12),
            "001110001110"
        );
        assert_eq!(
            wave(&mut clock(3, Some(ClkMode::Pulse)), 12),
            "001000100010"
        );
        assert_eq!(
            wave(&mut clock(1, Some(ClkMode::Pulse)), 6),
            wave(&mut clock(1, None), 6)
        );
    }

    /// Halting a square wave while it is high drops it low; a restart begins with a full low half.
    #[test]
    fn square_wave_halt_and_restart() {
        let mut b = BoardBuilder::new();
        b.add(clk(3, Some(ClkMode::Square)));
        b.component(CompType::UserInput, &[], &[0]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        assert_eq!(wave(&mut sim, 4), "0011");
        sim.set_input(1, &[true]).unwrap();
        assert_eq!(wave(&mut sim, 4), "0000", "the halt drops it low at once");
        sim.set_input(1, &[false]).unwrap();
        assert_eq!(wave(&mut sim, 9), "001110001", "restarts like a power-on");
    }

    #[test]
    fn rejects_period_0() {
        let mut b = BoardBuilder::new();
        b.add(clk(0, None));
        assert!(Simulation::from_descriptor(&b.finish()).is_err());
    }

    /// A period past `i32::MAX` is a long period, not a negative one that fires every tick.
    #[test]
    fn huge_period_stays_low() {
        let mut b = BoardBuilder::new();
        b.add(clk((1 << 31) + 1, None));
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        assert_eq!(wave(&mut sim, 8), "00000000");
    }
}
