// Cross-engine equivalence: drive the golden corpus through the **Node binding** (the built
// `.node` addon) and diff every tick against the same C++-oracle goldens the native `sim-core`
// suite and the WASM binding use (plan §10.1, phase 5). This proves the engine produces identical
// traces through the napi marshalling surface — constructor-from-object, tick, link, getOutputs,
// triggerInput, and the coherent `snapshot` Buffer plumbing.
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

const EVENT = { cont: 0, pulse: 1 };

function applyTriggers(sim, triggers, tick) {
  for (const t of triggers.filter((t) => t.tick === tick)) {
    const event = EVENT[t.event];
    assert.notEqual(event, undefined, `unknown trigger event '${t.event}'`);
    sim.triggerInput(t.comp, event, t.state);
  }
}

/// Diff one golden frame against the live sim: links (via `link()` and the Full-snapshot bytes) and
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
  const triggers = fx.triggers ?? [];
  try {
    // Trigger timing mirrors the generator: apply(tick) then observe the frame.
    applyTriggers(sim, triggers, 0);
    await diffFrame(sim, counts, golden.trace[0]);
    for (let i = 1; i < golden.trace.length; i++) {
      sim.tick();
      applyTriggers(sim, triggers, i);
      await diffFrame(sim, counts, golden.trace[i]);
    }
  } finally {
    sim.destroy();
  }
}

for (const name of fixtures()) {
  test(`golden trace matches on node: ${name}`, () => runFixture(name));
}
