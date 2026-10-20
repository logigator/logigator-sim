// Cross-engine equivalence: drive the golden corpus through the **Node binding** (the built
// `.node` addon) and diff every tick against the same goldens the native `sim-core` suite and the
// WASM binding use. This proves the engine produces identical traces through the napi marshalling
// surface — constructor-from-object, tick, link, getOutputs, submitInput, and the coherent
// `snapshot` Buffer plumbing.
//
// A fixture's `inputWrites` requests are passed to `submitInput` as-is, each once the simulation has
// run its `tick` ticks, in the order `sim trace` records the goldens: step, submit, observe.
//
// Run after `napi build` (the justfile `test-node` recipe builds first): `node --test __test__/`.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { Simulation } from "../index.js";

const here = dirname(fileURLToPath(import.meta.url));
const boardsDir = join(here, "..", "..", "..", "corpus", "boards");
const goldenDir = join(here, "..", "..", "..", "corpus", "golden");

/// Every corpus board that has a matching golden trace, discovered from the filesystem.
function fixtures() {
  return readdirSync(boardsDir)
    .filter((f) => f.endsWith(".json"))
    .map((f) => f.slice(0, -5))
    .filter((name) => existsSync(join(goldenDir, `${name}.json`)))
    .sort();
}

/// Submit the requests due once the simulation has run `tick` ticks.
function submitDue(sim, requests, tick) {
  for (const r of requests.filter((r) => r.tick === tick)) sim.submitInput(r.writes);
}

/// Diff one golden frame against the live sim: links (via `link()` and the Full-snapshot bytes), the
/// snapshot's invalid links, and
/// component output pins (via `getOutputs`, segmented by per-component output counts).
async function diffFrame(sim, counts, frame) {
  const linkCount = frame.links.length;

  for (let l = 0; l < linkCount; l++) {
    assert.equal(sim.link(l), frame.links[l] === "1", `tick ${frame.tick}: link ${l} via link()`);
  }

  const snap = await sim.snapshot(false, 0.0);
  assert.equal(snap.isDelta, false, "a delta=false request must be Full");
  assert.equal(snap.links.length, Math.ceil(linkCount / 8), "Full snapshot byte length");
  for (let l = 0; l < linkCount; l++) {
    const bit = ((snap.links[l >> 3] >> (l & 7)) & 1) === 1;
    assert.equal(bit, frame.links[l] === "1", `tick ${frame.tick}: link ${l} via snapshot bytes`);
  }
  assert.deepEqual([...snap.invalid], frame.invalid ?? [], `tick ${frame.tick}: invalid links`);

  const out = sim.getOutputs();
  let off = 0;
  for (let c = 0; c < frame.outputs.length; c++) {
    const pins = frame.outputs[c];
    for (let p = 0; p < pins.length; p++) {
      assert.equal(
        out[off + p] !== 0,
        pins[p] === "1",
        `tick ${frame.tick}: component ${c} output[${p}] via getOutputs`,
      );
    }
    off += counts[c];
  }
}

async function runFixture(name) {
  const fx = JSON.parse(readFileSync(join(boardsDir, `${name}.json`), "utf8"));
  const golden = JSON.parse(readFileSync(join(goldenDir, `${name}.json`), "utf8"));
  assert.equal(golden.trace.length, fx.ticks + 1, `[${name}] golden frame count`);

  const counts = fx.board.components.map((c) => c.outputs.length);
  const sim = new Simulation(fx.board);
  const requests = fx.inputWrites ?? [];
  try {
    for (let i = 0; i < golden.trace.length; i++) {
      if (i > 0) sim.tick();
      submitDue(sim, requests, i);
      await diffFrame(sim, counts, golden.trace[i]);
    }
  } finally {
    sim.destroy();
  }
}

for (const name of fixtures()) {
  test(`golden trace matches on node: ${name}`, () => runFixture(name));
}

// Confirms the `negInputs`/`negOutputs` fields marshal across napi and compute the
// hand-derived truth table — a NAND (negated output) and a NOR (both inputs negated) over the same
// two inputs. The WASM suite asserts the same board, so the two bindings agree.
test("negated ports marshal across napi (NAND + NOR)", () => {
  const board = {
    components: [
      { type: 200, outputs: [0] }, // a
      { type: 200, outputs: [1] }, // b
      { type: 2, inputs: [0, 1], outputs: [2], negOutputs: [0] }, // NAND
      { type: 2, inputs: [0, 1], outputs: [3], negInputs: [0, 1] }, // NOR
    ],
  };
  const sim = new Simulation(board);
  try {
    const check = (a, b, nand, nor) => {
      sim.submitInput([
        { comp: 0, frames: [{ state: [a], coalesce: true }] },
        { comp: 1, frames: [{ state: [b], coalesce: true }] },
      ]);
      sim.run({ ticks: 8 });
      assert.equal(sim.link(2), nand, `NAND a=${a} b=${b}`);
      assert.equal(sim.link(3), nor, `NOR a=${a} b=${b}`);
    };
    check(false, false, true, true);
    check(true, false, true, false);
    check(false, true, true, false);
    check(true, true, false, false);
  } finally {
    sim.destroy();
  }
});

// The constructor hands ids to the engine as they are, and the engine's own validation rejects
// them: an id whose low 16 bits name a real type or pin (`Not`, pin 0) is not narrowed onto it.
// The WASM suite asserts the same messages.
test("the constructor leaves validation to the engine, without narrowing ids", () => {
  const not = (extra) => ({ components: [{ type: 1, inputs: [0], outputs: [1], ...extra }] });
  assert.throws(() => new Simulation(not({ type: 0x1_0001 })), /component 0: unknown component type id 65537/);
  assert.throws(() => new Simulation(not({ negInputs: [0x1_0000] })), /component 0: negated input index 65536 out of range/);
  assert.throws(() => new Simulation(not({ inputs: [2 ** 31] })), /component 0: link id 2147483648 exceeds/);
  assert.throws(() => new Simulation(not({ period: 1 })), /component 0 \(Not\): period is not a parameter of this type/);
});

// A ROM's data reads the same as a Uint8Array or a plain byte array, and what the core types cannot
// hold (an unknown mode, a byte past 255) is refused at the boundary. The WASM suite asserts the
// same.
test("per-type parameters marshal across napi", () => {
  // 1 address bit, 2-bit words: word 0 = 0b10, word 1 = 0b11.
  const rom = (data) => ({
    components: [
      { type: 200, outputs: [0] },
      { type: 12, inputs: [0], outputs: [1, 2], data },
    ],
  });
  for (const data of [Uint8Array.of(0b1110), [0b1110]]) {
    const sim = new Simulation(rom(data));
    try {
      const word = (address) => {
        sim.submitInput([{ comp: 0, frames: [{ state: [address === 1], coalesce: true }] }]);
        sim.run({ ticks: 4 });
        return [sim.link(1), sim.link(2)];
      };
      assert.deepEqual(word(0), [false, true], `word 0 from ${data.constructor.name}`);
      assert.deepEqual(word(1), [true, true], `word 1 from ${data.constructor.name}`);
    } finally {
      sim.destroy();
    }
  }

  const clk = (extra) => ({ components: [{ type: 6, inputs: [0], outputs: [1], period: 1, ...extra }] });
  assert.throws(() => new Simulation(clk({ mode: "square", read: "clocked" })), /component 0 \(Clk\): read is not a parameter/);
});
