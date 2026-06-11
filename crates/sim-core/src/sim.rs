//! The owned `Simulation` object: run state, construction (init-seeding + first-read priming), user
//! input, and coherent state accessors (plan §5.2, §6.1, §6.3). The tick mechanics live in
//! [`crate::tick`].

use crate::BoardDescriptor;
use crate::bitset::BitSet;
use crate::board::Board;
use crate::components::{self, N_TYPES, TickCtx};
use crate::error::{Result, SimError};
use crate::scratch::Scratch;
use crate::types::{CompType, InputEvent, SimState};
use core::sync::atomic::AtomicU16;
// `web_time` is `std::time` on native and a `performance.now()`-backed clock on wasm (sim-core
// Cargo.toml); `Duration` is the same `core::time::Duration` either way, so the public `RunConfig`
// signature is unchanged.
use web_time::{Duration, Instant};

/// How a run should terminate (plan §7.2). `par_threshold`/`threads` are accepted now for API
/// stability but ignored until the adaptive parallel driver lands in plan phase 6.
#[derive(Clone, Copy, Debug)]
pub struct RunConfig {
    /// Maximum ticks to run.
    pub ticks: u64,
    /// Optional wall-clock budget.
    pub timeout: Option<Duration>,
    /// Frontier size above which a tick parallelizes (phase 6).
    pub par_threshold: usize,
    /// Worker thread count (phase 6); `1` forces the single-threaded path.
    pub threads: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            ticks: u64::MAX,
            timeout: None,
            par_threshold: 2048,
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        }
    }
}

/// A snapshot of simulation status (plan §7.2).
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Status {
    pub state: SimState,
    pub tick: u64,
    pub speed: u32,
    pub link_count: u32,
    pub component_count: u32,
    /// Whether the last tick ran on the parallel path (always `false` until phase 6).
    pub parallel: bool,
}

/// A pending one-shot `Pulse` on a `UserInput` (the analog of the old `tickEvent` subscription).
pub(crate) struct UiPulse {
    pub(crate) comp: u32,
    pub(crate) state: Vec<bool>,
    /// `true` for the tick that asserts the pulse; cleared so the next between-tick section drops
    /// the assertion and unsubscribes.
    pub(crate) pending: bool,
}

/// An independent, owned simulation (plan D12 — no global singleton). Drop releases everything.
pub struct Simulation {
    pub(crate) board: Board,

    // --- mutable run state (plan §5.2) ---
    /// Visible powered value per link (the frozen snapshot `compute()` reads).
    pub(crate) link_state: BitSet,
    /// Each component-output pin's own value.
    pub(crate) output_state: BitSet,
    /// Incremental count of currently-powered drivers per link (D3); `!= 0` ⟺ wired-OR powered.
    pub(crate) driver_count: Box<[AtomicU16]>,

    /// Links to (re)evaluate this read phase; swapped with `write_buf` each tick.
    pub(crate) read_buf: Vec<u32>,
    /// Links scheduled (by `set_output`) for the next read phase.
    pub(crate) write_buf: Vec<u32>,
    /// Per-type dirty component queues, indexed by `components::type_index`.
    pub(crate) compute_queue: Vec<Vec<u32>>,
    /// Dedup bit per component, used **only** by the parallel compute phase to collapse each type's
    /// queue to one entry per component before sharding — so no component is computed by two threads
    /// at once (the JK self-toggle would otherwise race; advisor / plan §1.10). Set while deduping,
    /// cleared over the deduped queue, so it is all-zero between ticks.
    #[cfg(feature = "threads")]
    pub(crate) compute_queued: crate::bitset::BitSet,
    /// Number of components enqueued this tick (= Σ `compute_queue[*].len()`), maintained
    /// incrementally so the adaptive driver's compute-parallelism decision is a single integer
    /// compare (plan §8.1) rather than a per-tick sweep of every per-type queue. Only the parallel
    /// read paths touch it (`read_phase::<true>` / `read_phase_par`); the single-threaded tick
    /// compiles the counter out entirely (`read_phase::<false>`), so the default path pays nothing.
    pub(crate) compute_frontier: usize,
    /// Precomputed `type_index(comp_ty[c])` for O(1) enqueue in the read phase.
    pub(crate) comp_ty_index: Box<[u8]>,
    /// Per-component mutable scratch for stateful kernels (sel-latch, edge-clock, …).
    pub(crate) scratch: Scratch,

    // --- snapshot dirty-tracking (plan §6.4, D11) ---
    /// Dedup bitset over `link_state`: bit `l` set ⟺ link `l` is already in `poll_ids` for the
    /// current accumulation window. Sized `link_count`.
    pub(crate) poll_seen: BitSet,
    /// Unique link ids that flipped since the last [`Simulation::snapshot`] poll (always-on).
    pub(crate) poll_ids: Vec<u32>,
    /// Reused output buffer: the changed link ids of the last `Delta` snapshot (u32 LE).
    pub(crate) snap_ids: Vec<u32>,
    /// Reused output buffer: packed values of the last `Delta`, bit `i` ⟺ `snap_ids[i]`.
    pub(crate) snap_values: Vec<u8>,
    /// Whether a `Full` snapshot has been emitted since construction — a `Delta` needs a baseline.
    pub(crate) delta_baseline: bool,

    /// Subscribed `UserInput` one-shot pulses, drained in the between-tick section.
    pub(crate) ui_pending: Vec<UiPulse>,
    /// Component ids of every CLK (6), iterated by the between-tick period toggle.
    pub(crate) clk_ids: Vec<u32>,
    /// Per-component CLK period counter (only CLK entries used); advanced in the between-tick
    /// section, so plain `i32` — no interior mutability needed.
    pub(crate) clk_tick_count: Box<[i32]>,

    // --- bookkeeping ---
    pub(crate) link_count: u32,
    pub(crate) comp_count: u32,
    pub(crate) state: SimState,
    pub(crate) tick: u64,
    pub(crate) speed: u32,
    pub(crate) last_capture: Instant,
    pub(crate) last_capture_tick: u64,
    /// Whether the most recent tick took the parallel path (reported by [`Status::parallel`]). A
    /// single-threaded `run`/`tick` leaves it `false`; the adaptive driver sets it per tick.
    pub(crate) last_parallel: bool,
}

impl Simulation {
    /// Build a simulation from a compiled board: allocate zeroed state, replay every component's
    /// power-on init through the runtime flip→count→push path, then prime one read boundary so the
    /// first `tick()` matches the reference engine (plan §6.1 steps 3–5, invariant I5).
    pub fn new(board: Board) -> Result<Self> {
        let link_count = board.link_count;
        let comp_count = board.comp_count;
        let output_count = board.output_count;
        let ram_bytes = board.ram_bytes;

        let comp_ty_index = board
            .comp_ty
            .iter()
            .map(|&t| components::type_index(t) as u8)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let driver_count = (0..link_count)
            .map(|_| AtomicU16::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let clk_ids: Vec<u32> = board
            .comp_ty
            .iter()
            .enumerate()
            .filter(|&(_, &t)| t == CompType::Clk)
            .map(|(i, _)| i as u32)
            .collect();

        let mut sim = Simulation {
            board,
            link_state: BitSet::new(link_count),
            output_state: BitSet::new(output_count),
            driver_count,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            compute_queue: (0..N_TYPES).map(|_| Vec::new()).collect(),
            #[cfg(feature = "threads")]
            compute_queued: BitSet::new(comp_count),
            compute_frontier: 0,
            comp_ty_index,
            scratch: Scratch::new(comp_count, ram_bytes),
            poll_seen: BitSet::new(link_count),
            poll_ids: Vec::new(),
            snap_ids: Vec::new(),
            snap_values: Vec::new(),
            delta_baseline: false,
            ui_pending: Vec::new(),
            clk_ids,
            clk_tick_count: vec![0i32; comp_count as usize].into_boxed_slice(),
            link_count,
            comp_count,
            state: SimState::Stopped,
            tick: 0,
            speed: 0,
            last_capture: Instant::now(),
            last_capture_tick: 0,
            last_parallel: false,
        };
        // Every CLK starts subscribed to the period toggle (the C++ CLK ctor subscribes), output
        // low. The enable-input gating may later unsubscribe it (§5.3a / clk.h::outputChange).
        for &c in &sim.clk_ids {
            sim.scratch.set_clk_subscribed::<false>(c, true);
        }
        sim.seed_init();
        Ok(sim)
    }

    /// Compile a descriptor and build a simulation in one step.
    pub fn from_descriptor(desc: &BoardDescriptor) -> Result<Self> {
        Simulation::new(Board::compile(desc)?)
    }

    /// Seed power-on state then prime the first read buffer (plan §6.1 steps 4–5).
    ///
    /// Each component's `init` runs through `set_output`, so its seeds land in `write_buf` and bump
    /// `driver_count`. The prime then swaps them into `read_buf` (where the first read phase will
    /// flip `link_state`) and leaves `write_buf` empty — **without** running a read phase, so
    /// `link_state` is still all-zero. This is the buffer discipline that makes tick 1 match the
    /// reference (a NOT output reads high after tick 1; a Cont-triggered input after tick 2).
    fn seed_init(&mut self) {
        for c in 0..self.comp_count {
            let ty = self.board.comp_ty[c as usize];
            let mut ctx = self.make_ctx();
            components::dispatch_init(ty, c, &mut ctx);
        }
        std::mem::swap(&mut self.read_buf, &mut self.write_buf);
        self.write_buf.clear();
    }

    /// Construct a [`TickCtx`] borrowing this simulation's topology + state for one out-of-compute
    /// write (init seeding, `trigger_input`, between-tick pulses). The compute phase builds its ctx
    /// inline so it can also read the per-type queue (see [`crate::tick`]).
    pub(crate) fn make_ctx(&mut self) -> TickCtx<'_, false> {
        TickCtx::<false>::new(
            &self.board,
            &self.link_state,
            &self.output_state,
            &self.driver_count,
            &self.scratch,
            self.tick,
            &mut self.write_buf,
        )
    }

    /// Apply external input to a `UserInput` component at a tick boundary (plan §6.3).
    ///
    /// `Cont` latches the outputs immediately; `Pulse` arms a one-tick assertion drained by the
    /// between-tick section. A state slice shorter than the output count pads with `false` (matching
    /// the old binding). Errors if `comp_id` is not a `UserInput`.
    pub fn trigger_input(&mut self, comp_id: u32, event: InputEvent, state: &[bool]) -> Result<()> {
        if comp_id >= self.comp_count || self.board.comp_ty[comp_id as usize] != CompType::UserInput
        {
            return Err(SimError::NotAnInput(comp_id));
        }
        match event {
            InputEvent::Cont => {
                let oids = self.board.output_ids(comp_id);
                let mut ctx = self.make_ctx();
                for (pin, oid) in oids.enumerate() {
                    ctx.set_output(oid, state.get(pin).copied().unwrap_or(false));
                }
            }
            InputEvent::Pulse => {
                let n = self.board.output_ids(comp_id).len();
                let s: Vec<bool> = (0..n)
                    .map(|pin| state.get(pin).copied().unwrap_or(false))
                    .collect();
                if let Some(e) = self.ui_pending.iter_mut().find(|e| e.comp == comp_id) {
                    e.state = s;
                    e.pending = true;
                } else {
                    self.ui_pending.push(UiPulse {
                        comp: comp_id,
                        state: s,
                        pending: true,
                    });
                }
            }
        }
        Ok(())
    }

    // --- status / coherent accessors (plan §7.2) ---

    /// A snapshot of run status.
    pub fn status(&self) -> Status {
        Status {
            state: self.state,
            tick: self.tick,
            speed: self.speed,
            link_count: self.link_count,
            component_count: self.comp_count,
            parallel: self.last_parallel,
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> SimState {
        self.state
    }

    /// Ticks elapsed.
    pub fn tick_count(&self) -> u64 {
        self.tick
    }

    /// Powered value of a single link (coherent between ticks).
    pub fn link(&self, id: u32) -> bool {
        self.link_state.get(id)
    }

    /// Zero-copy borrow of the packed `link_state` bitset (the internal `u64` layout, plan §7.2).
    /// Read words via `.load(Relaxed)`.
    pub fn link_words(&self) -> &[core::sync::atomic::AtomicU64] {
        self.link_state.words()
    }

    /// Packed `ceil(link_count / 8)`-byte little-endian copy of `link_state` — the `--dump-format
    /// bin` payload and full-snapshot buffer length (plan §7.6). Link `l` is bit `l & 7` of byte
    /// `l >> 3`, matching the `u64`-word layout `link_words()` exposes.
    pub fn link_bytes(&self) -> Vec<u8> {
        use core::sync::atomic::Ordering::Relaxed;
        let n_bytes = (self.link_count as usize).div_ceil(8);
        let mut out = Vec::with_capacity(self.link_state.words().len() * 8);
        for w in self.link_state.words() {
            out.extend_from_slice(&w.load(Relaxed).to_le_bytes());
        }
        out.truncate(n_bytes);
        out
    }

    /// One byte (`0`/`1`) per output pin, in output-id order — i.e. component-major, submission
    /// order (D17), pins of component `c` at `comp_out_off[c]..comp_out_off[c+1]`. The
    /// `getOutputs()` binding payload (plan §7.3); a consumer segments it with the per-component
    /// output counts of the board descriptor it submitted. Unpacked (one byte per pin) for direct
    /// per-pin indexing — distinct from the *packed* [`Simulation::link_bytes`].
    pub fn output_bytes(&self) -> Vec<u8> {
        (0..self.output_state.bits())
            .map(|i| self.output_state.get(i) as u8)
            .collect()
    }

    /// Powered value of output pin `pin` of component `comp_id` (submission-order id, D17).
    pub fn output(&self, comp_id: u32, pin: usize) -> bool {
        let oid = self.board.comp_out_off[comp_id as usize] + pin as u32;
        self.output_state.get(oid)
    }

    /// Copy all of component `comp_id`'s output pin values into `out` (up to `out.len()`).
    pub fn copy_outputs(&self, comp_id: u32, out: &mut [bool]) {
        let range = self.board.output_ids(comp_id);
        for (slot, oid) in out.iter_mut().zip(range) {
            *slot = self.output_state.get(oid);
        }
    }

    /// Cooperatively request a running simulation to stop at the next tick boundary.
    pub fn stop(&mut self) {
        if self.state == SimState::Running {
            self.state = SimState::Stopping;
        }
    }
}
