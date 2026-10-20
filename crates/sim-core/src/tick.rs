//! The single-threaded tick: input service → read phase → compute phase → between-tick section →
//! swap.
//!
//! The input service runs first and writes into the current tick's pending set, so a `UserInput`
//! frame reaches its wires in the tick it is applied ([`crate::input`]). `link_state` changes only
//! in the read phase; `set_output` during compute writes only `output_state`/`driver_count` and the
//! next tick's pending set. Duplicate link pushes and double-computes within a tick are idempotent: order within a tick is irrelevant and a
//! component recomputed twice converges. That cost nothing to keep and stays the fail-soft backstop
//! for the read phase's enqueue; an earlier adaptive parallel driver also relied on it, but the
//! engine is single-threaded today.

use crate::components::flat::N_QUEUES;
use crate::components::{self, TickCtx};
use crate::sim::{RunConfig, Simulation};
use core::sync::atomic::Ordering::Relaxed;
use web_time::Instant;

/// Most ticks between clock reads in a timed run. A read costs a `clock_gettime` (~15–25 ns), which
/// would dominate an idle tick whose own work is only tens of ns.
const CHECK_EVERY: u64 = 1024;

/// The links scheduled for one read boundary. The simulation holds two: the current tick's, which
/// the input service fills and the read phase drains, and the next tick's, which the compute phase
/// and the between-tick section fill.
#[derive(Default)]
pub(crate) struct Pending {
    /// Links whose value must be resolved from `driver_count` (several drivers, or watched for
    /// invalidity).
    pub(crate) links: Vec<u32>,
    /// Single-driver links that flip at the boundary: no resolution needed.
    pub(crate) flips: Vec<u32>,
}

/// Enqueue a flipped link's consumers, a whole same-type run at a time: the consumer slice is
/// sorted by type, so each group streams into one queue with no per-element type lookup.
#[inline(always)]
fn enqueue(
    queues: &mut [Vec<u32>; N_QUEUES],
    queued: &mut u32,
    consumers: &[u32],
    groups: &[(u8, u32)],
) {
    let mut pos = 0usize;
    for &(ti, len) in groups {
        let len = len as usize;
        *queued |= 1 << ti;
        // SAFETY: a group's queue index is below `N_QUEUES`, and the groups partition the consumer
        // slice, so `pos + len <= consumers.len()` (board invariants).
        let q = unsafe { queues.get_unchecked_mut(ti as usize) };
        // Single-consumer links (a fan-out-1 gate output) are the common case; the inlined `push`
        // fast path beats `extend_from_slice`'s generic copy for one element.
        if len == 1 {
            q.push(unsafe { *consumers.get_unchecked(pos) });
        } else {
            q.extend_from_slice(unsafe { consumers.get_unchecked(pos..pos + len) });
        }
        pos += len;
    }
}

impl Simulation {
    /// One deterministic step. Does not consult the lifecycle state — callers (`run`, tests) drive
    /// it directly.
    pub fn tick(&mut self) {
        self.run_tick();
    }

    /// Run until the tick budget is spent or the timeout elapses. A run with a timeout ends within
    /// about one tick of it on any board, so the bindings slice their background runs with it.
    pub fn run(&mut self, cfg: RunConfig) {
        let start = Instant::now();
        let mut remaining = cfg.ticks;
        // Ticks until the next clock read; an untimed run never reads it. The windows start at one
        // tick and grow at most ×4 each, so a slow board reads the clock before it can overrun a
        // short timeout; each is cut to the ticks that fit in the time left at this run's rate so
        // far. A clock too coarse to time the first windows (a reduced-precision `performance.now()`)
        // leaves them growing.
        let mut window = if cfg.timeout.is_some() { 1 } else { u64::MAX };
        while remaining > 0 {
            let n = window.min(remaining);
            self.run_ticks(n);
            remaining -= n;
            let Some(timeout) = cfg.timeout else {
                break;
            };
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                break;
            }
            let fit = match elapsed.as_secs_f64() {
                0.0 => u64::MAX,
                secs => {
                    ((cfg.ticks - remaining) as f64 / secs * (timeout - elapsed).as_secs_f64())
                        as u64
                }
            };
            window = fit.clamp(1, (window * 4).min(CHECK_EVERY));
        }
    }

    /// `n` ticks: the hot loop, kept out of line so its code doesn't change with the clock logic
    /// around it in `run()`.
    #[inline(never)]
    fn run_ticks(&mut self, n: u64) {
        for _ in 0..n {
            self.run_tick();
        }
    }

    /// The tick body shared by `tick()` and `run()`.
    fn run_tick(&mut self) {
        // A board that never submits input pays only this check.
        if !self.active.is_empty() {
            self.service_inputs();
        }
        self.read_phase();
        self.compute_phase();
        self.between_tick();
        // Swap the pending sets and advance the tick. (The read phase emptied the current set, so
        // the swapped-out one starts empty; the compute phase already emptied the queues.)
        std::mem::swap(&mut self.cur, &mut self.next);
        self.tick += 1;
    }

    /// READ PHASE: for each scheduled link, recompute its net value as `driver_count != 0`
    /// (any driver powered) and, for a link that can be invalid, its validity; on a flip, update
    /// `link_state` (the only place it changes) and enqueue every consuming component onto its
    /// per-type queue.
    ///
    /// Single-driver links skip the resolution: `set_output` lists them in `flips` only when they
    /// flip, so here they toggle `link_state` and enqueue without a driver count.
    ///
    /// The per-link reads skip their bounds checks: every id in `links` was pushed by
    /// `set_output`, `set_invalid` or the power-on seeding, each of which takes it from
    /// `output_link`, so it is below `link_count` (a [`Board`](crate::board::Board) invariant), the
    /// length of `driver_count` and of every link bitset.
    pub(crate) fn read_phase(&mut self) {
        let cur = &mut *self.cur;
        for i in 0..cur.links.len() {
            let l = cur.links[i];
            // SAFETY: `l < link_count` (see above).
            let drivers = unsafe { self.driver_count.get_unchecked(l as usize) }.load(Relaxed);
            if self.board.invalid_watch.get(l) {
                // Before the unchanged-value exit: validity can change while the value holds.
                let total = self.board.driver_total[l as usize];
                let invalid =
                    (drivers != 0 && drivers != total) || self.scratch.invalid_drivers(l) != 0;
                self.invalid.set(l, invalid);
            }
            let v = drivers != 0;
            // SAFETY (every unchecked read below): `l < link_count`, the length of `link_state`
            // and `poll_seen`.
            if v == unsafe { self.link_state.get_unchecked(l) } {
                continue;
            }
            unsafe { self.link_state.set_unchecked(l, v) };
            // Always-on dirty tracking for snapshot deltas: record the flip once per
            // accumulation window. Short-circuits on an already-marked link, so ~1 cycle/flip.
            if !unsafe { self.poll_seen.get_unchecked(l) } {
                unsafe { self.poll_seen.set_unchecked(l, true) };
                self.poll_ids.push(l);
            }
            let (consumers, groups) = unsafe { self.board.link_fanout_unchecked(l) };
            enqueue(&mut self.compute_queue, &mut self.queued, consumers, groups);
        }
        cur.links.clear();
        // Single-driver flips: the value is known to change, so no driver count or comparison.
        for &l in &cur.flips {
            // SAFETY: every id in `flips` was taken from `output_link` by `set_output` (or the
            // power-on seeding), so it is below `link_count` (see above).
            unsafe {
                self.link_state
                    .set_unchecked(l, !self.link_state.get_unchecked(l));
                if !self.poll_seen.get_unchecked(l) {
                    self.poll_seen.set_unchecked(l, true);
                    self.poll_ids.push(l);
                }
                let (consumers, groups) = self.board.link_fanout_unchecked(l);
                enqueue(&mut self.compute_queue, &mut self.queued, consumers, groups);
            }
        }
        cur.flips.clear();
    }

    /// COMPUTE PHASE: drain each non-empty per-type queue through its kernel, emptying it. Kernels
    /// read the frozen `link_state` and write via `set_output`.
    ///
    /// Only the `queued` bits are visited, in type order, so a tick that touches two types pays
    /// for two, not for a walk over every type — that walk was most of an idle tick's fixed cost.
    /// Kernels never enqueue, so the mask taken here stays complete for the whole phase.
    pub(crate) fn compute_phase(&mut self) {
        let mut queued = core::mem::take(&mut self.queued);
        while queued != 0 {
            let qi = queued.trailing_zeros() as usize;
            queued &= queued - 1;
            // ctx borrows `next` (mut) + board/link_state/output_state/driver_count (shared);
            // the queue is a disjoint field, so the shared borrow below coexists.
            let mut ctx = TickCtx::new(
                &self.board,
                &self.link_state,
                &self.output_state,
                &self.driver_count,
                &self.scratch,
                self.tick,
                &mut self.next,
            );
            components::dispatch_queue(qi, &self.compute_queue[qi], &mut ctx);
            self.compute_queue[qi].clear();
        }
    }

    /// BETWEEN-TICK SECTION (fires *before* the swap): the CLK period toggles.
    pub(crate) fn between_tick(&mut self) {
        // CLK period toggle: each subscribed clock counts the ticks spent at its current level and
        // flips once that reaches the level's length (`config.a` low, `config.b` high). The
        // halt-input gating in the compute phase has already (un)subscribed clocks this tick.
        for idx in 0..self.clk_ids.len() {
            let c = self.clk_ids[idx];
            if !self.scratch.clk_subscribed(c) {
                // Halted: hold the count at the start of a cycle, so a restarted clock always
                // runs a full low period first, whatever point it was halted at.
                self.clk_tick_count[c as usize] = 0;
                continue;
            }
            let o0 = self.board.comp_out_off[c as usize];
            let high = self.output_state.get(o0);
            let cfg = self.board.config(c);
            let len = if high { cfg.b } else { cfg.a };
            let tc = &mut self.clk_tick_count[c as usize];
            *tc += 1;
            if *tc >= len {
                *tc = 0;
                let mut ctx = self.make_ctx();
                ctx.set_output(o0, !high);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, RunConfig, Simulation};
    use std::time::{Duration, Instant};

    /// A timed run on a board whose ticks are slow still ends near its timeout: the bindings slice
    /// their background runs with short timeouts, and a run that only read the clock after a full
    /// window of ticks would hold the thread far past them.
    #[test]
    fn timed_run_on_a_slow_board_ends_near_its_timeout() {
        // Rings of three NOTs: every link flips every tick.
        let mut b = BoardBuilder::new();
        for i in 0..3 * 20_000 {
            b.component(
                CompType::Not,
                &[i],
                &[if i % 3 == 2 { i - 2 } else { i + 1 }],
            );
        }
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        let timeout = Duration::from_millis(5);
        let start = Instant::now();
        sim.run(RunConfig {
            ticks: u64::MAX,
            timeout: Some(timeout),
        });
        let elapsed = start.elapsed();
        let per_tick = elapsed / sim.tick_count() as u32;
        assert!(
            elapsed < timeout + per_tick * 8 + Duration::from_millis(5),
            "{elapsed:?} for {} ticks of {per_tick:?}",
            sim.tick_count()
        );
    }

    /// Init-seeding (the most error-prone init step): a NOT seeds its output high
    /// on init, so its output *pin* reads high immediately (post-init `output_state`) but the driven
    /// *link* only flips at the first read boundary — i.e. high after tick 1, not before.
    #[test]
    fn not_output_seeds_and_appears_after_tick_1() {
        let mut b = BoardBuilder::new();
        b.component(CompType::Not, &[0], &[1]); // in: link0, out: link1
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

    /// A submitted input frame is on the pin *and* the link after the next tick: the input service
    /// writes into the read buffer of the tick it runs in. (If the link lagged the pin by a tick,
    /// the service would be scheduling into the write buffer.)
    #[test]
    fn input_frame_reaches_link_after_one_tick() {
        let mut b = BoardBuilder::new();
        b.component(CompType::UserInput, &[], &[0]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        sim.set_input(0, &[true]).unwrap();

        assert!(!sim.output(0, 0) && !sim.link(0), "t0: nothing applied yet");
        sim.tick();
        assert!(sim.output(0, 0), "t1: pin driven");
        assert!(sim.link(0), "t1: link driven in the same tick");
    }

    /// Wired-OR via incremental driver_count: a link driven by two sources stays powered
    /// until *both* go low. Exercises driver_count crossing 2 → 1 → 0.
    #[test]
    fn wired_or_two_drivers() {
        let mut b = BoardBuilder::new();
        let a = b.component(CompType::UserInput, &[], &[0]);
        let c = b.component(CompType::UserInput, &[], &[0]); // both drive link 0
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

        let settle = |s: &mut Simulation| (0..3).for_each(|_| s.tick());

        sim.set_input(a, &[true]).unwrap();
        sim.set_input(c, &[true]).unwrap();
        settle(&mut sim);
        assert!(sim.link(0), "both drivers high → powered");

        sim.set_input(a, &[false]).unwrap();
        settle(&mut sim);
        assert!(
            sim.link(0),
            "one driver still high → still powered (count 2→1)"
        );

        sim.set_input(c, &[false]).unwrap();
        settle(&mut sim);
        assert!(!sim.link(0), "both low → unpowered (count 1→0)");
    }

    /// A wired-OR handoff inside one tick window — one driver drops while another rises before
    /// the next read boundary — must never glitch the link's visible value low.
    #[test]
    fn wired_or_handoff_within_one_tick_no_glitch() {
        let mut b = BoardBuilder::new();
        let a = b.component(CompType::UserInput, &[], &[0]);
        let c = b.component(CompType::UserInput, &[], &[0]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

        sim.set_input(a, &[true]).unwrap();
        (0..3).for_each(|_| sim.tick());
        assert!(sim.link(0), "driver a powers the link");

        // Both writes land before the same read boundary: the driver count dips 1→0→1.
        sim.set_input(a, &[false]).unwrap();
        sim.set_input(c, &[true]).unwrap();
        for _ in 0..3 {
            sim.tick();
            assert!(sim.link(0), "handoff must not glitch the link low");
        }
    }

    /// Two links sharing one bitset word and flipping in the same tick must both wake their
    /// consumers (word-iteration correctness for the read phase).
    #[test]
    fn two_links_in_one_word_flip_same_tick() {
        let mut b = BoardBuilder::new();
        let src = b.component(CompType::UserInput, &[], &[1, 2]); // links 1, 2: word 0
        b.component(CompType::Not, &[1], &[3]);
        b.component(CompType::Not, &[2], &[4]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        (0..2).for_each(|_| sim.tick());
        assert!(sim.link(3) && sim.link(4), "NOTs settle high on low inputs");

        sim.set_input(src, &[true, true]).unwrap();
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
        let mut b = BoardBuilder::new();
        let src = b.component(CompType::UserInput, &[], &[63]);
        b.component(CompType::Not, &[63], &[64]);
        b.component(CompType::Not, &[64], &[65]);
        let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
        (0..3).for_each(|_| sim.tick());
        assert!(!sim.link(63) && sim.link(64) && !sim.link(65), "settled");

        sim.set_input(src, &[true]).unwrap();
        (0..4).for_each(|_| sim.tick());
        assert!(sim.link(63), "driven across the boundary");
        assert!(!sim.link(64), "first NOT follows");
        assert!(sim.link(65), "second NOT follows");
    }
}
