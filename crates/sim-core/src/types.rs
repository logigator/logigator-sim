//! Shared wire contracts, re-exported from the crate root.
//!
//! These numeric ids are a **frozen** public contract: editors and saved boards address component
//! types by these `u16`s, and `SimState` crosses the binding boundary as a small int.

/// Lifecycle of a [`Simulation`](crate::Simulation).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SimState {
    Uninitialized = 0,
    Stopped = 1,
    Running = 2,
    Stopping = 3,
}

/// Frozen numeric component-type ids.
///
/// The variants present here are the implemented set. A descriptor carries the raw id; an id with
/// no variant (one not yet implemented, or one the contract never assigns) is rejected by
/// `Board::compile` with [`SimError::UnknownComponentType`](crate::SimError).
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum CompType {
    Not = 1,
    And = 2,
    Or = 3,
    Xor = 4,
    Delay = 5,
    Clk = 6,
    HalfAdder = 10,
    FullAdder = 11,
    Rom = 12,
    DFf = 13,
    JkFf = 14,
    SrFf = 15,
    Rng = 16,
    Ram = 17,
    Decoder = 18,
    Encoder = 19,
    Mux = 20,
    Demux = 21,
    UserInput = 200,
    Recorder = 201,
    AddressableRegister = 204,
}

impl CompType {
    /// The frozen wire id.
    #[inline]
    pub const fn id(self) -> u32 {
        self as u32
    }

    /// Map a wire id to a `CompType`; `None` for an id with no implemented type.
    #[inline]
    pub const fn from_id(id: u32) -> Option<Self> {
        match id {
            1 => Some(CompType::Not),
            2 => Some(CompType::And),
            3 => Some(CompType::Or),
            4 => Some(CompType::Xor),
            5 => Some(CompType::Delay),
            6 => Some(CompType::Clk),
            10 => Some(CompType::HalfAdder),
            11 => Some(CompType::FullAdder),
            12 => Some(CompType::Rom),
            13 => Some(CompType::DFf),
            14 => Some(CompType::JkFf),
            15 => Some(CompType::SrFf),
            16 => Some(CompType::Rng),
            17 => Some(CompType::Ram),
            18 => Some(CompType::Decoder),
            19 => Some(CompType::Encoder),
            20 => Some(CompType::Mux),
            21 => Some(CompType::Demux),
            200 => Some(CompType::UserInput),
            201 => Some(CompType::Recorder),
            204 => Some(CompType::AddressableRegister),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_ids() {
        assert_eq!(CompType::Not.id(), 1);
        assert_eq!(CompType::And.id(), 2);
        assert_eq!(CompType::Or.id(), 3);
        assert_eq!(CompType::Xor.id(), 4);
        assert_eq!(CompType::Delay.id(), 5);
        assert_eq!(CompType::HalfAdder.id(), 10);
        assert_eq!(CompType::FullAdder.id(), 11);
        assert_eq!(CompType::UserInput.id(), 200);
        assert_eq!(CompType::Recorder.id(), 201);
        assert_eq!(CompType::AddressableRegister.id(), 204);
    }

    #[test]
    fn user_input_maps_from_exactly_200() {
        assert_eq!(CompType::from_id(200), Some(CompType::UserInput));
        // 201 is the recorder, 204 the addressable register.
        assert_eq!(CompType::from_id(201), Some(CompType::Recorder));
        assert_eq!(CompType::from_id(204), Some(CompType::AddressableRegister));
        // Other ids that used to fall in the 200..=299 catch-all are no longer inputs.
        for v in [202, 250, 299, 300] {
            assert_eq!(CompType::from_id(v), None);
        }
    }

    #[test]
    fn from_id_round_trips_every_implemented_type() {
        // `from_id` is a hand-written match separate from the dispatch macro; this guards every
        // implemented type's wire id against a typo/omission.
        for &t in crate::components::ALL_TYPES {
            assert_eq!(
                CompType::from_id(t.id()),
                Some(t),
                "wire id {} must map back to {t:?}",
                t.id()
            );
        }
    }

    #[test]
    fn reserved_unimplemented_ids_rejected() {
        // Ids with no component type in the frozen contract stay `None`, including ids past the
        // u16 range whose low bits name a real type (0x1_0001 must not read as `Not`).
        for v in [0, 7, 8, 9, 22, 300, 0x1_0001, u32::MAX] {
            assert_eq!(CompType::from_id(v), None);
        }
    }
}
