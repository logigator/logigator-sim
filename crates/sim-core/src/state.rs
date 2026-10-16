//! Component state: what a component holds inside, read by the host through one versioned API.
//!
//! Every component has a **state version**, a `u32` that changes (wrapping) whenever its state does
//! and stays 0 for a type without state, so a host re-reads only what moved. The state itself is
//! either **bytes** in a layout fixed per type (RAM: its memory, also writable) or **records** (a
//! recorder's ring). Reads never change state, and between ticks every read is coherent.
//!
//! Only types whose state is not already on their outputs expose any: flip-flops, the LED matrix and
//! the clock show theirs on links, and a ROM's contents are its `ops`.

use core::ops::Range;
use core::sync::atomic::Ordering::Relaxed;

use crate::board::ComponentDescriptor;
use crate::error::{Result, SimError};
use crate::sim::Simulation;
use crate::types::CompType;

/// Most records a recorder keeps (its `capacity`, `ops[0]`).
pub const MAX_RECORDER_CAPACITY: u32 = 1 << 20;

/// Recorder `flags` (`ops[1]`) bit: also record each record's tick. Every other bit must be 0.
pub const RECORD_TICKS: u32 = 1;

/// What a component exposes through the state API. Fixed per type and decodable from the
/// component's descriptor alone ([`StateLayout::of`]), so a host that runs the engine elsewhere can
/// check a request against its own copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateLayout {
    /// No state: the version stays 0, no bytes, no records.
    None,
    /// `len` bytes, read with [`Simulation::read_state`], and written with
    /// [`Simulation::write_state`] when `writable`. A RAM's memory: bit-packed words, LSB-first,
    /// the word at address `a` in bits `[a × w, a × w + w)` (`w` = its output count), bit `b` at
    /// byte `b >> 3`, bit `b & 7`; the last byte zero-padded.
    Bytes { len: usize, writable: bool },
    /// A recorder's records, read with [`Simulation::read_records`]: the last `capacity`, with
    /// their ticks when `ticks`.
    Records { capacity: u32, ticks: bool },
}

/// Bits of memory of a RAM with `ins` inputs and `outs` outputs, `outs × 2^addressSize`; `None` if
/// the pin counts are not a RAM's.
fn ram_bits(ins: usize, outs: usize) -> Option<u64> {
    let addr_size = ins.checked_sub(outs.checked_add(2)?)?;
    (addr_size <= 24).then(|| (outs as u64) << addr_size)
}

impl StateLayout {
    /// The layout of a component as described; [`StateLayout::None`] for a type without state and
    /// for a component that would not compile.
    pub fn of(c: &ComponentDescriptor) -> StateLayout {
        match c.ty {
            CompType::Ram => match ram_bits(c.inputs.len(), c.outputs.len()) {
                Some(bits) => StateLayout::Bytes {
                    len: bits.div_ceil(8) as usize,
                    writable: true,
                },
                None => StateLayout::None,
            },
            CompType::Recorder if c.ops.len() == 2 => StateLayout::Records {
                capacity: c.ops[0],
                ticks: c.ops[1] & RECORD_TICKS != 0,
            },
            _ => StateLayout::None,
        }
    }

    /// Size of the byte state; 0 for a layout without bytes.
    pub fn byte_len(&self) -> usize {
        match *self {
            StateLayout::Bytes { len, .. } => len,
            _ => 0,
        }
    }

    /// Whether the state has a version that can move.
    pub fn is_versioned(&self) -> bool {
        *self != StateLayout::None
    }

    /// Check that component `comp`'s byte state can be read from byte `offset` for `len` bytes, and
    /// return that range.
    pub fn check_read(&self, comp: u32, offset: usize, len: usize) -> Result<Range<usize>> {
        let size = self.byte_len();
        match offset.checked_add(len) {
            Some(end) if end <= size => Ok(offset..end),
            _ => Err(SimError::StateRange {
                comp,
                offset,
                len,
                size,
            }),
        }
    }

    /// Check that component `comp`'s byte state can be written from byte `offset` for `len` bytes,
    /// and return that range.
    pub fn check_write(&self, comp: u32, offset: usize, len: usize) -> Result<Range<usize>> {
        match *self {
            StateLayout::Bytes { writable: true, .. } => self.check_read(comp, offset, len),
            _ => Err(SimError::StateReadOnly(comp)),
        }
    }
}

/// What a [`Simulation::read_records`] call returned: records `first .. first + len` of `count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Records {
    /// Records the recorder has ever written.
    pub count: u64,
    /// Index of the first record copied. Greater than the `since` asked for when the records in
    /// between were overwritten before the read.
    pub first: u64,
    /// Records copied.
    pub len: usize,
}

impl Simulation {
    /// Type of component `comp`, or [`SimError::ComponentOutOfRange`].
    fn comp_type(&self, comp: u32) -> Result<CompType> {
        self.board
            .comp_ty
            .get(comp as usize)
            .copied()
            .ok_or(SimError::ComponentOutOfRange(comp))
    }

    /// Input and output pin counts of component `comp` (in range).
    fn pin_counts(&self, comp: u32) -> (usize, usize) {
        let c = comp as usize;
        let b = &self.board;
        (
            (b.comp_in_off[c + 1] - b.comp_in_off[c]) as usize,
            (b.comp_out_off[c + 1] - b.comp_out_off[c]) as usize,
        )
    }

    /// What component `comp` exposes through the state API; the same as [`StateLayout::of`] its
    /// descriptor.
    pub fn state_layout(&self, comp: u32) -> Result<StateLayout> {
        Ok(match self.comp_type(comp)? {
            CompType::Ram => {
                let (ins, outs) = self.pin_counts(comp);
                let bits = ram_bits(ins, outs).expect("compiled: a RAM's pin counts");
                StateLayout::Bytes {
                    len: bits.div_ceil(8) as usize,
                    writable: true,
                }
            }
            CompType::Recorder => {
                let rec = &self.board.recorders[self.board.config(comp).a as usize];
                StateLayout::Records {
                    capacity: rec.capacity,
                    ticks: rec.ticks.is_some(),
                }
            }
            _ => StateLayout::None,
        })
    }

    /// The state version of component `comp`; 0 for a type without state. A RAM's moves on every
    /// write that changes a stored bit, from the circuit or from [`Simulation::write_state`]; a
    /// recorder's is its record count, truncated.
    pub fn state_version(&self, comp: u32) -> Result<u32> {
        Ok(match self.comp_type(comp)? {
            CompType::Ram => self.scratch.ram_version(self.board.config(comp).b),
            CompType::Recorder => self.scratch.rec_count(self.board.config(comp).a) as u32,
            _ => 0,
        })
    }

    /// Size of component `comp`'s byte state; 0 for a type without byte state.
    pub fn state_len(&self, comp: u32) -> Result<usize> {
        Ok(self.state_layout(comp)?.byte_len())
    }

    /// Copy `out.len()` bytes of component `comp`'s byte state, from byte `offset`, into `out`.
    pub fn read_state(&self, comp: u32, offset: usize, out: &mut [u8]) -> Result<()> {
        let range = self
            .state_layout(comp)?
            .check_read(comp, offset, out.len())?;
        // RAM is the only type with byte state, and its memory is kept in the contract layout.
        let base = self.board.config(comp).a as usize;
        let mem = self.scratch.mem(base + range.start..base + range.end);
        for (o, b) in out.iter_mut().zip(mem) {
            *o = b.load(Relaxed);
        }
        Ok(())
    }

    /// Overwrite `bytes.len()` bytes of component `comp`'s byte state from byte `offset`; only for a
    /// type whose state is writable (RAM). Padding bits past a RAM's last word stay 0.
    ///
    /// A RAM's outputs keep their value until a rising clock reads a word, as after a write from
    /// the circuit to another address. The version moves if a stored bit changed.
    pub fn write_state(&mut self, comp: u32, offset: usize, bytes: &[u8]) -> Result<()> {
        let layout = self.state_layout(comp)?;
        let range = layout.check_write(comp, offset, bytes.len())?;
        let (ins, outs) = self.pin_counts(comp);
        let bits = ram_bits(ins, outs).expect("compiled: a RAM's pin counts");
        // The valid bits of the last byte: all 8 unless the memory ends mid-byte.
        let last = layout.byte_len() - 1;
        let last_mask = match bits % 8 {
            0 => 0xFF,
            n => (1u8 << n) - 1,
        };
        let base = self.board.config(comp).a as usize;
        let mem = self.scratch.mem(base + range.start..base + range.end);
        let mut changed = false;
        for (i, (m, &b)) in mem.iter().zip(bytes).enumerate() {
            let b = if range.start + i == last {
                b & last_mask
            } else {
                b
            };
            changed |= m.swap(b, Relaxed) != b;
        }
        if changed {
            self.scratch.bump_ram_version(self.board.config(comp).b);
        }
        Ok(())
    }

    /// Copy recorder `comp`'s records from index `since` on, oldest first, into `values` (and their
    /// ticks into `ticks`, which must be `Some` exactly when the recorder records ticks).
    ///
    /// Copying starts at `first = max(since, count − capacity)`: if `first > since`, the
    /// `first − since` records in between were overwritten before the read. It copies
    /// `len = min(count − first, values.len())` records (and no more than `ticks` holds), so a reader
    /// with a small buffer pages through by passing `first + len` as the next `since`. A `since`
    /// past `count` is an error: it is a count from another simulation.
    pub fn read_records(
        &self,
        comp: u32,
        since: u64,
        values: &mut [u32],
        mut ticks: Option<&mut [u64]>,
    ) -> Result<Records> {
        if self.comp_type(comp)? != CompType::Recorder {
            return Err(SimError::NotARecorder(comp));
        }
        let rec = self.board.config(comp).a;
        let cfg = &self.board.recorders[rec as usize];
        if cfg.ticks.is_some() != ticks.is_some() {
            return Err(SimError::RecordTicks {
                comp,
                records_ticks: cfg.ticks.is_some(),
            });
        }
        let count = self.scratch.rec_count(rec);
        if since > count {
            return Err(SimError::RecordsAhead { comp, since, count });
        }
        let first = since.max(count.saturating_sub(cfg.capacity as u64));
        let mut len = ((count - first) as usize).min(values.len());
        if let Some(t) = &ticks {
            len = len.min(t.len());
        }
        for (i, v) in values[..len].iter_mut().enumerate() {
            let (value, tick) = self.scratch.record_at(cfg, first + i as u64);
            *v = value;
            if let (Some(t), Some(tick)) = (ticks.as_deref_mut(), tick) {
                t[i] = tick;
            }
        }
        Ok(Records { count, first, len })
    }
}
