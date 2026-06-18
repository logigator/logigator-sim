//! Typed engine errors. `BadBinary` covers the `.lgb` codec path; JSON parsing stays in the
//! binding/CLI layer (`serde_json`) rather than wrapped here.

use crate::CompType;

/// Which side of a component a negated pin index refers to, for error reporting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinKind {
    Input,
    Output,
}

impl core::fmt::Display for PinKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PinKind::Input => f.write_str("input"),
            PinKind::Output => f.write_str("output"),
        }
    }
}

/// Errors raised while compiling a board or driving a simulation.
#[derive(thiserror::Error, Debug)]
pub enum SimError {
    /// A component carried a type id that is not a known/implemented `CompType`.
    #[error("unknown component type id {0}")]
    UnknownComponentType(u16),

    /// A component referenced a link id outside `0..link_count`.
    #[error("component {idx}: link id {link} out of range (link_count={count})")]
    LinkOutOfRange { idx: u32, link: u32, count: u32 },

    /// A component's input/output/ops counts violate its type's arity.
    #[error("component {idx} ({ty:?}): bad arity in={ins} out={outs} ops={ops}")]
    BadArity {
        idx: u32,
        ty: CompType,
        ins: usize,
        outs: usize,
        ops: usize,
    },

    /// A component's `negInputs`/`negOutputs` referenced a pin index outside its arity.
    #[error("component {idx}: negated {pin_kind} index {pin} out of range (count={count})")]
    NegateOutOfRange {
        idx: u32,
        pin_kind: PinKind,
        pin: u16,
        count: u32,
    },

    /// `trigger_input` targeted a component that is not a `UserInput`.
    #[error("component {0} is not a user-input component")]
    NotAnInput(u32),

    /// A `.lgb` binary board was truncated or carried a bad header/field.
    #[error("malformed .lgb binary: {0}")]
    BadBinary(String),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, SimError>;
