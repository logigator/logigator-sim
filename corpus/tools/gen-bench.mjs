// Generate the synthetic benchmark boards into corpus/bench/.
//
// Unlike the corpus boards (small functional fixtures), these span the perf regimes the
// improvement phases target: fixed per-tick overhead (idle), hot-loop cost at several working-set
// sizes (active NOT rings), consumer-enqueue stress (fanout), and correlated multi-input flips
// (correlated). Each board is a corpus-fixture wrapper (`{name, description, ticks, board}`) so
// every CLI subcommand accepts it; none needs triggers — activity is self-sustaining (NOT rings,
// free-running clocks) and "constant low" inputs are undriven links, like `not_chain.json`.
//
// Everything is deterministic — no RNG — so regenerating reproduces the checked-in files
// byte-for-byte. Large boards are emitted one component per line to keep diffs reviewable.
//
// Usage: node gen-bench.mjs   (writes corpus/bench/*.json, progress to stderr)

import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const OUT_DIR = join(dirname(fileURLToPath(import.meta.url)), '..', 'bench');

// Wire-format component type ids (sim-core/src/types.rs).
const NOT = 1;
const AND = 2;
const XOR = 4;
const DELAY = 5;
const CLK = 6;
const FULL_ADDER = 11;
const D_FF = 13;

/** Minimal board builder mirroring the descriptor shape: links are just ids handed out in order. */
function builder() {
  let links = 0;
  const components = [];
  return {
    link: () => links++,
    links: (n) => Array.from({ length: n }, () => links++),
    comp(type, inputs, outputs, ops = []) {
      components.push({ type, inputs, outputs, ops });
      return components.length - 1;
    },
    finish: () => ({ links, components }),
  };
}

/**
 * A ring of `n` NOT gates: gate i reads link i and drives link (i+1) % n.
 *
 * Init seeding (I5) sets every NOT output high, so the whole ring flips low together on tick 1,
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
  b.comp(CLK, [enable], [out], [1]);
  return out;
}

/**
 * The idle shape shared by medium_idle / large_idle: one free-running clock whose output drives a
 * small NOT chain (the "active corner" — a handful of links flip every tick), plus enough idle
 * DELAY filler to reach `total` components. Per-tick cost should track the corner, not `total`.
 */
function idleBoard(total, cornerLen = 8) {
  const b = builder();
  const clk = freeClock(b);
  let prev = clk;
  for (let i = 0; i < cornerLen; i++) {
    const out = b.link();
    b.comp(NOT, [prev], [out]);
    prev = out;
  }
  idleFiller(b, total - 1 - cornerLen);
  return b.finish();
}

// --- the eight boards -------------------------------------------------------------------------

/** small_idle: the corpus `clk.json` board as-is (two clocks, one UserInput enable), minus the
 *  timed triggers — bench only applies tick-0 triggers, so both clocks free-run here. */
function smallIdle() {
  return {
    links: 4,
    components: [
      { type: CLK, inputs: [0], outputs: [1], ops: [1] },
      { type: 200, inputs: [], outputs: [2], ops: [] },
      { type: CLK, inputs: [2], outputs: [3], ops: [2] },
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

  // Counter bit 0 is a plain toggle: D = Qbar fed straight back.
  const q = [];
  const qb = [];
  {
    const [q0, qb0] = b.links(2);
    b.comp(D_FF, [qb0, clk], [q0, qb0]);
    q.push(q0);
    qb.push(qb0);
  }
  // Bits 1..: D_i = XOR(Q_i, T_i) with T_1 = Q_0 and T_i = AND(Q_0..Q_{i-1}).
  for (let i = 1; i < BITS; i++) {
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
    qb.push(qbi);
  }

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

// --- emit -------------------------------------------------------------------------------------

const BOARDS = [
  {
    name: 'small_idle',
    description:
      'clk.json board as-is, both clocks free-running (no triggers). Measures fixed per-tick overhead at a tiny working set.',
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
    name: 'correlated',
    description:
      '16-bit synchronous D-FF counter + 600 ripple-carry full adders + 354-D-FF bank on a common speed-1 clock (1000 components). Multi-input components see several inputs flip in the same tick (duplicate-compute stress); doubles as a medium board with a realistic mix.',
    board: correlated(600, 354),
  },
];

/** Fixture JSON with one component per line: reviewable diffs without 10 MB of pretty-printing. */
function render({ name, description, board }) {
  const comps = board.components
    .map((c) => '      ' + JSON.stringify(c))
    .join(',\n');
  return `{
  "name": ${JSON.stringify(name)},
  "description": ${JSON.stringify(description)},
  "ticks": 32,
  "triggers": [],
  "board": {
    "links": ${board.links},
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
  console.error(
    `wrote ${path} (${spec.board.components.length} components, ${spec.board.links} links)`,
  );
}
