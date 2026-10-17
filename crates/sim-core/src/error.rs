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
    #[error("component {idx}: unknown component type id {ty}")]
    UnknownComponentType { idx: u32, ty: u32 },

    /// A component referenced a link id of `2^31` or more, so the id cannot reserve its top bit for
    /// the negate flag packed into the input/output CSR words.
    #[error(
        "component {idx}: link id {link} exceeds the highest supported, {max} (top bit reserved for negation)"
    )]
    LinkIdOverflow { idx: u32, link: u32, max: u32 },

    /// More outputs drive one link than its driver count can hold.
    #[error("link {link}: {count} outputs drive it, more than the {max} supported")]
    TooManyDrivers { link: u32, count: u32, max: u32 },

    /// A component's input/output counts violate its type's arity.
    #[error("component {idx} ({ty:?}): bad arity in={ins} out={outs}")]
    BadArity {
        idx: u32,
        ty: CompType,
        ins: usize,
        outs: usize,
    },

    /// A component's per-type parameter (named as in JSON) is not one its type takes, is missing,
    /// or is out of range.
    #[error("component {idx} ({ty:?}): {param} {reason}")]
    BadParam {
        idx: u32,
        ty: CompType,
        param: &'static str,
        reason: &'static str,
    },

    /// A component's `negInputs`/`negOutputs` referenced a pin index outside its arity.
    #[error("component {idx}: negated {pin_kind} index {pin} out of range (count={count})")]
    NegateOutOfRange {
        idx: u32,
        pin_kind: PinKind,
        pin: u32,
        count: u32,
    },

    /// An input write targeted a component that is out of range or not a `UserInput`.
    #[error("component {0} is not a user-input component")]
    NotAnInput(u32),

    /// An input frame's `state` does not have exactly one entry per output pin of its input.
    #[error("input {comp} frame {frame}: state has {got} entries, the input has {want} pins")]
    InputWidth {
        comp: u32,
        frame: usize,
        got: usize,
        want: u32,
    },

    /// An input frame's `state` entry is not `true`, `false` or `null`. Raised by the JS bindings
    /// only; a Rust `Option<bool>` cannot hold anything else.
    #[error("input {comp} frame {frame}: state[{pin}] must be true, false or null")]
    InputState { comp: u32, frame: usize, pin: usize },

    /// An input frame's `hold` is 0 (or, from JS, not an integer in `1..=2^32-1`).
    #[error("input {comp} frame {frame}: hold must be an integer of at least 1")]
    InputHold { comp: u32, frame: usize },

    /// The same input appears twice in one request.
    #[error("input {0} appears more than once in one request")]
    DuplicateInput(u32),

    /// A write would leave more than [`MAX_PENDING_EVENTS`](crate::MAX_PENDING_EVENTS) event
    /// frames pending on its input.
    #[error(
        "input {comp}: {pending} pending + {adding} new event frames exceed the limit of {limit}"
    )]
    InputQueueFull {
        comp: u32,
        pending: u32,
        adding: usize,
        limit: u32,
    },

    /// A component id is not below the board's component count.
    #[error("component {0} out of range")]
    ComponentOutOfRange(u32),

    /// A state byte range does not lie within the component's byte state (any non-empty range on a
    /// type without byte state, or an `offset + len` that overflows).
    #[error("component {comp}: state range {offset}+{len} exceeds its {size} state bytes")]
    StateRange {
        comp: u32,
        offset: usize,
        len: usize,
        size: usize,
    },

    /// `write_state` on a component whose state is not writable.
    #[error("component {0}: state is read-only")]
    StateReadOnly(u32),

    /// A records read targeted a component that is not a recorder.
    #[error("component {0} is not a recorder")]
    NotARecorder(u32),

    /// A records read asked for records from `since` on, but the recorder has only written `count`:
    /// `since` is a count from another simulation.
    #[error("recorder {comp}: since {since} is past its {count} records")]
    RecordsAhead { comp: u32, since: u64, count: u64 },

    /// A records read passed a tick buffer to a recorder that does not record ticks, or none to one
    /// that does.
    #[error("recorder {comp}: {}", if *.records_ticks { "records ticks, pass a tick buffer" } else { "does not record ticks, pass no tick buffer" })]
    RecordTicks { comp: u32, records_ticks: bool },

    /// A `.lgb` binary board was truncated or carried a bad header/field.
    #[error("malformed .lgb binary: {0}")]
    BadBinary(String),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, SimError>;
