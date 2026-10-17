//! Board description, builder, and the compile step that lowers a board to struct-of-arrays with
//! CSR adjacency.
//!
//! Public component ids are the **submission order** the caller used, and the internal layout keeps
//! that same order. (A locality-renumbering pass was tried and reverted as perf-neutral; any future
//! renumbering must slot in behind a translation table without changing this contract.)

use crate::CompType;
use crate::bitset::BitSet;
use crate::components;
use crate::error::{PinKind, Result, SimError};
use crate::state::MAX_RECORDER_CAPACITY;

/// Negate flag packed into the top bit of a link-id word. Output negation lives here in
/// `output_link`, read for free alongside the link id the drive step already loads — no separate
/// negate-bitset load on the hot path. A no-negation board leaves every top bit clear, so masking
/// is the `& LINK_ID_MASK` identity and behaviour is byte-identical.
pub(crate) const LINK_NEG_BIT: u32 = 1 << 31;
/// Recovers the link id from a packed word (clears [`LINK_NEG_BIT`]).
pub(crate) const LINK_ID_MASK: u32 = !LINK_NEG_BIT;

/// A clock's waveform (`mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum ClkMode {
    /// A one-tick pulse after `period` ticks low.
    Pulse,
    /// `period` ticks low then `period` ticks high.
    #[default]
    Square,
}

/// When a RAM's outputs pick up a word (`read`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "lowercase")
)]
pub enum RamRead {
    /// The outputs follow the addressed word; a write waits for a rising clock.
    #[default]
    Async,
    /// The legacy mode: reads also wait for a rising clock, and the outputs keep the last word
    /// read or written until the next one.
    Clocked,
}

/// One component in a board description. `ty` is the [`CompType`] wire id; input/output entries
/// are link ids; `neg_inputs` and `neg_outputs` are *pin indices* (into `inputs`/`outputs`), not
/// link ids — each listed pin reads (input) or drives (output) the inverted value, folded into the
/// read/drive steps with no added delay.
///
/// The optional fields after them are per-type parameters, each taken by one type only:
/// - CLK: `period` (required), the ticks low per cycle, and `mode` (default [`ClkMode::Square`]).
/// - RAM: `read` (default [`RamRead::Async`]).
/// - ROM: `data`, the contents as bit-packed words, LSB-first: the word at address `a` in bits
///   `[a·w, a·w + w)` (`w` = its output count), bit `b` at byte `b >> 3`, bit `b & 7`. Shorter
///   data is zero-filled; absent data is all zeros.
/// - Recorder: `capacity` (required), the records kept, and `record_ticks` (default `false`).
/// - Addressable register: `data_bus` (required), the data-bus width.
///
/// The fields hold what arrives from JSON, JS or `.lgb`, unvalidated: `compile` checks the type id,
/// link ids, arity, pin indices and parameters once, for every way a board comes in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields, rename_all = "camelCase")
)]
pub struct ComponentDescriptor {
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub ty: u32,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub inputs: Vec<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub outputs: Vec<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub neg_inputs: Vec<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub neg_outputs: Vec<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub period: Option<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub mode: Option<ClkMode>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub read: Option<RamRead>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub data: Option<Vec<u8>>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub capacity: Option<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub record_ticks: Option<bool>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub data_bus: Option<u32>,
}

impl ComponentDescriptor {
    /// A component with no negated pins and no parameters; set those with struct-update syntax.
    pub fn new(ty: CompType, inputs: &[u32], outputs: &[u32]) -> Self {
        ComponentDescriptor {
            ty: ty.id(),
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
            ..Default::default()
        }
    }

    /// The parameters it carries, by their JSON names.
    fn params(&self) -> impl Iterator<Item = &'static str> {
        [
            ("period", self.period.is_some()),
            ("mode", self.mode.is_some()),
            ("read", self.read.is_some()),
            ("data", self.data.is_some()),
            ("capacity", self.capacity.is_some()),
            ("recordTicks", self.record_ticks.is_some()),
            ("dataBus", self.data_bus.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
    }
}

/// A board: its components, which name the links they connect by id. The links are numbered
/// `0..=` the highest id any pin uses; an id no pin uses is a link nothing touches. The single
/// public board shape, consumed by `Board::compile` and every binding's constructor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct BoardDescriptor {
    pub components: Vec<ComponentDescriptor>,
}

/// Programmatic board construction.
#[derive(Default)]
pub struct BoardBuilder {
    components: Vec<ComponentDescriptor>,
}

impl BoardBuilder {
    /// Start a board with no components.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a component with no negated pins and no parameters; returns its (public,
    /// submission-order) component id.
    pub fn component(&mut self, ty: CompType, inputs: &[u32], outputs: &[u32]) -> u32 {
        self.add(ComponentDescriptor::new(ty, inputs, outputs))
    }

    /// Append a component, marking the listed input/output *pin indices* as negated; returns its
    /// (public, submission-order) component id.
    pub fn component_neg(
        &mut self,
        ty: CompType,
        inputs: &[u32],
        outputs: &[u32],
        neg_inputs: &[u32],
        neg_outputs: &[u32],
    ) -> u32 {
        self.add(ComponentDescriptor {
            neg_inputs: neg_inputs.to_vec(),
            neg_outputs: neg_outputs.to_vec(),
            ..ComponentDescriptor::new(ty, inputs, outputs)
        })
    }

    /// Append a component as described; returns its (public, submission-order) component id.
    pub fn add(&mut self, component: ComponentDescriptor) -> u32 {
        let id = self.components.len() as u32;
        self.components.push(component);
        id
    }

    /// Finish into a `BoardDescriptor`.
    pub fn finish(self) -> BoardDescriptor {
        BoardDescriptor {
            components: self.components,
        }
    }
}

/// Per-component compiled configuration: the constant parameters derived from the input/output
/// array lengths and the per-type parameters, captured once at compile time. Two `u32` slots reused per component type:
///
/// - `a`: ROM (12) → byte offset of its data blob in [`Board::rom_data`]; RAM (17) → byte offset
///   into the simulation `mem` scratch pool; MUX (20) → number of select bits (derived from the
///   input count); encoder (19) → number of number bits (its valid output, if any, follows them); CLK (6) → ticks
///   low per cycle; addressable register (204) → data-bus width; recorder (201) → its index in
///   [`Board::recorders`].
/// - `b`: addressable register (204) → address-bus length; RAM (17) → its index among the board's RAMs (the
///   slot of its state version); CLK (6) → ticks high per cycle (1 for a pulse, the period for a
///   square wave).
///
/// Both are `0` for purely combinational/parameterless types (gates, adders, DEC/DEMUX, FFs).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompConfig {
    pub a: u32,
    pub b: u32,
}

/// A recorder's (201) rings in the simulation's recorder arenas.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecorderConfig {
    /// Records kept: the ring length.
    pub capacity: u32,
    /// Offset of its value ring in the value arena.
    pub values: usize,
    /// Offset of its tick ring in the tick arena, if it records ticks.
    pub ticks: Option<usize>,
}

/// What compiling a board allocates beside the per-component [`CompConfig`]s.
#[derive(Default)]
struct Pools {
    /// Concatenated ROM data blobs.
    rom_data: Vec<u8>,
    /// Bytes of RAM backing store.
    ram_bytes: u32,
    /// RAMs so far.
    ram_count: u32,
    /// Per RAM, by index: whether it reads only on a clock edge (the legacy mode).
    ram_clocked_read: Vec<bool>,
    /// Recorders so far.
    recorders: Vec<RecorderConfig>,
    /// Slots of the recorder value arena.
    rec_values: usize,
    /// Slots of the recorder tick arena.
    rec_ticks: usize,
}

/// `ceil(log2(n))` for `n >= 1` (0 for `n <= 1`), computed with integers to avoid the rounding
/// hazard of a float `log2`. Used for the addressable register's address-bus width.
fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        u32::BITS - (n - 1).leading_zeros()
    }
}

/// A compiled board: immutable topology in SoA + CSR form. Shared read-only by the
/// tick loop.
///
/// # Invariants
///
/// Only [`Board::compile`] builds a board and nothing changes it afterwards, so the tick loop
/// indexes by these without bounds checks (each such read names the one it relies on):
///
/// - **Link ids.** Every link id in `comp_inputs` and `output_link`, with [`LINK_NEG_BIT`] masked
///   off, is below `link_count`, the length of every per-link array and bitset (`link_off` has one
///   more entry).
/// - **CSR offsets.** `comp_in_off`, `comp_out_off` and both columns of `link_off` start at 0,
///   never decrease, and end at the length of the array they slice. A link's consumer groups
///   partition its consumer slice, and every group's type index is below `N_TYPES`.
///
/// `debug_check` asserts all of them at the end of every compile in debug builds.
#[derive(Debug)]
pub struct Board {
    pub(crate) link_count: u32,
    pub(crate) comp_count: u32,
    /// Total number of output pins across all components (sizes `output_state`).
    pub(crate) output_count: u32,
    /// Component type per internal id.
    pub(crate) comp_ty: Box<[CompType]>,
    /// Per-component compiled configuration (see [`CompConfig`]).
    pub(crate) comp_config: Box<[CompConfig]>,
    /// Concatenated immutable ROM (type 12) data blobs; each ROM's slice starts at `config.a`.
    pub(crate) rom_data: Box<[u8]>,
    /// Total bytes of RAM (type 17) backing store across the board; sizes the `mem` scratch pool.
    pub(crate) ram_bytes: u32,
    /// Number of RAMs; sizes the RAM state versions.
    pub(crate) ram_count: u32,
    /// Per RAM, by its index (`config.b`): whether it reads only on a rising clock (the legacy
    /// mode, `read: "clocked"`) instead of following its address.
    pub(crate) ram_clocked_read: Box<[bool]>,
    /// Every recorder (201), indexed by its `config.a`.
    pub(crate) recorders: Box<[RecorderConfig]>,
    /// Slots of the recorder value and tick arenas.
    pub(crate) rec_values: usize,
    pub(crate) rec_ticks: usize,
    /// CSR: input links per component. `comp_inputs[comp_in_off[c] + i]` is the link read by input
    /// pin `i` of component `c`, with the negate flag packed into its top bit ([`LINK_NEG_BIT`]):
    /// set ⟺ the pin reads the inverted link value. The read step recovers both from one load; mask
    /// with [`LINK_ID_MASK`] for the link id, or use [`Board::input_negated`].
    pub(crate) comp_in_off: Box<[u32]>,
    pub(crate) comp_inputs: Box<[u32]>,
    /// CSR: outputs per component are dense & contiguous. `output_link[oid]` is the driven link id
    /// with the negate flag packed into its top bit ([`LINK_NEG_BIT`]): set ⟺ output pin `oid`
    /// drives the inverted computed value. Mask with [`LINK_ID_MASK`] for the link id, or use
    /// [`Board::output_link_id`] / [`Board::output_negated`].
    pub(crate) comp_out_off: Box<[u32]>,
    pub(crate) output_link: Box<[u32]>,
    /// CSR: components that read each link (built from the inputs above). Within each link's slice
    /// the consumers are sorted by `type_index` (stable), so same-type consumers form contiguous
    /// runs the read phase can bulk-enqueue.
    pub(crate) link_consumers: Box<[u32]>,
    /// Side CSR over `link_consumers`: for each link, the runs of same-type consumers as
    /// `(type_index, len)` pairs (len relative to the link's consumer slice). A typical link has one
    /// group, so this lets the read phase enqueue a whole run with one `extend_from_slice` instead
    /// of a per-consumer type lookup + push.
    pub(crate) consumer_groups: Box<[(u8, u32)]>,
    /// The offsets of both CSRs above, interleaved per link: `[consumers, groups]`. Link `l`'s
    /// slices are bounded by entries `l` and `l + 1`, so a flipped link's lookups share one cache
    /// line instead of touching two offset arrays.
    pub(crate) link_off: Box<[[u32; 2]]>,
    /// Links that can be invalid: driven by two or more outputs (which can disagree) or by an SR
    /// flip-flop (which goes invalid when set and reset together). Only these are checked in the
    /// read phase, so a board without either pays one bit test per changed link.
    pub(crate) invalid_watch: BitSet,
    /// The [`invalid_watch`](Self::invalid_watch) links, ascending.
    pub(crate) watched_links: Box<[u32]>,
    /// Outputs driving each link (≤ `u16::MAX`, checked at compile).
    pub(crate) driver_total: Box<[u16]>,
}

impl Board {
    /// Validate and lower a board description to SoA + CSR.
    ///
    /// Validation — the only validation a board gets, whichever way it came in: each component's
    /// type id names an implemented type, every input/output link id is below `2^31`, the
    /// input/output counts satisfy the type's arity, every negated pin index addresses a pin, the
    /// parameters are the type's own and in range, and no link has more drivers than its `u16`
    /// driver count holds. Errors are reported against the public (submission-order) component
    /// index.
    pub fn compile(desc: &BoardDescriptor) -> Result<Board> {
        // One past the highest link id any pin uses.
        let mut link_count = 0u32;
        let comp_count = desc.components.len() as u32;

        // --- validate + size CSR offsets ---
        let mut comp_in_off = Vec::with_capacity(desc.components.len() + 1);
        let mut comp_out_off = Vec::with_capacity(desc.components.len() + 1);
        comp_in_off.push(0u32);
        comp_out_off.push(0u32);
        let mut in_total: u32 = 0;
        let mut out_total: u32 = 0;
        let mut comp_ty = Vec::with_capacity(desc.components.len());
        let mut comp_config = Vec::with_capacity(desc.components.len());
        let mut pools = Pools::default();

        for (i, c) in desc.components.iter().enumerate() {
            let idx = i as u32;
            let ty =
                CompType::from_id(c.ty).ok_or(SimError::UnknownComponentType { idx, ty: c.ty })?;
            for &l in c.inputs.iter().chain(c.outputs.iter()) {
                // Link ids reserve their top bit for the packed negate flag.
                if l >= LINK_NEG_BIT {
                    return Err(SimError::LinkIdOverflow {
                        idx,
                        link: l,
                        max: LINK_NEG_BIT - 1,
                    });
                }
                link_count = link_count.max(l + 1);
            }
            if !components::arity(ty).accepts(c.inputs.len(), c.outputs.len()) {
                return Err(SimError::BadArity {
                    idx,
                    ty,
                    ins: c.inputs.len(),
                    outs: c.outputs.len(),
                });
            }
            if let Some(param) = c.params().find(|p| !components::params(ty).contains(p)) {
                return Err(SimError::BadParam {
                    idx,
                    ty,
                    param,
                    reason: "is not a parameter of this type",
                });
            }
            // Negated pin indices address pins, not links; validate each against the component's
            // pin counts.
            for &p in &c.neg_inputs {
                if p as usize >= c.inputs.len() {
                    return Err(SimError::NegateOutOfRange {
                        idx,
                        pin_kind: PinKind::Input,
                        pin: p,
                        count: c.inputs.len() as u32,
                    });
                }
            }
            for &p in &c.neg_outputs {
                if p as usize >= c.outputs.len() {
                    return Err(SimError::NegateOutOfRange {
                        idx,
                        pin_kind: PinKind::Output,
                        pin: p,
                        count: c.outputs.len() as u32,
                    });
                }
            }
            comp_config.push(Self::configure(idx, ty, c, &mut pools)?);
            comp_ty.push(ty);
            // Checked: the offsets index the pin arrays unchecked, so they must not wrap. A board
            // needs 4 Gi pins to get here, far past what fits in memory.
            in_total = in_total
                .checked_add(c.inputs.len() as u32)
                .expect("input pin count fits in u32");
            out_total = out_total
                .checked_add(c.outputs.len() as u32)
                .expect("output pin count fits in u32");
            comp_in_off.push(in_total);
            comp_out_off.push(out_total);
        }

        // --- input CSR + output->link map (submission order) ---
        let mut comp_inputs = Vec::with_capacity(in_total as usize);
        let mut output_link = Vec::with_capacity(out_total as usize);
        for c in &desc.components {
            comp_inputs.extend_from_slice(&c.inputs);
            output_link.extend_from_slice(&c.outputs);
        }

        // --- drivers per link: the runtime keeps a `u16` count of a link's high drivers, which
        // must not wrap ---
        let mut drivers = vec![0u32; link_count as usize];
        for &l in &output_link {
            drivers[l as usize] += 1;
        }
        if let Some((link, &count)) = drivers
            .iter()
            .enumerate()
            .find(|&(_, &n)| n > u16::MAX as u32)
        {
            return Err(SimError::TooManyDrivers {
                link: link as u32,
                count,
                max: u16::MAX as u32,
            });
        }

        // --- per-pin negation, packed into the top bit of the link-id word ---
        // A negated pin sets the top bit of its link-id slot — `comp_inputs` for an input pin,
        // `output_link` for an output pin — so the read and drive steps recover the flag from the
        // same word they already load for the link id, with no separate negate-bitset load. A
        // no-negation board leaves every top bit clear.
        for (ci, c) in desc.components.iter().enumerate() {
            let in_base = comp_in_off[ci] as usize;
            for &p in &c.neg_inputs {
                comp_inputs[in_base + p as usize] |= LINK_NEG_BIT;
            }
            let out_base = comp_out_off[ci] as usize;
            for &p in &c.neg_outputs {
                output_link[out_base + p as usize] |= LINK_NEG_BIT;
            }
        }

        // --- links that can be invalid: several drivers, or an SR flip-flop's output ---
        let invalid_watch = BitSet::new(link_count);
        for (l, &n) in drivers.iter().enumerate() {
            if n >= 2 {
                invalid_watch.set(l as u32, true);
            }
        }
        for (ci, c) in desc.components.iter().enumerate() {
            if comp_ty[ci] == CompType::SrFf {
                for &l in &c.outputs {
                    invalid_watch.set(l, true);
                }
            }
        }
        let watched_links: Vec<u32> = (0..link_count).filter(|&l| invalid_watch.get(l)).collect();

        // --- consumer CSR: for each link, the components that read it ---
        // counts[l] = #components with l as an input
        let mut off = vec![0u32; link_count as usize + 1];
        for c in &desc.components {
            for &l in &c.inputs {
                off[l as usize + 1] += 1;
            }
        }
        for i in 0..link_count as usize {
            off[i + 1] += off[i];
        }
        let total_refs = *off.last().unwrap_or(&0);
        let ty_index = |c: u32| components::type_index(comp_ty[c as usize]) as u8;
        // Fill each link's slice in (type_index, component id) order, so it comes out grouped by
        // type with submission order kept within a type: visit the components in that order (a
        // counting sort over the few types) instead of sorting every slice afterwards.
        let mut type_start = [0u32; components::N_TYPES + 1];
        for ci in 0..comp_count {
            type_start[ty_index(ci) as usize + 1] += 1;
        }
        for t in 0..components::N_TYPES {
            type_start[t + 1] += type_start[t];
        }
        let mut by_type = vec![0u32; comp_count as usize];
        for ci in 0..comp_count {
            let slot = &mut type_start[ty_index(ci) as usize];
            by_type[*slot as usize] = ci;
            *slot += 1;
        }
        let mut link_consumers = vec![0u32; total_refs as usize];
        let mut cursor = off.clone();
        for &ci in &by_type {
            for &l in &desc.components[ci as usize].inputs {
                let slot = &mut cursor[l as usize];
                link_consumers[*slot as usize] = ci;
                *slot += 1;
            }
        }

        // --- consumer groups: collapse each link's same-type runs into (type_index, len) pairs.
        // The read phase walks these groups and bulk-enqueues each run, dropping the per-consumer
        // type lookup.
        let mut consumer_groups_off = Vec::with_capacity(link_count as usize + 1);
        let mut consumer_groups: Vec<(u8, u32)> = Vec::new();
        consumer_groups_off.push(0u32);
        for l in 0..link_count as usize {
            let start = off[l] as usize;
            let end = off[l + 1] as usize;
            let slice = &link_consumers[start..end];
            let mut i = 0;
            while i < slice.len() {
                let ti = ty_index(slice[i]);
                let run_start = i;
                while i < slice.len() && ty_index(slice[i]) == ti {
                    i += 1;
                }
                consumer_groups.push((ti, (i - run_start) as u32));
            }
            consumer_groups_off.push(consumer_groups.len() as u32);
        }

        // Each group's members all share its type, and the groups partition every consumer slice
        // (the read phase relies on both to enqueue without re-checking types).
        #[cfg(debug_assertions)]
        for l in 0..link_count as usize {
            let consumers = &link_consumers[off[l] as usize..off[l + 1] as usize];
            let groups = &consumer_groups
                [consumer_groups_off[l] as usize..consumer_groups_off[l + 1] as usize];
            let mut pos = 0usize;
            for &(ti, len) in groups {
                for &c in &consumers[pos..pos + len as usize] {
                    debug_assert_eq!(ty_index(c), ti, "consumer group type mismatch");
                }
                pos += len as usize;
            }
            debug_assert_eq!(
                pos,
                consumers.len(),
                "consumer groups must cover the whole slice"
            );
        }

        let board = Board {
            link_count,
            comp_count,
            output_count: out_total,
            comp_ty: comp_ty.into_boxed_slice(),
            comp_config: comp_config.into_boxed_slice(),
            rom_data: pools.rom_data.into_boxed_slice(),
            ram_bytes: pools.ram_bytes,
            ram_count: pools.ram_count,
            ram_clocked_read: pools.ram_clocked_read.into_boxed_slice(),
            recorders: pools.recorders.into_boxed_slice(),
            rec_values: pools.rec_values,
            rec_ticks: pools.rec_ticks,
            comp_in_off: comp_in_off.into_boxed_slice(),
            comp_inputs: comp_inputs.into_boxed_slice(),
            comp_out_off: comp_out_off.into_boxed_slice(),
            output_link: output_link.into_boxed_slice(),
            link_consumers: link_consumers.into_boxed_slice(),
            consumer_groups: consumer_groups.into_boxed_slice(),
            link_off: off
                .iter()
                .zip(&consumer_groups_off)
                .map(|(&c, &g)| [c, g])
                .collect(),
            invalid_watch,
            watched_links: watched_links.into_boxed_slice(),
            driver_total: drivers.iter().map(|&n| n as u16).collect(),
        };
        #[cfg(debug_assertions)]
        board.debug_check();
        Ok(board)
    }

    /// Assert the [invariants](Board#invariants) the tick loop's unchecked reads rely on.
    #[cfg(debug_assertions)]
    fn debug_check(&self) {
        let csr = |off: &mut dyn Iterator<Item = u32>, len: usize, what: &str| {
            let mut prev = 0;
            let mut first = true;
            for o in off {
                assert!(if first { o == 0 } else { o >= prev }, "{what}: offsets");
                (prev, first) = (o, false);
            }
            assert_eq!(prev as usize, len, "{what}: last offset");
        };
        let links = self.link_count as usize;
        csr(
            &mut self.comp_in_off.iter().copied(),
            self.comp_inputs.len(),
            "inputs",
        );
        csr(
            &mut self.comp_out_off.iter().copied(),
            self.output_link.len(),
            "outputs",
        );
        assert_eq!(self.link_off.len(), links + 1);
        csr(
            &mut self.link_off.iter().map(|o| o[0]),
            self.link_consumers.len(),
            "consumers",
        );
        csr(
            &mut self.link_off.iter().map(|o| o[1]),
            self.consumer_groups.len(),
            "groups",
        );
        for &raw in self.comp_inputs.iter().chain(self.output_link.iter()) {
            assert!(
                ((raw & LINK_ID_MASK) as usize) < links,
                "link id out of range"
            );
        }
        assert_eq!(self.driver_total.len(), links);
        assert_eq!(self.invalid_watch.bits() as usize, links);
        for l in 0..self.link_count {
            let (consumers, groups) = self.link_fanout(l);
            let mut pos = 0;
            for &(ti, len) in groups {
                assert!((ti as usize) < components::N_TYPES, "group type index");
                pos += len as usize;
            }
            assert_eq!(pos, consumers.len(), "groups partition the consumers");
        }
    }

    /// Derive a component's compiled [`CompConfig`] from its descriptor, allocating its share of
    /// `pools` (a ROM's data blob, a RAM's memory, a recorder's rings). Coarse input/output counts
    /// are already arity-validated and the parameters checked to be the type's own; this adds the
    /// cross-field constraints between them (e.g. a decoder's `outputs == 2^inputs`) and checks
    /// and captures the parameters.
    fn configure(
        idx: u32,
        ty: CompType,
        c: &ComponentDescriptor,
        pools: &mut Pools,
    ) -> Result<CompConfig> {
        let ins = c.inputs.len();
        let outs = c.outputs.len();
        let bad = || SimError::BadArity { idx, ty, ins, outs };
        let bad_param = |param, reason| SimError::BadParam {
            idx,
            ty,
            param,
            reason,
        };
        Ok(match ty {
            CompType::Clk => {
                // period ≥ 1 (the editor's default is 1); a square wave is `period` ticks low then
                // `period` high, a pulse one tick high after `period` ticks low.
                let period = c.period.ok_or_else(|| bad_param("period", "is required"))?;
                if period < 1 {
                    return Err(bad_param("period", "must be at least 1"));
                }
                let high = match c.mode.unwrap_or_default() {
                    ClkMode::Pulse => 1,
                    ClkMode::Square => period,
                };
                CompConfig { a: period, b: high }
            }
            CompType::Rom => {
                // `ceil(outputCount * 2^inputCount / 8)` bytes, zero-filled then the data copied
                // in. inputs ≤ 16 by arity, so `1 << ins` fits in u64. Data past the table means
                // the pin counts do not match the contents, so it is refused rather than dropped.
                let bits = (outs as u64) << ins; // outputCount * 2^inputCount
                let size = bits.div_ceil(8) as usize;
                let data = c.data.as_deref().unwrap_or_default();
                if data.len() > size {
                    return Err(bad_param("data", "is longer than the ROM's table"));
                }
                let rom_data = &mut pools.rom_data;
                let off = rom_data.len();
                rom_data.extend_from_slice(data);
                rom_data.resize(off + size, 0);
                CompConfig {
                    a: off as u32,
                    b: 0,
                }
            }
            CompType::Decoder => {
                // One-hot: outputs == 2^inputs (inputs ≤ 16 by arity).
                if outs != 1usize << ins {
                    return Err(bad());
                }
                CompConfig::default()
            }
            CompType::Encoder => {
                // n number bits (1–16) and an optional valid output after them: inputs == 2^n with
                // n = outputs, or n = outputs - 1 when the last output is the valid pin. The two
                // input counts differ, so the input count says which.
                let bits = if ins == 1usize << outs.min(16) {
                    outs
                } else if outs >= 2 && ins == 1usize << (outs - 1) {
                    outs - 1
                } else {
                    return Err(bad());
                };
                CompConfig {
                    a: bits as u32,
                    b: 0,
                }
            }
            CompType::Mux => {
                // inputs == sel + 2^sel, which grows with sel, so the input count names at most one
                // select width (1–16).
                let sel = (1..=16)
                    .find(|&s| s + (1usize << s) == ins)
                    .ok_or_else(bad)?;
                CompConfig {
                    a: sel as u32,
                    b: 0,
                }
            }
            CompType::Demux => {
                // s select bits then the data input: outputs == 2^s with s = inputs - 1 ≥ 1 (by
                // arity).
                if ins - 1 > 16 || outs != 1usize << (ins - 1) {
                    return Err(bad());
                }
                CompConfig::default()
            }
            CompType::Ram => {
                // inputs = addressSize + wordSize + 2 (address, data, write-enable, clock);
                // wordSize = outputs. Address bus capped at 24 bits (16M words) to bound the
                // backing store against a malformed board.
                let clocked_read = c.read.unwrap_or_default() == RamRead::Clocked;
                let word_size = outs as u64;
                if ins < outs + 2 {
                    return Err(bad());
                }
                let addr_size = (ins - outs - 2) as u32;
                if addr_size > 24 {
                    return Err(bad());
                }
                let bits = word_size << addr_size; // wordSize * 2^addressSize
                let size = bits.div_ceil(8) as u32;
                let off = pools.ram_bytes;
                pools.ram_bytes += size;
                let index = pools.ram_count;
                pools.ram_count += 1;
                pools.ram_clocked_read.push(clocked_read);
                CompConfig { a: off, b: index }
            }
            CompType::Recorder => {
                // inputs = 1..=32 data pins + the trigger (by arity).
                let capacity = c
                    .capacity
                    .ok_or_else(|| bad_param("capacity", "is required"))?;
                if capacity == 0 || capacity > MAX_RECORDER_CAPACITY {
                    return Err(bad_param("capacity", "must be 1 to 2^20"));
                }
                let ticks = c.record_ticks.unwrap_or(false).then(|| {
                    let off = pools.rec_ticks;
                    pools.rec_ticks += capacity as usize;
                    off
                });
                let index = pools.recorders.len() as u32;
                pools.recorders.push(RecorderConfig {
                    capacity,
                    values: pools.rec_values,
                    ticks,
                });
                pools.rec_values += capacity as usize;
                CompConfig { a: index, b: 0 }
            }
            CompType::AddressableRegister => {
                // dataBus = data-bus width, 1..=outputs. The outputs form rows of that width (the
                // last may be partial), and the address bus is just wide enough to name every row:
                // ceil(log2(ceil(outputs / width))). inputs = addr + data + clock.
                let data_bus = c
                    .data_bus
                    .ok_or_else(|| bad_param("dataBus", "is required"))?;
                let data_bus = data_bus as usize;
                if data_bus == 0 || data_bus > outs {
                    return Err(bad_param("dataBus", "must be 1 to the output count"));
                }
                let rows = outs.div_ceil(data_bus) as u32;
                let addr_bus = ceil_log2(rows);
                if ins != addr_bus as usize + data_bus + 1 {
                    return Err(bad());
                }
                CompConfig {
                    a: data_bus as u32,
                    b: addr_bus,
                }
            }
            _ => CompConfig::default(),
        })
    }

    /// Compiled configuration of component `c`.
    #[inline]
    pub(crate) fn config(&self, c: u32) -> CompConfig {
        self.comp_config[c as usize]
    }

    /// Number of links.
    #[inline]
    pub fn link_count(&self) -> u32 {
        self.link_count
    }

    /// Number of components.
    #[inline]
    pub fn component_count(&self) -> u32 {
        self.comp_count
    }

    /// Components reading link `l` (CSR slice), and their same-type runs as `(type_index, len)`
    /// pairs, `len` relative to the consumer slice. The tick uses
    /// [`Board::link_fanout_unchecked`].
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn link_fanout(&self, l: u32) -> (&[u32], &[(u8, u32)]) {
        let l = l as usize;
        let [c0, g0] = self.link_off[l];
        let [c1, g1] = self.link_off[l + 1];
        (
            &self.link_consumers[c0 as usize..c1 as usize],
            &self.consumer_groups[g0 as usize..g1 as usize],
        )
    }

    /// [`Board::link_fanout`] without bounds checks.
    ///
    /// # Safety
    ///
    /// `l` must be below `link_count`.
    #[inline]
    pub(crate) unsafe fn link_fanout_unchecked(&self, l: u32) -> (&[u32], &[(u8, u32)]) {
        debug_assert!(l < self.link_count, "link {l} out of range");
        let l = l as usize;
        // SAFETY: `l < link_count` (the caller's contract), and `link_off` has `link_count + 1`
        // entries whose columns are non-decreasing and end at the arrays they slice (invariants).
        unsafe {
            let [c0, g0] = *self.link_off.get_unchecked(l);
            let [c1, g1] = *self.link_off.get_unchecked(l + 1);
            (
                self.link_consumers.get_unchecked(c0 as usize..c1 as usize),
                self.consumer_groups.get_unchecked(g0 as usize..g1 as usize),
            )
        }
    }

    /// Components reading link `l` (CSR slice).
    #[cfg(test)]
    pub(crate) fn link_consumers(&self, l: u32) -> &[u32] {
        self.link_fanout(l).0
    }

    /// Global output-id range of component `c` (`output_link[id]` is the driven link).
    #[inline]
    pub(crate) fn output_ids(&self, c: u32) -> core::ops::Range<u32> {
        let c = c as usize;
        self.comp_out_off[c]..self.comp_out_off[c + 1]
    }

    /// The link driven by output pin `oid`, with the packed negate flag masked off.
    #[inline]
    pub(crate) fn output_link_id(&self, oid: u32) -> u32 {
        self.output_link[oid as usize] & LINK_ID_MASK
    }

    /// Whether output pin `oid` drives the inverted computed value (flag packed in the link-id MSB).
    #[inline]
    pub(crate) fn output_negated(&self, oid: u32) -> bool {
        self.output_link[oid as usize] & LINK_NEG_BIT != 0
    }

    /// Whether input pin at global input-CSR index `gi` reads the inverted link value (flag packed
    /// in the link-id MSB of `comp_inputs`).
    #[inline]
    pub(crate) fn input_negated(&self, gi: u32) -> bool {
        self.comp_inputs[gi as usize] & LINK_NEG_BIT != 0
    }

    /// Whether any input pin of component `c` is negated (the power-on negated-input scan predicate).
    #[inline]
    pub(crate) fn comp_has_negated_input(&self, c: u32) -> bool {
        let c = c as usize;
        (self.comp_in_off[c]..self.comp_in_off[c + 1]).any(|gi| self.input_negated(gi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two NOTs feeding a 2-input AND (link 2 driven by both NOT outputs would be a bus; here a
    // simple chain): in0 -> NOT -> l1 ; in1 -> NOT -> l2 ; (l1,l2) -> AND -> l3.
    fn sample() -> BoardDescriptor {
        let mut b = BoardBuilder::new();
        b.component(CompType::Not, &[0], &[1]);
        b.component(CompType::Not, &[3], &[2]);
        b.component(CompType::And, &[1, 2], &[4]);
        b.finish()
    }

    #[test]
    fn compiles_csr_shapes() {
        let board = Board::compile(&sample()).unwrap();
        assert_eq!(board.link_count(), 5);
        assert_eq!(board.component_count(), 3);
        assert_eq!(board.output_count, 3);
        // AND (comp 2) inputs are links 1 and 2.
        assert_eq!(
            &board.comp_inputs[board.comp_in_off[2] as usize..board.comp_in_off[3] as usize],
            &[1, 2]
        );
        // output ids: comp0 -> oid0 (link1), comp1 -> oid1 (link2), comp2 -> oid2 (link4)
        assert_eq!(board.output_ids(0), 0..1);
        assert_eq!(board.output_link_id(0), 1);
        assert_eq!(board.output_link_id(2), 4);
        // link 1 is consumed by the AND (comp 2); link 0 by NOT (comp 0); link 4 by no one.
        assert_eq!(board.link_consumers(1), &[2]);
        assert_eq!(board.link_consumers(0), &[0]);
        assert_eq!(board.link_consumers(4), &[] as &[u32]);
    }

    #[test]
    fn consumer_groups_collapse_same_type_runs() {
        // Link 0 feeds four components in interleaved type order: Not, And, Not, And. After the
        // stable sort the slice is grouped by type, so it collapses to two groups, and flattening
        // the groups reproduces the same multiset of consumers as the (now sorted) slice.
        let mut b = BoardBuilder::new();
        b.component(CompType::Not, &[0], &[1]); // comp 0
        b.component(CompType::And, &[0, 1], &[2]); // comp 1
        b.component(CompType::Not, &[0], &[1]); // comp 2
        b.component(CompType::And, &[0, 2], &[1]); // comp 3
        let board = Board::compile(&b.finish()).unwrap();

        let (consumers, groups) = board.link_fanout(0);
        assert_eq!(groups.len(), 2, "two types → two groups");

        // Group lengths cover the slice, members all match the group's type_index, and within a
        // type the stable sort preserved submission order (Not: 0 before 2; And: 1 before 3).
        let mut pos = 0usize;
        let mut flat = Vec::new();
        for &(ti, len) in groups {
            for &c in &consumers[pos..pos + len as usize] {
                assert_eq!(components::type_index(board.comp_ty[c as usize]) as u8, ti);
                flat.push(c);
            }
            pos += len as usize;
        }
        assert_eq!(pos, consumers.len());
        assert_eq!(flat, consumers, "flattened groups reproduce the slice");

        let mut sorted = flat.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3], "all four consumers present once");
    }

    /// The links are numbered by the pins: one past the highest id used, an unused id below it
    /// included as a link nothing touches.
    #[test]
    fn link_count_is_one_past_the_highest_id() {
        let mut b = BoardBuilder::new();
        b.component(CompType::Not, &[2], &[7]);
        let board = Board::compile(&b.finish()).unwrap();
        assert_eq!(board.link_count(), 8);
        assert_eq!(board.link_consumers(5), &[] as &[u32]);
        assert_eq!(
            Board::compile(&BoardDescriptor::default())
                .unwrap()
                .link_count(),
            0
        );
    }

    /// A link id's top bit is the packed negate flag, so `2^31 - 1` is the highest id.
    #[test]
    fn rejects_link_id_with_the_negate_bit() {
        let mut b = BoardBuilder::new();
        b.component(CompType::Not, &[0], &[1]);
        b.component(CompType::Not, &[LINK_NEG_BIT], &[1]);
        let err = Board::compile(&b.finish()).unwrap_err();
        assert!(matches!(
            err,
            SimError::LinkIdOverflow {
                idx: 1,
                link: LINK_NEG_BIT,
                max
            } if max == LINK_NEG_BIT - 1
        ));
    }

    #[test]
    fn rejects_bad_arity() {
        let mut b = BoardBuilder::new();
        b.component(CompType::Not, &[0, 1], &[2]); // NOT takes exactly 1 input
        let err = Board::compile(&b.finish()).unwrap_err();
        assert!(matches!(
            err,
            SimError::BadArity {
                idx: 0,
                ty: CompType::Not,
                ins: 2,
                ..
            }
        ));
    }

    /// A link's high drivers are counted in a `u16`: 65 535 drivers compile, one more is refused
    /// (it would wrap the count to 0 and read low with every driver high).
    #[test]
    fn rejects_more_drivers_than_the_count_holds() {
        let board = |n: u32| {
            let mut b = BoardBuilder::new();
            for _ in 0..n {
                b.component(CompType::UserInput, &[], &[1]);
            }
            Board::compile(&b.finish())
        };
        assert!(board(u16::MAX as u32).is_ok());
        assert!(matches!(
            board(u16::MAX as u32 + 1),
            Err(SimError::TooManyDrivers {
                link: 1,
                count: 65_536,
                max: 65_535
            })
        ));
    }

    /// ROM data fills the table from the start and zero-fills the rest; data past the table is
    /// refused, since it means the pin counts do not match the contents.
    #[test]
    fn rom_data_fills_its_table() {
        let rom = |data: &[u8]| {
            let mut b = BoardBuilder::new();
            b.add(ComponentDescriptor {
                data: Some(data.to_vec()),
                // 2 address bits × 4-bit words: a 2-byte table.
                ..ComponentDescriptor::new(CompType::Rom, &[0, 1], &[2, 3, 4, 5])
            });
            Board::compile(&b.finish())
        };
        assert_eq!(&*rom(&[0xAB]).unwrap().rom_data, &[0xAB, 0]);
        assert_eq!(&*rom(&[0xAB, 0xCD]).unwrap().rom_data, &[0xAB, 0xCD]);
        assert!(matches!(
            rom(&[1, 2, 3]).unwrap_err(),
            SimError::BadParam {
                idx: 0,
                param: "data",
                ..
            }
        ));
    }

    /// Each parameter belongs to one type: it is refused on any other, and a required one is
    /// refused when missing.
    #[test]
    fn params_belong_to_their_type() {
        let compile = |c: ComponentDescriptor| {
            let mut b = BoardBuilder::new();
            b.add(c);
            Board::compile(&b.finish())
        };
        let not = ComponentDescriptor::new(CompType::Not, &[0], &[1]);
        let clk = ComponentDescriptor::new(CompType::Clk, &[0], &[1]);
        assert!(matches!(
            compile(ComponentDescriptor {
                period: Some(1),
                ..not.clone()
            }),
            Err(SimError::BadParam {
                param: "period",
                ..
            })
        ));
        assert!(matches!(
            compile(ComponentDescriptor {
                period: Some(1),
                data_bus: Some(1),
                ..clk.clone()
            }),
            Err(SimError::BadParam {
                param: "dataBus",
                ..
            })
        ));
        assert!(matches!(
            compile(clk.clone()),
            Err(SimError::BadParam {
                param: "period",
                reason: "is required",
                ..
            })
        ));
        assert!(
            compile(ComponentDescriptor {
                period: Some(1),
                mode: Some(ClkMode::Pulse),
                ..clk
            })
            .is_ok()
        );
    }

    #[test]
    fn rejects_negate_index_out_of_range() {
        use crate::error::PinKind;
        // AND has 2 inputs (pins 0,1); pin 2 is out of range.
        let mut b = BoardBuilder::new();
        b.component_neg(CompType::And, &[0, 1], &[2], &[2], &[]);
        let err = Board::compile(&b.finish()).unwrap_err();
        assert!(matches!(
            err,
            SimError::NegateOutOfRange {
                idx: 0,
                pin_kind: PinKind::Input,
                pin: 2,
                count: 2
            }
        ));

        // A negated output index past the single output pin is rejected too.
        let mut b = BoardBuilder::new();
        b.component_neg(CompType::And, &[0, 1], &[2], &[], &[1]);
        let err = Board::compile(&b.finish()).unwrap_err();
        assert!(matches!(
            err,
            SimError::NegateOutOfRange {
                idx: 0,
                pin_kind: PinKind::Output,
                pin: 1,
                count: 1
            }
        ));
    }

    #[test]
    fn builds_contiguous_negate_masks() {
        // comp0: AND(in 0,1; neg in pin1; out 2 neg). comp1: NOT(in 2; out 3).
        let mut b = BoardBuilder::new();
        b.component_neg(CompType::And, &[0, 1], &[2], &[1], &[0]);
        b.component(CompType::Not, &[2], &[3]);
        let board = Board::compile(&b.finish()).unwrap();
        // comp0 inputs occupy global input ids 0,1; only pin 1 is negated.
        assert!(!board.input_negated(0));
        assert!(board.input_negated(1));
        assert!(board.comp_has_negated_input(0));
        assert!(!board.comp_has_negated_input(1));
        // comp0 output oid 0 is negated (flag packed in the link-id MSB); comp1 output oid 1 is not.
        assert!(board.output_negated(0));
        assert!(!board.output_negated(1));
        assert_eq!(board.output_link_id(0), 2); // masking recovers the driven link id
        assert_eq!(board.output_link_id(1), 3);
    }

    /// The descriptor carries raw ids, so `compile` is what rejects an unknown type — including
    /// one whose low 16 bits name a real type, which a narrowing reader would have let through.
    #[test]
    fn rejects_unknown_type_id() {
        for ty in [7, 0x1_0000 | CompType::Not.id()] {
            let mut desc = BoardBuilder::new();
            desc.component(CompType::Not, &[0], &[1]);
            let mut desc = desc.finish();
            desc.components.push(ComponentDescriptor {
                ty,
                ..desc.components[0].clone()
            });
            let err = Board::compile(&desc).unwrap_err();
            assert!(
                matches!(err, SimError::UnknownComponentType { idx: 1, ty: t } if t == ty),
                "{ty}: {err}"
            );
        }
    }

    /// A negated pin index past the u16 range is out of range, not wrapped onto a real pin.
    #[test]
    fn rejects_wide_negate_index() {
        let mut b = BoardBuilder::new();
        b.component_neg(CompType::Not, &[0], &[1], &[0x1_0000], &[]);
        assert!(matches!(
            Board::compile(&b.finish()).unwrap_err(),
            SimError::NegateOutOfRange { pin: 0x1_0000, .. }
        ));
    }
}
