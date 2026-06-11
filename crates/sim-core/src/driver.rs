//! The adaptive multi-threaded run loop (plan §8, D5/D15). Compiled only with the `threads` feature.
//!
//! Single-threaded by default; parallelism is an *escape hatch* taken per phase, per tick, only when
//! the frontier is large enough to amortize rayon's coordination cost (`par_threshold`, plan §8.1).
//! On a small tick the loop runs the exact same [`Simulation::read_phase`]/[`compute_phase`] the
//! single-threaded driver does (plain load/store, zero atomic cost — §1.3a/I7). On a big tick it
//! shards the work across a rayon pool and the kernels take the `PAR = true` specialization
//! (atomic-RMW `set_output`, §5.3a/D15).
//!
//! **Why the per-tick state is bit-identical to single-threaded (§8.6):** (1) compute reads the
//! *frozen* `link_state` (phase separation, I1), so cross-component order is irrelevant; (2) every
//! cross-thread read of another thread's write is deferred past a phase boundary (the rayon join's
//! release/acquire edge), so the RMWs need only `Relaxed` (§8.3); (3) the compute queue is
//! **deduplicated** before sharding, so no component is computed by two threads at once — which is
//! what makes the JK flip-flop's self-referential toggle (`Q = !Q`) safe, since that one kernel
//! reads its *live* output rather than a frozen input (advisor; the other stateful kernels would be
//! idempotent under double-compute, but dedup makes the whole class correct-by-construction). The
//! between-tick section (CLK/pulse) always runs single-threaded (I4).
//!
//! Worklist *ordering* is not byte-identical across thread counts (per-thread lists concatenate in
//! schedule order), but the settled *state* is — see the §8.6 scope caveat.

use crate::components::{self, N_TYPES, TickCtx};
use crate::error::SimError;
use crate::sim::{RunConfig, Simulation};
use crate::types::SimState;
use core::sync::atomic::{AtomicU16, Ordering::Relaxed};
use rayon::ThreadPool;
use rayon::prelude::*;
use web_time::Instant;

/// Per-worker scratch for the parallel read phase: the components each flipped link enqueues
/// (bucketed by type, exactly like the serial read phase fills `compute_queue`) plus the links this
/// worker observed flip (for the poll-dirty accumulator). Concatenated at the read→compute boundary
/// — no shared atomic worklist on the hot path (plan §8.3 pt 4 & 8).
struct ReadLocal {
    queues: [Vec<u32>; N_TYPES],
    poll: Vec<u32>,
}

impl ReadLocal {
    fn new() -> Self {
        ReadLocal {
            queues: std::array::from_fn(|_| Vec::new()),
            poll: Vec::new(),
        }
    }
}

impl Simulation {
    /// The adaptive parallel run loop (plan §8.2). Builds a rayon pool sized to `cfg.threads`
    /// (caller guarantees `> 1`) and ticks until the budget/timeout/stop, choosing the parallel
    /// path per phase. `cfg.par_threshold` is the frontier at which a phase parallelizes.
    pub(crate) fn run_parallel(&mut self, cfg: RunConfig) -> crate::Result<()> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(cfg.threads)
            .build()
            .map_err(|e| SimError::ThreadPool(e.to_string()))?;

        self.state = SimState::Running;
        let start = Instant::now();
        self.last_capture = start;
        self.last_capture_tick = self.tick;

        let threshold = cfg.par_threshold;
        let timeout = cfg.timeout;
        let mut remaining = cfg.ticks;
        // Sample the wall clock once per `CHECK_EVERY` ticks (see `tick::run_single`); a large
        // parallel tick already dwarfs a `clock_gettime`, but this keeps both loops uniform.
        let mut countdown = crate::tick::CHECK_EVERY.min(remaining);
        while remaining > 0 {
            if self.state == SimState::Stopping {
                break;
            }
            self.run_tick_adaptive(&pool, threshold);
            remaining -= 1;
            countdown -= 1;
            if countdown == 0 {
                self.update_speed(start);
                if timeout.is_some_and(|t| start.elapsed() >= t) {
                    break;
                }
                countdown = crate::tick::CHECK_EVERY.min(remaining);
            }
        }
        self.state = SimState::Stopped;
        Ok(())
    }

    /// One adaptive tick, for callers that own the batching/lifecycle themselves (the Node worker:
    /// it ticks in batches and drains commands between them, plan §7.4). `pool` is the run's worker
    /// pool — only the parallel phases install it (around their `par_chunks`), so a small/ST tick
    /// runs as plain method calls with no `install` boundary. Unlike [`run`](Simulation::run) this
    /// does **not** touch the lifecycle state — the caller owns `state`/speed.
    pub fn tick_adaptive(&mut self, pool: &ThreadPool, par_threshold: usize) {
        self.run_tick_adaptive(pool, par_threshold);
    }

    /// One tick, picking ST vs MT per phase on the frontier size (plan §8.2). Read and compute are
    /// decided independently: a tick can have a small read frontier (ST read) but a large compute
    /// frontier from high fan-out (MT compute), or vice versa. Only the parallel branches install
    /// `pool`; the ST branches are plain calls (zero coordination cost on a small tick, §1.1/§8.1).
    fn run_tick_adaptive(&mut self, pool: &ThreadPool, threshold: usize) {
        let read_par = self.read_buf.len() >= threshold;
        if read_par {
            self.read_phase_par(pool);
        } else {
            self.read_phase::<true>(); // maintains compute_frontier
        }

        // `compute_frontier` was maintained incrementally by the read phase (plan §8.1), so the
        // compute-parallelism decision is one integer compare — not a sweep of the per-type queues.
        let compute_par = self.compute_frontier >= threshold;
        if compute_par {
            self.compute_phase_par(pool);
        } else {
            self.compute_phase();
        }

        self.between_tick();
        // Swap buffers, clear the new write buffer and the per-type queues, advance the tick
        // (identical to the single-threaded `run_tick`, plus the frontier reset).
        std::mem::swap(&mut self.read_buf, &mut self.write_buf);
        self.write_buf.clear();
        for q in &mut self.compute_queue {
            q.clear();
        }
        self.compute_frontier = 0;
        self.tick += 1;
        self.last_parallel = read_par || compute_par;
    }

    /// Parallel READ PHASE (plan §8.2/§8.3 pts 2,4,8). Shards `read_buf` across the pool; each worker
    /// recomputes its links' net value, flips `link_state` with an atomic RMW (so two workers writing
    /// different bits of a shared word don't lose updates), and — only if *it* won the flip — records
    /// the flip into thread-local lists. The lists are merged serially at the boundary: per-type
    /// queues concatenated into `compute_queue`, poll flips deduped through `poll_seen`.
    fn read_phase_par(&mut self, pool: &ThreadPool) {
        let board = &self.board;
        let link_state = &self.link_state;
        let driver_count: &[AtomicU16] = &self.driver_count;
        let comp_ty_index: &[u8] = &self.comp_ty_index;
        let read_buf: &[u32] = &self.read_buf;

        let nthreads = pool.current_num_threads();
        let chunk = read_buf.len().div_ceil(nthreads).max(1);

        // `install` so the `par_chunks` runs on `pool` (chunk count == cfg.threads). Only the
        // parallel branch pays the install boundary — small/ST ticks never reach here.
        let locals: Vec<ReadLocal> = pool.install(|| {
            read_buf
                .par_chunks(chunk)
                .map(|links| {
                    let mut local = ReadLocal::new();
                    for &l in links {
                        let v = driver_count[l as usize].load(Relaxed) != 0;
                        if v == link_state.get(l) {
                            continue; // no flip
                        }
                        // Atomic flip: bail unless this worker actually changed the bit (read_buf
                        // may list a link more than once, in different chunks).
                        if link_state.fetch_set(l, v) == v {
                            continue;
                        }
                        local.poll.push(l);
                        for &c in board.link_consumers(l) {
                            let qi = comp_ty_index[c as usize] as usize;
                            local.queues[qi].push(c);
                        }
                    }
                    local
                })
                .collect()
        });

        // Serial merge at the read→compute boundary (cheap Vec concat + a dedup-bit test, §1.3a).
        // Maintain compute_frontier here too (it starts at 0, reset by run_tick_adaptive each tick).
        for local in &locals {
            for qi in 0..N_TYPES {
                self.compute_frontier += local.queues[qi].len();
                self.compute_queue[qi].extend_from_slice(&local.queues[qi]);
            }
            for &l in &local.poll {
                if !self.poll_seen.get(l) {
                    self.poll_seen.set(l, true);
                    self.poll_ids.push(l);
                }
            }
        }
    }

    /// Parallel COMPUTE PHASE (plan §8.2/§8.3 pts 1,3,7). For each non-empty per-type queue: dedup it
    /// in place (so each component is computed exactly once — see the module-level note on the JK
    /// toggle), then shard across the pool. Each worker writes into its own `write_buf` and applies
    /// the atomic-RMW `set_output`; the write buffers concatenate at the end.
    fn compute_phase_par(&mut self, pool: &ThreadPool) {
        let nthreads = pool.current_num_threads();

        for qi in 0..N_TYPES {
            if self.compute_queue[qi].is_empty() {
                continue;
            }

            // Dedup in place via the per-component `compute_queued` bit (set as we keep an entry,
            // cleared afterward so the bitset is all-zero between ticks). O(frontier).
            {
                let q = &mut self.compute_queue[qi];
                let queued = &self.compute_queued;
                let mut w = 0usize;
                for r in 0..q.len() {
                    let c = q[r];
                    if !queued.get(c) {
                        queued.set(c, true);
                        q[w] = c;
                        w += 1;
                    }
                }
                q.truncate(w);
            }

            let ty = components::type_from_index(qi);
            let board = &self.board;
            let link_state = &self.link_state;
            let output_state = &self.output_state;
            let driver_count: &[AtomicU16] = &self.driver_count;
            let scratch = &self.scratch;
            let tick = self.tick;
            let q: &[u32] = &self.compute_queue[qi];
            let chunk = q.len().div_ceil(nthreads).max(1);

            let bufs: Vec<Vec<u32>> = pool.install(|| {
                q.par_chunks(chunk)
                    .map(|comps| {
                        let mut wb = Vec::new();
                        let mut ctx = TickCtx::<true>::new(
                            board,
                            link_state,
                            output_state,
                            driver_count,
                            scratch,
                            tick,
                            &mut wb,
                        );
                        components::dispatch_compute::<true>(ty, comps, &mut ctx);
                        wb
                    })
                    .collect()
            });

            // Clear the dedup bits over the (deduped) queue, then concatenate the per-thread pushes.
            for &c in &self.compute_queue[qi] {
                self.compute_queued.set(c, false);
            }
            for wb in &bufs {
                self.write_buf.extend_from_slice(wb);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoardBuilder, CompType, InputEvent, RunConfig, Simulation};

    /// Force the parallel path on tiny boards: `par_threshold = 1` parallelizes any non-empty
    /// frontier, so even a handful of links shards across the workers (otherwise the corpus never
    /// crosses the real 2048 threshold and the MT path is never taken — advisor).
    fn forced_par(threads: usize, ticks: u64) -> RunConfig {
        RunConfig {
            ticks,
            timeout: None,
            par_threshold: 1,
            threads,
        }
    }

    /// A board mixing combinational gates, a two-driver wired-OR bus, and a D flip-flop — enough
    /// distinct kernels and a multi-driver link to exercise the read/compute shards and the merge.
    fn build() -> Simulation {
        // links: 0,1 = two UI sources (wired-OR onto link 2 below is via two drivers on one link);
        // 2 = OR-bus, 3 = NOT(2), 4 = clk source, 5 = Q, 6 = Qbar
        let mut b = BoardBuilder::new(7);
        b.component(CompType::UserInput, &[], &[2], &[]); // driver A of link 2
        b.component(CompType::UserInput, &[], &[2], &[]); // driver B of link 2 (wired-OR)
        b.component(CompType::UserInput, &[], &[4], &[]); // clock source
        b.component(CompType::Not, &[2], &[3], &[]); // NOT off the bus
        b.component(CompType::DFf, &[3, 4], &[5, 6], &[]); // D=NOT(bus), clk=link4
        Simulation::from_descriptor(&b.finish()).unwrap()
    }

    /// Drive an identical scripted sequence and return the full settled state (packed link bits +
    /// per-pin output bytes) so two runs can be compared exactly.
    fn drive(cfg_threads: usize) -> (Vec<u8>, Vec<u8>) {
        let mut sim = build();
        let cfg = forced_par(cfg_threads, 5);
        // A pulse train on the clock + toggling the bus drivers, re-running between scripted steps.
        for step in 0..8u32 {
            let a = step & 1 == 1;
            let bb = step & 2 == 2;
            let clk = step & 4 == 4;
            sim.trigger_input(0, InputEvent::Cont, &[a]).unwrap();
            sim.trigger_input(1, InputEvent::Cont, &[bb]).unwrap();
            sim.trigger_input(2, InputEvent::Cont, &[clk]).unwrap();
            sim.run(cfg).unwrap();
        }
        (sim.link_bytes(), sim.output_bytes())
    }

    /// The §8.6 determinism guarantee in miniature: a forced-parallel run is bit-identical to the
    /// single-threaded run. (The thorough corpus/proptest/JK-race coverage lives in the test phase.)
    #[test]
    fn parallel_state_matches_single_threaded() {
        let st = drive(1);
        for threads in [2usize, 4, 8] {
            let mt = drive(threads);
            assert_eq!(st, mt, "state diverged at threads={threads}");
        }
    }

    /// The adversarial JK case the dedup exists for. A self-NOT oscillator on link 0 flips it every
    /// tick; three `Delay`s fan it onto links 1/2/3 (J, clk, K), which therefore all flip on the
    /// *same* tick — so the JK is enqueued **three times** per tick. With `par_threshold = 1` and
    /// many workers those copies land in different chunks, i.e. the JK would be computed
    /// concurrently; its `Q = !Q` toggle reads its own *live* output, so without the compute-queue
    /// dedup two computes could cancel the toggle. Since J=K=1 on every rising clk edge, Q follows a
    /// fixed toggle train — and every thread count must reproduce the single-threaded result exactly.
    fn jk_race_board() -> Simulation {
        let mut b = BoardBuilder::new(6);
        b.component(CompType::Not, &[0], &[0], &[]); // oscillator: link0 flips every tick
        b.component(CompType::Delay, &[0], &[1], &[]); // J  = delayed link0
        b.component(CompType::Delay, &[0], &[2], &[]); // clk = delayed link0
        b.component(CompType::Delay, &[0], &[3], &[]); // K  = delayed link0
        b.component(CompType::JkFf, &[1, 2, 3], &[4, 5], &[]); // J,clk,K → Q=link4, Qbar=link5
        Simulation::from_descriptor(&b.finish()).unwrap()
    }

    fn jk_state_after(ticks: u64, threads: usize) -> (Vec<u8>, Vec<u8>) {
        let mut sim = jk_race_board();
        sim.run(forced_par(threads, ticks)).unwrap();
        (sim.link_bytes(), sim.output_bytes())
    }

    #[test]
    fn jk_self_toggle_is_race_free_under_mt() {
        // Many checkpoints so a toggle that cancels under a race shows up wherever it happens.
        for ticks in [1u64, 2, 3, 4, 7, 16, 41, 100] {
            let st = jk_state_after(ticks, 1);
            for threads in [2usize, 4, 8, 16] {
                let mt = jk_state_after(ticks, threads);
                assert_eq!(
                    st, mt,
                    "JK toggle diverged at ticks={ticks}, threads={threads}"
                );
            }
        }
        // Sanity: the JK actually toggles (the board isn't accidentally static).
        let q_early = jk_state_after(4, 1).1;
        let q_late = jk_state_after(6, 1).1;
        assert_ne!(
            q_early, q_late,
            "the JK toggle train should change Q over time"
        );
    }

    /// A wide gate (≥256 inputs) exercises the wide-fan-in reduction (the AVX2 `vpgatherdd` path on
    /// an AVX2 host, the scalar chunk path elsewhere) **through `compute_batch` under MT** — where
    /// several workers read the frozen `link_state` through the raw-pointer gather concurrently. The
    /// `simd` unit tests cover the reduction directly; this closes the integration seam. Two
    /// oscillators feed a 300-input OR, recomputed every tick; ST≡MT must hold at every checkpoint.
    #[test]
    fn wide_gate_st_equals_mt() {
        let n = 300u32; // ≥ simd::WIDE_FANIN so the AVX2 dispatch fires on AVX2 hosts
        let mut b = BoardBuilder::new(3);
        b.component(CompType::Not, &[0], &[0], &[]); // oscillator: link 0 flips every tick
        b.component(CompType::Not, &[0], &[1], &[]); // link 1 = !link 0
        let inputs: Vec<u32> = (0..n).map(|i| i % 2).collect(); // 300 inputs over links 0/1
        b.component(CompType::Or, &inputs, &[2], &[]); // wide OR → link 2
        let desc = b.finish();

        let run = |threads: usize, ticks: u64| {
            let mut s = Simulation::from_descriptor(&desc).unwrap();
            s.run(forced_par(threads, ticks)).unwrap();
            (s.link_bytes(), s.output_bytes())
        };
        for ticks in [1u64, 2, 3, 7, 30] {
            let st = run(1, ticks);
            for threads in [2usize, 8] {
                assert_eq!(
                    st,
                    run(threads, ticks),
                    "wide-gate ST≠MT at ticks={ticks}, threads={threads}"
                );
            }
        }
    }

    /// `status().parallel` reflects whether the last tick took the parallel path.
    #[test]
    fn status_reports_parallel_path() {
        let mut sim = build();
        sim.trigger_input(0, InputEvent::Cont, &[true]).unwrap();
        sim.run(forced_par(4, 3)).unwrap();
        assert!(
            sim.status().parallel,
            "forced-parallel run should report parallel"
        );

        sim.run(RunConfig {
            ticks: 3,
            timeout: None,
            par_threshold: 2048,
            threads: 1,
        })
        .unwrap();
        assert!(!sim.status().parallel, "threads=1 run is single-threaded");
    }
}
