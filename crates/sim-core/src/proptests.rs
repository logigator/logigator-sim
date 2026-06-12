//! Property tests over random boards. An in-crate `#[cfg(test)]` module so the invariant checks
//! can read engine internals (`driver_count`, `output_link`) the public API hides.
//!
//! The invariant under test: after every tick, `driver_count[l]` equals the number of
//! currently-powered outputs driving link `l` — i.e. the incremental count never drifts from the
//! literal `popcount(drivers)` the wired-OR would gather.
//!
//! Boards are drawn from an "easy-arity" palette — gates, adders, the three flip-flops (the JK's
//! live self-toggle exercises a kernel reading its own output), the per-component-seeded RNG (the
//! one stateful kernel with no corpus board), and `UserInput` sources, with outputs allowed to
//! collide so wired-OR buses arise. The data/ops/2ⁿ-arity types (DEC/DEMUX/MUX/RAM/ROM/CLK/LED) are
//! awkward to generate randomly and are covered deterministically by the corpus golden test instead.

use crate::scratch::splitmix64;
use crate::{BoardDescriptor, CompType, ComponentDescriptor, Simulation};
use core::sync::atomic::Ordering::Relaxed;
use proptest::prelude::*;

/// A `ComponentDescriptor` with no ops (every palette type takes none).
fn cd(ty: CompType, inputs: Vec<u32>, outputs: Vec<u32>) -> ComponentDescriptor {
    ComponentDescriptor {
        ty,
        inputs,
        outputs,
        ops: vec![],
    }
}

/// `n` random link ids in `0..l`.
fn links(l: u32, n: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0..l, n)
}

/// One random component over a board with `l` links, valid by construction for its type's arity.
fn component(l: u32) -> impl Strategy<Value = ComponentDescriptor> {
    prop_oneof![
        (0..l, 0..l).prop_map(|(i, o)| cd(CompType::Not, vec![i], vec![o])),
        (links(l, 2..=4), 0..l).prop_map(|(i, o)| cd(CompType::And, i, vec![o])),
        (links(l, 2..=4), 0..l).prop_map(|(i, o)| cd(CompType::Or, i, vec![o])),
        (links(l, 2..=4), 0..l).prop_map(|(i, o)| cd(CompType::Xor, i, vec![o])),
        (0..l, 0..l).prop_map(|(i, o)| cd(CompType::Delay, vec![i], vec![o])),
        (links(l, 2..=2), links(l, 2..=2)).prop_map(|(i, o)| cd(CompType::HalfAdder, i, o)),
        (links(l, 3..=3), links(l, 2..=2)).prop_map(|(i, o)| cd(CompType::FullAdder, i, o)),
        (links(l, 2..=2), links(l, 2..=2)).prop_map(|(i, o)| cd(CompType::DFf, i, o)),
        (links(l, 3..=3), links(l, 2..=2)).prop_map(|(i, o)| cd(CompType::JkFf, i, o)),
        (links(l, 3..=3), links(l, 2..=2)).prop_map(|(i, o)| cd(CompType::SrFf, i, o)),
        // RNG: enable input + 1..=4 outputs. Output is a pure function of (seed, tick), so it is the
        // one stateful kernel with no corpus board — include it here so the invariant covers it too.
        (0..l, links(l, 1..=4)).prop_map(|(en, o)| cd(CompType::Rng, vec![en], o)),
        links(l, 1..=3).prop_map(|o| cd(CompType::UserInput, vec![], o)),
    ]
}

/// A random board: 2..=24 links, 1..=30 components, plus a `u64` seed for the input triggers.
fn board_and_seed() -> impl Strategy<Value = (BoardDescriptor, u64)> {
    (2u32..=24)
        .prop_flat_map(|l| {
            prop::collection::vec(component(l), 1..=30).prop_map(move |components| {
                BoardDescriptor {
                    link_count: l,
                    components,
                }
            })
        })
        .prop_flat_map(|board| (Just(board), any::<u64>()))
}

/// Latch every `UserInput` to a seed-derived bit pattern at tick 0 (deterministic per (seed, board)).
fn apply_inputs(sim: &mut Simulation, board: &BoardDescriptor, seed: u64) {
    for (i, comp) in board.components.iter().enumerate() {
        if comp.ty == CompType::UserInput {
            let w = splitmix64(seed ^ i as u64);
            let state: Vec<bool> = (0..comp.outputs.len()).map(|p| (w >> p) & 1 == 1).collect();
            sim.trigger_input(i as u32, crate::InputEvent::Cont, &state)
                .expect("UserInput trigger");
        }
    }
}

/// Oracle: recompute each link's powered-driver count from `output_state` + `output_link` and
/// assert it matches the incrementally-maintained `driver_count`.
fn assert_driver_count_matches(sim: &Simulation) {
    let mut expected = vec![0u32; sim.link_count as usize];
    for o in 0..sim.output_state.bits() {
        if sim.output_state.get(o) {
            expected[sim.board.output_link[o as usize] as usize] += 1;
        }
    }
    for (l, &exp) in expected.iter().enumerate() {
        let dc = sim.driver_count[l].load(Relaxed) as u32;
        assert_eq!(
            dc, exp,
            "driver_count[{l}] = {dc}, but {exp} outputs drive it powered"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// The incremental `driver_count` equals the literal popcount of powered drivers after
    /// every tick.
    #[test]
    fn driver_count_never_drifts((board, seed) in board_and_seed()) {
        let mut sim = Simulation::from_descriptor(&board).expect("compile");
        apply_inputs(&mut sim, &board, seed);
        assert_driver_count_matches(&sim);
        for _ in 0..24 {
            sim.tick();
            assert_driver_count_matches(&sim);
        }
    }
}
