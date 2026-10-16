//! Property tests over random boards. An in-crate `#[cfg(test)]` module so the invariant checks
//! can read engine internals (`driver_count`, `output_link`) the public API hides.
//!
//! The invariant under test: after every tick, `driver_count[l]` equals the number of
//! currently-powered outputs driving link `l` — i.e. the incremental count never drifts from the
//! literal `popcount(drivers)` the wired-OR would gather.
//!
//! Boards are drawn from an "easy-arity" palette — gates, adders, the three flip-flops (the JK's
//! live self-toggle exercises a kernel reading its own output), the per-component-seeded RNG, and
//! `UserInput` sources, with outputs allowed to collide so wired-OR buses arise. The
//! data/ops/2ⁿ-arity types (DEC/DEMUX/MUX/RAM/ROM/CLK/LED) are awkward to generate randomly and are
//! covered deterministically by the corpus golden test instead.

use crate::scratch::splitmix64;
use crate::{BoardDescriptor, CompType, ComponentDescriptor, InputFrame, InputWrite, Simulation};
use core::sync::atomic::Ordering::Relaxed;
use proptest::prelude::*;

/// A `ComponentDescriptor` with no ops and no negated pins (every palette type takes none).
fn cd(ty: CompType, inputs: Vec<u32>, outputs: Vec<u32>) -> ComponentDescriptor {
    ComponentDescriptor {
        ty: ty.id(),
        inputs,
        outputs,
        ops: vec![],
        neg_inputs: vec![],
        neg_outputs: vec![],
    }
}

/// `n` random link ids in `0..l`.
fn links(l: u32, n: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0..l, n)
}

/// One random component over a board with `l` links, valid by construction for its type's arity,
/// with a random per-pin negation pattern folded in so the invariant covers the negation paths.
fn component(l: u32) -> impl Strategy<Value = ComponentDescriptor> {
    let base = prop_oneof![
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
        // RNG: enable input + 1..=4 outputs.
        (0..l, links(l, 1..=4)).prop_map(|(en, o)| cd(CompType::Rng, vec![en], o)),
        links(l, 1..=3).prop_map(|o| cd(CompType::UserInput, vec![], o)),
    ];
    // Fold a random per-pin negate pattern in: bit `i` of each mask negates pin `i` (palette pins are
    // all `< 32`, so a `u32` mask covers every pin). The `^0` identity keeps an empty mask byte-equal.
    (base, any::<u32>(), any::<u32>()).prop_map(|(mut c, in_mask, out_mask)| {
        c.neg_inputs = (0..c.inputs.len() as u32)
            .filter(|&i| (in_mask >> i) & 1 == 1)
            .collect();
        c.neg_outputs = (0..c.outputs.len() as u32)
            .filter(|&i| (out_mask >> i) & 1 == 1)
            .collect();
        c
    })
}

/// A random board: 2..=24 links, 1..=30 components, plus a `u64` seed for the input frames.
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

/// Queue seed-derived frames on every `UserInput` (deterministic per (seed, board)): a few frames
/// each, mixing events and coalescing frames, holds of 1–3 and `None` (keep) entries, so the input
/// service drives pins on later ticks too.
fn submit_inputs(sim: &mut Simulation, board: &BoardDescriptor, seed: u64) {
    for (i, comp) in board.components.iter().enumerate() {
        if comp.ty != CompType::UserInput.id() {
            continue;
        }
        let mut w = splitmix64(seed ^ i as u64);
        let width = comp.outputs.len();
        let states: Vec<Vec<Option<bool>>> = (0..1 + w % 4)
            .map(|_| {
                w = splitmix64(w);
                (0..width)
                    .map(|p| match (w >> (2 * p)) & 3 {
                        0 => None,
                        b => Some(b & 1 == 1),
                    })
                    .collect()
            })
            .collect();
        let frames: Vec<InputFrame<'_>> = states
            .iter()
            .enumerate()
            .map(|(k, state)| InputFrame {
                state,
                hold: 1 + ((w >> (40 + 2 * k)) % 3) as u32,
                coalesce: (w >> (50 + k)) & 1 == 1,
            })
            .collect();
        sim.submit_input(&[InputWrite {
            comp: i as u32,
            replace: false,
            frames: &frames,
        }])
        .expect("valid input write");
    }
}

/// Oracle: recompute each link's powered-driver count from the **driven** output values
/// (`output_state ^ negate`) + the driven link id and assert it matches the incrementally-
/// maintained `driver_count`. Counting the driven (not logical) value is what validates the
/// negate math, including wired-OR of mixed-polarity drivers and the no-underflow property.
fn assert_driver_count_matches(sim: &Simulation) {
    let mut expected = vec![0u32; sim.link_count as usize];
    for o in 0..sim.output_state.bits() {
        if sim.output_state.get(o) ^ sim.board.output_negated(o) {
            expected[sim.board.output_link_id(o) as usize] += 1;
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

    /// The incremental `driver_count` equals the literal popcount of powered (driven) drivers after
    /// every tick — including on the cyclic generator's random per-pin negation patterns.
    #[test]
    fn driver_count_never_drifts((board, seed) in board_and_seed()) {
        let mut sim = Simulation::from_descriptor(&board).expect("compile");
        submit_inputs(&mut sim, &board, seed);
        assert_driver_count_matches(&sim);
        for _ in 0..24 {
            sim.tick();
            assert_driver_count_matches(&sim);
        }
    }
}
