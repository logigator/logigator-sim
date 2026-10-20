// The WASM build's runAsync under Node, against the built package (`just build-wasm`): run status,
// slice length on a heavy board, concurrent runs, and a process that exits once its run has ended.
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

const STOPPED = 1;
const RUNNING = 2;

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

test("getStatus reports Running and a live speed during runAsync, Stopped and 0 after", async () => {
  const sim = new Simulation(rings(1));
  try {
    await whileRunning(sim, async () => {
      assert.equal(sim.getStatus().state, RUNNING, "Running as soon as runAsync() returns");
      await sleep(300);
      const live = sim.getStatus();
      assert.equal(live.state, RUNNING);
      assert.ok(live.speed > 0, `speed during the run: ${live.speed}`);
    });
    const after = sim.getStatus();
    assert.equal(after.state, STOPPED);
    assert.equal(after.speed, 0);
  } finally {
    sim.free();
  }
});

test("a blocking run() reports its average speed", () => {
  const sim = new Simulation(rings(1));
  try {
    sim.run({ ms: 50 });
    assert.ok(sim.getStatus().speed > 0);
  } finally {
    sim.free();
  }
});

test("a heavy board still yields every few milliseconds", async () => {
  // About 1000 ticks run before the core first reads the clock; on this board they take hundreds
  // of milliseconds, far past the 5 ms slice.
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
      assert.equal(sim.getStatus().state, STOPPED);
    }
  } finally {
    sim.stop();
    sim.free();
  }
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
