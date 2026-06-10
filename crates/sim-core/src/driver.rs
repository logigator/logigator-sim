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
        // All rayon ops inside run within this pool (so the chunk count == cfg.threads).
        pool.install(|| {
            while remaining > 0 {
                if self.state == SimState::Stopping {
                    break;
                }
                self.run_tick_adaptive(threshold);
                remaining -= 1;
                self.update_speed(start);
                if timeout.is_some_and(|t| start.elapsed() >= t) {
                    break;
                }
            }
        });
        self.state = SimState::Stopped;
        Ok(())
    }

    /// One tick, picking ST vs MT per phase on the frontier size (plan §8.2). Read and compute are
    /// decided independently: a tick can have a small read frontier (ST read) but a large compute
    /// frontier from high fan-out (MT compute), or vice versa.
    fn run_tick_adaptive(&mut self, threshold: usize) {
        let read_par = self.read_buf.len() >= threshold;
        if read_par {
            self.read_phase_par();
        } else {
            self.read_phase();
        }

        let frontier: usize = self.compute_queue.iter().map(Vec::len).sum();
        let compute_par = frontier >= threshold;
        if compute_par {
            self.compute_phase_par();
        } else {
            self.compute_phase();
        }

        self.between_tick();
        // Swap buffers, clear the new write buffer and the per-type queues, advance the tick
        // (identical to the single-threaded `run_tick`).
        std::mem::swap(&mut self.read_buf, &mut self.write_buf);
        self.write_buf.clear();
        for q in &mut self.compute_queue {
            q.clear();
        }
        self.tick += 1;
        self.last_parallel = read_par || compute_par;
    }

    /// Parallel READ PHASE (plan §8.2/§8.3 pts 2,4,8). Shards `read_buf` across the pool; each worker
    /// recomputes its links' net value, flips `link_state` with an atomic RMW (so two workers writing
    /// different bits of a shared word don't lose updates), and — only if *it* won the flip — records
    /// the flip into thread-local lists. The lists are merged serially at the boundary: per-type
    /// queues concatenated into `compute_queue`, poll flips deduped through `poll_seen`.
    fn read_phase_par(&mut self) {
        let board = &self.board;
        let link_state = &self.link_state;
        let driver_count: &[AtomicU16] = &self.driver_count;
        let comp_ty_index: &[u8] = &self.comp_ty_index;
        let read_buf: &[u32] = &self.read_buf;

        let nthreads = rayon::current_num_threads();
        let chunk = read_buf.len().div_ceil(nthreads).max(1);

        let locals: Vec<ReadLocal> = read_buf
            .par_chunks(chunk)
            .map(|links| {
                let mut local = ReadLocal::new();
                for &l in links {
                    let v = driver_count[l as usize].load(Relaxed) != 0;
                    if v == link_state.get(l) {
                        continue; // no flip
                    }
                    // Atomic flip: bail unless this worker is the one that actually changed the bit
                    // (read_buf may list a link more than once, in different chunks).
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
            .collect();

        // Serial merge at the read→compute boundary (cheap Vec concat + a dedup-bit test, §1.3a).
        for local in &locals {
            for qi in 0..N_TYPES {
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
    fn compute_phase_par(&mut self) {
        let nthreads = rayon::current_num_threads();

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

            let bufs: Vec<Vec<u32>> = q
                .par_chunks(chunk)
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
                .collect();

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
