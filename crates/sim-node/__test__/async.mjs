// The async / background-run surface: runAsync (bounded + interruptible),
// the coherent in-run snapshot handoff, delta snapshots, and the liveness guarantee that
// getStatus / snapshot stay responsive during an *unbounded* run.
//
// Every async test is bounded (finite `ticks`, or an explicit `stop()` then `await`) so the suite
// can never hang on a runaway run.

import test from "node:test";
import assert from "node:assert/strict";

import { Simulation } from "../index.js";

// inA → NOT → link1 ; inB → NOT → link3 — small, well-defined change sets.
const BOARD = {
  links: 4,
  components: [
    { type: 200, inputs: [], outputs: [0] },
    { type: 1, inputs: [0], outputs: [1] },
    { type: 200, inputs: [], outputs: [2] },
    { type: 1, inputs: [2], outputs: [3] },
  ],
};

const STOPPED = 1;
const RUNNING = 2;

test("bounded runAsync resolves and reports the final tick", async () => {
  const sim = new Simulation(BOARD);
  try {
    await sim.runAsync({ ticks: 1000 });
    const s = sim.getStatus();
    assert.equal(s.state, STOPPED);
    assert.equal(s.tick, 1000);
  } finally {
    sim.destroy();
  }
});

test("unbounded runAsync stays live: getStatus is non-blocking, stop() resolves it", async () => {
  const sim = new Simulation(BOARD);
  try {
    const run = sim.runAsync({}); // unbounded
    // getStatus must return promptly while the sim ticks in the background (lock-free atomics).
    const t0 = Date.now();
    let sawRunning = false;
    for (let i = 0; i < 5; i++) {
      if (sim.getStatus().state === RUNNING) sawRunning = true;
      await new Promise((r) => setTimeout(r, 5));
    }
    assert.ok(Date.now() - t0 < 1000, "polling getStatus during a run must not block");
    assert.ok(sawRunning, "an unbounded run reports Running");
    sim.stop();
    await run; // resolves at the next batch boundary
    assert.equal(sim.getStatus().state, STOPPED);
  } finally {
    sim.destroy();
  }
});

test("snapshot resolves coherently during an unbounded run", async () => {
  const sim = new Simulation(BOARD);
  try {
    const run = sim.runAsync({});
    const snap = await sim.snapshot(false, 0.5); // served at a tick boundary
    assert.equal(snap.isDelta, false);
    assert.equal(snap.links.length, Math.ceil(sim.linkCount() / 8));
    sim.stop();
    await run;
  } finally {
    sim.destroy();
  }
});

test("Full then Delta: a small post-baseline change is a coherent delta", async () => {
  const sim = new Simulation(BOARD);
  try {
    sim.run({ ticks: 5 });

    // First delta request returns the Full baseline.
    const base = await sim.snapshot(true, 1.0);
    assert.equal(base.isDelta, false, "first snapshot is the Full baseline");

    // Drive A high; after settling exactly links 0 (inA) and 1 (NOT A) have flipped.
    sim.triggerInput(0, 0, [true]);
    sim.run({ ticks: 5 });

    const d = await sim.snapshot(true, 1.0);
    assert.equal(d.isDelta, true, "a small post-baseline change set is a Delta");
    const ids = new Uint32Array(d.ids.buffer, d.ids.byteOffset, d.ids.length / 4);
    assert.ok(ids.length >= 1, "delta carries the changed links");
    for (let i = 0; i < ids.length; i++) {
      const bit = ((d.values[i >> 3] >> (i & 7)) & 1) === 1;
      assert.equal(bit, sim.link(ids[i]), `delta value for link ${ids[i]}`);
    }
    assert.ok([...ids].includes(0) && [...ids].includes(1), "links 0 and 1 flipped");
  } finally {
    sim.destroy();
  }
});

test("run() rejects an unbounded blocking run", () => {
  const sim = new Simulation(BOARD);
  try {
    assert.throws(() => sim.run({}), /finite/);
  } finally {
    sim.destroy();
  }
});

test("fromJson builds an equivalent simulation", async () => {
  const sim = Simulation.fromJson(JSON.stringify(BOARD));
  try {
    sim.run({ ticks: 5 });
    // NOT of a low input settles high: links 1 and 3.
    assert.equal(sim.link(1), true);
    assert.equal(sim.link(3), true);
  } finally {
    sim.destroy();
  }
});
