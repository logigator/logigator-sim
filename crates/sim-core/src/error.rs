//! Typed engine errors (plan §7.2). Codec/JSON-path variants (`BadBinary`, `Json`) land with the
//! `.lgb` codec and `from_json` entry points in plan phase 3.

use crate::CompType;

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

    /// `trigger_input` targeted a component that is not a `UserInput`.
    #[error("component {0} is not a user-input component")]
    NotAnInput(u32),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, SimError>;
