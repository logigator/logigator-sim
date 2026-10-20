// The async / background-run surface: runAsync (bounded + interruptible),
// the coherent in-run snapshot handoff, delta snapshots, and the liveness guarantee that
// getStatus / snapshot stay responsive during an *unbounded* run.
//
// Every async test is bounded (finite `ticks`, or an explicit `stop()` then `await`) so the suite
// can never hang on a runaway run.

import test from "node:test";
import assert from "node:assert/strict";
import { setTimeout as sleep } from "node:timers/promises";

import { Simulation } from "../index.js";

// inA → NOT → link1 ; inB → NOT → link3 — small, well-defined change sets.
const BOARD = {
  components: [
    { type: 200, outputs: [0] },
    { type: 1, inputs: [0], outputs: [1] },
    { type: 200, outputs: [2] },
    { type: 1, inputs: [2], outputs: [3] },
  ],
};

// `n` rings of three NOT gates: every link flips every tick, so a tick's cost grows with `n`.
function rings(n) {
  return {
    components: Array.from({ length: 3 * n }, (_, i) => ({
      type: 1,
      inputs: [i],
      outputs: [i % 3 === 2 ? i - 2 : i + 1],
    })),
  };
}

test("on a heavy board, a timed run() ends near its bound and an in-run snapshot is prompt", async () => {
  // On this board a few thousand ticks take a second or more, so a run that read the clock or
  // drained commands only every few thousand ticks would overrun both by far.
  const sim = new Simulation(rings(12000));
  try {
    let t = performance.now();
    sim.run({ ms: 5 });
    const runMs = performance.now() - t;
    assert.ok(runMs < 60, `run({ ms: 5 }) took ${runMs.toFixed(1)} ms`);

    const run = sim.runAsync({});
    try {
      await sleep(50);
      t = performance.now();
      await sim.snapshot(false, 0);
      const snapMs = performance.now() - t;
      assert.ok(snapMs < 60, `snapshot during the run took ${snapMs.toFixed(1)} ms`);
    } finally {
      sim.stop();
      await run;
    }
  } finally {
    sim.destroy();
  }
});

test("bounded runAsync resolves and reports the final tick", async () => {
  const sim = new Simulation(BOARD);
  try {
    await sim.runAsync({ ticks: 1000 });
    const s = sim.getStatus();
    assert.equal(s.running, false);
    assert.equal(s.tick, 1000);
  } finally {
    sim.destroy();
  }
});

test("unbounded runAsync stays live: getStatus is non-blocking, stop() resolves it", async () => {
  const sim = new Simulation(BOARD);
  try {
    const run = sim.runAsync({}); // unbounded
    assert.equal(sim.getStatus().running, true, "running as soon as runAsync() returns");
    // getStatus must return promptly while the sim ticks in the background (lock-free atomics).
    const t0 = Date.now();
    let sawRunning = false;
    for (let i = 0; i < 5; i++) {
      if (sim.getStatus().running) sawRunning = true;
      await new Promise((r) => setTimeout(r, 5));
    }
    assert.ok(Date.now() - t0 < 1000, "polling getStatus during a run must not block");
    assert.ok(sawRunning, "an unbounded run reports Running");
    sim.stop();
    await run; // resolves at the next batch boundary
    assert.equal(sim.getStatus().running, false);
  } finally {
    sim.destroy();
  }
});

test("stop() straight after runAsync() ends the run, even before the worker picks it up", async () => {
  // No round trip between the two calls, so the worker often dequeues the run after the stop. A
  // lost stop would leave the run going forever; the unref'd timer turns that into a failure, and
  // destroy() still ends the run so the process can exit.
  for (let i = 0; i < 200; i++) {
    const sim = new Simulation(BOARD);
    try {
      const run = sim.runAsync({});
      sim.stop();
      const outcome = await Promise.race([
        run.then(() => "stopped"),
        sleep(2000, "still running", { ref: false }),
      ]);
      assert.equal(outcome, "stopped", `attempt ${i}`);
    } finally {
      sim.destroy();
    }
  }
});

test("stop() then runAsync() restarts: the old run ends, the new one is accepted", async () => {
  const sim = new Simulation(BOARD);
  try {
    for (let i = 0; i < 200; i++) {
      const old = sim.runAsync({});
      await sleep(Math.random() * 2);
      sim.stop();
      const next = sim.runAsync({ ticks: 10 });
      await old;
      await next; // rejects if the worker still saw the stopped run as in progress
      assert.equal(sim.getStatus().running, false, `attempt ${i}`);
    }
  } finally {
    sim.destroy();
  }
});

test("a refused second runAsync still leaves running false once the first ends", async () => {
  const sim = new Simulation(BOARD);
  try {
    const run = sim.runAsync({});
    await assert.rejects(sim.runAsync({ ticks: 10 }), /already in progress/);
    assert.equal(sim.getStatus().running, true);
    sim.stop();
    await run;
    assert.equal(sim.getStatus().running, false);
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
    sim.submitInput([{ comp: 0, frames: [{ state: [true] }] }]);
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
    // Bounds too large to represent lower to unbounded; `ms: Infinity` used to panic instead.
    assert.throws(() => sim.run({ ms: Infinity }), /finite/);
    assert.throws(() => sim.run({ ticks: Infinity }), /finite/);
  } finally {
    sim.destroy();
  }
});
