//! The single-threaded tick (plan §6.2): read phase → compute phase → between-tick section → swap.
//!
//! Order mirrors the old engine exactly — read, compute, fire the between-tick section, *then*
//! swap the buffers (invariant I4). `link_state` changes only in the read phase (I1); `set_output`
//! during compute writes only `output_state`/`driver_count`/`write_buf` (D4). Each component
//! computes at most once per tick — the read phase dedups its enqueues via the `queued_tick`
//! stamps — and evaluation order within a tick is irrelevant (I3). Kernels stay idempotent under
//! duplicate computes anyway (all their latching machinery is double-duty — see
//! [`crate::components`]), so a duplicate that slipped past the dedup would fail soft, not corrupt
//! state.

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
        // Advancing the tick also invalidates every `queued_tick` dedup stamp (they encode the
        // tick they were written in), so the stamps need no clearing.
        std::mem::swap(&mut self.read_buf, &mut self.write_buf);
        self.write_buf.clear();
        for q in &mut self.compute_queue {
            q.clear();
        }
        self.tick += 1;
    }

    /// READ PHASE: for each scheduled link, recompute its net value as `driver_count != 0`
    /// (== the old `any_of(drivers)`, I2); on a flip, update `link_state` (the only place it
    /// changes, I1) and enqueue each not-yet-queued consuming component onto its per-type queue
    /// (`queued_tick` dedup, I3).
    pub(crate) fn read_phase(&mut self) {
        // The dedup stamp for this tick: `tick + 1` is never 0, so the zeroed initial stamps can't
        // collide with it (`self.tick` is fixed for the whole phase).
        let stamp = self.tick + 1;
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
            // Enqueue consumers a same-type group at a time: the consumer slice is sorted by type,
            // so each group streams into one queue with no per-element type lookup. Groups of
            // single-input consumers can't contain a component enqueued twice in a tick (the dedup
            // flag is compile-time, see `ConsumerGroup`), so they take the bulk paths; only groups
            // with multi-input members pay a per-element stamp test (I3: at most one compute per
            // component per tick — multi-input components whose inputs flip together would
            // otherwise recompute once per flipped input). Both slices borrow `self.board`,
            // disjoint from the `compute_queue`/`queued_tick` writes.
            let consumers = self.board.link_consumers(l);
            let groups = self.board.consumer_groups(l);
            let mut pos = 0usize;
            for g in groups {
                let len = g.len as usize;
                let q = &mut self.compute_queue[g.ty as usize];
                if g.dedup {
                    for &c in &consumers[pos..pos + len] {
                        let slot = &mut self.queued_tick[c as usize];
                        if *slot != stamp {
                            *slot = stamp;
                            q.push(c);
                        }
                    }
                } else if len == 1 {
                    // Single-consumer links (a fan-out-1 gate output) are the common case; the
                    // inlined `push` fast path beats `extend_from_slice`'s generic copy for one
                    // element.
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
        for idx in 0..self.clk_ids.len() {
            let c = self.clk_ids[idx];
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

    /// Queue dedup (I3): a multi-input component whose inputs all flip in the same tick lands in
    /// its type queue exactly once. Runs the read phase in isolation so the queue is observable
    /// before the end-of-tick clear.
    #[test]
    fn multi_input_flip_enqueues_component_once() {
        use crate::components;
        let mut b = BoardBuilder::new(4);
        b.component(CompType::UserInput, &[], &[0, 1, 2], &[]);
        let and = b.component(CompType::And, &[0, 1, 2], &[3], &[]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

        sim.trigger_input(0, InputEvent::Cont, &[true, true, true])
            .unwrap();
        sim.tick(); // the swap moves the triggered links into read_buf
        sim.read_phase(); // flips links 0–2 high and enqueues their consumers
        let q = &sim.compute_queue[components::type_index(CompType::And)];
        assert_eq!(q.as_slice(), &[and], "three input flips, one enqueue");
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
