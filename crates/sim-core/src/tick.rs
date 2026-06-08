//! The single-threaded tick (plan §6.2): read phase → compute phase → between-tick section → swap.
//!
//! Order mirrors the old engine exactly — read, compute, fire the between-tick section, *then*
//! swap the buffers (invariant I4). `link_state` changes only in the read phase (I1); `set_output`
//! during compute writes only `output_state`/`driver_count`/`write_buf` (D4). Duplicate link pushes
//! and double-computes within a tick are idempotent (I3), which is what authorizes the future
//! parallel/reordered evaluation.

use crate::components::{self, N_TYPES, TickCtx};
use crate::sim::{RunConfig, Simulation};
use crate::types::SimState;
use core::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

impl Simulation {
    /// One deterministic step. Does not consult the lifecycle state — callers (`run`, tests) drive
    /// it directly.
    pub fn tick(&mut self) {
        self.run_tick();
    }

    /// Run until the tick budget is spent, the timeout elapses, or `stop()` is requested (plan
    /// §7.2). Single-threaded; `cfg.threads`/`par_threshold` are ignored until phase 6.
    pub fn run(&mut self, cfg: RunConfig) -> crate::Result<()> {
        self.state = SimState::Running;
        let start = Instant::now();
        self.last_capture = start;
        self.last_capture_tick = self.tick;

        let mut remaining = cfg.ticks;
        while remaining > 0 {
            if self.state == SimState::Stopping {
                break;
            }
            self.run_tick();
            remaining -= 1;
            self.update_speed(start);
            // (avoid a let-chain here: those stabilized after the 1.85 MSRV floor)
            if cfg.timeout.is_some_and(|t| start.elapsed() >= t) {
                break;
            }
        }
        self.state = SimState::Stopped;
        Ok(())
    }

    /// The tick body shared by `tick()` and `run()`.
    fn run_tick(&mut self) {
        self.read_phase();
        self.compute_phase();
        self.between_tick();
        // Swap buffers, clear the new write buffer and the per-type queues, advance the tick.
        std::mem::swap(&mut self.read_buf, &mut self.write_buf);
        self.write_buf.clear();
        for q in &mut self.compute_queue {
            q.clear();
        }
        self.tick += 1;
    }

    /// READ PHASE: for each scheduled link, recompute its net value as `driver_count != 0`
    /// (== the old `any_of(drivers)`, I2); on a flip, update `link_state` (the only place it
    /// changes, I1) and enqueue every consuming component onto its per-type queue.
    fn read_phase(&mut self) {
        let mut i = 0;
        while i < self.read_buf.len() {
            let l = self.read_buf[i];
            i += 1;
            let v = self.driver_count[l as usize].load(Relaxed) != 0;
            if v == self.link_state.get(l) {
                continue;
            }
            self.link_state.set(l, v);
            for k in 0..self.board.link_consumers(l).len() {
                let c = self.board.link_consumers(l)[k];
                let qi = self.comp_ty_index[c as usize] as usize;
                self.compute_queue[qi].push(c);
            }
        }
    }

    /// COMPUTE PHASE: drain each non-empty per-type queue through its kernel. Kernels read the
    /// frozen `link_state` and write via `set_output`.
    fn compute_phase(&mut self) {
        for qi in 0..N_TYPES {
            if self.compute_queue[qi].is_empty() {
                continue;
            }
            let ty = components::type_from_index(qi);
            // ctx borrows write_buf (mut) + board/link_state/output_state/driver_count (shared);
            // the queue is a disjoint field, so the shared borrow below coexists.
            let mut ctx = TickCtx::new(
                &self.board,
                &self.link_state,
                &self.output_state,
                &self.driver_count,
                &mut self.write_buf,
            );
            components::dispatch_compute(ty, &self.compute_queue[qi], &mut ctx);
        }
    }

    /// BETWEEN-TICK SECTION (always single-threaded, fires *before* the swap, I4): drain armed
    /// `UserInput` pulses. A pending pulse asserts its outputs this tick then disarms; a disarmed
    /// entry clears its outputs and unsubscribes — matching the old `tickEvent` handler.
    fn between_tick(&mut self) {
        let mut k = 0;
        while k < self.ui_pending.len() {
            let comp = self.ui_pending[k].comp;
            let pending = self.ui_pending[k].pending;
            let oids = self.board.output_ids(comp);
            {
                // Inline ctx (not make_ctx) so `ui_pending[k].state` stays readable: ctx borrows
                // only board/link_state/output_state/driver_count/write_buf, all disjoint from it.
                let mut ctx = TickCtx::new(
                    &self.board,
                    &self.link_state,
                    &self.output_state,
                    &self.driver_count,
                    &mut self.write_buf,
                );
                for (pin, oid) in oids.enumerate() {
                    let v = pending && self.ui_pending[k].state.get(pin).copied().unwrap_or(false);
                    ctx.set_output(oid, v);
                }
            }
            if pending {
                self.ui_pending[k].pending = false;
                k += 1;
            } else {
                self.ui_pending.swap_remove(k); // unsubscribe; do not advance k
            }
        }
    }

    /// Update the ticks/sec readout over ~1s wall-clock windows (matches the old engine).
    fn update_speed(&mut self, _start: Instant) {
        let elapsed = self.last_capture.elapsed();
        if elapsed.as_secs() >= 1 {
            let dt = elapsed.as_nanos().max(1);
            self.speed =
                (((self.tick - self.last_capture_tick) as u128 * 1_000_000_000) / dt) as u32;
            self.last_capture = Instant::now();
            self.last_capture_tick = self.tick;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, InputEvent, Simulation};

    /// Init-seeding (invariant I5, the plan's most error-prone step): a NOT seeds its output high
    /// on init, so its output *pin* reads high immediately (post-init `output_state`) but the driven
    /// *link* only flips at the first read boundary — i.e. high after tick 1, not before.
    #[test]
    fn not_output_seeds_and_appears_after_tick_1() {
        let mut b = BoardBuilder::new(2);
        b.component(CompType::Not, &[0], &[1], &[]); // in: link0, out: link1
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

        // t0: pin seeded high, link still low (no read phase ran during the prime).
        assert!(
            sim.output(0, 0),
            "NOT output pin should be seeded high at init"
        );
        assert!(!sim.link(1), "NOT link must still be low before tick 1");

        sim.tick();
        assert!(sim.link(1), "NOT link must be high after tick 1");
    }

    /// A `Cont`-triggered UserInput link takes *two* ticks to appear: the trigger lands in
    /// write_buf, one swap moves it into read_buf, the next read phase flips the link. (If this
    /// showed at tick 1, the trigger/prime buffer routing would be wrong — advisor's check.)
    #[test]
    fn cont_input_link_appears_after_tick_2() {
        let mut b = BoardBuilder::new(1);
        b.component(CompType::UserInput, &[], &[0], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        sim.trigger_input(0, InputEvent::Cont, &[true]).unwrap();

        assert!(!sim.link(0), "t0: not yet visible");
        sim.tick();
        assert!(!sim.link(0), "t1: still in flight (one swap behind)");
        sim.tick();
        assert!(sim.link(0), "t2: now visible");
    }

    /// Wired-OR via incremental driver_count (D3/I2): a link driven by two sources stays powered
    /// until *both* go low. Exercises driver_count crossing 2 → 1 → 0.
    #[test]
    fn wired_or_two_drivers() {
        let mut b = BoardBuilder::new(1);
        let a = b.component(CompType::UserInput, &[], &[0], &[]);
        let c = b.component(CompType::UserInput, &[], &[0], &[]); // both drive link 0
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

        let settle = |s: &mut Simulation| (0..3).for_each(|_| s.tick());

        sim.trigger_input(a, InputEvent::Cont, &[true]).unwrap();
        sim.trigger_input(c, InputEvent::Cont, &[true]).unwrap();
        settle(&mut sim);
        assert!(sim.link(0), "both drivers high → powered");

        sim.trigger_input(a, InputEvent::Cont, &[false]).unwrap();
        settle(&mut sim);
        assert!(
            sim.link(0),
            "one driver still high → still powered (count 2→1)"
        );

        sim.trigger_input(c, InputEvent::Cont, &[false]).unwrap();
        settle(&mut sim);
        assert!(!sim.link(0), "both low → unpowered (count 1→0)");
    }

    /// A one-shot `Pulse` asserts its link for exactly one tick window then auto-clears.
    #[test]
    fn pulse_is_one_shot() {
        let mut b = BoardBuilder::new(1);
        b.component(CompType::UserInput, &[], &[0], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        sim.trigger_input(0, InputEvent::Pulse, &[true]).unwrap();

        // The between-tick handler asserts on the first tick (lands in write_buf), the read phase
        // makes it visible on the next, and the following between-tick clears it.
        let mut seen_high = false;
        let mut seen_low_after = false;
        for _ in 0..6 {
            sim.tick();
            if sim.link(0) {
                seen_high = true;
            } else if seen_high {
                seen_low_after = true;
            }
        }
        assert!(seen_high, "pulse must drive the link high for a tick");
        assert!(seen_low_after, "pulse must auto-clear");
    }

    /// trigger_input on a non-UserInput is rejected.
    #[test]
    fn trigger_rejects_non_input() {
        let mut b = BoardBuilder::new(2);
        b.component(CompType::Not, &[0], &[1], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        assert!(sim.trigger_input(0, InputEvent::Cont, &[true]).is_err());
    }
}
