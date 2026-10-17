//! Component state: RAM memory read and written as bytes, the recorder's records, and the state
//! versions that tell a host what moved — checked against what the circuit itself sees, and
//! independent of how the ticks are batched.

use proptest::prelude::*;
use sim_core::{
    BoardBuilder, BoardDescriptor, CompType, InputFrame, InputWrite, MAX_RECORDER_CAPACITY,
    RAM_CLOCKED_READ, RECORD_TICKS, Records, RunConfig, SimError, Simulation, StateLayout,
};

/// Queue one event frame per entry of `frames` on input `comp`, each driving every pin.
fn drive(sim: &mut Simulation, comp: u32, frames: &[Vec<bool>]) {
    let states: Vec<Vec<Option<bool>>> = frames
        .iter()
        .map(|f| f.iter().map(|&v| Some(v)).collect())
        .collect();
    let frames: Vec<InputFrame<'_>> = states
        .iter()
        .map(|state| InputFrame {
            state,
            hold: 1,
            coalesce: false,
        })
        .collect();
    sim.submit_input(&[InputWrite {
        comp,
        replace: false,
        frames: &frames,
    }])
    .unwrap();
}

/// The low `n` bits of `v`, bit 0 first.
fn bits(v: u64, n: usize) -> Vec<bool> {
    (0..n).map(|i| v >> i & 1 == 1).collect()
}

/// Bits `[start, start + n)` of a bit-packed, LSB-first byte string, as a number.
fn word_at(bytes: &[u8], start: usize, n: usize) -> u64 {
    (0..n)
        .map(|i| start + i)
        .map(|b| ((bytes[b >> 3] >> (b & 7)) & 1) as u64)
        .enumerate()
        .fold(0, |w, (i, bit)| w | bit << i)
}

fn ticks(sim: &mut Simulation, n: u64) {
    sim.run(RunConfig {
        ticks: n,
        timeout: None,
    })
    .unwrap();
}

// --- RAM ---------------------------------------------------------------------------------------

/// A RAM of `w`-bit words and `a` address bits, its every input driven by input 0
/// (`[address, data, write-enable, clock]`); the RAM is component 1. `new` reads asynchronously,
/// `clocked` in the legacy clocked-read mode.
struct Ram {
    sim: Simulation,
    a: usize,
    w: usize,
}

const RAM: u32 = 1;

impl Ram {
    fn new(a: usize, w: usize) -> Ram {
        Ram::with_ops(a, w, &[])
    }

    fn clocked(a: usize, w: usize) -> Ram {
        Ram::with_ops(a, w, &[RAM_CLOCKED_READ])
    }

    fn with_ops(a: usize, w: usize, ops: &[u32]) -> Ram {
        let ins = (a + w + 2) as u32;
        let mut b = BoardBuilder::new(ins + w as u32);
        b.component(CompType::UserInput, &[], &(0..ins).collect::<Vec<_>>(), &[]);
        b.component(
            CompType::Ram,
            &(0..ins).collect::<Vec<_>>(),
            &(ins..ins + w as u32).collect::<Vec<_>>(),
            ops,
        );
        Ram {
            sim: Simulation::from_descriptor(&b.finish()).unwrap(),
            a,
            w,
        }
    }

    /// Pins of input 0 for one clock phase.
    fn pins(&self, addr: u64, data: u64, write: bool, clock: bool) -> Vec<bool> {
        let mut p = bits(addr, self.a);
        p.extend(bits(data, self.w));
        p.extend([write, clock]);
        p
    }

    /// Set the pins without a clock edge (the clock stays low), and let them settle.
    fn set(&mut self, addr: u64, data: u64, write: bool) {
        let frames = [self.pins(addr, data, write, false)];
        drive(&mut self.sim, 0, &frames);
        ticks(&mut self.sim, 3);
    }

    /// One clock cycle through the circuit: a write of `data` to `addr`, or a read of `addr`.
    fn cycle(&mut self, addr: u64, data: u64, write: bool) {
        let frames = [
            self.pins(addr, data, write, true),
            self.pins(addr, data, write, false),
        ];
        drive(&mut self.sim, 0, &frames);
        ticks(&mut self.sim, 3);
    }

    /// The RAM's output pins as a number.
    fn outputs(&self) -> u64 {
        (0..self.w).fold(0, |v, i| v | (self.sim.output(RAM, i) as u64) << i)
    }

    /// The word at `addr`, read as the byte range covering it.
    fn read_word(&self, addr: u64) -> u64 {
        let start = addr as usize * self.w;
        let first = start / 8;
        let end = (start + self.w).div_ceil(8);
        let mut buf = vec![0; end - first];
        self.sim.read_state(RAM, first, &mut buf).unwrap();
        word_at(&buf, start - first * 8, self.w)
    }

    fn memory(&self) -> Vec<u8> {
        let mut buf = vec![0; self.sim.state_len(RAM).unwrap()];
        self.sim.read_state(RAM, 0, &mut buf).unwrap();
        buf
    }

    fn version(&self) -> u32 {
        self.sim.state_version(RAM).unwrap()
    }
}

/// Words written by the circuit decode out of `read_state` at `address × w`, read whole or as the
/// byte range covering one word (which starts and ends mid-byte for most word sizes); the last
/// byte's padding stays zero.
#[test]
fn ram_read_decodes_circuit_writes() {
    for w in [1usize, 7, 8, 13] {
        let a = 3;
        let mut ram = Ram::new(a, w);
        assert_eq!(ram.sim.state_len(RAM).unwrap(), (w << a).div_ceil(8));
        let mask = (1u64 << w) - 1;
        let written: Vec<(u64, u64)> = [(1, 0x1A5B), (2, u64::MAX), (5, 0x0F0F), (7, 0x1234)]
            .into_iter()
            .map(|(addr, v)| (addr, v & mask))
            .collect();
        for &(addr, v) in &written {
            ram.cycle(addr, v, true);
        }
        let mem = ram.memory();
        for addr in 0..1u64 << a {
            let want = written
                .iter()
                .find(|&&(x, _)| x == addr)
                .map_or(0, |&(_, v)| v);
            assert_eq!(
                word_at(&mem, addr as usize * w, w),
                want,
                "w={w} addr={addr}"
            );
            assert_eq!(ram.read_word(addr), want, "w={w} addr={addr}, ranged");
        }
        let used = w << a;
        if used % 8 != 0 {
            assert_eq!(
                mem[mem.len() - 1] >> (used % 8),
                0,
                "w={w}: padding stays 0"
            );
        }
    }
}

/// The version moves on a circuit write that changes a stored bit, and only then.
#[test]
fn ram_version_moves_only_when_a_bit_changes() {
    let mut ram = Ram::new(2, 7);
    assert_eq!(ram.version(), 0);
    ram.cycle(1, 0x55, true);
    let v1 = ram.version();
    assert_ne!(v1, 0, "a write that changes bits moves the version");
    ram.cycle(1, 0x55, true);
    assert_eq!(ram.version(), v1, "rewriting the same word keeps it");
    ram.cycle(2, 0, true);
    assert_eq!(ram.version(), v1, "writing zeros over zeros keeps it");
    ram.cycle(1, 0, false);
    assert_eq!(ram.version(), v1, "a read keeps it");
    ram.cycle(1, 0x54, true);
    assert_ne!(ram.version(), v1, "one changed bit moves it");
}

/// Overwrite the word at `addr` with `write_state`, as the covering bytes, keeping the bits of the
/// neighbors that share them (read first, as a host writing a partial byte does). Returns the
/// offset and bytes written.
fn write_word(ram: &mut Ram, addr: usize, word: u64) -> (usize, Vec<u8>) {
    let w = ram.w;
    let (start, first, end) = (addr * w, addr * w / 8, ((addr + 1) * w).div_ceil(8));
    let mut buf = vec![0; end - first];
    ram.sim.read_state(RAM, first, &mut buf).unwrap();
    for i in 0..w {
        let b = start - first * 8 + i;
        let bit = 1 << (b & 7);
        if word >> i & 1 == 1 {
            buf[b >> 3] |= bit;
        } else {
            buf[b >> 3] &= !bit;
        }
    }
    ram.sim.write_state(RAM, first, &buf).unwrap();
    (first, buf)
}

/// The outputs follow the address with no clock: each address shows its word as soon as it is
/// set, a clocked write shows the written word, and the data inputs alone change nothing.
#[test]
fn ram_outputs_follow_the_address() {
    let mut ram = Ram::new(2, 8);
    ram.cycle(1, 0xA5, true);
    ram.cycle(2, 0x3C, true);
    assert_eq!(ram.outputs(), 0x3C, "a write shows the written word");
    ram.set(1, 0, false);
    assert_eq!(ram.outputs(), 0xA5, "addressing a word shows it");
    ram.set(1, 0xFF, true);
    assert_eq!(
        ram.outputs(),
        0xA5,
        "data and write-enable without a clock change nothing"
    );
    ram.set(0, 0, false);
    assert_eq!(ram.outputs(), 0);
    ram.set(2, 0, false);
    assert_eq!(ram.outputs(), 0x3C);
}

/// A `write_state` to the addressed word shows on the outputs after a tick, with no clock; one to
/// another address leaves them alone.
#[test]
fn ram_write_state_shows_on_the_outputs() {
    let mut ram = Ram::new(3, 13);
    ram.set(4, 0, false);
    write_word(&mut ram, 5, 0x0FED);
    ticks(&mut ram.sim, 2);
    assert_eq!(ram.outputs(), 0, "another address's word is not shown");
    write_word(&mut ram, 4, 0x1357);
    ticks(&mut ram.sim, 1);
    assert_eq!(
        ram.outputs(),
        0x1357,
        "the addressed word shows a tick later"
    );
    ram.set(5, 0, false);
    assert_eq!(ram.outputs(), 0x0FED);
}

/// In the clocked-read mode, bytes written with `write_state` are what the circuit reads at its
/// next clock; until then the outputs keep their value, even when the address changes.
#[test]
fn clocked_ram_write_state_is_read_at_the_next_clock() {
    let mut ram = Ram::clocked(3, 13);
    ram.cycle(4, 0x0ABC, true); // outputs echo the written word
    assert_eq!(ram.outputs(), 0x0ABC);

    let word = 0x1357u64;
    let v = ram.version();
    let (first, buf) = write_word(&mut ram, 4, word);
    assert_ne!(
        ram.version(),
        v,
        "a write that changes bits moves the version"
    );
    assert_eq!(ram.read_word(4), word);

    ticks(&mut ram.sim, 5);
    ram.set(0, 0, false);
    assert_eq!(
        ram.outputs(),
        0x0ABC,
        "outputs keep their value until a clock reads"
    );
    ram.cycle(4, 0, false);
    assert_eq!(
        ram.outputs(),
        word,
        "the next read clock sees the written word"
    );

    let v = ram.version();
    ram.sim.write_state(RAM, first, &buf).unwrap();
    assert_eq!(
        ram.version(),
        v,
        "rewriting the same bytes keeps the version"
    );
}

/// `write_state` keeps the padding past the last word zero, so a read still decodes as a ROM's
/// `ops` would.
#[test]
fn ram_write_state_keeps_padding_zero() {
    let mut ram = Ram::new(1, 7); // 14 bits: 2 bytes, the top 2 bits padding
    ram.sim.write_state(RAM, 0, &[0xFF, 0xFF]).unwrap();
    assert_eq!(ram.memory(), [0xFF, 0x3F]);
    let v = ram.version();
    ram.sim.write_state(RAM, 1, &[0xFF]).unwrap();
    assert_eq!(ram.version(), v, "only padding bits differed");
}

/// A range past the end is refused, and a refused write leaves the memory as it was.
#[test]
fn ram_state_ranges_are_checked() {
    let mut ram = Ram::new(2, 8); // 4 bytes
    ram.sim.write_state(RAM, 0, &[1, 2, 3, 4]).unwrap();
    let v = ram.version();

    let err = ram.sim.write_state(RAM, 3, &[9, 9]).unwrap_err();
    assert!(
        matches!(
            err,
            SimError::StateRange {
                comp: RAM,
                offset: 3,
                len: 2,
                size: 4
            }
        ),
        "{err:?}"
    );
    assert_eq!(ram.memory(), [1, 2, 3, 4], "nothing written");
    assert_eq!(ram.version(), v);

    let mut buf = [0; 2];
    assert!(matches!(
        ram.sim.read_state(RAM, 3, &mut buf),
        Err(SimError::StateRange { .. })
    ));
    assert!(matches!(
        ram.sim.read_state(RAM, usize::MAX, &mut buf),
        Err(SimError::StateRange { .. })
    ));
    assert!(matches!(
        ram.sim.write_state(RAM, usize::MAX, &[0]),
        Err(SimError::StateRange { .. })
    ));
    // Empty ranges inside (or at the end of) the state are fine.
    ram.sim.read_state(RAM, 4, &mut []).unwrap();
    ram.sim.write_state(RAM, 0, &[]).unwrap();
}

// --- Recorder ----------------------------------------------------------------------------------

/// Input 0 drives `n` data pins (links `0..n`) and a trigger (link `n`). Component 1 is a recorder
/// of them; components `2..2 + n` are D flip-flops latching each data bit on the same trigger.
fn recorder_board(n: u32, capacity: u32, flags: u32, neg_trigger: bool) -> BoardDescriptor {
    let mut b = BoardBuilder::new(3 * n + 1);
    b.component(CompType::UserInput, &[], &(0..=n).collect::<Vec<_>>(), &[]);
    let neg: &[u32] = if neg_trigger { &[n] } else { &[] };
    b.component_neg(
        CompType::Recorder,
        &(0..=n).collect::<Vec<_>>(),
        &[],
        &[capacity, flags],
        neg,
        &[],
    );
    for bit in 0..n {
        let q = n + 1 + 2 * bit;
        b.component_neg(
            CompType::DFf,
            &[bit, n],
            &[q, q + 1],
            &[],
            neg_from(neg),
            &[],
        );
    }
    b.finish()
}

/// The flip-flops' clock is their pin 1: negate it when the recorder's trigger is negated.
fn neg_from(neg: &[u32]) -> &'static [u32] {
    if neg.is_empty() { &[] } else { &[1] }
}

const REC: u32 = 1;

/// What [`all_records`] read: the call's result, the values, and the ticks if recorded.
type AllRecords = (Records, Vec<u32>, Option<Vec<u64>>);

/// Every record of a recorder, with ticks when it records them.
fn all_records(sim: &Simulation, comp: u32) -> AllRecords {
    let StateLayout::Records { capacity, ticks } = sim.state_layout(comp).unwrap() else {
        panic!("not a recorder");
    };
    let mut values = vec![0; capacity as usize];
    let mut tick_buf = ticks.then(|| vec![0; capacity as usize]);
    let r = sim
        .read_records(comp, 0, &mut values, tick_buf.as_deref_mut())
        .unwrap();
    values.truncate(r.len);
    if let Some(t) = &mut tick_buf {
        t.truncate(r.len);
    }
    (r, values, tick_buf)
}

/// The value the D flip-flops of [`recorder_board`] hold.
fn latched(sim: &Simulation, n: u32) -> u32 {
    (0..n).fold(0, |v, bit| v | (sim.output(2 + bit, 0) as u32) << bit)
}

/// A pseudo-random walk of the data bus and trigger, where data often changes in the same tick
/// the trigger rises.
fn walk(n: u32, len: usize, seed: u64) -> Vec<Vec<bool>> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            bits(x >> 20, n as usize + 1)
        })
        .collect()
}

/// One record per edge of the trigger, holding what a D flip-flop on the same trigger latches, with
/// the tick of the first snapshot that shows the edge on its link; rising edges by default, falling
/// with a negated trigger.
#[test]
fn records_one_value_per_trigger_edge() {
    for neg in [false, true] {
        let n = 5;
        let mut sim =
            Simulation::from_descriptor(&recorder_board(n, 1024, RECORD_TICKS, neg)).unwrap();
        drive(&mut sim, 0, &walk(n, 300, 7));
        let mut edges = Vec::new();
        for _ in 0..310 {
            let before = sim.link(n);
            sim.tick();
            let after = sim.link(n);
            // A negated trigger fires on the link's falling edge.
            if (before, after) == (neg, !neg) {
                edges.push((sim.tick_count(), latched(&sim, n)));
            }
        }
        let (r, values, ticks) = all_records(&sim, REC);
        assert!(edges.len() > 50, "the walk makes plenty of edges");
        assert_eq!(
            r.count,
            edges.len() as u64,
            "neg={neg}: one record per edge"
        );
        assert_eq!(r.first, 0);
        let got: Vec<(u64, u32)> = ticks.unwrap().into_iter().zip(values).collect();
        assert_eq!(got, edges, "neg={neg}");
    }
}

/// A negated trigger rests high, which a recorder does not mistake for an edge at power-on.
#[test]
fn negated_trigger_records_nothing_at_power_on() {
    let mut sim = Simulation::from_descriptor(&recorder_board(3, 8, 0, true)).unwrap();
    assert_eq!(sim.state_version(REC).unwrap(), 0);
    ticks(&mut sim, 10);
    assert_eq!(
        sim.state_version(REC).unwrap(),
        0,
        "nothing before the trigger moves"
    );
    drive(&mut sim, 0, &[bits(0b1011, 4), bits(0b0011, 4)]); // trigger up, then down
    ticks(&mut sim, 1);
    assert_eq!(
        sim.state_version(REC).unwrap(),
        0,
        "a rising trigger is not an edge"
    );
    ticks(&mut sim, 1);
    assert_eq!(sim.state_version(REC).unwrap(), 1, "the falling one is");
    assert_eq!(all_records(&sim, REC).1, [0b011]);
}

/// Records beyond `capacity` overwrite the oldest; `count` keeps counting, and `read_records` returns
/// the last `capacity` records in order, from `first = count − capacity`, a page at a time.
#[test]
fn records_wrap_and_page() {
    let n = 8;
    let mut sim = Simulation::from_descriptor(&recorder_board(n, 10, 0, false)).unwrap();
    let frames: Vec<Vec<bool>> = (0..25u64)
        .flat_map(|v| [bits(v | 1 << n, 9), bits(v, 9)])
        .collect();
    drive(&mut sim, 0, &frames);
    ticks(&mut sim, 60);

    let (r, values, ticks) = all_records(&sim, REC);
    assert_eq!(
        r,
        Records {
            count: 25,
            first: 15,
            len: 10
        }
    );
    assert_eq!(values, (15..25).collect::<Vec<_>>());
    assert_eq!(ticks, None, "no tick flag, no ticks");
    assert_eq!(
        sim.state_version(REC).unwrap(),
        25,
        "the version is the count"
    );

    // Paging with a 4-record buffer from an old `since`: the gap is reported, the rest arrives in
    // order.
    let mut since = 3;
    let mut buf = [0u32; 4];
    let mut got = Vec::new();
    let mut firsts = Vec::new();
    loop {
        let r = sim.read_records(REC, since, &mut buf, None).unwrap();
        firsts.push(r.first);
        got.extend_from_slice(&buf[..r.len]);
        since = r.first + r.len as u64;
        if r.len == 0 {
            break;
        }
    }
    assert_eq!(
        firsts,
        [15, 19, 23, 25],
        "first jumps over the lost records once"
    );
    assert_eq!(got, (15..25).collect::<Vec<_>>());

    // Reading at `count` returns nothing; past it is an error.
    let r = sim.read_records(REC, 25, &mut buf, None).unwrap();
    assert_eq!((r.first, r.len), (25, 0));
    assert!(matches!(
        sim.read_records(REC, 26, &mut buf, None),
        Err(SimError::RecordsAhead {
            comp: REC,
            since: 26,
            count: 25
        })
    ));
}

/// The recorder's other state methods: no byte state, read-only; and a records read checks its
/// target and its tick buffer.
#[test]
fn recorder_state_errors() {
    let mut sim = Simulation::from_descriptor(&recorder_board(2, 4, RECORD_TICKS, false)).unwrap();
    assert_eq!(sim.state_len(REC).unwrap(), 0);
    sim.read_state(REC, 0, &mut []).unwrap();
    assert!(matches!(
        sim.read_state(REC, 0, &mut [0]),
        Err(SimError::StateRange { size: 0, .. })
    ));
    assert!(matches!(
        sim.write_state(REC, 0, &[]),
        Err(SimError::StateReadOnly(REC))
    ));

    let mut values = [0; 4];
    let mut ticks = [0; 4];
    assert!(matches!(
        sim.read_records(REC, 0, &mut values, None),
        Err(SimError::RecordTicks {
            comp: REC,
            records_ticks: true
        })
    ));
    assert!(matches!(
        sim.read_records(0, 0, &mut values, Some(&mut ticks)),
        Err(SimError::NotARecorder(0))
    ));
    assert!(matches!(
        sim.read_records(99, 0, &mut values, Some(&mut ticks)),
        Err(SimError::ComponentOutOfRange(99))
    ));

    let mut sim = Simulation::from_descriptor(&recorder_board(2, 4, 0, false)).unwrap();
    assert!(matches!(
        sim.read_records(REC, 0, &mut values, Some(&mut ticks)),
        Err(SimError::RecordTicks {
            comp: REC,
            records_ticks: false
        })
    ));
    assert!(matches!(
        sim.write_state(0, 0, &[]),
        Err(SimError::StateReadOnly(0))
    ));
}

/// A recorder needs 1–32 data pins plus the trigger, no outputs, a capacity in
/// `1..=MAX_RECORDER_CAPACITY` and no flag but the tick bit.
#[test]
fn recorder_arity() {
    let compile = |ins: u32, outs: &[u32], ops: &[u32]| {
        let mut b = BoardBuilder::new(40);
        b.component(CompType::Recorder, &(0..ins).collect::<Vec<_>>(), outs, ops);
        Simulation::from_descriptor(&b.finish()).map(|_| ())
    };
    let bad = |r: Result<(), SimError>| {
        matches!(
            r,
            Err(SimError::BadArity {
                ty: CompType::Recorder,
                ..
            })
        )
    };
    assert!(compile(2, &[], &[1, 0]).is_ok());
    assert!(compile(33, &[], &[MAX_RECORDER_CAPACITY, RECORD_TICKS]).is_ok());
    assert!(bad(compile(1, &[], &[4, 0])), "no data pin");
    assert!(bad(compile(34, &[], &[4, 0])), "33 data pins");
    assert!(bad(compile(3, &[39], &[4, 0])), "an output");
    assert!(bad(compile(3, &[], &[0, 0])), "capacity 0");
    assert!(bad(compile(3, &[], &[MAX_RECORDER_CAPACITY + 1, 0])));
    assert!(bad(compile(3, &[], &[4, 2])), "an unknown flag bit");
    assert!(bad(compile(3, &[], &[4, 1 << 31])));
    assert!(bad(compile(3, &[], &[4])));
    assert!(bad(compile(3, &[], &[4, 0, 0])));
}

/// A 12-bit ripple counter of JK flip-flops on a clock, and a recorder of it on the same clock:
/// record `k` holds `k mod 4096`, the count before the `k`-th edge advances it.
#[test]
fn recorder_of_a_counter() {
    let mut sim = Simulation::from_descriptor(&counter_board(12, 16, 64)).unwrap();
    ticks(&mut sim, 17 * 200);
    let (r, values, _) = all_records(&sim, COUNTER_REC);
    assert!(r.count >= 190, "about one record per clock period");
    assert_eq!(r.first, r.count - 64);
    for (i, &v) in values.iter().enumerate() {
        assert_eq!(
            v as u64,
            (r.first + i as u64) % 4096,
            "record {}",
            r.first + i as u64
        );
    }
}

/// [`counter_board`]'s recorder.
const COUNTER_REC: u32 = 1;

/// Link 0 is a clock of period `period + 1` ticks (high for one); link 1 is never driven. Bit `i` of
/// the counter is a JK flip-flop with J = K = high (negated reads of link 1) toggling on the falling
/// edge of bit `i - 1` (bit 0: the clock's rising edge). Q of bit `i` is link `2 + 2i`. The
/// recorder (component 1) samples the counter on the clock.
fn counter_board(n: u32, period: u32, capacity: u32) -> BoardDescriptor {
    let mut b = BoardBuilder::new(2 + 2 * n);
    b.component(CompType::Clk, &[1], &[0], &[period]);
    let data: Vec<u32> = (0..n).map(|i| 2 + 2 * i).collect();
    b.component(
        CompType::Recorder,
        &[&data[..], &[0]].concat(),
        &[],
        &[capacity, 0],
    );
    for i in 0..n {
        let (clock, neg_clock): (u32, &[u32]) = if i == 0 { (0, &[]) } else { (2 * i, &[1]) };
        b.component_neg(
            CompType::JkFf,
            &[1, clock, 1],
            &[2 + 2 * i, 3 + 2 * i],
            &[],
            &[&[0, 2][..], neg_clock].concat(),
            &[],
        );
    }
    b.finish()
}

// --- shared ------------------------------------------------------------------------------------

/// Reads never change state: the same data and version twice, and no tick moved.
#[test]
fn reads_change_nothing() {
    let mut sim = Simulation::from_descriptor(&recorder_board(4, 8, RECORD_TICKS, false)).unwrap();
    drive(&mut sim, 0, &walk(4, 40, 3));
    ticks(&mut sim, 45);
    let first = (all_records(&sim, REC), sim.state_version(REC).unwrap());
    assert_eq!(
        (all_records(&sim, REC), sim.state_version(REC).unwrap()),
        first
    );

    let mut ram = Ram::new(3, 7);
    ram.cycle(3, 0x41, true);
    let first = (ram.memory(), ram.version(), ram.sim.tick_count());
    assert_eq!((ram.memory(), ram.version(), ram.sim.tick_count()), first);
}

/// `StateLayout::of` a descriptor (what a host running the engine elsewhere checks against) agrees
/// with the simulation's, for every component; stateless ones have version 0.
#[test]
fn layouts_follow_the_descriptor() {
    let mut desc = recorder_board(3, 16, RECORD_TICKS, false);
    // 3-bit words and 4 address bits: 6 bytes.
    desc.components.push(sim_core::ComponentDescriptor {
        ty: CompType::Ram.id(),
        inputs: (0..9).collect(),
        outputs: vec![0, 1, 2],
        ops: vec![],
        neg_inputs: vec![],
        neg_outputs: vec![],
    });
    let sim = Simulation::from_descriptor(&desc).unwrap();
    for (c, comp) in desc.components.iter().enumerate() {
        assert_eq!(
            StateLayout::of(comp),
            sim.state_layout(c as u32).unwrap(),
            "{:?}",
            comp.ty
        );
    }
    let ram = desc.components.len() as u32 - 1;
    assert_eq!(
        sim.state_layout(ram).unwrap(),
        StateLayout::Bytes {
            len: 6,
            writable: true
        }
    );
    assert_eq!(sim.state_version(0).unwrap(), 0);
    assert!(matches!(
        sim.state_version(ram + 1),
        Err(SimError::ComponentOutOfRange(_))
    ));
    assert!(matches!(
        sim.state_len(ram + 1),
        Err(SimError::ComponentOutOfRange(_))
    ));
}

/// A board with RAM and two recorders, all fed by RNGs on clocks of coprime periods, so their state
/// keeps moving without any input.
fn busy_board() -> BoardDescriptor {
    let mut b = BoardBuilder::new(40);
    b.component(CompType::Clk, &[39], &[0], &[2]); // link 39 never driven: runs
    b.component(CompType::Clk, &[39], &[1], &[3]);
    b.component(CompType::Rng, &[1], &(2..16).collect::<Vec<_>>(), &[]);
    // RAM: address 2..6 (4 bits), data 6..14 (8), write-enable 14, clock 0.
    b.component(
        CompType::Ram,
        &[&(2..14).collect::<Vec<_>>()[..], &[14, 0]].concat(),
        &(20..28).collect::<Vec<_>>(),
        &[],
    );
    b.component(
        CompType::Recorder,
        &[&(2..14).collect::<Vec<_>>()[..], &[0]].concat(),
        &[],
        &[50, RECORD_TICKS],
    );
    b.component(
        CompType::Recorder,
        &[&(20..28).collect::<Vec<_>>()[..], &[1]].concat(),
        &[],
        &[7, 0],
    );
    b.finish()
}

/// Everything the state API shows of [`busy_board`].
fn busy_state(sim: &Simulation) -> (Vec<u32>, Vec<u8>, Vec<AllRecords>) {
    let versions = (0..6).map(|c| sim.state_version(c).unwrap()).collect();
    let mut mem = vec![0; sim.state_len(3).unwrap()];
    sim.read_state(3, 0, &mut mem).unwrap();
    (
        versions,
        mem,
        vec![all_records(sim, 4), all_records(sim, 5)],
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The state after N ticks is the same whether they run one at a time or in `run()` batches of
    /// random sizes.
    #[test]
    fn state_is_batch_independent(batches in prop::collection::vec(1u64..300, 1..20)) {
        let desc = busy_board();
        let end: u64 = batches.iter().sum();
        let mut stepped = Simulation::from_descriptor(&desc).unwrap();
        for _ in 0..end {
            stepped.tick();
        }
        let mut batched = Simulation::from_descriptor(&desc).unwrap();
        for &n in &batches {
            ticks(&mut batched, n);
        }
        let state = busy_state(&stepped);
        prop_assert!(end < 20 || state.2[0].0.count > 0, "the recorders record");
        prop_assert_eq!(busy_state(&batched), state);
    }
}
