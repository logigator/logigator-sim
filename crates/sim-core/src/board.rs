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
            comp_in_off: comp_in_off.into_boxed_slice(),
            comp_inputs: comp_inputs.into_boxed_slice(),
            comp_out_off: comp_out_off.into_boxed_slice(),
            output_link: output_link.into_boxed_slice(),
            link_consumers_off: off.into_boxed_slice(),
            link_consumers: link_consumers.into_boxed_slice(),
        })
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
        // type 13 (D flip-flop) is reserved but unimplemented in phase 1.
        let json = r#"{"links":2,"components":[{"type":13,"inputs":[0],"outputs":[1]}]}"#;
        let err = serde_json::from_str::<BoardDescriptor>(json).unwrap_err();
        assert!(
            err.to_string().contains("unknown component type id 13"),
            "got: {err}"
        );
    }
}
