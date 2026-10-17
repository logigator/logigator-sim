//! Invalid links: a link whose drivers disagree, or that an SR flip-flop drives after an edge with S
//! and R both high, is flagged invalid. The flag is information for the host only: the link keeps
//! its wired-OR value, the circuit reads that value, and nothing downstream is flagged.

use sim_core::{BoardBuilder, BoardDescriptor, InputFrame, InputWrite, Simulation, SnapshotConfig};

/// Drive every pin of input `comp` (one frame) and run `n` ticks.
fn set(sim: &mut Simulation, comp: u32, pins: &[bool], n: u64) {
    let state: Vec<Option<bool>> = pins.iter().map(|&v| Some(v)).collect();
    let frames = [InputFrame {
        state: &state,
        hold: 1,
        coalesce: false,
    }];
    sim.submit_input(&[InputWrite {
        comp,
        replace: false,
        frames: &frames,
    }])
    .unwrap();
    for _ in 0..n {
        sim.tick();
    }
}

fn invalid(sim: &Simulation) -> Vec<u32> {
    sim.invalid_links().collect()
}

/// Inputs 0 and 1 both drive link 0; a NOT (component 2) reads it into link 1.
fn shared_wire(neg_second: bool) -> BoardDescriptor {
    use sim_core::CompType::*;
    let mut b = BoardBuilder::new(2);
    b.component(UserInput, &[], &[0], &[]);
    let neg: &[u32] = if neg_second { &[0] } else { &[] };
    b.component_neg(UserInput, &[], &[0], &[], &[], neg);
    b.component(Not, &[0], &[1], &[]);
    b.finish()
}

/// A shared link is invalid exactly while its drivers disagree. Its value is still the wired-OR,
/// which the NOT reads, and the NOT's own output is not flagged.
#[test]
fn disagreeing_drivers_flag_the_link() {
    let mut sim = Simulation::from_descriptor(&shared_wire(false)).unwrap();
    for (a, b, want) in [
        (false, false, false),
        (true, false, true),
        (true, true, false),
        (false, true, true),
        (false, false, false),
    ] {
        set(&mut sim, 0, &[a], 0);
        set(&mut sim, 1, &[b], 3);
        assert_eq!(sim.link_invalid(0), want, "drivers {a} {b}");
        assert_eq!(
            sim.link(0),
            a || b,
            "drivers {a} {b}: the value is the wired-OR"
        );
        assert_eq!(
            sim.link(1),
            !(a || b),
            "drivers {a} {b}: the NOT reads the wired-OR"
        );
        assert!(
            !sim.link_invalid(1),
            "drivers {a} {b}: the NOT's output is valid"
        );
    }
}

/// A negated output drives high at rest, so it disagrees with a plain low driver from power-on,
/// from the first tick, when the link itself first reads high.
#[test]
fn power_on_disagreement_is_flagged() {
    let mut sim = Simulation::from_descriptor(&shared_wire(true)).unwrap();
    assert!(
        !sim.link(0) && !sim.link_invalid(0),
        "nothing read before the first tick"
    );
    sim.tick();
    assert!(sim.link(0) && sim.link_invalid(0));
}

/// Input 0 drives S, the clock and R of an SR flip-flop (component 1), whose Q and Q̄ are links 3
/// and 4.
fn sr() -> Simulation {
    use sim_core::CompType::*;
    let mut b = BoardBuilder::new(5);
    b.component(UserInput, &[], &[0, 1, 2], &[]);
    b.component(SrFf, &[0, 1, 2], &[3, 4], &[]);
    Simulation::from_descriptor(&b.finish()).unwrap()
}

/// One rising clock edge with S and R as given, then the clock back low.
fn clock(sim: &mut Simulation, s: bool, r: bool) {
    set(sim, 0, &[s, false, r], 2);
    set(sim, 0, &[s, true, r], 3);
    set(sim, 0, &[s, false, r], 3);
}

/// S and R together set Q and flag both outputs invalid; an edge with neither holds the invalid
/// state, and an edge with only S or only R leaves it.
#[test]
fn sr_with_set_and_reset_is_invalid() {
    let mut sim = sr();
    clock(&mut sim, true, true);
    assert!(sim.link(3) && !sim.link(4), "S wins: Q high, Q̄ low");
    assert_eq!(invalid(&sim), [3, 4]);

    clock(&mut sim, false, false);
    assert_eq!(invalid(&sim), [3, 4], "holding keeps the invalid state");

    clock(&mut sim, false, true);
    assert!(!sim.link(3) && sim.link(4), "R resets");
    assert!(invalid(&sim).is_empty(), "R alone leaves the invalid state");

    clock(&mut sim, true, true);
    clock(&mut sim, true, false);
    assert!(sim.link(3) && !sim.link(4));
    assert!(
        invalid(&sim).is_empty(),
        "S alone leaves it too, with Q unchanged"
    );
}

/// Both kinds of snapshot list every invalid link, not only those that changed since the last one.
#[test]
fn snapshots_list_every_invalid_link() {
    let mut sim = Simulation::from_descriptor(&shared_wire(false)).unwrap();
    let delta = SnapshotConfig {
        delta: true,
        delta_threshold: 1.0,
    };
    sim.snapshot(delta);
    set(&mut sim, 0, &[true], 3);
    assert!(sim.snapshot(delta).is_delta);
    assert_eq!(sim.snapshot_invalid(), [0]);
    assert!(sim.snapshot(delta).is_delta);
    assert_eq!(
        sim.snapshot_invalid(),
        [0],
        "still listed with nothing changed"
    );
    sim.snapshot(SnapshotConfig::default());
    assert_eq!(sim.snapshot_invalid(), [0]);
    set(&mut sim, 1, &[true], 3);
    sim.snapshot(delta);
    assert!(sim.snapshot_invalid().is_empty());
}
