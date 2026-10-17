//! Timed input frames (`submit_input` / `input_progress`): timing, spacing, coalescing, keep
//! (`None`), replace, negated pins, validation and the queue limit — and the key property that the
//! trace does not depend on how the ticks are batched.

use proptest::prelude::*;
use sim_core::{
    BoardBuilder, BoardDescriptor, CompType, ComponentDescriptor, InputFrame, InputWrite,
    MAX_PENDING_EVENTS, RunConfig, SimError, Simulation,
};

/// An owned frame, so tests can build requests without juggling borrows.
#[derive(Clone, Debug)]
struct F {
    state: Vec<Option<bool>>,
    hold: u32,
    coalesce: bool,
}

/// Pin pattern shorthand, pin 0 first: `1` drive high, `0` drive low, `-` keep.
fn pins(p: &str) -> Vec<Option<bool>> {
    p.chars()
        .map(|c| match c {
            '1' => Some(true),
            '0' => Some(false),
            '-' => None,
            _ => panic!("bad pin char {c:?}"),
        })
        .collect()
}

/// An event frame.
fn ev(p: &str, hold: u32) -> F {
    F {
        state: pins(p),
        hold,
        coalesce: false,
    }
}

/// A coalescing frame.
fn co(p: &str) -> F {
    F {
        state: pins(p),
        hold: 1,
        coalesce: true,
    }
}

/// Submit one request of `(comp, replace, frames)` writes.
fn submit_many(sim: &mut Simulation, writes: &[(u32, bool, &[F])]) -> Result<(), SimError> {
    let frames: Vec<Vec<InputFrame<'_>>> = writes
        .iter()
        .map(|(_, _, fs)| {
            fs.iter()
                .map(|f| InputFrame {
                    state: &f.state,
                    hold: f.hold,
                    coalesce: f.coalesce,
                })
                .collect()
        })
        .collect();
    let writes: Vec<InputWrite<'_>> = writes
        .iter()
        .zip(&frames)
        .map(|(&(comp, replace, _), frames)| InputWrite {
            comp,
            replace,
            frames,
        })
        .collect();
    sim.submit_input(&writes)
}

fn submit(sim: &mut Simulation, comp: u32, frames: &[F]) {
    submit_many(sim, &[(comp, false, frames)]).unwrap();
}

fn replace(sim: &mut Simulation, comp: u32, frames: &[F]) {
    submit_many(sim, &[(comp, true, frames)]).unwrap();
}

/// The driven values of `comp`'s first `n` output pins as a `0`/`1` string, pin 0 first.
fn out(sim: &Simulation, comp: u32, n: usize) -> String {
    (0..n)
        .map(|p| if sim.output(comp, p) { '1' } else { '0' })
        .collect()
}

/// Links `range` as a `0`/`1` string.
fn wires(sim: &Simulation, range: std::ops::Range<u32>) -> String {
    range.map(|l| if sim.link(l) { '1' } else { '0' }).collect()
}

fn progress(sim: &Simulation) -> Vec<u32> {
    let mut p = vec![0; sim.input_count() as usize];
    sim.input_progress(&mut p);
    p
}

/// A single input of `width` pins driving links `0..width`.
fn one_input(width: u32) -> Simulation {
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &(0..width).collect::<Vec<_>>());
    Simulation::from_descriptor(&b.finish()).unwrap()
}

/// Tick until `n` ticks have elapsed in total.
fn tick_to(sim: &mut Simulation, n: u64) {
    while sim.tick_count() < n {
        sim.tick();
    }
}

// --- keyboard: text round trip ---------------------------------------------------------------

/// Keyboard: one input with 7 data bits (pins 0–6, links 0–6) and a strobe (pin 7, link 7), plus
/// seven D flip-flops latching the data bits on the strobe's rising edge (Q on links 8–14).
fn keyboard() -> Simulation {
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &(0..8).collect::<Vec<_>>());
    for bit in 0..7 {
        b.component(CompType::DFf, &[bit, 7], &[8 + bit, 15 + bit]);
    }
    Simulation::from_descriptor(&b.finish()).unwrap()
}

/// A press (data + strobe) and a release (data only) per character.
fn type_text(text: &str, hold: u32) -> Vec<F> {
    text.bytes()
        .flat_map(|ch| {
            let data: String = (0..7)
                .map(|bit| if ch >> bit & 1 == 1 { '1' } else { '0' })
                .collect();
            [ev(&format!("{data}1"), hold), ev(&format!("{data}0"), hold)]
        })
        .collect()
}

/// The data bits read off links 0–6.
fn data_on_wires(sim: &Simulation) -> u8 {
    (0..7).fold(0, |acc, bit| acc | (sim.link(bit) as u8) << bit)
}

/// The byte the flip-flops latched (Q pins of components 1–7).
fn latched(sim: &Simulation) -> u8 {
    (0..7).fold(0, |acc, bit| acc | (sim.output(1 + bit, 0) as u8) << bit)
}

const TEXT: &str = "Hello, World! ~{|}";

/// Paused, one `tick()` at a time: reading the data wires on every rising strobe edge returns the
/// text, and the progress count drains by one per applied frame.
#[test]
fn text_round_trip_single_ticks() {
    for hold in 1..=3 {
        let mut sim = keyboard();
        submit(&mut sim, 0, &type_text(TEXT, hold));
        assert_eq!(progress(&sim), [2 * TEXT.len() as u32]);
        let mut got = Vec::new();
        let mut strobe = false;
        for _ in 0..(2 * TEXT.len() as u32 * hold + 4) {
            sim.tick();
            if sim.link(7) && !strobe {
                got.push(data_on_wires(&sim));
            }
            strobe = sim.link(7);
        }
        assert_eq!(String::from_utf8(got).unwrap(), TEXT, "hold {hold}");
        assert_eq!(progress(&sim), [0]);
    }
}

/// Under `run()` batches of one character's worth of ticks, the circuit's own latch holds each
/// character in turn: the sequencing is counted inside the tick loop, not per host call.
#[test]
fn text_round_trip_in_run_batches() {
    for hold in 1..=3 {
        let mut sim = keyboard();
        submit(&mut sim, 0, &type_text(TEXT, hold));
        for (i, ch) in TEXT.bytes().enumerate() {
            sim.run(RunConfig {
                ticks: 2 * hold as u64,
                timeout: None,
            })
            .unwrap();
            assert_eq!(latched(&sim), ch, "hold {hold}, char {i}");
        }
    }
}

// --- timing and spacing -----------------------------------------------------------------------

/// The keyboard example: `'A'` with hold 2 submitted at tick count 10.
#[test]
fn timing_table() {
    let mut sim = keyboard();
    tick_to(&mut sim, 10);
    submit(
        &mut sim,
        0,
        &[ev("10000011", 2), ev("10000010", 2), ev("00000000", 2)],
    );
    let expected = [
        (11, "10000011"),
        (12, "10000011"),
        (13, "10000010"),
        (14, "10000010"),
        (15, "00000000"),
        (16, "00000000"),
    ];
    for (tick, p) in expected {
        sim.tick();
        assert_eq!(sim.tick_count(), tick);
        assert_eq!(out(&sim, 0, 8), p, "pins after tick {tick}");
        assert_eq!(wires(&sim, 0..8), p, "wires after tick {tick}");
    }
    // The latch computes on the strobe edge in tick 11; its output reaches its wire after tick 12.
    let mut sim = keyboard();
    tick_to(&mut sim, 10);
    submit(&mut sim, 0, &[ev("10000011", 2)]);
    sim.tick();
    assert!(
        sim.output(1, 0) && !sim.link(8),
        "after 11: latch pin set, wire not yet"
    );
    sim.tick();
    assert!(sim.link(8), "after 12: latch wire set");
}

/// Every frame that has a successor is on the pins and the wires for exactly `hold` ticks; the
/// last frame stays.
#[test]
fn exact_spacing() {
    for hold in 1..=3u32 {
        let mut sim = one_input(2);
        sim.tick();
        let seq = ["10", "01", "11", "00", "10"];
        let frames: Vec<F> = seq.iter().map(|p| ev(p, hold)).collect();
        submit(&mut sim, 0, &frames);
        let mut seen = Vec::new();
        for _ in 0..(seq.len() as u32 * hold + 5) {
            sim.tick();
            assert_eq!(out(&sim, 0, 2), wires(&sim, 0..2), "pins and wires agree");
            seen.push(out(&sim, 0, 2));
        }
        let mut expected: Vec<String> = seq[..seq.len() - 1]
            .iter()
            .flat_map(|p| std::iter::repeat_n(p.to_string(), hold as usize))
            .collect();
        expected.resize(seen.len(), seq[seq.len() - 1].to_string());
        assert_eq!(seen, expected, "hold {hold}");
    }
}

/// A frame submitted at tick count t is on the pins and the wire after tick t + 1; a NOT fed by the
/// input shows the inverse on its wire after tick t + 2.
#[test]
fn input_and_consumer_delay() {
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &[0]);
    b.component(CompType::Not, &[0], &[1]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
    tick_to(&mut sim, 5);
    assert!(sim.link(1), "NOT of the idle input settled high");
    submit(&mut sim, 0, &[ev("1", 1)]);
    assert!(
        !sim.output(0, 0) && !sim.link(0),
        "nothing applied before a tick"
    );
    sim.tick();
    assert!(sim.output(0, 0) && sim.link(0), "after t+1: pin and wire");
    assert!(sim.link(1), "after t+1: NOT wire not yet");
    sim.tick();
    assert!(!sim.link(1), "after t+2: NOT wire follows");
}

/// On a wire shared with a gate, the input's delay is one tick whatever the gate does: the wire is
/// always the OR of the input's pin after this tick and the gate's pin after the previous one —
/// including the ticks right after the gate changed, where a write could otherwise be picked up a
/// tick early or a cancelling change show a glitch.
#[test]
fn delay_on_a_wire_shared_with_a_gate() {
    // Link 0 is driven by the input and by a CLK (period 1: high every other tick, a change every
    // tick). Link 1 is the CLK's enable, held low.
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &[0]);
    b.add(ComponentDescriptor {
        period: Some(1),
        ..ComponentDescriptor::new(CompType::Clk, &[1], &[0])
    });
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
    let mut clk_prev = sim.output(1, 0);
    // Submit after every tick so writes meet both rising and falling clock edges.
    let script = [
        "1", "0", "1", "1", "0", "0", "1", "0", "0", "1", "1", "1", "0",
    ];
    for (i, p) in script.iter().enumerate() {
        submit(&mut sim, 0, &[ev(p, 1)]);
        sim.tick();
        assert_eq!(
            sim.link(0),
            sim.output(0, 0) || clk_prev,
            "step {i}: wire = input pin now | clock pin one tick ago"
        );
        clk_prev = sim.output(1, 0);
    }
}

// --- coalescing ------------------------------------------------------------------------------

/// Coalescing frames submitted between two ticks reach the circuit once, as the last value.
#[test]
fn coalescing_reaches_circuit_as_last_value() {
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &[0]);
    b.component(CompType::Not, &[0], &[1]); // observer: recomputes only if link 0 flips
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
    tick_to(&mut sim, 3);
    // A switch flipped five times while paused ends where it started: nothing reaches the wire.
    for p in ["1", "0", "1", "1", "0"] {
        submit(&mut sim, 0, &[co(p)]);
    }
    for _ in 0..3 {
        sim.tick();
        assert!(!sim.link(0) && sim.link(1), "no flip reached the circuit");
    }
    for p in ["0", "1", "0", "1"] {
        submit(&mut sim, 0, &[co(p)]);
    }
    sim.tick();
    assert!(sim.link(0), "the final position, applied on the next tick");
    sim.tick();
    assert!(!sim.link(1));
    assert_eq!(progress(&sim), [0], "coalescing frames never count");
}

/// The gamepad example: A pressed then B pressed before the next tick merge, keeping A where the
/// later frame has `None`; releasing A before that tick too leaves only B down.
#[test]
fn coalescing_merge_keeps_earlier_pins() {
    let mut sim = one_input(2);
    submit(&mut sim, 0, &[co("1-")]);
    submit(&mut sim, 0, &[co("-1")]);
    sim.tick();
    assert_eq!(out(&sim, 0, 2), "11", "both buttons down after one step");

    let mut sim = one_input(2);
    submit(&mut sim, 0, &[co("1-")]);
    submit(&mut sim, 0, &[co("-1")]);
    submit(&mut sim, 0, &[co("0-")]);
    sim.tick();
    assert_eq!(out(&sim, 0, 2), "01");
}

/// A coalescing frame never merges into a frame that is already applied, nor past a pending event.
#[test]
fn coalescing_never_merges_into_applied_or_past_event() {
    // Applied: the first frame is on the pins with its hold running; the next waits it out.
    let mut sim = one_input(1);
    submit(&mut sim, 0, &[F { hold: 3, ..co("1") }]);
    sim.tick();
    assert_eq!(out(&sim, 0, 1), "1");
    submit(&mut sim, 0, &[co("0")]);
    sim.tick();
    sim.tick();
    assert_eq!(out(&sim, 0, 1), "1", "applied frame keeps its hold");
    sim.tick();
    assert_eq!(
        out(&sim, 0, 1),
        "0",
        "the later level follows after the hold"
    );

    // Past an event: [level 1, event 0] then level 1 again — the event still shows.
    let mut sim = one_input(1);
    submit(&mut sim, 0, &[co("1"), ev("0", 1)]);
    submit(&mut sim, 0, &[co("1")]);
    let mut seen = String::new();
    for _ in 0..4 {
        sim.tick();
        seen += &out(&sim, 0, 1);
    }
    assert_eq!(seen, "1011");
}

/// Without `replace`, every event frame reaches the pins: each distinct event shows for its tick
/// and the pending count drains to zero, while coalescing frames in between never block one.
#[test]
fn every_event_reaches_the_pins() {
    let mut sim = one_input(4);
    let mut expected = Vec::new();
    for v in 0..16u32 {
        let p: String = (0..4)
            .map(|b| if v >> b & 1 == 1 { '1' } else { '0' })
            .collect();
        submit(&mut sim, 0, &[ev(&p, 1)]);
        submit(&mut sim, 0, &[co("----")]);
        expected.push(p);
    }
    assert_eq!(progress(&sim), [16]);
    let mut seen = Vec::new();
    while progress(&sim)[0] > 0 {
        sim.tick();
        seen.push(out(&sim, 0, 4));
    }
    seen.dedup();
    expected.dedup();
    assert_eq!(seen, expected);
}

// --- keep (`None`) ----------------------------------------------------------------------------

/// A `None` pin keeps the value it has when the frame is applied — including a value written by a
/// frame that was still pending when the keeping frame was submitted.
#[test]
fn none_resolves_when_applied() {
    let mut sim = one_input(2);
    submit(&mut sim, 0, &[ev("11", 2)]);
    // Submitted while `11` is still pending: pin 0 must keep the `1` that frame writes, not the
    // `0` on the pins right now.
    submit(&mut sim, 0, &[ev("-0", 1)]);
    assert_eq!(out(&sim, 0, 2), "00");
    sim.tick();
    sim.tick();
    sim.tick();
    assert_eq!(out(&sim, 0, 2), "10");
}

/// An all-`None` frame changes no pin and delays the next frame by its hold.
#[test]
fn all_none_frame_waits() {
    let mut sim = one_input(2);
    submit(&mut sim, 0, &[ev("10", 1), ev("--", 3), ev("01", 1)]);
    let mut seen = Vec::new();
    for _ in 0..6 {
        sim.tick();
        seen.push(out(&sim, 0, 2));
    }
    assert_eq!(seen, ["10", "10", "10", "10", "01", "01"]);
}

// --- negated pins ----------------------------------------------------------------------------

/// Frames carry logical values: a strobe in `negOutputs` rests high on its wire, goes low while a
/// frame with strobe = 1 is applied and high again after strobe = 0. `output()` reports the wire
/// value, and a `None` keeps the logical value.
#[test]
fn negated_strobe() {
    let mut b = BoardBuilder::new();
    b.component_neg(CompType::UserInput, &[], &[0, 1], &[], &[1]); // data, strobe (negated)
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
    sim.tick();
    assert!(sim.link(1) && sim.output(0, 1), "negated strobe rests high");

    submit(&mut sim, 0, &[ev("11", 1), ev("0-", 1), ev("-0", 1)]);
    sim.tick();
    assert_eq!(wires(&sim, 0..2), "10", "strobe = 1 drives the wire low");
    assert_eq!(out(&sim, 0, 2), "10", "output() reports the wire value");
    sim.tick();
    assert_eq!(
        wires(&sim, 0..2),
        "00",
        "None keeps the logical 1, so the wire stays low"
    );
    sim.tick();
    assert_eq!(
        wires(&sim, 0..2),
        "01",
        "strobe = 0 drives the wire high again"
    );
    assert_eq!(sim.output_bytes(), [0, 1]);
}

// --- replace ---------------------------------------------------------------------------------

/// Replace drops unapplied frames and preempts a running hold: the reset is on the pins on the
/// next tick, and a frame submitted after it waits out the reset's hold.
#[test]
fn replace_preempts() {
    let mut sim = one_input(2);
    submit(&mut sim, 0, &[ev("11", 100), ev("01", 1), ev("10", 1)]);
    sim.tick();
    assert_eq!(out(&sim, 0, 2), "11");
    assert_eq!(progress(&sim), [2]);
    replace(&mut sim, 0, &[ev("00", 2)]);
    assert_eq!(progress(&sim), [1], "the two pending events were dropped");
    submit(&mut sim, 0, &[ev("01", 1)]);
    sim.tick();
    assert_eq!(
        out(&sim, 0, 2),
        "00",
        "reset applied on the next tick despite the hold"
    );
    sim.tick();
    assert_eq!(out(&sim, 0, 2), "00", "the reset's hold still runs");
    sim.tick();
    assert_eq!(out(&sim, 0, 2), "01");
    assert_eq!(progress(&sim), [0]);
}

/// Replace with no frames cancels only: the pins keep their current values, and a frame
/// submitted afterwards applies on the next tick even though the cancelled hold had not run out.
#[test]
fn replace_with_no_frames_cancels() {
    let mut sim = one_input(1);
    submit(&mut sim, 0, &[ev("1", 50), ev("0", 1)]);
    sim.tick();
    replace(&mut sim, 0, &[]);
    assert_eq!(progress(&sim), [0]);
    for _ in 0..60 {
        sim.tick();
        assert_eq!(out(&sim, 0, 1), "1", "pins keep their values");
    }
    let mut sim = one_input(1);
    submit(&mut sim, 0, &[ev("1", 50)]);
    sim.tick();
    replace(&mut sim, 0, &[]);
    submit(&mut sim, 0, &[ev("0", 1)]);
    sim.tick();
    assert_eq!(out(&sim, 0, 1), "0", "the cancelled hold no longer delays");
}

// --- independence ----------------------------------------------------------------------------

/// A long hold on one input never delays another, and two inputs due on the same tick are both
/// applied in it.
#[test]
fn inputs_are_independent() {
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &[0]);
    b.component(CompType::UserInput, &[], &[1]);
    let mut sim = Simulation::from_descriptor(&b.finish()).unwrap();
    submit_many(
        &mut sim,
        &[
            (0, false, &[ev("1", 10), ev("0", 1)]),
            (1, false, &[ev("1", 1), ev("0", 1)]),
        ],
    )
    .unwrap();
    sim.tick();
    assert_eq!(wires(&sim, 0..2), "11", "both applied in the same tick");
    sim.tick();
    assert_eq!(
        wires(&sim, 0..2),
        "10",
        "input 1 is not held back by input 0"
    );
    assert_eq!(progress(&sim), [1, 0]);
}

// --- validation and the queue limit ----------------------------------------------------------

/// Every invalid entry refuses the whole request, even after valid writes in it: queues, progress
/// and pins are unchanged.
#[test]
fn validation_is_all_or_nothing() {
    let mut b = BoardBuilder::new();
    b.component(CompType::UserInput, &[], &[0, 1]);
    b.component(CompType::Not, &[0], &[2]);
    b.component(CompType::UserInput, &[], &[3]);
    let desc = b.finish();
    let fresh = || {
        let mut sim = Simulation::from_descriptor(&desc).unwrap();
        submit(&mut sim, 0, &[ev("10", 3)]);
        sim
    };
    let good: &[F] = &[ev("11", 1)];
    let zero_hold: &[F] = &[ev("1", 1), ev("0", 0)];
    type Case<'a> = (Vec<(u32, bool, &'a [F])>, fn(&SimError) -> bool);
    let cases: Vec<Case> = vec![
        (vec![(0, true, good), (1, false, good)], |e| {
            matches!(e, SimError::NotAnInput(1))
        }),
        (vec![(0, true, good), (9, false, good)], |e| {
            matches!(e, SimError::NotAnInput(9))
        }),
        (vec![(0, true, good), (2, false, good)], |e| {
            matches!(
                e,
                SimError::InputWidth {
                    comp: 2,
                    frame: 0,
                    got: 2,
                    want: 1
                }
            )
        }),
        (vec![(2, false, zero_hold)], |e| {
            matches!(e, SimError::InputHold { comp: 2, frame: 1 })
        }),
        (vec![(0, true, good), (0, false, good)], |e| {
            matches!(e, SimError::DuplicateInput(0))
        }),
    ];
    for (i, (writes, is_expected)) in cases.into_iter().enumerate() {
        let mut sim = fresh();
        let err = submit_many(&mut sim, &writes).unwrap_err();
        assert!(is_expected(&err), "case {i}: unexpected {err:?}");
        assert_eq!(progress(&sim), [1, 0], "case {i}: progress untouched");
        for _ in 0..5 {
            sim.tick();
        }
        assert_eq!(
            out(&sim, 0, 2),
            "10",
            "case {i}: only the earlier frame applied"
        );
        assert_eq!(out(&sim, 2, 1), "0", "case {i}: input 2 untouched");
    }
}

/// The per-input limit counts pending event frames only: a write reaching exactly the limit is
/// accepted, one more is refused whole, `replace` on a full queue is accepted, and coalescing
/// frames never count.
#[test]
fn queue_limit() {
    let max = MAX_PENDING_EVENTS as usize;
    let mut sim = one_input(1);
    submit(&mut sim, 0, &vec![ev("1", 1); max - 1]);
    submit(&mut sim, 0, &[co("0"), ev("1", 1), co("0"), co("1")]);
    assert_eq!(progress(&sim), [MAX_PENDING_EVENTS], "exactly at the limit");

    let err = submit_many(&mut sim, &[(0, false, &[co("1"), ev("0", 1)])]).unwrap_err();
    assert!(matches!(
        err,
        SimError::InputQueueFull {
            comp: 0,
            pending,
            adding: 1,
            limit: MAX_PENDING_EVENTS,
        } if pending == MAX_PENDING_EVENTS
    ));
    submit(&mut sim, 0, &[co("1"), co("0")]);
    assert_eq!(
        progress(&sim),
        [MAX_PENDING_EVENTS],
        "coalescing frames are not limited"
    );

    replace(&mut sim, 0, &vec![ev("0", 1); max]);
    assert_eq!(
        progress(&sim),
        [MAX_PENDING_EVENTS],
        "replace starts from an empty queue"
    );

    let mut sim = one_input(1);
    let err = submit_many(&mut sim, &[(0, true, &vec![ev("0", 1); max + 1])]).unwrap_err();
    assert!(matches!(err, SimError::InputQueueFull { pending: 0, .. }));
    assert_eq!(progress(&sim), [0]);
}

/// Inputs wider than 64 pins span several mask words; pins on both sides of each word boundary
/// are driven, kept and merged independently.
#[test]
fn wide_inputs() {
    let width = 130;
    let mut sim = one_input(width);
    let pattern = |f: &dyn Fn(usize) -> char| (0..width as usize).map(f).collect::<String>();
    let a = pattern(&|p| if p % 3 == 0 { '1' } else { '0' });
    let keep_low_words = pattern(&|p| if p < 64 { '-' } else { '1' });
    submit(&mut sim, 0, &[ev(&a, 1), ev(&keep_low_words, 1)]);
    sim.tick();
    assert_eq!(out(&sim, 0, width as usize), a);
    assert_eq!(wires(&sim, 0..width), a);
    sim.tick();
    let expected = pattern(&|p| if p >= 64 || p % 3 == 0 { '1' } else { '0' });
    assert_eq!(out(&sim, 0, width as usize), expected);

    submit(
        &mut sim,
        0,
        &[co(&pattern(&|p| if p == 64 { '0' } else { '-' }))],
    );
    submit(
        &mut sim,
        0,
        &[co(&pattern(&|p| if p == 63 { '1' } else { '-' }))],
    );
    sim.tick();
    let expected = pattern(&|p| match p {
        63 => '1',
        64 => '0',
        _ if p >= 64 || p % 3 == 0 => '1',
        _ => '0',
    });
    assert_eq!(out(&sim, 0, width as usize), expected);
}

// --- the key property: same trace however it runs -------------------------------------------

/// One scripted submission: at tick count `at`, a request of `(input index, replace, frames)`.
#[derive(Clone, Debug)]
struct Step {
    at: u64,
    writes: Vec<(usize, bool, Vec<F>)>,
}

/// A random board: `widths.len()` inputs (some wide), each pin driving its own link — except that
/// some pins share a link with a pin of another input (wired-OR) — some pins negated, and a
/// layer of gates and flip-flops over those links so the inputs' wires have consumers.
fn random_board(widths: &[u32], seed: u64) -> BoardDescriptor {
    let mut s = seed;
    let mut next = || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s >> 33
    };
    let pins_total: u32 = widths.iter().sum();
    let gates = 12;
    let mut components = Vec::new();
    let mut first = 0;
    for &w in widths {
        let outputs: Vec<u32> = (0..w)
            .map(|p| {
                if next() % 8 == 0 {
                    (next() % pins_total as u64) as u32
                } else {
                    first + p
                }
            })
            .collect();
        let neg_outputs = (0..w).filter(|_| next() % 4 == 0).collect();
        components.push(ComponentDescriptor {
            ty: CompType::UserInput.id(),
            inputs: vec![],
            outputs,
            neg_outputs,
            ..Default::default()
        });
        first += w;
    }
    for g in 0..gates {
        // Read any input pin's link or an earlier gate's output: no cycles.
        let out = pins_total + g * 2;
        let (a, b) = ((next() % out as u64) as u32, (next() % out as u64) as u32);
        let (ty, inputs, outputs) = match next() % 4 {
            0 => (CompType::Not, vec![a], vec![out]),
            1 => (CompType::Xor, vec![a, b], vec![out]),
            2 => (CompType::And, vec![a, b], vec![out]),
            _ => (CompType::DFf, vec![a, b], vec![out, out + 1]),
        };
        components.push(ComponentDescriptor::new(ty, &inputs, &outputs));
    }
    BoardDescriptor { components }
}

fn frame_strategy(width: u32) -> impl Strategy<Value = F> {
    (
        prop::collection::vec(
            prop_oneof![Just(None), Just(Some(false)), Just(Some(true))],
            width as usize,
        ),
        1u32..=3,
        any::<bool>(),
    )
        .prop_map(|(state, hold, coalesce)| F {
            state,
            hold,
            coalesce,
        })
}

fn script_strategy(widths: Vec<u32>) -> impl Strategy<Value = Vec<Step>> {
    let n = widths.len();
    let write = (0..n).prop_flat_map(move |i| {
        (
            Just(i),
            prop::bool::weighted(0.15),
            prop::collection::vec(frame_strategy(widths[i]), 0..=5),
        )
    });
    let step = (0u64..6, prop::collection::vec(write, 1..=3)).prop_map(|(gap, mut writes)| {
        // One write per input per request (duplicates are refused).
        let mut seen = std::collections::HashSet::new();
        writes.retain(|w| seen.insert(w.0));
        Step { at: gap, writes }
    });
    prop::collection::vec(step, 1..=12).prop_map(|mut steps| {
        // `at` was a gap; make it a running tick count.
        let mut t = 0;
        for s in &mut steps {
            t += s.at;
            s.at = t;
        }
        steps
    })
}

/// A plain model of the queue semantics, kept independent of the engine's packed representation:
/// per input, a list of frames, the logical pin values and the tick the next frame may apply on.
struct Model {
    queues: Vec<std::collections::VecDeque<F>>,
    pins: Vec<Vec<bool>>,
    ready_at: Vec<u64>,
}

impl Model {
    fn submit(&mut self, writes: &[(usize, bool, Vec<F>)]) {
        for (i, replace, frames) in writes {
            let q = &mut self.queues[*i];
            if *replace {
                q.clear();
                self.ready_at[*i] = 0;
            }
            for f in frames {
                match q.back_mut() {
                    Some(tail) if f.coalesce && tail.coalesce => {
                        for (t, n) in tail.state.iter_mut().zip(&f.state) {
                            if n.is_some() {
                                *t = *n;
                            }
                        }
                        tail.hold = f.hold;
                    }
                    _ => q.push_back(f.clone()),
                }
            }
        }
    }

    /// Apply the frames due in tick `n`.
    fn tick(&mut self, n: u64) {
        for i in 0..self.queues.len() {
            if n >= self.ready_at[i] {
                if let Some(f) = self.queues[i].pop_front() {
                    for (p, s) in self.pins[i].iter_mut().zip(&f.state) {
                        if let Some(v) = s {
                            *p = *v;
                        }
                    }
                    self.ready_at[i] = n + f.hold as u64;
                }
            }
        }
    }

    fn pending_events(&self) -> Vec<u32> {
        self.queues
            .iter()
            .map(|q| q.iter().filter(|f| !f.coalesce).count() as u32)
            .collect()
    }
}

fn submit_step(sim: &mut Simulation, step: &Step, input_comp: &[u32]) {
    let writes: Vec<(u32, bool, &[F])> = step
        .writes
        .iter()
        .map(|(i, r, fs)| (input_comp[*i], *r, fs.as_slice()))
        .collect();
    submit_many(sim, &writes).unwrap();
}

/// The full observable state: driven output pins and links.
fn state(sim: &Simulation) -> (Vec<u8>, Vec<u8>) {
    (sim.output_bytes(), sim.link_bytes())
}

fn board_and_script() -> impl Strategy<Value = (Vec<u32>, u64, Vec<Step>, Vec<u64>)> {
    prop::collection::vec(prop_oneof![1u32..=8, 60u32..=70], 1..=3).prop_flat_map(|widths| {
        (
            Just(widths.clone()),
            any::<u64>(),
            script_strategy(widths),
            prop::collection::vec(1u64..=40, 1..=20),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// The same submissions at the same tick counts give the same trace whether the ticks run one
    /// at a time or in `run()` batches of random sizes; and tick by tick, every input's pins and
    /// pending count match the model of the queue semantics.
    #[test]
    fn same_trace_however_it_runs((widths, seed, script, batches) in board_and_script()) {
        let desc = random_board(&widths, seed);
        let input_comp: Vec<u32> = (0..widths.len() as u32).collect();
        let end = script.last().map_or(0, |s| s.at) + 12;

        // Tick by tick, recording the trace and checking against the model.
        let mut sim = Simulation::from_descriptor(&desc).unwrap();
        let mut model = Model {
            queues: widths.iter().map(|_| Default::default()).collect(),
            pins: widths.iter().map(|&w| vec![false; w as usize]).collect(),
            ready_at: vec![0; widths.len()],
        };
        let mut trace = vec![state(&sim)];
        let mut next = 0;
        for t in 0..end {
            while next < script.len() && script[next].at == t {
                submit_step(&mut sim, &script[next], &input_comp);
                model.submit(&script[next].writes);
                next += 1;
            }
            prop_assert_eq!(progress(&sim), model.pending_events(), "progress at {}", t);
            sim.tick();
            model.tick(t + 1);
            for (i, &c) in input_comp.iter().enumerate() {
                let neg = &desc.components[c as usize].neg_outputs;
                for (p, &v) in model.pins[i].iter().enumerate() {
                    let driven = v ^ neg.contains(&(p as u32));
                    prop_assert_eq!(sim.output(c, p), driven, "input {} pin {} after tick {}", i, p, t + 1);
                }
            }
            trace.push(state(&sim));
        }

        // In random `run()` batches, split only where the script submits.
        let mut sim = Simulation::from_descriptor(&desc).unwrap();
        let mut next = 0;
        let mut b = 0;
        while sim.tick_count() < end {
            let t = sim.tick_count();
            while next < script.len() && script[next].at == t {
                submit_step(&mut sim, &script[next], &input_comp);
                next += 1;
            }
            let until_submit = script.get(next).map_or(end, |s| s.at) - t;
            let ticks = batches[b % batches.len()].min(until_submit).min(end - t);
            b += 1;
            sim.run(RunConfig { ticks, timeout: None }).unwrap();
            prop_assert_eq!(&state(&sim), &trace[sim.tick_count() as usize], "after tick {}", sim.tick_count());
        }
    }
}
