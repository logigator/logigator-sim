//! Targeted negation semantics, pinned against hand-reasoned expectations.
//!
//! The golden corpus has no negation (it exercises only the `^0` identity path), so these tests are
//! the primary correctness gate for the feature: each pins one of the power-on / per-pin rules that
//! the always-on negate read introduces. A negated port is, by definition, a zero-delay NOT folded
//! into the read/drive step, so every expectation here is what the equivalent NOT-inserted circuit
//! would settle to.

use sim_core::{BoardBuilder, CompType, InputEvent, Simulation};

/// Run enough ticks for a small combinational board to settle.
fn settle(sim: &mut Simulation) {
    for _ in 0..6 {
        sim.tick();
    }
}

/// NAND = AND with a negated output. The link rests **high** (both inputs low → AND low → driven
/// high); driving both inputs high pulls it low.
#[test]
fn nand_via_negated_output() {
    let mut b = BoardBuilder::new(3);
    let a = b.component(CompType::UserInput, &[], &[0], &[]);
    let c = b.component(CompType::UserInput, &[], &[1], &[]);
    let nand = b.component_neg(CompType::And, &[0, 1], &[2], &[], &[], &[0]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    settle(&mut sim);
    assert!(sim.link(2), "NAND output rests high (inputs low)");
    assert!(sim.output(nand, 0), "driven output pin reads high at rest");

    sim.trigger_input(a, InputEvent::Cont, &[true]).unwrap();
    sim.trigger_input(c, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(!sim.link(2), "both inputs high → NAND low");
    assert!(!sim.output(nand, 0), "driven output pin reads low");
}

/// Buffer = NOT with a negated output: `!in ^ 1 = in`. The output follows the input, resting low.
#[test]
fn buffer_via_negated_not_output() {
    let mut b = BoardBuilder::new(2);
    let src = b.component(CompType::UserInput, &[], &[0], &[]);
    let buf = b.component_neg(CompType::Not, &[0], &[1], &[], &[], &[0]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    settle(&mut sim);
    assert!(!sim.link(1), "buffer of a low input rests low (no power-on glitch high)");
    assert!(!sim.output(buf, 0), "driven output pin reads low");

    sim.trigger_input(src, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(sim.link(1), "buffer follows the input high");
    assert!(sim.output(buf, 0), "driven output pin reads high");
}

/// A negated output and a straight output share one link (wired-OR): the link is
/// `(!a) | b`. Verifies the driven-value `driver_count` math composes across mixed-polarity drivers.
#[test]
fn wired_or_of_negated_and_straight() {
    // Bus link2 is driven by two sources: `OR(a,a)` with a negated output drives `!(a|a) = !a`, and
    // UserInput `b` drives `b` straight onto the same link. The wired-OR of the two is `(!a) | b`.
    let mut b = BoardBuilder::new(3);
    let a = b.component(CompType::UserInput, &[], &[0], &[]);
    let bsrc = b.component(CompType::UserInput, &[], &[2], &[]); // straight driver on the bus
    b.component_neg(CompType::Or, &[0, 0], &[2], &[], &[], &[0]); // drives !a onto the bus
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    let check = |sim: &mut Simulation, av: bool, bv: bool| {
        sim.trigger_input(a, InputEvent::Cont, &[av]).unwrap();
        sim.trigger_input(bsrc, InputEvent::Cont, &[bv]).unwrap();
        settle(sim);
        assert_eq!(sim.link(2), (!av) | bv, "wired-OR (!a)|b for a={av} b={bv}");
    };
    check(&mut sim, false, false); // (!0)|0 = 1
    check(&mut sim, true, false); // (!1)|0 = 0
    check(&mut sim, true, true); // (!1)|1 = 1
    check(&mut sim, false, true); // (!0)|1 = 1
}

/// NOR = AND with both inputs negated: `(!a) & (!b) = !(a|b)`. Rests **high** (both inputs low) — the
/// §8(c) power-on compute seeds the non-default rest output; the link only flips low once any input
/// goes high.
#[test]
fn nor_via_both_inputs_negated_and() {
    let mut b = BoardBuilder::new(3);
    let a = b.component(CompType::UserInput, &[], &[0], &[]);
    let c = b.component(CompType::UserInput, &[], &[1], &[]);
    let nor = b.component_neg(CompType::And, &[0, 1], &[2], &[], &[0, 1], &[]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    settle(&mut sim);
    assert!(sim.link(2), "NOR rests high (both inputs low)");
    assert!(sim.output(nor, 0));

    sim.trigger_input(a, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(!sim.link(2), "one input high → NOR low");

    sim.trigger_input(a, InputEvent::Cont, &[false]).unwrap();
    sim.trigger_input(c, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(!sim.link(2), "the other input high → still NOR low");
}

/// A negated *input* on a NOT is a buffer: `!(in ^ 1) = in`. The §8(c) pass corrects the NOT's
/// power-on high seed down to the effective rest value (low input → low output).
#[test]
fn not_with_negated_input_is_buffer() {
    let mut b = BoardBuilder::new(2);
    let src = b.component(CompType::UserInput, &[], &[0], &[]);
    let buf = b.component_neg(CompType::Not, &[0], &[1], &[], &[0], &[]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    settle(&mut sim);
    assert!(!sim.link(1), "negated-input NOT (a buffer) of a low input rests low");
    assert!(!sim.output(buf, 0));

    sim.trigger_input(src, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(sim.link(1), "buffer follows the input high");
    assert!(sim.output(buf, 0));
}

/// A per-pin (non-reduce) kernel with a negated input: a half adder whose `a` input is negated
/// computes `sum = (!a) ^ b`, `carry = (!a) & b`. At rest (`a=b=0`) the effective `a` is high, so the
/// sum pin rests high — seeded by §8(c).
#[test]
fn half_adder_with_negated_input() {
    let mut b = BoardBuilder::new(4);
    let a = b.component(CompType::UserInput, &[], &[0], &[]);
    let bsrc = b.component(CompType::UserInput, &[], &[1], &[]);
    let ha = b.component_neg(CompType::HalfAdder, &[0, 1], &[2, 3], &[], &[0], &[]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    let check = |sim: &mut Simulation, av: bool, bv: bool| {
        sim.trigger_input(a, InputEvent::Cont, &[av]).unwrap();
        sim.trigger_input(bsrc, InputEvent::Cont, &[bv]).unwrap();
        settle(sim);
        let ea = !av;
        assert_eq!(sim.link(2), ea ^ bv, "sum=(!a)^b a={av} b={bv}");
        assert_eq!(sim.link(3), ea & bv, "carry=(!a)&b a={av} b={bv}");
        assert_eq!(sim.output(ha, 0), ea ^ bv);
        assert_eq!(sim.output(ha, 1), ea & bv);
    };
    check(&mut sim, false, false); // sum=1, carry=0
    check(&mut sim, true, false); // sum=0, carry=0
    check(&mut sim, false, true); // sum=0, carry=1
    check(&mut sim, true, true); // sum=1, carry=0
}

/// A negated **clock** inverts the trigger sense: the D flip-flop becomes falling-edge, with no
/// power-on latch and Q̄ rest-high preserved.
#[test]
fn negated_clock_dff_is_falling_edge() {
    let mut b = BoardBuilder::new(4);
    let data = b.component(CompType::UserInput, &[], &[0], &[]);
    let clk = b.component(CompType::UserInput, &[], &[1], &[]);
    // Clock is pin 1; mark it negated.
    let dff = b.component_neg(CompType::DFf, &[0, 1], &[2, 3], &[], &[1], &[]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    sim.trigger_input(data, InputEvent::Cont, &[true]).unwrap(); // hold D = 1
    settle(&mut sim);
    assert!(!sim.output(dff, 0), "no power-on latch: Q low");
    assert!(sim.output(dff, 1), "Q̄ rests high");

    // Real RISING edge on the clock → effective falling → must NOT latch.
    sim.trigger_input(clk, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(!sim.output(dff, 0), "real rising edge does not latch a negated-clock DFF");

    // Real FALLING edge → effective rising → latches D = 1.
    sim.trigger_input(clk, InputEvent::Cont, &[false]).unwrap();
    settle(&mut sim);
    assert!(sim.output(dff, 0), "real falling edge latches D=1");
    assert!(!sim.output(dff, 1), "Q̄ low after latch");
}

/// A negated **data** input on a D flip-flop latches the inverted data on each (real, un-negated)
/// rising clock.
#[test]
fn negated_data_dff_latches_inverted() {
    let mut b = BoardBuilder::new(4);
    let data = b.component(CompType::UserInput, &[], &[0], &[]);
    let clk = b.component(CompType::UserInput, &[], &[1], &[]);
    let dff = b.component_neg(CompType::DFf, &[0, 1], &[2, 3], &[], &[0], &[]); // data pin 0 negated
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    // D = 0 (effective 1); rising clock latches Q = 1.
    sim.trigger_input(data, InputEvent::Cont, &[false]).unwrap();
    settle(&mut sim);
    sim.trigger_input(clk, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(sim.output(dff, 0), "latched !D = !0 = 1");

    // Drop clock, set D = 1 (effective 0); next rising clock latches Q = 0.
    sim.trigger_input(clk, InputEvent::Cont, &[false]).unwrap();
    settle(&mut sim);
    sim.trigger_input(data, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    sim.trigger_input(clk, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert!(!sim.output(dff, 0), "latched !D = !1 = 0");
}

/// A negated **enable** inverts the CLK freeze gate: a real low enable (effective high) freezes the
/// clock at power-on; the clock runs only once the real enable goes high (effective low).
#[test]
fn negated_enable_clk_frozen_until_real_enable_high() {
    let mut b = BoardBuilder::new(2);
    let en = b.component(CompType::UserInput, &[], &[0], &[]);
    // period 1, enable pin 0 negated.
    b.component_neg(CompType::Clk, &[0], &[1], &[1], &[0], &[]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    // Real enable low (effective high) → frozen: the output never toggles.
    for _ in 0..20 {
        sim.tick();
        assert!(!sim.link(1), "negated-enable CLK stays frozen while the real enable is low");
    }

    // Real enable high (effective low) → runs.
    sim.trigger_input(en, InputEvent::Cont, &[true]).unwrap();
    let mut toggled = false;
    for _ in 0..20 {
        sim.tick();
        toggled |= sim.link(1);
    }
    assert!(toggled, "CLK runs once the real enable goes high");
}

/// A negated **enable** inverts the RNG draw gate: it draws at power-on (effective enable high) and
/// on the real enable's falling edges, and holds while the real enable is high.
#[test]
fn negated_enable_rng_draws_at_power_on() {
    let mut b = BoardBuilder::new(9);
    let en = b.component(CompType::UserInput, &[], &[0], &[]);
    b.component_neg(
        CompType::Rng,
        &[0],
        &[1, 2, 3, 4, 5, 6, 7, 8],
        &[],
        &[0],
        &[],
    );
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();

    let read = |s: &Simulation| (0..8).fold(0u8, |acc, l| acc | ((s.link(1 + l) as u8) << l));

    settle(&mut sim);
    let v0 = read(&sim);
    assert_ne!(v0, 0, "negated-enable RNG draws at power-on (effective enable high)");

    // Real enable high (effective low) → holds the drawn value.
    sim.trigger_input(en, InputEvent::Cont, &[true]).unwrap();
    settle(&mut sim);
    assert_eq!(read(&sim), v0, "RNG holds while the real enable is high");

    // Real enable falling edge (effective rising) → draws a new value.
    sim.trigger_input(en, InputEvent::Cont, &[false]).unwrap();
    settle(&mut sim);
    assert_ne!(read(&sim), v0, "real enable falling edge draws a new value");
}
