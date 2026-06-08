//! UserInput (type 200, and the `200..=299` range): an external input source with no inputs.
//!
//! Its outputs are driven only between ticks — by `trigger_input` (`Cont` latches; `Pulse` asserts
//! for one tick) handled in the simulation's between-tick section, mirroring the old
//! `UserInput::triggerUserInput` / `tickEvent` handler (`src/components/user_input.h`). The
//! per-tick `compute` therefore does nothing, and there is no power-on seed.

use super::{Kernel, TickCtx};

/// UserInput kernel: a no-op during compute (outputs are set out-of-band by `trigger_input`).
pub(crate) struct UserInput;

impl Kernel for UserInput {
    #[inline]
    fn compute_batch(_dirty: &[u32], _ctx: &mut TickCtx<'_>) {}
}
