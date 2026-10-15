//! Timed input: a FIFO of **frames** per `UserInput` component, sequenced inside the tick.
//!
//! The host submits frames between ticks ([`Simulation::submit_input`]); the engine applies them at
//! the start of a tick, before the read phase, so a frame is on the input's pins *and* wires after
//! the first tick following its submission — the same one-tick delay every other component has. Each
//! frame stays on the pins for at least its `hold` ticks before the input's next frame replaces it.
//! Because the sequencing is counted in ticks inside the engine, the trace is the same whether the
//! ticks run one `tick()` at a time or in `run()` batches of any size.
//!
//! A frame's `state` holds one entry per output pin: `Some(v)` drives the pin's **logical** value
//! (a pin listed in `negOutputs` drives the inverse onto its wire, like every other output), `None`
//! keeps whatever the pin shows when the frame is applied. A frame is either an **event** (never
//! dropped or merged by the engine) or **coalescing**: a coalescing frame merges per pin into a
//! pending coalescing tail, so a level changed several times between two ticks reaches the circuit
//! once, in its final state. `replace` drops an input's pending frames and any running hold before
//! queueing its own.

use crate::components::TickCtx;
use crate::error::{Result, SimError};
use crate::sim::Simulation;
use crate::types::CompType;
use std::collections::{HashSet, VecDeque};

/// Most event frames one input may hold pending (about 32 000 characters of pasted text at two
/// frames per character). A write that would exceed it is refused whole; a host sending more
/// submits it in chunks as [`Simulation::input_progress`] drains.
///
/// Only event frames count: a coalescing frame stays separate only when an event frame follows it,
/// so an input never holds more than `2 × MAX_PENDING_EVENTS + 1` frames in total.
pub const MAX_PENDING_EVENTS: u32 = 65_536;

/// One timed step of an input's pins.
#[derive(Clone, Copy, Debug)]
pub struct InputFrame<'a> {
    /// Exactly one entry per output pin of the input: `Some(v)` drives the pin's logical value,
    /// `None` keeps the value the pin shows when the frame is applied.
    pub state: &'a [Option<bool>],
    /// Minimum number of ticks the frame stays on the pins before the input's next frame replaces
    /// it. Must be at least 1.
    pub hold: u32,
    /// `false`: an event, never dropped or merged by the engine. `true`: merges into the input's
    /// last pending frame when that one is coalescing too.
    pub coalesce: bool,
}

/// Frames for one input, as part of a [`Simulation::submit_input`] request.
#[derive(Clone, Copy, Debug)]
pub struct InputWrite<'a> {
    /// Component id of a `UserInput`.
    pub comp: u32,
    /// Drop the input's pending frames and end any running hold before queueing `frames`.
    pub replace: bool,
    pub frames: &'a [InputFrame<'a>],
}

impl InputWrite<'_> {
    /// Number of event (non-coalescing) frames in this write — the unit of progress and of the
    /// queue limit.
    pub fn event_count(&self) -> usize {
        self.frames.iter().filter(|f| !f.coalesce).count()
    }
}

/// What request validation needs to know about one input.
#[derive(Clone, Copy, Debug)]
pub struct InputInfo {
    /// Number of output pins.
    pub width: u32,
    /// Event frames pending on the input (an upper bound is fine: the limit check then refuses
    /// early but never lets the queue exceed [`MAX_PENDING_EVENTS`]).
    pub pending_events: u32,
}

/// The all-or-nothing check [`Simulation::submit_input`] runs before queueing anything. `info`
/// maps a component id to its input's [`InputInfo`], or `None` if the component is not a
/// `UserInput`.
///
/// Exposed so a host that queues requests for an engine running elsewhere (the Node binding's
/// worker thread) can refuse a bad request synchronously, from its own copy of the input widths
/// and a pending count that never under-reports.
pub fn validate_input_writes(
    writes: &[InputWrite<'_>],
    mut info: impl FnMut(u32) -> Option<InputInfo>,
) -> Result<()> {
    // Refused because a `replace` on the second entry would silently drop the first entry's frames.
    let mut seen = HashSet::with_capacity(writes.len());
    for w in writes {
        let input = info(w.comp).ok_or(SimError::NotAnInput(w.comp))?;
        if !seen.insert(w.comp) {
            return Err(SimError::DuplicateInput(w.comp));
        }
        for (frame, f) in w.frames.iter().enumerate() {
            if f.state.len() != input.width as usize {
                return Err(SimError::InputWidth {
                    comp: w.comp,
                    frame,
                    got: f.state.len(),
                    want: input.width,
                });
            }
            if f.hold == 0 {
                return Err(SimError::InputHold {
                    comp: w.comp,
                    frame,
                });
            }
        }
        let pending = if w.replace { 0 } else { input.pending_events };
        let adding = w.event_count();
        if pending as usize + adding > MAX_PENDING_EVENTS as usize {
            return Err(SimError::InputQueueFull {
                comp: w.comp,
                pending,
                adding,
                limit: MAX_PENDING_EVENTS,
            });
        }
    }
    Ok(())
}

/// Per-frame scalars; the pin bits live alongside in [`InputQueue::bits`].
#[derive(Clone, Copy, Debug)]
struct FrameMeta {
    hold: u32,
    coalesce: bool,
}

/// One `UserInput`'s pending frames and timing.
#[derive(Debug)]
pub(crate) struct InputQueue {
    /// The input's component id.
    comp: u32,
    /// Output pin count.
    width: u32,
    /// `u64` words per mask (and per value): `ceil(width / 64)`.
    words: usize,
    /// Unapplied frames, oldest first. A frame leaves the queue when it is applied.
    meta: VecDeque<FrameMeta>,
    /// Packed pins of the frames in `meta`: per frame, `words` mask words (bit set ⟺ the frame
    /// drives that pin; clear = keep) followed by `words` value words (logical values).
    bits: VecDeque<u64>,
    /// Earliest tick the next frame may be applied on: the previous frame's apply tick + its hold.
    ready_at: u64,
    /// Event frames in `meta`.
    pending_events: u32,
}

/// Pack up to 64 pin entries into `(mask, value)` words.
fn pack(chunk: &[Option<bool>]) -> (u64, u64) {
    let mut mask = 0u64;
    let mut value = 0u64;
    for (i, s) in chunk.iter().enumerate() {
        if let Some(v) = *s {
            mask |= 1 << i;
            value |= (v as u64) << i;
        }
    }
    (mask, value)
}

impl InputQueue {
    fn new(comp: u32, width: u32) -> Self {
        InputQueue {
            comp,
            width,
            words: (width as usize).div_ceil(64),
            meta: VecDeque::new(),
            bits: VecDeque::new(),
            ready_at: 0,
            pending_events: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    /// Drop every pending frame and end any running hold.
    fn clear(&mut self) {
        self.meta.clear();
        self.bits.clear();
        self.pending_events = 0;
        self.ready_at = 0;
    }

    /// Append a validated frame, merging it into a pending coalescing tail when it coalesces too.
    fn enqueue(&mut self, f: &InputFrame<'_>) {
        let w = self.words;
        if f.coalesce && self.meta.back().is_some_and(|m| m.coalesce) {
            // Per pin, the later value wins; a `None` keeps the earlier frame's entry.
            let base = self.bits.len() - 2 * w;
            for (wi, chunk) in f.state.chunks(64).enumerate() {
                let (mask, value) = pack(chunk);
                self.bits[base + wi] |= mask;
                let v = &mut self.bits[base + w + wi];
                *v = (*v & !mask) | value;
            }
            if let Some(m) = self.meta.back_mut() {
                m.hold = f.hold;
            }
            return;
        }
        let base = self.bits.len();
        self.bits.resize(base + 2 * w, 0);
        for (wi, chunk) in f.state.chunks(64).enumerate() {
            let (mask, value) = pack(chunk);
            self.bits[base + wi] = mask;
            self.bits[base + w + wi] = value;
        }
        self.meta.push_back(FrameMeta {
            hold: f.hold,
            coalesce: f.coalesce,
        });
        if !f.coalesce {
            self.pending_events += 1;
        }
    }

    /// Pop the head frame and drive its masked pins at tick `n`. `ctx` writes into the read buffer
    /// of the tick being run, so the read phase that follows picks the changed links up.
    fn apply_head(&mut self, n: u64, ctx: &mut TickCtx<'_>) {
        let Some(meta) = self.meta.pop_front() else {
            return;
        };
        let w = self.words;
        let first = ctx.first_output(self.comp);
        for wi in 0..w {
            let mut mask = self.bits[wi];
            let value = self.bits[w + wi];
            while mask != 0 {
                let b = mask.trailing_zeros();
                ctx.set_output(first + (wi * 64) as u32 + b, (value >> b) & 1 == 1);
                mask &= mask - 1;
            }
        }
        self.bits.drain(..2 * w);
        self.ready_at = n + meta.hold as u64;
        if !meta.coalesce {
            self.pending_events -= 1;
        }
    }
}

impl Simulation {
    /// One queue per `UserInput`, in component-id order: input `i` is the `i`-th `UserInput` of the
    /// descriptor.
    pub(crate) fn build_input_queues(&self) -> Vec<InputQueue> {
        (0..self.comp_count)
            .filter(|&c| self.board.comp_ty[c as usize] == CompType::UserInput)
            .map(|c| InputQueue::new(c, self.board.output_ids(c).len() as u32))
            .collect()
    }

    /// Dense input index of component `comp`, or `None` if it is not a `UserInput`.
    fn input_index(&self, comp: u32) -> Option<usize> {
        self.inputs.binary_search_by_key(&comp, |q| q.comp).ok()
    }

    /// Number of `UserInput` components — the length [`Simulation::input_progress`] fills.
    pub fn input_count(&self) -> u32 {
        self.inputs.len() as u32
    }

    /// Queue frames on one or more inputs.
    ///
    /// The whole request is validated first ([`validate_input_writes`]); on any error nothing is
    /// queued. A frame submitted to an input with nothing pending and no running hold is applied on
    /// the next tick and shows on the pins and wires after it. Each later frame is applied once its
    /// predecessor's `hold` has run out. Frames of different inputs due on the same tick are applied
    /// together, in ascending component id.
    pub fn submit_input(&mut self, writes: &[InputWrite<'_>]) -> Result<()> {
        validate_input_writes(writes, |comp| {
            self.input_index(comp).map(|i| InputInfo {
                width: self.inputs[i].width,
                pending_events: self.inputs[i].pending_events,
            })
        })?;
        for w in writes {
            let i = self
                .input_index(w.comp)
                .expect("validated: comp is a UserInput");
            let q = &mut self.inputs[i];
            if w.replace {
                q.clear();
            }
            for f in w.frames {
                q.enqueue(f);
            }
            // Keep `active` sorted by input index (= component id order). An input whose queue
            // empties leaves it even while a hold runs; `ready_at` covers that hold.
            let pending = !q.is_empty();
            match self.active.binary_search(&(i as u32)) {
                Ok(pos) if !pending => {
                    self.active.remove(pos);
                }
                Err(pos) if pending => self.active.insert(pos, i as u32),
                _ => {}
            }
        }
        Ok(())
    }

    /// Fill `out[i]` with the number of input `i`'s event frames that are not yet applied. A frame
    /// whose hold is still running is not pending, and coalescing frames are never counted.
    ///
    /// # Panics
    ///
    /// If `out` is shorter than [`Simulation::input_count`].
    pub fn input_progress(&self, out: &mut [u32]) {
        for (o, q) in out[..self.inputs.len()].iter_mut().zip(&self.inputs) {
            *o = q.pending_events;
        }
    }

    /// Apply every active input's head frame whose `ready_at` has come. Runs at the start of the
    /// tick, before the read phase; only called while `active` is non-empty.
    ///
    /// `#[cold]` keeps the call out of the tick loop's hot path: without it, a board that never
    /// submits input ran ~15% slower when its per-tick work is only a handful of links.
    #[cold]
    #[inline(never)]
    pub(crate) fn service_inputs(&mut self) {
        let n = self.tick + 1;
        // Schedule into `read_buf`, not `write_buf`: the read phase of this same tick re-reads the
        // links, so the input reaches its wires one tick after submission, like any other driver.
        let mut ctx = TickCtx::new(
            &self.board,
            &self.link_state,
            &self.output_state,
            &self.driver_count,
            &self.scratch,
            self.tick,
            &mut self.read_buf,
        );
        let mut keep = 0;
        for k in 0..self.active.len() {
            let i = self.active[k];
            let q = &mut self.inputs[i as usize];
            if n >= q.ready_at {
                q.apply_head(n, &mut ctx);
            }
            if !q.is_empty() {
                self.active[keep] = i;
                keep += 1;
            }
        }
        self.active.truncate(keep);
    }
}

#[cfg(test)]
impl Simulation {
    /// Test shorthand for a level change: queue one coalescing frame that drives every pin of
    /// input `comp` to `state`.
    pub(crate) fn set_input(&mut self, comp: u32, state: &[bool]) -> Result<()> {
        let state: Vec<Option<bool>> = state.iter().map(|&v| Some(v)).collect();
        self.submit_input(&[InputWrite {
            comp,
            replace: false,
            frames: &[InputFrame {
                state: &state,
                hold: 1,
                coalesce: true,
            }],
        }])
    }
}
