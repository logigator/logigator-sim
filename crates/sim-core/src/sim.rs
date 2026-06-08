//! The owned `Simulation` object: run state, construction (init-seeding + first-read priming), user
//! input, and coherent state accessors (plan §5.2, §6.1, §6.3). The tick mechanics live in
//! [`crate::tick`].

use crate::BoardDescriptor;
use crate::bitset::BitSet;
use crate::board::Board;
use crate::components::{self, N_TYPES, TickCtx};
use crate::error::{Result, SimError};
use crate::types::{CompType, InputEvent, SimState};
use core::sync::atomic::AtomicU16;
use std::time::{Duration, Instant};

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
    /// Precomputed `type_index(comp_ty[c])` for O(1) enqueue in the read phase.
    pub(crate) comp_ty_index: Box<[u8]>,

    /// Subscribed `UserInput` one-shot pulses, drained in the between-tick section.
    pub(crate) ui_pending: Vec<UiPulse>,

    // --- bookkeeping ---
    pub(crate) link_count: u32,
    pub(crate) comp_count: u32,
    pub(crate) state: SimState,
    pub(crate) tick: u64,
    pub(crate) speed: u32,
    pub(crate) last_capture: Instant,
    pub(crate) last_capture_tick: u64,
}

impl Simulation {
    /// Build a simulation from a compiled board: allocate zeroed state, replay every component's
    /// power-on init through the runtime flip→count→push path, then prime one read boundary so the
    /// first `tick()` matches the reference engine (plan §6.1 steps 3–5, invariant I5).
    pub fn new(board: Board) -> Result<Self> {
        let link_count = board.link_count;
        let comp_count = board.comp_count;
        let output_count = board.output_count;

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

        let mut sim = Simulation {
            board,
            link_state: BitSet::new(link_count),
            output_state: BitSet::new(output_count),
            driver_count,
            read_buf: Vec::new(),
            write_buf: Vec::new(),
            compute_queue: (0..N_TYPES).map(|_| Vec::new()).collect(),
            comp_ty_index,
            ui_pending: Vec::new(),
            link_count,
            comp_count,
            state: SimState::Stopped,
            tick: 0,
            speed: 0,
            last_capture: Instant::now(),
            last_capture_tick: 0,
        };
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
    pub(crate) fn make_ctx(&mut self) -> TickCtx<'_> {
        TickCtx::new(
            &self.board.comp_in_off,
            &self.board.comp_inputs,
            &self.board.comp_out_off,
            &self.board.output_link,
            &self.link_state,
            &self.output_state,
            &self.driver_count,
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
            parallel: false, // single-threaded until plan phase 6
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
