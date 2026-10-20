// The WASM build's runAsync under Node, against the built package (`just build-wasm`): run status,
// batch length on a heavy board, concurrent runs, and a process that exits once its run has ended.
// These live here rather than in the wasm-bindgen-test suite, whose Node runner hangs awaiting a
// runAsync future.
//
// Every unbounded run goes through `whileRunning`, which stops it even when an assertion fails, so
// the suite can never hang on a runaway run.

import test from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { readFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import init, { Simulation } from "../pkg/sim_wasm.js";

const WASM = new URL("../pkg/sim_wasm_bg.wasm", import.meta.url);
await init({ module_or_path: readFileSync(WASM) });

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

// Run `body` during an unbounded runAsync, then stop the run and wait for it to end.
async function whileRunning(sim, body) {
  const run = sim.runAsync({});
  try {
    await body();
  } finally {
    sim.stop();
    await run;
  }
}

test("getStatus reports running from the runAsync() call until its promise settles", async () => {
  const sim = new Simulation(rings(1));
  try {
    await whileRunning(sim, async () => {
      assert.equal(sim.getStatus().running, true, "running as soon as runAsync() returns");
      await sleep(50);
      assert.equal(sim.getStatus().running, true);
    });
    assert.equal(sim.getStatus().running, false);
  } finally {
    sim.free();
  }
});

test("a heavy board still yields every few milliseconds", async () => {
  // On this board a window of ~1000 ticks between clock reads takes hundreds of milliseconds, far
  // past the 5 ms batch.
  const sim = new Simulation(rings(12000));
  try {
    let maxGap = 0;
    await whileRunning(sim, async () => {
      let last = performance.now();
      const until = last + 500;
      while (performance.now() < until) {
        await new Promise((r) => setTimeout(r, 0));
        const now = performance.now();
        maxGap = Math.max(maxGap, now - last);
        last = now;
      }
    });
    assert.ok(sim.getStatus().tick > 0, "the run made progress");
    assert.ok(maxGap < 60, `longest gap between event-loop turns: ${maxGap.toFixed(1)} ms`);
  } finally {
    sim.free();
  }
});

test("on a slow board, batch boundaries reach every phase of short periods", async () => {
  // A few ticks per batch: batches of one constant length would show a signal whose period
  // divides it in the same phase at every boundary, where the host reads state.
  const sim = new Simulation(rings(60000));
  try {
    const ticks = new Set();
    await whileRunning(sim, async () => {
      const until = performance.now() + 500;
      while (performance.now() < until) {
        await new Promise((r) => setImmediate(r));
        ticks.add(sim.getStatus().tick);
      }
    });
    for (let p = 2; p <= 6; p++) {
      const phases = new Set([...ticks].map((t) => t % p));
      assert.equal(phases.size, p, `phases of period ${p} seen over ${ticks.size} boundaries`);
    }
  } finally {
    sim.free();
  }
});

test("two simulations run at once: both progress, both stop", async () => {
  const a = new Simulation(rings(1));
  const b = new Simulation(rings(1));
  try {
    await whileRunning(a, () =>
      whileRunning(b, async () => {
        await sleep(50);
        const ticks = [a.getStatus().tick, b.getStatus().tick];
        assert.ok(ticks.every((t) => t > 0), `ticks after 50 ms: ${ticks}`);
      }),
    );
  } finally {
    a.free();
    b.free();
  }
});

test("a second run while runAsync is in flight is refused", async () => {
  const sim = new Simulation(rings(1));
  try {
    await whileRunning(sim, async () => {
      await assert.rejects(sim.runAsync({ ticks: 10 }), /already in progress/);
      assert.throws(() => sim.run({ ticks: 10 }), /in progress/);
    });
    sim.run({ ticks: 10 }); // allowed again once the run has ended
  } finally {
    sim.free();
  }
});

test("stop() then runAsync() restarts: the old run ends, the new one runs to its bound", async () => {
  const sim = new Simulation(rings(1));
  try {
    for (let i = 0; i < 50; i++) {
      const old = sim.runAsync({});
      await sleep(1);
      sim.stop();
      const before = sim.getStatus().tick;
      await sim.runAsync({ ticks: 10 });
      await old;
      assert.equal(sim.getStatus().tick, before + 10, `attempt ${i}`);
      assert.equal(sim.getStatus().running, false);
    }
  } finally {
    sim.stop();
    sim.free();
  }
});

test("free() during runAsync ends the run", async () => {
  const sim = new Simulation(rings(1));
  const run = sim.runAsync({});
  await sleep(5);
  sim.free();
  const outcome = await Promise.race([
    run.then(() => "ended"),
    sleep(2000, "still running", { ref: false }),
  ]);
  assert.equal(outcome, "ended");
});

test("a process exits by itself once its runAsync has ended", async () => {
  const pkg = fileURLToPath(new URL("../pkg/", import.meta.url));
  const script = `
    import { readFileSync } from "node:fs";
    const { default: init, Simulation } = await import(${JSON.stringify(pkg + "sim_wasm.js")});
    await init({ module_or_path: readFileSync(${JSON.stringify(pkg + "sim_wasm_bg.wasm")}) });
    const sim = new Simulation(${JSON.stringify(rings(1))});
    await sim.runAsync({ ms: 50 });
  `;
  await new Promise((resolve, reject) => {
    execFile(
      process.execPath,
      ["--input-type=module", "-e", script],
      { timeout: 10_000 },
      (err) => (err ? reject(err) : resolve()),
    );
  });
});
