//! The compact little-endian `.lgb` binary board format (plan §7.6).
//!
//! ```text
//! Header (16 B):  u32 magic=0x4C474231 ("LGB1") | u16 version=1 | u16 reserved
//!                 u32 link_count | u32 component_count
//! Per component:  u16 type | u16 reserved
//!                 u32 in_count i | u32 out_count o | u32 op_count p
//!                 u32×i input link ids | u32×o output link ids | u32×p ops
//! ```
//!
//! Everything is little-endian. This is the on-disk/on-wire dual of the JSON [`BoardDescriptor`]:
//! it carries the *same* information in a denser form, so a board round-trips
//! `decode_board(encode_board(b)) == b` (the serialized **state** dump — the packed link bitset —
//! is produced by [`Simulation::link_bytes`](crate::Simulation::link_bytes), not here).

use crate::board::{BoardDescriptor, ComponentDescriptor};
use crate::error::{Result, SimError};
use crate::types::CompType;

/// File magic, the value fixed by the plan (§7.6). Its hex digits spell `LGB1`
/// (`4C`=`L` `47`=`G` `42`=`B` `31`=`1`); written as a little-endian `u32` like every other field,
/// so on disk the file actually begins with the bytes `31 42 47 4C`. (The only reader is this
/// codec, so the on-disk byte order is internal — what matters is that encode/decode agree.)
pub const LGB_MAGIC: u32 = 0x4C47_4231;
/// Current `.lgb` format version.
pub const LGB_VERSION: u16 = 1;

/// Serialize a board descriptor to the `.lgb` binary format.
pub fn encode_board(desc: &BoardDescriptor) -> Vec<u8> {
    // Header (16 B) + a lower bound on the body, so the common board allocates once.
    let mut out = Vec::with_capacity(16 + desc.components.len() * 16);
    out.extend_from_slice(&LGB_MAGIC.to_le_bytes());
    out.extend_from_slice(&LGB_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&desc.link_count.to_le_bytes());
    out.extend_from_slice(&(desc.components.len() as u32).to_le_bytes());
    for c in &desc.components {
        out.extend_from_slice(&c.ty.id().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&(c.inputs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(c.outputs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(c.ops.len() as u32).to_le_bytes());
        for v in c.inputs.iter().chain(&c.outputs).chain(&c.ops) {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Parse a board descriptor from `.lgb` binary. Validates the magic/version and that every length
/// field is backed by enough bytes, so a truncated or corrupt file errors rather than panicking.
/// Component *type* ids are mapped through [`CompType::try_from_u16`]; arity/link-range validation
/// is deferred to [`Board::compile`](crate::Board::compile), exactly like the JSON path.
pub fn decode_board(bytes: &[u8]) -> Result<BoardDescriptor> {
    let mut r = Reader::new(bytes);

    let magic = r.u32()?;
    if magic != LGB_MAGIC {
        return Err(SimError::BadBinary(format!(
            "bad magic 0x{magic:08X} (expected 0x{LGB_MAGIC:08X})"
        )));
    }
    let version = r.u16()?;
    if version != LGB_VERSION {
        return Err(SimError::BadBinary(format!(
            "unsupported version {version} (this build reads {LGB_VERSION})"
        )));
    }
    let _reserved = r.u16()?;
    let link_count = r.u32()?;
    let comp_count = r.u32()?;

    let mut components = Vec::new();
    for _ in 0..comp_count {
        let ty_id = r.u16()?;
        let _reserved = r.u16()?;
        let ty = CompType::try_from_u16(ty_id).ok_or(SimError::UnknownComponentType(ty_id))?;
        let in_count = r.u32()? as usize;
        let out_count = r.u32()? as usize;
        let op_count = r.u32()? as usize;
        let inputs = r.u32_vec(in_count)?;
        let outputs = r.u32_vec(out_count)?;
        let ops = r.u32_vec(op_count)?;
        components.push(ComponentDescriptor {
            ty,
            inputs,
            outputs,
            ops,
        });
    }

    Ok(BoardDescriptor {
        link_count,
        components,
    })
}

/// A bounds-checked little-endian cursor over the input bytes.
struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }

    /// Borrow the next `n` bytes, advancing the cursor; errors if fewer remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len());
        match end {
            Some(end) => {
                let s = &self.b[self.pos..end];
                self.pos = end;
                Ok(s)
            }
            None => Err(SimError::BadBinary(format!(
                "truncated: need {n} bytes at offset {} but only {} remain",
                self.pos,
                self.b.len().saturating_sub(self.pos)
            ))),
        }
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// Read `n` little-endian `u32`s. `take` bounds-checks the whole span *before* the loop, so a
    /// bogus `n` (a corrupt count) errors instead of over-reserving or spinning.
    fn u32_vec(&mut self, n: usize) -> Result<Vec<u32>> {
        let span = self.take(n.checked_mul(4).ok_or_else(|| {
            SimError::BadBinary(format!("element count {n} overflows the byte length"))
        })?)?;
        Ok(span
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BoardBuilder;

    /// A board spanning several arities (no inputs, multi-output, ops) round-trips byte-for-byte
    /// back to an equal descriptor.
    fn sample() -> BoardDescriptor {
        let mut b = BoardBuilder::new(8);
        b.component(CompType::UserInput, &[], &[0, 1], &[]); // no inputs, 2 outputs
        b.component(CompType::And, &[0, 1], &[2], &[]);
        b.component(CompType::Clk, &[3], &[4], &[7]); // carries an op
        b.component(CompType::Not, &[4], &[5], &[]);
        b.finish()
    }

    #[test]
    fn round_trips() {
        let desc = sample();
        let bytes = encode_board(&desc);
        assert_eq!(decode_board(&bytes).unwrap(), desc);
    }

    #[test]
    fn header_is_well_formed() {
        let bytes = encode_board(&sample());
        // Pin the literal on-disk bytes (not `LGB_MAGIC.to_le_bytes()`, which would be tautological):
        // the little-endian magic begins `31 42 47 4C`, then version 1 as a u16.
        assert_eq!(&bytes[0..4], &[0x31, 0x42, 0x47, 0x4C]);
        assert_eq!(&bytes[4..6], &[0x01, 0x00]);
        // u32 link_count at offset 8, u32 component_count at offset 12.
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            8
        );
        assert_eq!(
            u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            4
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode_board(&sample());
        bytes[0] ^= 0xFF;
        assert!(matches!(decode_board(&bytes), Err(SimError::BadBinary(_))));
    }

    #[test]
    fn rejects_truncation() {
        let bytes = encode_board(&sample());
        // Drop the last 3 bytes of the final component's op array.
        assert!(matches!(
            decode_board(&bytes[..bytes.len() - 3]),
            Err(SimError::BadBinary(_))
        ));
        // A header cut short errors too (no panic).
        assert!(matches!(
            decode_board(&bytes[..10]),
            Err(SimError::BadBinary(_))
        ));
        assert!(matches!(decode_board(&[]), Err(SimError::BadBinary(_))));
    }

    #[test]
    fn rejects_unknown_type() {
        let mut bytes = encode_board(&sample());
        // The first component's type id lives right after the 16-byte header; stamp an unassigned id.
        bytes[16..18].copy_from_slice(&7u16.to_le_bytes());
        assert!(matches!(
            decode_board(&bytes),
            Err(SimError::UnknownComponentType(7))
        ));
    }
}
