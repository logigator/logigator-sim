//! Binary adder kernels: half adder (10) and full adder (11).
//!
//! Both are pure combinational functions of their inputs — no per-tick state, so trivially
//! idempotent under reorder/re-execution within a tick. Output pin 0 is the sum bit, pin 1 the
//! carry. Both compute from their flat records ([`super::flat`]) and seed nothing at power-on.

use super::Kernel;

/// Half adder (type 10): `sum = a ^ b`, `carry = a & b`. Two inputs, two outputs.
pub(crate) struct HalfAdder;

impl Kernel for HalfAdder {}

/// Full adder (type 11): `sum = a ^ b ^ cin`, `carry = (a & b) | ((a ^ b) & cin)`. Three inputs
/// (a, b, cin), two outputs (sum, carry).
pub(crate) struct FullAdder;

impl Kernel for FullAdder {}
