//! The compact little-endian `.lgb` binary board format.
//!
//! ```text
//! Header (12 B):  u32 magic=0x4C474231 ("LGB1") | u16 version=3 | u16 reserved
//!                 u32 component_count
//! Per component:  u16 type | u16 params (bit mask of the parameters that follow the pins)
//!                 u32 in_count i | u32 out_count o
//!                 u32×i input link ids | u32×o output link ids
//!                 u32 neg_in_count ni | u32 neg_out_count no
//!                 u16×ni negated-input pin indices | u16×no negated-output pin indices
//!                 then each parameter whose bit is set, in bit order:
//!                   bit 0 period: u32         bit 4 capacity: u32
//!                   bit 1 mode: u8            bit 5 recordTicks: u8 (0 or 1)
//!                     (0 pulse, 1 square)     bit 6 dataBus: u32
//!                   bit 2 read: u8
//!                     (0 async, 1 clocked)
//!                   bit 3 data: u32 length n | n bytes
//! ```
//!
//! Everything is little-endian. This is the on-disk/on-wire dual of the JSON [`BoardDescriptor`]:
//! it carries the *same* information in a denser form, so a board round-trips
//! `decode_board(encode_board(b)) == b` (the serialized **state** dump — the packed link bitset —
//! is produced by [`Simulation::link_bytes`](crate::Simulation::link_bytes), not here).
//!
//! The negated-pin counts are `u32` (uniform with the other count fields); the index *elements* are
//! `u16` to match the descriptor type. The byte cursor reads sequentially, so the 2-byte elements
//! need no alignment. Counts are `0` for un-negated components.

use crate::board::{BoardDescriptor, ClkMode, ComponentDescriptor, RamRead};
use crate::error::{Result, SimError};

/// File magic. Its hex digits spell `LGB1`
/// (`4C`=`L` `47`=`G` `42`=`B` `31`=`1`); written as a little-endian `u32` like every other field,
/// so on disk the file actually begins with the bytes `31 42 47 4C`. (The only reader is this
/// codec, so the on-disk byte order is internal — what matters is that encode/decode agree.)
pub const LGB_MAGIC: u32 = 0x4C47_4231;
/// Current `.lgb` format version.
pub const LGB_VERSION: u16 = 3;

/// The parameter bits of a component's `params` mask.
const PERIOD: u16 = 1 << 0;
const MODE: u16 = 1 << 1;
const READ: u16 = 1 << 2;
const DATA: u16 = 1 << 3;
const CAPACITY: u16 = 1 << 4;
const RECORD_TICKS: u16 = 1 << 5;
const DATA_BUS: u16 = 1 << 6;

/// Serialize a board descriptor to the `.lgb` binary format. The format stores type ids and
/// negated pin indices as `u16`; a descriptor with a wider one (which no valid board has) is
/// refused rather than truncated onto a different id or pin.
pub fn encode_board(desc: &BoardDescriptor) -> Result<Vec<u8>> {
    // Header (12 B) + a lower bound on the body, so the common board allocates once.
    let mut out = Vec::with_capacity(12 + desc.components.len() * 16);
    out.extend_from_slice(&LGB_MAGIC.to_le_bytes());
    out.extend_from_slice(&LGB_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&(desc.components.len() as u32).to_le_bytes());
    for (i, c) in desc.components.iter().enumerate() {
        let narrow = |v: u32, what: &str| {
            u16::try_from(v).map_err(|_| {
                SimError::BadBinary(format!("component {i}: {what} {v} does not fit in a u16"))
            })
        };
        let params = [
            (PERIOD, c.period.is_some()),
            (MODE, c.mode.is_some()),
            (READ, c.read.is_some()),
            (DATA, c.data.is_some()),
            (CAPACITY, c.capacity.is_some()),
            (RECORD_TICKS, c.record_ticks.is_some()),
            (DATA_BUS, c.data_bus.is_some()),
        ]
        .into_iter()
        .filter(|&(_, set)| set)
        .fold(0, |mask, (bit, _)| mask | bit);
        out.extend_from_slice(&narrow(c.ty, "type id")?.to_le_bytes());
        out.extend_from_slice(&params.to_le_bytes());
        out.extend_from_slice(&(c.inputs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(c.outputs.len() as u32).to_le_bytes());
        for v in c.inputs.iter().chain(&c.outputs) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&(c.neg_inputs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(c.neg_outputs.len() as u32).to_le_bytes());
        for &v in c.neg_inputs.iter().chain(&c.neg_outputs) {
            out.extend_from_slice(&narrow(v, "negated pin index")?.to_le_bytes());
        }
        if let Some(period) = c.period {
            out.extend_from_slice(&period.to_le_bytes());
        }
        if let Some(mode) = c.mode {
            out.push(match mode {
                ClkMode::Pulse => 0,
                ClkMode::Square => 1,
            });
        }
        if let Some(read) = c.read {
            out.push(match read {
                RamRead::Async => 0,
                RamRead::Clocked => 1,
            });
        }
        if let Some(data) = &c.data {
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
        }
        if let Some(capacity) = c.capacity {
            out.extend_from_slice(&capacity.to_le_bytes());
        }
        if let Some(record_ticks) = c.record_ticks {
            out.push(record_ticks.into());
        }
        if let Some(data_bus) = c.data_bus {
            out.extend_from_slice(&data_bus.to_le_bytes());
        }
    }
    Ok(out)
}

/// Parse a board descriptor from `.lgb` binary. Validates the magic/version, that every length
/// field is backed by enough bytes, and that the parameter mask and the enum/flag bytes hold known
/// values, so a truncated or corrupt file errors rather than panicking. What the fields say — type
/// ids, link ids, arity, pin indices, which parameters a type takes — is left to
/// [`Board::compile`](crate::Board::compile), exactly like every other way a board comes in.
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
    let comp_count = r.u32()?;

    let mut components = Vec::new();
    for i in 0..comp_count {
        let ty = r.u16()?.into();
        let params = r.u16()?;
        if params >= DATA_BUS << 1 {
            return Err(SimError::BadBinary(format!(
                "component {i}: unknown parameter bits in 0x{params:04X}"
            )));
        }
        let in_count = r.u32()? as usize;
        let out_count = r.u32()? as usize;
        let inputs = r.u32_vec(in_count)?;
        let outputs = r.u32_vec(out_count)?;
        let neg_in_count = r.u32()? as usize;
        let neg_out_count = r.u32()? as usize;
        let neg_inputs = r.u16_vec(neg_in_count)?;
        let neg_outputs = r.u16_vec(neg_out_count)?;
        let has = |bit| params & bit != 0;
        let bad = |what: &str, v: u8| {
            SimError::BadBinary(format!(
                "component {i}: {what} byte {v} is not a known value"
            ))
        };
        let period = has(PERIOD).then(|| r.u32()).transpose()?;
        let mode = match has(MODE).then(|| r.u8()).transpose()? {
            None => None,
            Some(0) => Some(ClkMode::Pulse),
            Some(1) => Some(ClkMode::Square),
            Some(v) => return Err(bad("mode", v)),
        };
        let read = match has(READ).then(|| r.u8()).transpose()? {
            None => None,
            Some(0) => Some(RamRead::Async),
            Some(1) => Some(RamRead::Clocked),
            Some(v) => return Err(bad("read", v)),
        };
        let data = if has(DATA) {
            let n = r.u32()? as usize;
            Some(r.take(n)?.to_vec())
        } else {
            None
        };
        let capacity = has(CAPACITY).then(|| r.u32()).transpose()?;
        let record_ticks = match has(RECORD_TICKS).then(|| r.u8()).transpose()? {
            None => None,
            Some(0) => Some(false),
            Some(1) => Some(true),
            Some(v) => return Err(bad("recordTicks", v)),
        };
        let data_bus = has(DATA_BUS).then(|| r.u32()).transpose()?;
        components.push(ComponentDescriptor {
            ty,
            inputs,
            outputs,
            neg_inputs,
            neg_outputs,
            period,
            mode,
            read,
            data,
            capacity,
            record_ticks,
            data_bus,
        });
    }

    Ok(BoardDescriptor { components })
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

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
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

    /// Read `n` little-endian `u16`s (the negated-pin index arrays), bounds-checked as a whole span,
    /// widened to the descriptor's `u32`.
    fn u16_vec(&mut self, n: usize) -> Result<Vec<u32>> {
        let span = self.take(n.checked_mul(2).ok_or_else(|| {
            SimError::BadBinary(format!("element count {n} overflows the byte length"))
        })?)?;
        Ok(span
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]).into())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardBuilder, CompType};

    /// A board spanning several arities (no inputs, multi-output, a parameter) round-trips
    /// byte-for-byte back to an equal descriptor.
    fn sample() -> BoardDescriptor {
        let mut b = BoardBuilder::new();
        b.component(CompType::UserInput, &[], &[0, 1]); // no inputs, 2 outputs
        b.component(CompType::And, &[0, 1], &[2]);
        b.add(ComponentDescriptor {
            period: Some(7),
            ..ComponentDescriptor::new(CompType::Clk, &[3], &[4])
        });
        b.component(CompType::Not, &[4], &[5]);
        b.finish()
    }

    #[test]
    fn round_trips() {
        let desc = sample();
        let bytes = encode_board(&desc).unwrap();
        assert_eq!(decode_board(&bytes).unwrap(), desc);
    }

    /// Non-empty `negInputs`/`negOutputs` survive the round-trip (and an un-negated
    /// component in the same board encodes its zero counts).
    #[test]
    fn round_trips_negated_pins() {
        let mut b = BoardBuilder::new();
        // NAND-ish: AND with input pin 1 and output pin 0 negated.
        b.component_neg(CompType::And, &[0, 1], &[2], &[1], &[0]);
        b.component(CompType::Not, &[2], &[3]); // un-negated neighbor
        let desc = b.finish();
        let bytes = encode_board(&desc).unwrap();
        let back = decode_board(&bytes).unwrap();
        assert_eq!(back, desc);
        assert_eq!(back.components[0].neg_inputs, vec![1]);
        assert_eq!(back.components[0].neg_outputs, vec![0]);
        assert!(back.components[1].neg_inputs.is_empty());
    }

    /// Every parameter survives the round-trip, each in its own field: they are written in bit
    /// order, so a value read into the wrong one would show here.
    #[test]
    fn round_trips_every_param() {
        let c = ComponentDescriptor {
            period: Some(0x0102_0304),
            mode: Some(ClkMode::Pulse),
            read: Some(RamRead::Clocked),
            data: Some(vec![9, 8, 7]),
            capacity: Some(0x0506_0708),
            record_ticks: Some(true),
            data_bus: Some(0x090A_0B0C),
            ..ComponentDescriptor::new(CompType::Not, &[0], &[1])
        };
        let desc = BoardDescriptor {
            components: vec![
                c.clone(),
                ComponentDescriptor {
                    data: Some(vec![]),
                    ..c
                },
            ],
        };
        assert_eq!(decode_board(&encode_board(&desc).unwrap()).unwrap(), desc);
    }

    /// A recorder (no outputs, two parameters) decodes to the same descriptor and compiles from it.
    #[test]
    fn round_trips_recorder() {
        let mut b = BoardBuilder::new();
        b.component(CompType::UserInput, &[], &[0, 1, 2]);
        b.add(ComponentDescriptor {
            capacity: Some(64),
            record_ticks: Some(true),
            ..ComponentDescriptor::new(CompType::Recorder, &[0, 1, 2], &[])
        });
        let desc = b.finish();
        let back = decode_board(&encode_board(&desc).unwrap()).unwrap();
        assert_eq!(back, desc);
        let sim = crate::Simulation::from_descriptor(&back).unwrap();
        assert_eq!(
            sim.state_layout(1).unwrap(),
            crate::StateLayout::Records {
                capacity: 64,
                ticks: true
            }
        );
    }

    #[test]
    fn header_is_well_formed() {
        let bytes = encode_board(&sample()).unwrap();
        // Pin the literal on-disk bytes (not `LGB_MAGIC.to_le_bytes()`, which would be tautological):
        // the little-endian magic begins `31 42 47 4C`, then version 3 as a u16.
        assert_eq!(&bytes[0..4], &[0x31, 0x42, 0x47, 0x4C]);
        assert_eq!(&bytes[4..6], &[0x03, 0x00]);
        // u32 component_count at offset 8.
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            4
        );
    }

    /// A parameter mask bit with no parameter, or an enum/flag byte with no value, is a corrupt
    /// file, not a default.
    #[test]
    fn rejects_unknown_param_values() {
        let mut bytes = encode_board(&sample()).unwrap();
        // The first component's mask follows its type id, right after the 12-byte header.
        bytes[14..16].copy_from_slice(&0x80u16.to_le_bytes());
        assert!(matches!(decode_board(&bytes), Err(SimError::BadBinary(_))));

        let mut b = BoardBuilder::new();
        b.add(ComponentDescriptor {
            period: Some(1),
            mode: Some(ClkMode::Square),
            ..ComponentDescriptor::new(CompType::Clk, &[0], &[1])
        });
        let mut bytes = encode_board(&b.finish()).unwrap();
        *bytes.last_mut().unwrap() = 2; // the mode byte comes last
        assert!(matches!(decode_board(&bytes), Err(SimError::BadBinary(_))));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode_board(&sample()).unwrap();
        bytes[0] ^= 0xFF;
        assert!(matches!(decode_board(&bytes), Err(SimError::BadBinary(_))));
    }

    #[test]
    fn rejects_truncation() {
        let bytes = encode_board(&sample()).unwrap();
        // Drop the last 3 bytes, cutting the final component's trailing negated-pin count short.
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

    /// The decoder leaves type ids to `compile`, which rejects an unassigned one with its index.
    #[test]
    fn unknown_type_decodes_and_fails_to_compile() {
        let mut bytes = encode_board(&sample()).unwrap();
        // The first component's type id lives right after the 12-byte header; stamp an unassigned id.
        bytes[12..14].copy_from_slice(&7u16.to_le_bytes());
        let desc = decode_board(&bytes).unwrap();
        assert!(matches!(
            crate::Board::compile(&desc),
            Err(SimError::UnknownComponentType { idx: 0, ty: 7 })
        ));
    }

    /// Ids too wide for the format's `u16` fields are refused, not truncated onto another id: the
    /// low 16 bits of these name `Not` and pin 0.
    #[test]
    fn encode_refuses_wide_ids() {
        let mut desc = sample();
        desc.components[3].ty = 0x1_0000 | CompType::Not.id();
        assert!(matches!(encode_board(&desc), Err(SimError::BadBinary(_))));

        let mut desc = sample();
        desc.components[3].neg_inputs = vec![0x1_0000];
        assert!(matches!(encode_board(&desc), Err(SimError::BadBinary(_))));
    }
}
