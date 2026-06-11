//! The single-threaded tick (plan §6.2): read phase → compute phase → between-tick section → swap.
//!
//! Order mirrors the old engine exactly — read, compute, fire the between-tick section, *then*
//! swap the buffers (invariant I4). `link_state` changes only in the read phase (I1); `set_output`
//! during compute writes only `output_state`/`driver_count`/`write_buf` (D4). Duplicate link pushes
//! and double-computes within a tick are idempotent (I3): order within a tick is irrelevant and a
//! component recomputed twice converges. That cost nothing to keep and stays the fail-soft backstop
//! for the read phase's enqueue; an earlier adaptive parallel driver also relied on it, but the
//! engine is single-threaded today.

use crate::components::{self, N_TYPES, TickCtx};
use crate::sim::{RunConfig, Simulation};
use crate::types::SimState;
use core::sync::atomic::Ordering::Relaxed;
use web_time::Instant;

/// Ticks between wall-clock samples in the run loop. `update_speed` and the timeout test each cost
/// a `clock_gettime` (~15–25 ns), which dominates an idle tick whose own work is only tens of ns; a
/// small board pays it every tick for nothing. Sampling once per window amortizes it to near-zero.
/// The cost is timeout granularity: a run may overshoot its deadline by up to `CHECK_EVERY - 1`
/// ticks (the Node worker already batches 4096 ticks between checks, so this is not a new class of
/// imprecision), and on a board so slow that 1024 ticks take longer than a second the speed window
/// stretches past 1 s — acceptable for an approximate readout.
pub(crate) const CHECK_EVERY: u64 = 1024;

impl Simulation {
    /// One deterministic step. Does not consult the lifecycle state — callers (`run`, tests) drive
    /// it directly.
    pub fn tick(&mut self) {
        self.run_tick();
    }

    /// Run until the tick budget is spent, the timeout elapses, or `stop()` is requested (plan
    /// §7.2).
    pub fn run(&mut self, cfg: RunConfig) -> crate::Result<()> {
        self.run_single(cfg)
    }

    /// The run loop.
    pub(crate) fn run_single(&mut self, cfg: RunConfig) -> crate::Result<()> {
        self.state = SimState::Running;
        let start = Instant::now();
        self.last_capture = start;
        self.last_capture_tick = self.tick;

        let mut remaining = cfg.ticks;
        // Sample the wall clock only once per `CHECK_EVERY` ticks (capped by `remaining` so a short
        // finite run still checks at its end); the per-tick stop flag is a plain field load.
        let mut countdown = CHECK_EVERY.min(remaining);
        while remaining > 0 {
            if self.state == SimState::Stopping {
                break;
            }
            self.run_tick();
            remaining -= 1;
            countdown -= 1;
            if countdown == 0 {
                self.update_speed(start);
                // (avoid a let-chain here: those stabilized after the 1.85 MSRV floor)
                if cfg.timeout.is_some_and(|t| start.elapsed() >= t) {
                    break;
                }
                countdown = CHECK_EVERY.min(remaining);
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
    pub(crate) fn read_phase(&mut self) {
        let mut i = 0;
        while i < self.read_buf.len() {
            let l = self.read_buf[i];
            i += 1;
            let v = self.driver_count[l as usize].load(Relaxed) != 0;
            if v == self.link_state.get(l) {
                continue;
            }
            self.link_state.set(l, v);
            // Always-on dirty tracking for snapshot deltas (plan §6.4): record the flip once per
            // accumulation window. Short-circuits on an already-marked link, so ~1 cycle/flip.
            if !self.poll_seen.get(l) {
                self.poll_seen.set(l, true);
                self.poll_ids.push(l);
            }
            // Enqueue consumers a whole same-type run at a time (P2): the consumer slice is sorted
            // by type, so each group streams into one queue with no per-element type lookup. Both
            // slices borrow `self.board`, disjoint from the `compute_queue` write.
            let consumers = self.board.link_consumers(l);
            let groups = self.board.consumer_groups(l);
            let mut pos = 0usize;
            for &(ti, len) in groups {
                let len = len as usize;
                let q = &mut self.compute_queue[ti as usize];
                // Single-consumer links (a fan-out-1 gate output) are the common case; the inlined
                // `push` fast path beats `extend_from_slice`'s generic copy for one element.
                if len == 1 {
                    q.push(consumers[pos]);
                } else {
                    q.extend_from_slice(&consumers[pos..pos + len]);
                }
                pos += len;
            }
        }
    }

    /// COMPUTE PHASE: drain each non-empty per-type queue through its kernel. Kernels read the
    /// frozen `link_state` and write via `set_output`.
    pub(crate) fn compute_phase(&mut self) {
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
                &self.scratch,
                self.tick,
                &mut self.write_buf,
            );
            components::dispatch_compute(ty, &self.compute_queue[qi], &mut ctx);
        }
    }

    /// BETWEEN-TICK SECTION (always single-threaded, fires *before* the swap, I4): drain armed
    /// `UserInput` pulses, and the CLK period toggles. A pending pulse asserts its outputs this
    /// tick then disarms; a disarmed entry clears its outputs and unsubscribes — matching the old
    /// `tickEvent` handler. Clocks run first (they subscribe at construction, before any pulse).
    pub(crate) fn between_tick(&mut self) {
        // CLK period toggle: for each subscribed clock, flip its output high if the period counter
        // reaches `speed`, else back low the next tick (mirrors clk.h's tickEvent handler). The
        // enable-input gating in the compute phase has already (un)subscribed clocks this tick.
        // Internal ids are type-bucketed, so the CLKs are exactly one contiguous id range.
        for c in self.clk_range.0..self.clk_range.1 {
            if !self.scratch.clk_subscribed(c) {
                continue;
            }
            let o0 = self.board.comp_out_off[c as usize];
            let v = if self.output_state.get(o0) {
                Some(false) // currently high → drive low
            } else {
                let speed = self.board.config(c).a as i32;
                let tc = &mut self.clk_tick_count[c as usize];
                *tc += 1;
                if *tc >= speed {
                    *tc = 0;
                    Some(true)
                } else {
                    None // still counting up; output stays low
                }
            };
            if let Some(v) = v {
                let mut ctx = self.make_ctx();
                ctx.set_output(o0, v);
            }
        }

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
                    &self.scratch,
                    self.tick,
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
    pub(crate) fn update_speed(&mut self, _start: Instant) {
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

    /// A wired-OR handoff inside one tick window — one driver drops while another rises before
    /// the next read boundary — must never glitch the link's visible value low.
    #[test]
    fn wired_or_handoff_within_one_tick_no_glitch() {
        let mut b = BoardBuilder::new(1);
        let a = b.component(CompType::UserInput, &[], &[0], &[]);
        let c = b.component(CompType::UserInput, &[], &[0], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

        sim.trigger_input(a, InputEvent::Cont, &[true]).unwrap();
        (0..3).for_each(|_| sim.tick());
        assert!(sim.link(0), "driver a powers the link");

        // Both writes land before the same read boundary: the driver count dips 1→0→1.
        sim.trigger_input(a, InputEvent::Cont, &[false]).unwrap();
        sim.trigger_input(c, InputEvent::Cont, &[true]).unwrap();
        for _ in 0..3 {
            sim.tick();
            assert!(sim.link(0), "handoff must not glitch the link low");
        }
    }

    /// A link driven up and back down by two different outputs within one tick window shows no
    /// flip at the next read boundary (count 0→1→2→1→0; both crossings cancel).
    #[test]
    fn link_up_and_back_down_in_one_tick_no_flip() {
        let mut b = BoardBuilder::new(2);
        let a = b.component(CompType::UserInput, &[], &[0], &[]);
        let c = b.component(CompType::UserInput, &[], &[0], &[]);
        b.component(CompType::Not, &[0], &[1], &[]); // observer: recomputes only if link 0 flips
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        (0..2).for_each(|_| sim.tick());
        assert!(sim.link(1), "NOT of the low link settles high");

        sim.trigger_input(a, InputEvent::Cont, &[true]).unwrap();
        sim.trigger_input(c, InputEvent::Cont, &[true]).unwrap();
        sim.trigger_input(a, InputEvent::Cont, &[false]).unwrap();
        sim.trigger_input(c, InputEvent::Cont, &[false]).unwrap();
        for _ in 0..3 {
            sim.tick();
            assert!(!sim.link(0), "net value never changed");
            assert!(sim.link(1), "consumer saw no flip");
        }
    }

    /// Two links sharing one bitset word and flipping in the same tick must both wake their
    /// consumers (word-iteration correctness for the read phase).
    #[test]
    fn two_links_in_one_word_flip_same_tick() {
        let mut b = BoardBuilder::new(5);
        let src = b.component(CompType::UserInput, &[], &[1, 2], &[]); // links 1, 2: word 0
        b.component(CompType::Not, &[1], &[3], &[]);
        b.component(CompType::Not, &[2], &[4], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        (0..2).for_each(|_| sim.tick());
        assert!(sim.link(3) && sim.link(4), "NOTs settle high on low inputs");

        sim.trigger_input(src, InputEvent::Cont, &[true, true])
            .unwrap();
        (0..3).for_each(|_| sim.tick());
        assert!(sim.link(1) && sim.link(2), "both inputs flipped high");
        assert!(
            !sim.link(3) && !sim.link(4),
            "both consumers recomputed off the same-word flips"
        );
    }

    /// Links straddling a 64-bit word boundary (bits 63 and 64) propagate across it — off-by-one
    /// guard on the packed link layout.
    #[test]
    fn links_at_word_boundary_propagate() {
        let mut b = BoardBuilder::new(66);
        let src = b.component(CompType::UserInput, &[], &[63], &[]);
        b.component(CompType::Not, &[63], &[64], &[]);
        b.component(CompType::Not, &[64], &[65], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        (0..3).for_each(|_| sim.tick());
        assert!(!sim.link(63) && sim.link(64) && !sim.link(65), "settled");

        sim.trigger_input(src, InputEvent::Cont, &[true]).unwrap();
        (0..4).for_each(|_| sim.tick());
        assert!(sim.link(63), "driven across the boundary");
        assert!(!sim.link(64), "first NOT follows");
        assert!(sim.link(65), "second NOT follows");
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
