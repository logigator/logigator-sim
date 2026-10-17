//! RNG kernel (type 16): a per-component seeded random source.
//!
//! Each component draws from a seeded mixer whose output is a **pure function of
//! `(seed, tick)`** rather than a shared stream. That makes it reproducible and order-independent,
//! and — since re-execution within a tick recomputes the identical bits — idempotent under
//! double-compute with no scratch beyond the seed (no `last_tick` latch is needed; see
//! `Scratch::rng_seed`).
//!
//! It draws on each rising edge of its **trigger**, `inputs[0]`. It needs no edge latch: being a
//! plain compute-phase kernel (not between-tick), it is enqueued only when its single input flips,
//! so it draws when it finds the trigger high and a falling edge leaves the outputs held.

use super::{Kernel, TickCtx};
use crate::scratch::splitmix64;

/// RNG (type 16): fill the outputs with bits drawn from `splitmix64(seed, tick)` on a rising trigger.
pub(crate) struct Rng;

impl Kernel for Rng {
    #[inline]
    fn compute_batch(dirty: &[u32], ctx: &mut TickCtx<'_>) {
        for &c in dirty {
            if !ctx.input_at(c, 0) {
                continue; // trigger fell → hold previous outputs (no draw, no clear)
            }
            let out_count = ctx.output_count(c);
            let base = ctx.rng_seed(c) ^ splitmix64(ctx.tick());
            let mut word = 0u64;
            for i in 0..out_count {
                if i % 64 == 0 {
                    // A fresh 64-bit word per group of outputs (boards rarely exceed 64).
                    word = splitmix64(base ^ (i / 64) as u64);
                }
                let bit = (word >> (i % 64)) & 1 != 0;
                ctx.set_output(ctx.output_at(c, i), bit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, Simulation};

    /// Two 8-output RNGs fed by one UserInput trigger. Returns a fresh simulation each call so
    /// runs are independent.
    fn build() -> Simulation {
        let mut b = BoardBuilder::new(17);
        b.component(CompType::UserInput, &[], &[0], &[]); // comp0: trigger -> link0
        b.component(CompType::Rng, &[0], &[1, 2, 3, 4, 5, 6, 7, 8], &[]); // comp1
        b.component(CompType::Rng, &[0], &[9, 10, 11, 12, 13, 14, 15, 16], &[]); // comp2
        Simulation::from_descriptor(&b.finish()).unwrap()
    }

    /// Pack an RNG component's 8 output pins into a byte.
    fn read(s: &Simulation, comp: u32) -> u8 {
        (0..8).fold(0u8, |acc, pin| acc | ((s.output(comp, pin) as u8) << pin))
    }

    fn settle(s: &mut Simulation) {
        for _ in 0..4 {
            s.tick();
        }
    }

    /// The exact drawn bytes, pinned. The RNG stream is a pure function of the component's
    /// *public* (submission-order) id and the tick, so these constants are part of the
    /// reproducibility contract: any internal re-layout of components must keep the
    /// seed keyed on the public id, or this stream silently changes.
    #[test]
    fn rng_bytes_are_pinned() {
        // Each trigger change is submitted after one idle tick, so it reaches the RNG on tick 2, 6
        // and 10: the draw ticks these bytes were pinned on.
        let trigger = |s: &mut Simulation, v: bool| {
            s.tick();
            s.set_input(0, &[v]).unwrap();
            (0..3).for_each(|_| s.tick());
        };
        let mut a = build();
        trigger(&mut a, true);
        assert_eq!((read(&a, 1), read(&a, 2)), (0x80, 0x03), "first draw");

        // Drop and re-raise the trigger: the second draw lands on a known later tick.
        trigger(&mut a, false);
        trigger(&mut a, true);
        assert_eq!((read(&a, 1), read(&a, 2)), (0x9F, 0x73), "second draw");
    }

    /// The RNG contract: trigger-driven, reproducible, per-component distinct, tick-varying, and
    /// holding while not freshly clocked.
    #[test]
    fn rng_gated_reproducible_and_per_component() {
        // Trigger never raised → never drawn → all outputs stay low.
        let mut idle = build();
        settle(&mut idle);
        assert_eq!(read(&idle, 1), 0, "no draw while the trigger stays low");

        // Rising edge draws; identical across two independent runs (reproducible).
        let (mut a, mut b) = (build(), build());
        a.set_input(0, &[true]).unwrap();
        b.set_input(0, &[true]).unwrap();
        settle(&mut a);
        settle(&mut b);
        let (a1, a2) = (read(&a, 1), read(&a, 2));
        assert_eq!(
            (a1, a2),
            (read(&b, 1), read(&b, 2)),
            "reproducible run-to-run"
        );
        assert_ne!(a1, 0, "a triggered RNG actually drew");
        assert_ne!(a1, a2, "distinct seeds → distinct outputs per instance");

        // Holds while the trigger stays high (RNG re-runs only on a trigger edge).
        settle(&mut a);
        assert_eq!(
            read(&a, 1),
            a1,
            "output holds while the trigger is held high"
        );

        // Falling edge holds the value (no clear); a later rising edge draws a new value.
        a.set_input(0, &[false]).unwrap();
        settle(&mut a);
        assert_eq!(read(&a, 1), a1, "output holds after the trigger drops");
        a.set_input(0, &[true]).unwrap();
        settle(&mut a);
        assert_ne!(
            read(&a, 1),
            a1,
            "a later-tick rising edge draws a different value"
        );
    }
}
