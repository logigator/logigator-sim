//! Shared wire contracts (plan §7.1), re-exported from the crate root.
//!
//! These numeric ids are a **frozen** public contract: editors and saved boards address component
//! types by these `u16`s, and `InputEvent`/`SimState` cross the binding boundary as small ints.

/// Lifecycle of a [`Simulation`](crate::Simulation) (plan §7.1).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SimState {
    Uninitialized = 0,
    Stopped = 1,
    Running = 2,
    Stopping = 3,
}

/// How a `trigger_input` payload is applied to a `UserInput` component (plan §7.1).
///
/// `Cont` latches the outputs until changed; `Pulse` asserts them for exactly one tick then
/// auto-clears.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    /// Set-and-hold (old engine event `0`).
    Cont = 0,
    /// One-tick pulse (old engine event `1`).
    Pulse = 1,
}

impl TryFrom<u8> for InputEvent {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, u8> {
        match v {
            0 => Ok(InputEvent::Cont),
            1 => Ok(InputEvent::Pulse),
            other => Err(other),
        }
    }
}

/// Frozen numeric component-type ids (plan §7.1).
///
/// The variants present here are the implemented set; an id with no variant (one not yet
/// implemented, or one the contract never assigns) is rejected by [`CompType::try_from_u16`] with
/// [`SimError::UnknownComponentType`](crate::SimError).
#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
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
    Ram = 17,
    Decoder = 18,
    Encoder = 19,
    Mux = 20,
    Demux = 21,
    UserInput = 200,
    LedMatrix = 204,
}

impl CompType {
    /// The frozen wire id.
    #[inline]
    pub const fn id(self) -> u16 {
        self as u16
    }

    /// Map a wire id to a `CompType`.
    ///
    /// Besides the exact ids, **any id in `200..=299` except `204`** maps to `UserInput`, matching
    /// the old engine's `>=200 && <300` user-input range (`src/project.cpp:163-166`); editors emit
    /// input variants across that range. Reserved-but-unimplemented ids return `None` (the caller
    /// raises [`SimError::UnknownComponentType`](crate::SimError)).
    #[inline]
    pub const fn try_from_u16(v: u16) -> Option<Self> {
        match v {
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
            17 => Some(CompType::Ram),
            18 => Some(CompType::Decoder),
            19 => Some(CompType::Encoder),
            20 => Some(CompType::Mux),
            21 => Some(CompType::Demux),
            204 => Some(CompType::LedMatrix), // kept before the 200..=299 UserInput catch-all
            200..=299 => Some(CompType::UserInput),
            _ => None,
        }
    }
}

impl From<CompType> for u16 {
    #[inline]
    fn from(t: CompType) -> u16 {
        t.id()
    }
}

impl TryFrom<u16> for CompType {
    type Error = crate::SimError;
    #[inline]
    fn try_from(v: u16) -> Result<Self, Self::Error> {
        CompType::try_from_u16(v).ok_or(crate::SimError::UnknownComponentType(v))
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
        assert_eq!(CompType::LedMatrix.id(), 204);
    }

    #[test]
    fn user_input_range_maps_to_user_input() {
        for v in [200u16, 201, 250, 299] {
            assert_eq!(CompType::try_from_u16(v), Some(CompType::UserInput));
        }
        // 204 is the LED matrix — kept distinct from the UserInput range.
        assert_eq!(CompType::try_from_u16(204), Some(CompType::LedMatrix));
        // out of the range
        assert_eq!(CompType::try_from_u16(300), None);
    }

    #[test]
    fn reserved_unimplemented_ids_rejected() {
        // Ids with no component type in the frozen contract — stay `None` across all of phase 2.
        for v in [0u16, 7, 8, 9, 22, 300] {
            assert_eq!(CompType::try_from_u16(v), None);
        }
    }

    #[test]
    fn input_event_from_u8() {
        assert_eq!(InputEvent::try_from(0u8), Ok(InputEvent::Cont));
        assert_eq!(InputEvent::try_from(1u8), Ok(InputEvent::Pulse));
        assert_eq!(InputEvent::try_from(2u8), Err(2));
    }
}
