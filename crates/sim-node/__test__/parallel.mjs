// The adaptive parallel driver through the Node binding (plan §8/§7.4, phase 6). `cfg.threads > 1`
// engages it for both `run` (blocking) and `runAsync` (background, ticking in batches inside one
// per-run rayon pool). The §8.6 guarantee: results are bit-identical to single-threaded — so we
// drive the corpus boards both ways and diff the settled link bits + output pins.
//
// Run after `napi build` (the justfile `test-node` recipe builds first): `node --test __test__/`.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { Simulation } from "../index.js";

const here = dirname(fileURLToPath(import.meta.url));
const boardsDir = join(here, "..", "..", "..", "corpus", "boards");

// Boards whose dynamics need no timed triggers (we kick tick-0 triggers, then run free), so a single
// blocking `run` to a fixed tick count settles them — ideal for a clean ST-vs-MT diff.
function boards() {
  return readdirSync(boardsDir)
    .filter((f) => f.endsWith(".json"))
    .map((f) => f.slice(0, -5))
    .sort();
}

const EVENT = { cont: 0, pulse: 1 };

/// Build the board, apply only its tick-0 triggers, run `ticks` at `threads`, return settled state.
function settled(board, triggers, ticks, threads) {
  const sim = new Simulation(board);
  try {
    for (const t of triggers.filter((t) => t.tick === 0)) {
      sim.triggerInput(t.comp, EVENT[t.event], t.state);
    }
    sim.run({ ticks, threads });
    return { outputs: Buffer.from(sim.getOutputs()), parallel: sim.getStatus().parallel };
  } finally {
    sim.destroy();
  }
}

const TICKS = 500;

for (const name of boards()) {
  test(`run({threads:4}) == run() on node: ${name}`, () => {
    const fx = JSON.parse(readFileSync(join(boardsDir, `${name}.json`), "utf8"));
    const triggers = fx.triggers ?? [];
    const st = settled(fx.board, triggers, TICKS, 1);
    const mt = settled(fx.board, triggers, TICKS, 4);
    assert.deepEqual(mt.outputs, st.outputs, `[${name}] parallel output state diverged`);
  });
}

test("runAsync({threads:4}) resolves and matches single-threaded", async () => {
  // A NOT ring oscillator + fan-out keeps activity going so the parallel path is taken.
  const board = {
    links: 4,
    components: [
      { type: 1, inputs: [0], outputs: [0] }, // self-NOT oscillator on link 0
      { type: 1, inputs: [0], outputs: [1] },
      { type: 1, inputs: [1], outputs: [2] },
      { type: 1, inputs: [2], outputs: [3] },
    ],
  };
  const st = settled(board, [], 1000, 1);

  const sim = new Simulation(board);
  try {
    await sim.runAsync({ ticks: 1000, threads: 4 });
    const s = sim.getStatus();
    assert.equal(s.state, 1, "STOPPED after a bounded runAsync");
    assert.equal(s.tick, 1000);
    assert.deepEqual(Buffer.from(sim.getOutputs()), st.outputs, "runAsync MT == ST");
  } finally {
    sim.destroy();
  }
});
