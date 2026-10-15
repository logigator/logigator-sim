//! UserInput (type 200): an external input source with no inputs.
//!
//! Its outputs are driven only by the frames the host queues with `Simulation::submit_input`, which
//! the input service applies at the start of a tick (`crate::input`). The per-tick `compute`
//! therefore does nothing, and there is no power-on seed.

use super::{Kernel, TickCtx};

/// UserInput kernel: a no-op during compute (outputs are set by the input service).
pub(crate) struct UserInput;

impl Kernel for UserInput {
    #[inline]
    fn compute_batch(_dirty: &[u32], _ctx: &mut TickCtx<'_>) {}
}
