//! Track B: the NOT-insertion settled-state oracle for negation.
//!
//! A negated port is *by definition* a zero-delay NOT. So for any board, replacing every negated
//! pin with a real (one-tick) NOT — `expand_negations` below — yields a board the already-golden
//! engine runs with no negation, and whose **settled** state on the original links must equal the
//! negated board's. It is *not* tick-exact (each inserted NOT adds a tick; that one-tick difference
//! is the whole point of the feature), so this compares only the converged fixed point.
//!
//! **Acyclic only.** With feedback the inserted NOTs lengthen each loop, so the two boards oscillate
//! at different periods and never converge. This module therefore uses a dedicated acyclic generator
//! (each gate reads only already-produced links and writes fresh ones — a DAG) over the combinational
//! palette; the cyclic generator in `proptests.rs` covers the per-tick `driver_count` invariant under
//! negation instead.

use proptest::prelude::*;
use sim_core::{BoardDescriptor, CompType, ComponentDescriptor, InputEvent, Simulation};

/// SplitMix64, a local copy (the crate's is private) — the structural/input PRNG for the generator.
fn splitmix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A combinational palette entry: `(type, input count, output count)`.
fn gate_kind(sel: u64, fanin: u64) -> (CompType, usize, usize) {
    let wide = 2 + (fanin % 3) as usize; // 2..=4 inputs
    match sel % 7 {
        0 => (CompType::Not, 1, 1),
        1 => (CompType::Delay, 1, 1),
        2 => (CompType::And, wide, 1),
        3 => (CompType::Or, wide, 1),
        4 => (CompType::Xor, wide, 1),
        5 => (CompType::HalfAdder, 2, 2),
        _ => (CompType::FullAdder, 3, 2),
    }
}

fn mask_to_indices(mask: u64, count: usize) -> Vec<u16> {
    (0..count)
        .filter(|&i| (mask >> i) & 1 == 1)
        .map(|i| i as u16)
        .collect()
}

fn comp(
    ty: CompType,
    inputs: Vec<u32>,
    outputs: Vec<u32>,
    neg_in: Vec<u16>,
    neg_out: Vec<u16>,
) -> ComponentDescriptor {
    ComponentDescriptor {
        ty,
        inputs,
        outputs,
        ops: vec![],
        neg_inputs: neg_in,
        neg_outputs: neg_out,
    }
}

/// A random acyclic combinational board with per-pin negation. The first `max(n_inputs, 1)`
/// components are un-negated `UserInput` sources (so their ids align with the expanded board); each
/// subsequent gate reads only already-produced links and writes fresh links — a DAG.
fn gen_acyclic(n_inputs: u32, n_gates: u32, seed: u64) -> BoardDescriptor {
    let mut s = seed ^ 0x1357_9BDF_2468_ACE0;
    let mut rng = || {
        s = splitmix(s);
        s
    };

    let mut comps: Vec<ComponentDescriptor> = Vec::new();
    let mut produced: Vec<u32> = Vec::new();
    let mut next_link: u32 = 0;

    for _ in 0..n_inputs.max(1) {
        let l = next_link;
        next_link += 1;
        comps.push(comp(CompType::UserInput, vec![], vec![l], vec![], vec![]));
        produced.push(l);
    }

    for _ in 0..n_gates {
        let (ty, n_in, n_out) = gate_kind(rng(), rng());
        let inputs: Vec<u32> = (0..n_in)
            .map(|_| produced[(rng() % produced.len() as u64) as usize])
            .collect();
        let outputs: Vec<u32> = (0..n_out)
            .map(|_| {
                let l = next_link;
                next_link += 1;
                l
            })
            .collect();
        let neg_in = mask_to_indices(rng(), n_in);
        let neg_out = mask_to_indices(rng(), n_out);
        produced.extend_from_slice(&outputs);
        comps.push(comp(ty, inputs, outputs, neg_in, neg_out));
    }

    BoardDescriptor {
        link_count: next_link,
        components: comps,
    }
}

/// Replace every negated pin with a real (one-tick) NOT, yielding an equivalent **un-negated** board:
/// a negated input pin reading `L` gets `NOT(L) -> L'` with the pin repointed to `L'`; a negated
/// output pin driving `L` is repointed to a fresh `L''` with `NOT(L'') -> L` appended. UserInputs
/// (the leading, un-negated components) pass through unchanged, so their ids match the source board.
fn expand_negations(desc: &BoardDescriptor) -> BoardDescriptor {
    let mut next_link = desc.link_count;
    let mut out: Vec<ComponentDescriptor> = Vec::new();
    let not =
        |reads: u32, drives: u32| comp(CompType::Not, vec![reads], vec![drives], vec![], vec![]);

    for c in &desc.components {
        let mut inputs = c.inputs.clone();
        for &pin in &c.neg_inputs {
            let l = inputs[pin as usize];
            let lp = next_link;
            next_link += 1;
            out.push(not(l, lp));
            inputs[pin as usize] = lp;
        }
        let mut outputs = c.outputs.clone();
        let mut post: Vec<ComponentDescriptor> = Vec::new();
        for &pin in &c.neg_outputs {
            let l = outputs[pin as usize];
            let lpp = next_link;
            next_link += 1;
            outputs[pin as usize] = lpp;
            post.push(not(lpp, l));
        }
        out.push(comp(c.ty, inputs, outputs, vec![], vec![]));
        out.extend(post);
    }

    BoardDescriptor {
        link_count: next_link,
        components: out,
    }
}

/// Drive the same primary inputs into the negated board and its NOT-expanded twin, settle both, and
/// assert their link state agrees on every original link.
fn settled_states_agree(desc: &BoardDescriptor, in_seed: u64) -> Result<(), TestCaseError> {
    let exp = expand_negations(desc);
    let mut sim_n = Simulation::from_descriptor(desc).expect("compile negated");
    let mut sim_e = Simulation::from_descriptor(&exp).expect("compile expanded");

    let n_inputs = desc
        .components
        .iter()
        .filter(|c| c.ty == CompType::UserInput)
        .count() as u32;
    let mut s = in_seed | 1;
    for i in 0..n_inputs {
        s = splitmix(s);
        let v = (s & 1) == 1;
        sim_n.trigger_input(i, InputEvent::Cont, &[v]).unwrap();
        sim_e.trigger_input(i, InputEvent::Cont, &[v]).unwrap();
    }

    // Settle past the longest path of the *expanded* board (each component contributes at most one
    // tick to a path, and the inserted NOTs only lengthen it).
    let settle = exp.components.len() as u64 * 2 + 64;
    for _ in 0..settle {
        sim_n.tick();
        sim_e.tick();
    }

    for l in 0..desc.link_count {
        prop_assert_eq!(
            sim_n.link(l),
            sim_e.link(l),
            "settled link {} differs: negated vs NOT-expanded",
            l
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Settled link state of a random acyclic negated board equals that of its NOT-expanded twin.
    #[test]
    fn settled_state_matches_not_insertion(
        n_inputs in 1u32..=5,
        n_gates in 0u32..=24,
        struct_seed in any::<u64>(),
        in_seed in any::<u64>(),
    ) {
        let desc = gen_acyclic(n_inputs, n_gates, struct_seed);
        settled_states_agree(&desc, in_seed)?;
    }
}

/// A hand-built case kept deterministic for fast debugging: a NAND (AND + negated output) fed by two
/// inputs, one of which is also negated. Its settled state must match the explicit NOT expansion.
#[test]
fn deterministic_nand_with_negated_input() {
    let desc = BoardDescriptor {
        link_count: 3,
        components: vec![
            comp(CompType::UserInput, vec![], vec![0], vec![], vec![]),
            comp(CompType::UserInput, vec![], vec![1], vec![], vec![]),
            comp(CompType::And, vec![0, 1], vec![2], vec![1], vec![0]),
        ],
    };
    for in_seed in [0u64, 1, 2, 3, 0xDEAD_BEEF, u64::MAX] {
        settled_states_agree(&desc, in_seed).unwrap();
    }
}
