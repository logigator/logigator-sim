// Generate the synthetic benchmark boards into corpus/bench/.
//
// Unlike the corpus boards (small functional fixtures), these span the perf regimes the
// improvement phases target: fixed per-tick overhead (idle), hot-loop cost at several working-set
// sizes (active NOT rings), consumer-enqueue stress (fanout), correlated multi-input flips
// (correlated), the per-tick cost of an input with pending frames (input_paste), multi-driver links
// (bus), the types without a flat record (generic), negated pins (negated) and a realistic mix at a
// large working set (large_cpu). Each board is a corpus-fixture wrapper (`{name, description,
// ticks, board}`) so every CLI subcommand accepts it; none needs input to stay busy — activity is
// self-sustaining (NOT rings, free-running clocks) and "constant low" inputs are undriven links,
// like `not_chain.json`. input_paste also carries `inputWrites`: `submitInput` requests the
// harnesses submit before the timed run.
//
// Everything is deterministic — no RNG — so regenerating reproduces the checked-in files
// byte-for-byte. Large boards are emitted one component per line to keep diffs reviewable.
//
// Usage: node gen-bench.mjs   (writes corpus/bench/*.json, progress to stderr)

import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT_DIR = join(dirname(fileURLToPath(import.meta.url)), '..', 'bench');
const CPU_BOARD = join(dirname(fileURLToPath(import.meta.url)), '..', 'boards', 'cpu.json');

// Wire-format component type ids (sim-core/src/types.rs).
const NOT = 1;
const AND = 2;
const OR = 3;
const XOR = 4;
const DELAY = 5;
const CLK = 6;
const FULL_ADDER = 11;
const ROM = 12;
const D_FF = 13;
const JK_FF = 14;
const SR_FF = 15;
const RNG = 16;
const RAM = 17;
const DECODER = 18;
const ENCODER = 19;
const MUX = 20;
const DEMUX = 21;
const USER_INPUT = 200;
const RECORDER = 201;
const ADDRESSABLE_REGISTER = 204;

/** Minimal board builder mirroring the descriptor shape: links are just ids handed out in order.
 *  `params` are the component's per-type parameters (e.g. a CLK's `period`). */
function builder() {
  let links = 0;
  const components = [];
  return {
    link: () => links++,
    links: (n) => Array.from({ length: n }, () => links++),
    comp(type, inputs, outputs, params = {}) {
      components.push({ type, ...(inputs.length ? { inputs } : {}), outputs, ...params });
      return components.length - 1;
    },
    finish: () => ({ components }),
  };
}

/**
 * A ring of `n` NOT gates: gate i reads link i and drives link (i+1) % n.
 *
 * Init seeding sets every NOT output high, so the whole ring flips low together on tick 1,
 * high on tick 2, … — every link flips every tick, forever, regardless of ring length. This is the
 * "every link flips every tick" oscillator the active boards are built from.
 */
function notRing(b, n) {
  const ring = b.links(n);
  for (let i = 0; i < n; i++) {
    b.comp(NOT, [ring[i]], [ring[(i + 1) % n]]);
  }
  return ring;
}

/**
 * Idle filler: `n` DELAY gates in chains of `chain` links, every chain anchored at one shared
 * undriven (constant-low) link. DELAY(0) = 0, so after the init seed none of them ever computes
 * again — they exist only to occupy the board's arrays and working set.
 */
function idleFiller(b, n, chain = 64) {
  const anchor = b.link();
  let prev = anchor;
  for (let i = 0; i < n; i++) {
    if (i % chain === 0) prev = anchor;
    const out = b.link();
    b.comp(DELAY, [prev], [out]);
    prev = out;
  }
}

/** A free-running speed-1 clock (enable = fresh undriven link, stays low): output toggles every tick. */
function freeClock(b) {
  const enable = b.link();
  const out = b.link();
  b.comp(CLK, [enable], [out], { period: 1 });
  return out;
}

/**
 * The idle shape shared by medium_idle / large_idle: one free-running clock whose output drives a
 * small NOT chain (the "active corner" — a handful of links flip every tick), plus enough idle
 * DELAY filler to reach `total` components. Per-tick cost should track the corner, not `total`.
 * `extra(b)` adds components between the corner and the filler; the filler shrinks to keep
 * `total`.
 */
function idleBoard(total, cornerLen = 8, extra = () => {}) {
  const b = builder();
  const clk = freeClock(b);
  let prev = clk;
  for (let i = 0; i < cornerLen; i++) {
    const out = b.link();
    b.comp(NOT, [prev], [out]);
    prev = out;
  }
  extra(b);
  idleFiller(b, total - b.finish().components.length);
  return b.finish();
}

/**
 * A keyboard: one UserInput with 7 data pins and a strobe, plus seven D flip-flops latching the
 * data on the strobe's rising edge. Returns the input's component id.
 */
function keyboard(b) {
  const data = b.links(7);
  const strobe = b.link();
  const input = b.comp(USER_INPUT, [], [...data, strobe]);
  for (const bit of data) b.comp(D_FF, [bit, strobe], b.links(2));
  return input;
}

const PASTE_TEXT = 'The quick brown fox jumps over the lazy dog. ';
const PASTE_CHARS = 256;
// Long enough that the paste outlasts any bench window: 2 × 256 frames × 2^17 ticks ≈ 67M ticks,
// several seconds at idle-board speeds. Keeping the frame count small keeps the file small; the
// per-tick cost of a pending input does not depend on how many frames are queued.
const PASTE_HOLD = 1 << 17;

/** One request typing `PASTE_CHARS` characters into `comp`: a press (data + strobe) and a release
 *  (data only) per character, each held `PASTE_HOLD` ticks. */
function paste(comp) {
  const frames = [];
  for (let i = 0; i < PASTE_CHARS; i++) {
    const code = PASTE_TEXT.charCodeAt(i % PASTE_TEXT.length);
    const data = Array.from({ length: 7 }, (_, bit) => ((code >> bit) & 1) === 1);
    frames.push({ state: [...data, true], hold: PASTE_HOLD });
    frames.push({ state: [...data, false], hold: PASTE_HOLD });
  }
  return [{ tick: 0, writes: [{ comp, frames }] }];
}

/** input_paste: medium_idle's shape with a keyboard in place of a few filler gates, and a paste
 *  queued on it before the run. */
function inputPaste() {
  let comp;
  const board = idleBoard(1_000, 8, (b) => {
    comp = keyboard(b);
  });
  return { board, inputWrites: paste(comp) };
}

// --- the boards -----------------------------------------------------------------------------

/** small_idle: the corpus `clk.json` board as-is (two clocks, one UserInput enable), minus its
 *  timed input — so both clocks free-run here. */
function smallIdle() {
  return {
    components: [
      { type: CLK, inputs: [0], outputs: [1], period: 1 },
      { type: USER_INPUT, outputs: [2] },
      { type: CLK, inputs: [2], outputs: [3], period: 2 },
    ],
  };
}

function activeRing(n) {
  const b = builder();
  notRing(b, n);
  return b.finish();
}

/** fanout: a 4-NOT oscillator ring where each ring link is additionally consumed by `perLink` NOT
 *  sinks (own unconsumed output each). Every tick all four source links flip and enqueue all
 *  consumers — the read phase's consumer-enqueue loop dominates. */
function fanout(perLink) {
  const b = builder();
  const ring = notRing(b, 4);
  for (const src of ring) {
    for (let i = 0; i < perLink; i++) {
      b.comp(NOT, [src], [b.link()]);
    }
  }
  return b.finish();
}

/**
 * A `bits`-wide synchronous counter of D-FFs on `clk` with XOR/AND next-state logic
 * (`3 * bits - 3` components); returns the Q links, bit 0 first.
 */
function counter(b, clk, bits) {
  // Bit 0 is a plain toggle: D = Qbar fed straight back.
  const q = [];
  {
    const [q0, qb0] = b.links(2);
    b.comp(D_FF, [qb0, clk], [q0, qb0]);
    q.push(q0);
  }
  // Bits 1..: D_i = XOR(Q_i, T_i) with T_1 = Q_0 and T_i = AND(Q_0..Q_{i-1}).
  for (let i = 1; i < bits; i++) {
    const [qi, qbi, di] = b.links(3);
    let t;
    if (i === 1) {
      t = q[0];
    } else {
      t = b.link();
      b.comp(AND, q.slice(0, i), [t]);
    }
    b.comp(D_FF, [di, clk], [qi, qbi]);
    b.comp(XOR, [qi, t], [di]);
    q.push(qi);
  }
  return q;
}

/**
 * correlated: a 16-bit synchronous counter (D-FFs on a common speed-1 clock, XOR/AND next-state
 * logic), feeding a ripple-carry FULL_ADDER chain and a D-FF bank on the same clock. When the
 * counter steps, several low-order bits flip in the same tick, so the adders and FFs see multiple
 * inputs flip together — stressing duplicate computes of multi-input components (the engine
 * recomputes a component once per flipped input link). Sized to exactly `faCount + bankCount +
 * 46` components (1 clock + 16 FFs + 15 XOR + 14 AND), ~1k by default, so it doubles as a medium
 * board with a realistic component mix.
 */
function correlated(faCount, bankCount) {
  const BITS = 16;
  const b = builder();
  const clk = freeClock(b);
  const q = counter(b, clk, BITS);

  // Ripple-carry chain: a/b from counter bits (several flip together on a counter step), carry
  // threaded through, so flips also ripple down the chain across subsequent ticks.
  const sums = [];
  let carry = q[2];
  for (let j = 0; j < faCount; j++) {
    const [sum, cout] = b.links(2);
    b.comp(FULL_ADDER, [q[j % BITS], q[(j * 5 + 3) % BITS], carry], [sum, cout]);
    sums.push(sum);
    carry = cout;
  }

  // D-FF bank on the common clock: D (an adder sum) and clk frequently flip in the same tick.
  for (let k = 0; k < bankCount; k++) {
    b.comp(D_FF, [sums[k % sums.length], clk], b.links(2));
  }

  return b.finish();
}

/**
 * bus: tri-state-style buses as wired-OR links. A 16-bit counter on a speed-1 clock feeds `buses`
 * buses of `width` bits, each with `sources` drivers per bit: a DEC over a window of counter bits
 * (shifted per bus) selects one source, and source `r` drives its rotation of the counter onto the
 * bus, bit `w` through an AND2 of its select line and counter bit `(w + r + k) % 16`. One wide OR
 * reads each bus. Every bus link has `sources` drivers, so the drivers' flips take the counted path:
 * driver counts and the invalid check (a link with one driver high and the rest low reads
 * invalid).
 */
function bus(buses, sources, width) {
  const BITS = 16;
  const b = builder();
  const clk = freeClock(b);
  const q = counter(b, clk, BITS);
  const selBits = Math.log2(sources);
  for (let k = 0; k < buses; k++) {
    const sel = b.links(sources);
    b.comp(DECODER, q.slice(k, k + selBits), sel);
    const lines = b.links(width);
    for (let r = 0; r < sources; r++) {
      for (let w = 0; w < width; w++) {
        b.comp(AND, [sel[r], q[(w + r + k) % BITS]], [lines[w]]);
      }
    }
    b.comp(OR, lines, [b.link()]);
  }
  return b.finish();
}

/**
 * generic: `tiles` copies of a datapath built only from the types without a flat record, sharing a
 * speed-1 clock and a 4-bit JK counter (the address). Per tile, an RNG drawing 9 bits on each
 * rising clock feeds a ROM (4 address bits) and a 16 x 8-bit RAM (data r0..r7, WE r8); a MUX picks
 * one RAM output by address, a DEMUX routes it back out, an ENC with valid encodes the RAM word and
 * a DEC decodes it again; 8-input OR and XOR gates and a 3-input AND reduce them; a JK and an SR
 * flip-flop (S implies R, so it enters the invalid state) and a 4-bit-data addressable register
 * latch on the clock, and a recorder captures the RAM word. Measures the generic per-type kernels,
 * which the flat records leave to `cpu`'s wide gates elsewhere.
 */
function generic(tiles) {
  const b = builder();
  const clk = freeClock(b);
  const one = b.link();
  b.comp(NOT, [b.link()], [one]);
  // A synchronous JK counter: bit i toggles when every lower bit is high.
  const a = [];
  for (let i = 0; i < 4; i++) {
    let t = one;
    if (i === 1) t = a[0];
    if (i >= 2) {
      t = b.link();
      b.comp(AND, a.slice(0, i), [t]);
    }
    const [qi, qbi] = b.links(2);
    b.comp(JK_FF, [t, clk, t], [qi, qbi]);
    a.push(qi);
  }
  for (let t = 0; t < tiles; t++) {
    const r = b.links(9);
    b.comp(RNG, [clk], r);
    const o = b.links(4);
    const data = Array.from({ length: 8 }, (_, i) => (t * 37 + i * 101 + 13) & 255);
    b.comp(ROM, r.slice(0, 4), o, { data });
    const m = b.links(8);
    b.comp(RAM, [...a, ...r.slice(0, 8), r[8], clk], m);
    const x = b.link();
    b.comp(MUX, [...a.slice(0, 3), ...m], [x]);
    const e = b.links(8);
    b.comp(DEMUX, [...a.slice(1, 4), x], e);
    const v = b.links(4);
    b.comp(ENCODER, m, v);
    const d = b.links(8);
    b.comp(DECODER, v.slice(0, 3), d);
    const y = b.link();
    b.comp(OR, e, [y]);
    const p = b.link();
    b.comp(XOR, [...d.slice(0, 4), ...o], [p]);
    const z = b.link();
    b.comp(AND, [x, y, p], [z]);
    b.comp(JK_FF, [p, clk, y], b.links(2));
    b.comp(SR_FF, [z, clk, x], b.links(2));
    b.comp(ADDRESSABLE_REGISTER, [...a.slice(0, 2), ...o, clk], b.links(16), { dataBus: 4 });
    b.comp(RECORDER, [...m, clk], [], { capacity: 64 });
  }
  return b.finish();
}

/**
 * negated: `correlated` with its pins negated wherever that keeps each component's function, so it
 * runs the same circuit through the negate path: the full adders with every pin negated (the full
 * adder is self-dual), each wide AND as an OR with every pin negated (De Morgan), each XOR2 with
 * one input and its output negated, and each D-FF with its D input and both outputs negated. The
 * clock and the D-FF clock pins stay un-negated (a negated halt would freeze the clock, a negated
 * clock pin would move the edges). Its counter powers on at all-ones instead of zero; otherwise
 * each tick flips what `correlated`'s does, so the two compare directly.
 */
function negated(board) {
  const all = (pins) => pins.map((_, i) => i);
  return {
    components: board.components.map((c) => {
      switch (c.type) {
        case FULL_ADDER:
          return { ...c, negInputs: all(c.inputs), negOutputs: all(c.outputs) };
        case AND:
          return { ...c, type: OR, negInputs: all(c.inputs), negOutputs: all(c.outputs) };
        case XOR:
          return { ...c, negInputs: [0], negOutputs: [0] };
        case D_FF:
          return { ...c, negInputs: [0], negOutputs: [0, 1] };
        default:
          return c;
      }
    }),
  };
}

/**
 * large_cpu: `copies` disjoint copies of the corpus `cpu` board, each on its own links (ids offset
 * by the board's link count per copy). Each copy runs its own clock, so the per-tick work is
 * `copies` times `cpu`'s, spread over a working set far past the caches.
 */
function largeCpu(copies) {
  const { board } = JSON.parse(readFileSync(CPU_BOARD, 'utf8'));
  const links =
    1 + Math.max(...board.components.flatMap((c) => [...(c.inputs ?? []), ...(c.outputs ?? [])]));
  const components = [];
  for (let k = 0; k < copies; k++) {
    const shift = (ids) => ids.map((id) => id + k * links);
    for (const c of board.components) {
      components.push({
        ...c,
        ...(c.inputs ? { inputs: shift(c.inputs) } : {}),
        ...(c.outputs ? { outputs: shift(c.outputs) } : {}),
      });
    }
  }
  return { components };
}

// --- emit -------------------------------------------------------------------------------------

const BOARDS = [
  {
    name: 'small_idle',
    description:
      'clk.json board as-is, both clocks free-running (no input). Measures fixed per-tick overhead at a tiny working set.',
    board: smallIdle(),
  },
  {
    name: 'small_active',
    description:
      'A 50-gate NOT-oscillator ring: every link flips every tick. Measures hot-loop cost at a tiny working set.',
    board: activeRing(50),
  },
  {
    name: 'medium_idle',
    description:
      '~1k components: one free-running clock + 8-NOT active corner, the rest idle DELAY chains. The expected most-common real-world size; per-tick cost should track the corner, not the board.',
    board: idleBoard(1_000),
  },
  {
    name: 'medium_active',
    description:
      'A 1000-gate NOT-oscillator ring: every link flips every tick. Working set spills L1 but fits L2; the frontier (1000) stays below the default par_threshold (2048), so this is the pure ST path at realistic scale even with threads > 1.',
    board: activeRing(1_000),
  },
  {
    name: 'large_idle',
    description:
      '~200k components: one free-running clock + 8-NOT active corner, the rest idle DELAY chains. Checks cost is activity-proportional, not size-proportional.',
    board: idleBoard(200_000),
  },
  {
    name: 'large_active',
    description:
      'A 200k-gate NOT-oscillator ring: every link flips every tick. The worklist-stress board and the MT showcase (frontier of 200k >> par_threshold 2048).',
    board: activeRing(200_000),
  },
  {
    name: 'fanout',
    description:
      'A 4-NOT oscillator ring where each ring link also drives 2500 NOT sinks: 4 links flipping per tick enqueue 10k consumers. Stresses the consumer-enqueue loop.',
    board: fanout(2_500),
  },
  {
    name: 'input_paste',
    description:
      'medium_idle with a keyboard (one 8-pin UserInput + 7 D-FF latches) receiving a paste submitted before the run: 512 event frames held 2^17 ticks each, so the input has frames pending for the whole bench window. Compare with medium_idle (or with a baseline that ignores inputWrites) for the per-tick cost of pending input.',
    ...inputPaste(),
  },
  {
    name: 'correlated',
    description:
      '16-bit synchronous D-FF counter + 600 ripple-carry full adders + 354-D-FF bank on a common speed-1 clock (1000 components). Multi-input components see several inputs flip in the same tick (duplicate-compute stress); doubles as a medium board with a realistic mix.',
    board: correlated(600, 354),
  },
  {
    name: 'bus',
    description:
      'Eight 16-bit buses with 8 sources each (1086 components): a DEC over a window of a 16-bit counter selects a source, whose AND2s drive its rotation of the counter onto the bus, so each of the 128 bus links has 8 drivers; a wide OR reads each bus. About 70% of the output flips land on multi-driver links (cpu: about 13%), so it measures the counted path those links take (driver counts, the invalid check), which every other synthetic board skips.',
    board: bus(8, 8, 16),
  },
  {
    name: 'generic',
    description:
      '75 tiles of RNG, ROM, RAM, MUX, DEMUX, ENC, DEC, 8-input OR/XOR, 3-input AND, JK, SR, addressable register and recorder on a shared clock and 4-bit JK counter (1058 components). Measures the per-type kernels of the types without a flat record.',
    board: generic(75),
  },
  {
    name: 'negated',
    description:
      'correlated with its pins negated wherever that keeps the function (full adders on every pin, each wide AND as an all-negated OR, XOR2 on one input and the output, D-FF on D and both outputs): the same per-tick flips through the negate path. Compare with correlated for the cost of negated pins.',
    board: negated(correlated(600, 354)),
  },
  {
    name: 'large_cpu',
    description:
      '48 disjoint copies of the cpu board (196752 components), each with its own clock. A realistic component mix at a working set far past the caches; compare per-copy throughput with cpu.',
    board: largeCpu(48),
  },
];

/** `inputWrites` with one frame per line, or nothing for a board without input. */
function renderInputWrites(inputWrites) {
  if (!inputWrites) return '';
  const requests = inputWrites.map(({ tick, writes }) => {
    const ws = writes.map(({ comp, frames }) => {
      const fs = frames.map((f) => '          ' + JSON.stringify(f)).join(',\n');
      return `      { "comp": ${comp}, "frames": [\n${fs}\n      ] }`;
    });
    return `    { "tick": ${tick}, "writes": [\n${ws.join(',\n')}\n    ] }`;
  });
  return `  "inputWrites": [\n${requests.join(',\n')}\n  ],\n`;
}

/** Fixture JSON with one component per line: reviewable diffs without 10 MB of pretty-printing. */
function render({ name, description, board, inputWrites }) {
  const comps = board.components
    .map((c) => '      ' + JSON.stringify(c))
    .join(',\n');
  return `{
  "name": ${JSON.stringify(name)},
  "description": ${JSON.stringify(description)},
  "ticks": 32,
${renderInputWrites(inputWrites)}  "board": {
    "components": [
${comps}
    ]
  }
}
`;
}

mkdirSync(OUT_DIR, { recursive: true });
for (const spec of BOARDS) {
  const path = join(OUT_DIR, `${spec.name}.json`);
  writeFileSync(path, render(spec));
  console.error(`wrote ${path} (${spec.board.components.length} components)`);
}
