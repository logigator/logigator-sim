//! Board description, builder, and the compile step that lowers a board to struct-of-arrays with
//! CSR adjacency (plan §5.2, §6.1 steps 1–2).
//!
//! Public component ids are the **submission order** the caller used (D17). Phase 1 keeps the
//! internal layout in that same order; the locality-renumbering pass (D13) is a later,
//! semantics-preserving optimization and slots in behind a translation table without changing this
//! contract.

use crate::CompType;
use crate::components;
use crate::error::{Result, SimError};

/// One component in a board description (plan §7.2). Input/output entries are link ids.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ComponentDescriptor {
    #[cfg_attr(feature = "serde", serde(rename = "type"))]
    pub ty: CompType,
    pub inputs: Vec<u32>,
    pub outputs: Vec<u32>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub ops: Vec<u32>,
}

/// A board: a link count plus a list of components (plan §7.2). The single public board shape,
/// consumed by `Board::compile` and every binding's constructor.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BoardDescriptor {
    #[cfg_attr(feature = "serde", serde(rename = "links"))]
    pub link_count: u32,
    pub components: Vec<ComponentDescriptor>,
}

/// Programmatic board construction (plan §7.2).
pub struct BoardBuilder {
    link_count: u32,
    components: Vec<ComponentDescriptor>,
}

impl BoardBuilder {
    /// Start a board with `link_count` links and no components.
    pub fn new(link_count: u32) -> Self {
        BoardBuilder {
            link_count,
            components: Vec::new(),
        }
    }

    /// Append a component; returns its (public, submission-order) component id.
    pub fn component(&mut self, ty: CompType, inputs: &[u32], outputs: &[u32], ops: &[u32]) -> u32 {
        let id = self.components.len() as u32;
        self.components.push(ComponentDescriptor {
            ty,
            inputs: inputs.to_vec(),
            outputs: outputs.to_vec(),
            ops: ops.to_vec(),
        });
        id
    }

    /// Finish into a `BoardDescriptor`.
    pub fn finish(self) -> BoardDescriptor {
        BoardDescriptor {
            link_count: self.link_count,
            components: self.components,
        }
    }
}

/// Per-component compiled configuration (plan §6.1 step 2): the constant parameters the old C++
/// constructors derived from input/output array lengths and the `ops` array, captured once at
/// compile time. Two `u32` slots reused per component type:
///
/// - `a`: ROM (12) → byte offset of its data blob in [`Board::rom_data`]; RAM (17) → byte offset
///   into the simulation `mem` scratch pool; MUX (20) → number of select bits; CLK (6) → period;
///   LED matrix (204) → data-bus length.
/// - `b`: LED matrix (204) → address-bus length.
///
/// Both are `0` for purely combinational/parameterless types (gates, adders, DEC/ENC/DEMUX, FFs).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompConfig {
    pub a: u32,
    pub b: u32,
}

/// `ceil(log2(n))` for `n >= 1` (0 for `n <= 1`), computed with integers to avoid the float
/// rounding hazard of the C++ `ceil(log2(...))` while agreeing with it on the power-of-two row
/// counts boards actually use. Used for the LED-matrix / address-bus widths (plan §5.3a note).
fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        u32::BITS - (n - 1).leading_zeros()
    }
}

/// A compiled board: immutable topology in SoA + CSR form (plan §5.2). Shared read-only by the
/// tick loop (and, in phase 6, across threads).
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
    /// CSR: input links per component.
    pub(crate) comp_in_off: Box<[u32]>,
    pub(crate) comp_inputs: Box<[u32]>,
    /// CSR: outputs per component are dense & contiguous; `output_link[oid]` is the driven link.
    pub(crate) comp_out_off: Box<[u32]>,
    pub(crate) output_link: Box<[u32]>,
    /// CSR: components that read each link (built from the inputs above).
    pub(crate) link_consumers_off: Box<[u32]>,
    pub(crate) link_consumers: Box<[u32]>,
}

impl Board {
    /// Validate and lower a board description to SoA + CSR (plan §6.1 steps 1–2).
    ///
    /// Validation: every input/output link id is in `0..link_count`, and each component's
    /// input/output/ops counts satisfy its type's arity. Errors are reported against the public
    /// (submission-order) component index.
    pub fn compile(desc: &BoardDescriptor) -> Result<Board> {
        let link_count = desc.link_count;
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
        let mut rom_data: Vec<u8> = Vec::new();
        let mut ram_bytes: u32 = 0;

        for (i, c) in desc.components.iter().enumerate() {
            let idx = i as u32;
            for &l in c.inputs.iter().chain(c.outputs.iter()) {
                if l >= link_count {
                    return Err(SimError::LinkOutOfRange {
                        idx,
                        link: l,
                        count: link_count,
                    });
                }
            }
            if !components::arity(c.ty).accepts(c.inputs.len(), c.outputs.len(), c.ops.len()) {
                return Err(SimError::BadArity {
                    idx,
                    ty: c.ty,
                    ins: c.inputs.len(),
                    outs: c.outputs.len(),
                    ops: c.ops.len(),
                });
            }
            comp_config.push(Self::configure(idx, c, &mut rom_data, &mut ram_bytes)?);
            comp_ty.push(c.ty);
            in_total += c.inputs.len() as u32;
            out_total += c.outputs.len() as u32;
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
        let mut link_consumers = vec![0u32; total_refs as usize];
        let mut cursor = off.clone();
        for (ci, c) in desc.components.iter().enumerate() {
            for &l in &c.inputs {
                let slot = &mut cursor[l as usize];
                link_consumers[*slot as usize] = ci as u32;
                *slot += 1;
            }
        }

        Ok(Board {
            link_count,
            comp_count,
            output_count: out_total,
            comp_ty: comp_ty.into_boxed_slice(),
            comp_config: comp_config.into_boxed_slice(),
            rom_data: rom_data.into_boxed_slice(),
            ram_bytes,
            comp_in_off: comp_in_off.into_boxed_slice(),
            comp_inputs: comp_inputs.into_boxed_slice(),
            comp_out_off: comp_out_off.into_boxed_slice(),
            output_link: output_link.into_boxed_slice(),
            link_consumers_off: off.into_boxed_slice(),
            link_consumers: link_consumers.into_boxed_slice(),
        })
    }

    /// Derive a component's compiled [`CompConfig`] from its descriptor (plan §6.1 step 2),
    /// appending any immutable data blob to `rom_data`. Coarse input/output/ops counts are already
    /// arity-validated; this adds the cross-field constraints the old C++ constructors implied
    /// (e.g. a decoder's `outputs == 2^inputs`) and captures `ops`-derived parameters.
    fn configure(
        idx: u32,
        c: &ComponentDescriptor,
        rom_data: &mut Vec<u8>,
        ram_bytes: &mut u32,
    ) -> Result<CompConfig> {
        let ins = c.inputs.len();
        let outs = c.outputs.len();
        let bad = || SimError::BadArity {
            idx,
            ty: c.ty,
            ins,
            outs,
            ops: c.ops.len(),
        };
        Ok(match c.ty {
            CompType::Clk => {
                // ops[0] = period ("speed"); must be ≥ 1 (the editor's default is 1). ops.len()==1
                // by arity.
                let speed = c.ops[0];
                if speed < 1 {
                    return Err(bad());
                }
                CompConfig { a: speed, b: 0 }
            }
            CompType::Rom => {
                // C++ `ceil(outputCount * 2^inputCount / 8)` bytes, zero-filled then the first
                // `ops.len()` bytes copied in (`rom.h` ctor). inputs ≤ 16 by arity, so `1 << ins`
                // fits in u64.
                let bits = (outs as u64) << ins; // outputCount * 2^inputCount
                let size = bits.div_ceil(8) as usize;
                let off = rom_data.len() as u32;
                rom_data.resize(rom_data.len() + size, 0);
                for (j, &b) in c.ops.iter().take(size).enumerate() {
                    rom_data[off as usize + j] = b as u8;
                }
                CompConfig { a: off, b: 0 }
            }
            CompType::Decoder => {
                // One-hot: outputs == 2^inputs (inputs ≤ 16 by arity).
                if outs != 1usize << ins {
                    return Err(bad());
                }
                CompConfig::default()
            }
            CompType::Encoder => {
                // inputs == 2^outputs (outputs ≤ 16 by arity).
                if ins != 1usize << outs {
                    return Err(bad());
                }
                CompConfig::default()
            }
            CompType::Mux => {
                // ops[0] = select bits (ops.len()==1 by arity); inputs == 2^sel + sel.
                let sel = c.ops[0] as usize;
                if sel == 0 || sel > 16 || ins != (1usize << sel) + sel {
                    return Err(bad());
                }
                CompConfig {
                    a: sel as u32,
                    b: 0,
                }
            }
            CompType::Demux => {
                // outputs == 2^(inputs-1), inputs ≥ 2 (by arity).
                if ins - 1 > 16 || outs != 1usize << (ins - 1) {
                    return Err(bad());
                }
                CompConfig::default()
            }
            CompType::Ram => {
                // inputs = addressSize + wordSize + 2 (address, data, write-enable, clock);
                // wordSize = outputs. Address bus capped at 24 bits (16M words) to bound the
                // backing store against a malformed board.
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
                let off = *ram_bytes;
                *ram_bytes += size;
                CompConfig { a: off, b: 0 }
            }
            CompType::LedMatrix => {
                // ops[0] selects the data-bus width: >4 → 8, else 4 (project.cpp:160). LED count is
                // the output count; address bus = ceil(log2(ledCount / dataBus)) with the division
                // done in integers first (led_matrix.h ctor); inputs = addr + data + clock.
                let data_bus = if c.ops[0] > 4 { 8usize } else { 4 };
                if outs < data_bus {
                    return Err(bad());
                }
                let rows = (outs / data_bus) as u32; // integer division, matching C++
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

    /// Components reading link `l` (CSR slice).
    #[inline]
    pub(crate) fn link_consumers(&self, l: u32) -> &[u32] {
        let l = l as usize;
        &self.link_consumers
            [self.link_consumers_off[l] as usize..self.link_consumers_off[l + 1] as usize]
    }

    /// Global output-id range of component `c` (`output_link[id]` is the driven link).
    #[inline]
    pub(crate) fn output_ids(&self, c: u32) -> core::ops::Range<u32> {
        let c = c as usize;
        self.comp_out_off[c]..self.comp_out_off[c + 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two NOTs feeding a 2-input AND (link 2 driven by both NOT outputs would be a bus; here a
    // simple chain): in0 -> NOT -> l1 ; in1 -> NOT -> l2 ; (l1,l2) -> AND -> l3.
    fn sample() -> BoardDescriptor {
        let mut b = BoardBuilder::new(5);
        b.component(CompType::Not, &[0], &[1], &[]);
        b.component(CompType::Not, &[3], &[2], &[]);
        b.component(CompType::And, &[1, 2], &[4], &[]);
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
        assert_eq!(board.output_link[0], 1);
        assert_eq!(board.output_link[2], 4);
        // link 1 is consumed by the AND (comp 2); link 0 by NOT (comp 0); link 4 by no one.
        assert_eq!(board.link_consumers(1), &[2]);
        assert_eq!(board.link_consumers(0), &[0]);
        assert_eq!(board.link_consumers(4), &[] as &[u32]);
    }

    #[test]
    fn rejects_link_out_of_range() {
        let mut b = BoardBuilder::new(2);
        b.component(CompType::Not, &[5], &[1], &[]); // link 5 >= 2
        let err = Board::compile(&b.finish()).unwrap_err();
        assert!(matches!(
            err,
            SimError::LinkOutOfRange {
                idx: 0,
                link: 5,
                count: 2
            }
        ));
    }

    #[test]
    fn rejects_bad_arity() {
        let mut b = BoardBuilder::new(3);
        b.component(CompType::Not, &[0, 1], &[2], &[]); // NOT takes exactly 1 input
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

    #[cfg(feature = "serde")]
    #[test]
    fn unknown_type_id_rejected_on_deserialize() {
        // type 7 has no component in the frozen contract (stays unknown across all of phase 2).
        let json = r#"{"links":2,"components":[{"type":7,"inputs":[0],"outputs":[1]}]}"#;
        let err = serde_json::from_str::<BoardDescriptor>(json).unwrap_err();
        assert!(
            err.to_string().contains("unknown component type id 7"),
            "got: {err}"
        );
    }
}
