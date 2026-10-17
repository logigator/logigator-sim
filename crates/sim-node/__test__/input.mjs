// Input frames through the Node binding: `submitInput` validation on the JS thread (each bad entry
// refused with nothing queued), `inputProgress` counting a request before the worker has picked it
// up, and a typed text arriving intact whether the ticks run as `tick()`, `run()` or `runAsync()`.

import test from "node:test";
import assert from "node:assert/strict";

import { Simulation } from "../index.js";

// A keyboard: one input with 7 data bits (links 0–6) and a strobe (link 7), and seven D flip-flops
// latching the data bits on the strobe's rising edge (Q on links 8–14, Q̄ on 15–21).
const KEYBOARD = {
  components: [
    { type: 200, outputs: [0, 1, 2, 3, 4, 5, 6, 7] },
    ...Array.from({ length: 7 }, (_, bit) => ({
      type: 13,
      inputs: [bit, 7],
      outputs: [8 + bit, 15 + bit],
    })),
  ],
};

const TEXT = "Hello, World!";

/// A press (data + strobe) and a release (data only) per character.
function typeText(text, hold) {
  return [...text].flatMap((ch) => {
    const data = Array.from({ length: 7 }, (_, bit) => ((ch.charCodeAt(0) >> bit) & 1) === 1);
    return [
      { state: [...data, true], hold },
      { state: [...data, false], hold },
    ];
  });
}

/// The byte the flip-flops latched: the Q pins in `getOutputs` (input pins 0–7, then Q/Q̄ pairs).
function latched(sim) {
  const out = sim.getOutputs();
  let v = 0;
  for (let bit = 0; bit < 7; bit++) v |= out[8 + 2 * bit] << bit;
  return v;
}

/// Type `TEXT`, advancing one character's worth of ticks at a time with `step`, and collect what
/// the circuit latched after each.
async function roundTrip(hold, step) {
  const sim = new Simulation(KEYBOARD);
  try {
    sim.submitInput([{ comp: 0, frames: typeText(TEXT, hold) }]);
    let got = "";
    for (let i = 0; i < TEXT.length; i++) {
      await step(sim, 2 * hold);
      got += String.fromCharCode(latched(sim));
    }
    return got;
  } finally {
    sim.destroy();
  }
}

for (const hold of [1, 2, 3]) {
  test(`text round trip, hold ${hold}: tick(), run() and runAsync() agree`, async () => {
    const ticks = (sim, n) => {
      for (let i = 0; i < n; i++) sim.tick();
    };
    assert.equal(await roundTrip(hold, ticks), TEXT, "tick()");
    assert.equal(await roundTrip(hold, (sim, n) => sim.run({ ticks: n })), TEXT, "run()");
    assert.equal(await roundTrip(hold, (sim, n) => sim.runAsync({ ticks: n })), TEXT, "runAsync()");
  });
}

test("inputProgress counts a request submitted during runAsync before the worker drains it", async () => {
  const sim = new Simulation(KEYBOARD);
  const progress = new Uint32Array(1);
  try {
    const run = sim.runAsync({});
    // Long holds: none of these can finish while the test runs.
    sim.submitInput([{ comp: 0, frames: typeText(TEXT, 1_000_000) }]);
    sim.inputProgress(progress);
    assert.ok(progress[0] > 0, "pending right after the submit");
    assert.ok(progress[0] <= 2 * TEXT.length);

    // Coalescing frames never count.
    const before = progress[0];
    sim.submitInput([{ comp: 0, frames: [{ state: Array(8).fill(null), coalesce: true }] }]);
    sim.inputProgress(progress);
    assert.ok(progress[0] <= before);

    // Cancel: once the worker has served it, nothing is pending.
    sim.submitInput([{ comp: 0, replace: true, frames: [] }]);
    sim.stop();
    await run;
    sim.getOutputs(); // a round trip: the worker has handled every earlier command
    sim.inputProgress(progress);
    assert.equal(progress[0], 0);
  } finally {
    sim.destroy();
  }
});

test("inputProgress is exact once tick(), run() or runAsync() returns", async () => {
  const sim = new Simulation(KEYBOARD);
  const progress = new Uint32Array(1);
  const pending = () => (sim.inputProgress(progress), progress[0]);
  try {
    // Every call is checked straight after it returns: the worker must have published the frames
    // it applied before answering, not just eventually. A late publish is a race the JS thread
    // usually loses, so the check repeats a few hundred times.
    const text = "The quick brown fox jumps over the lazy dog. ".repeat(10);
    sim.submitInput([{ comp: 0, frames: typeText(text, 1) }]);
    let left = 2 * text.length;
    assert.equal(pending(), left);
    for (let i = 0; i < text.length; i++) {
      sim.tick();
      assert.equal(pending(), --left, "after tick()");
    }
    sim.run({ ticks: 7 });
    left -= 7;
    assert.equal(pending(), left, "after run()");
    await sim.runAsync({ ticks: 9 });
    left -= 9;
    assert.equal(pending(), left, "after runAsync()");
    sim.run({ ticks: left + 5 });
    assert.equal(pending(), 0, "drained");
    assert.throws(() => sim.inputProgress(new Uint32Array(0)), /out has 0 entries/);
  } finally {
    sim.destroy();
  }
});

test("submitInput refuses every bad entry whole, with nothing queued", () => {
  const board = {
    components: [
      { type: 200, outputs: [0, 1] },
      { type: 1, inputs: [0], outputs: [2] },
      { type: 200, outputs: [3] },
    ],
  };
  const good = { comp: 2, frames: [{ state: [true] }] };
  const frame = (f) => ({ comp: 0, frames: [f] });
  // eslint-disable-next-line no-sparse-arrays
  const hole = [true, , true].slice(1); // length 2, index 0 is a hole
  const cases = [
    [[good, { comp: 1, frames: [] }], /component 1 is not a user-input/],
    [[good, { comp: 9, frames: [] }], /component 9 is not a user-input/],
    [[good, frame({ state: [true] })], /input 0 frame 0: state has 1 entries, the input has 2 pins/],
    [[good, frame({ state: [true, 0] })], /input 0 frame 0: state\[1\] must be true, false or null/],
    [[good, frame({ state: [true, undefined] })], /state\[1\] must be true, false or null/],
    [[good, frame({ state: hole })], /state\[0\] must be true, false or null/],
    [[good, frame({ state: [true, true], hold: 0 })], /input 0 frame 0: hold must be/],
    [[good, frame({ state: [true, true], hold: 1.5 })], /hold must be/],
    [[good, frame({ state: [true, true], hold: NaN })], /hold must be/],
    [[good, frame({ state: [true, true], hold: 2 ** 32 })], /hold must be/],
    [[good, good], /input 2 appears more than once/],
    [[good, { comp: 0, frames: Array(65_537).fill({ state: [true, true] }) }], /exceed the limit of 65536/],
    [[good, { comp: 0, frames: {} }], /frames must be an array/],
    [[good, { comp: -1, frames: [] }], /comp must be a component id/],
  ];
  for (const [writes, message] of cases) {
    const sim = new Simulation(board);
    const progress = new Uint32Array(2);
    try {
      assert.throws(() => sim.submitInput(writes), message);
      sim.inputProgress(progress);
      assert.deepEqual([...progress], [0, 0], `${message}: nothing pending`);
      sim.run({ ticks: 3 });
      assert.deepEqual([...sim.getOutputs()], [0, 0, 1, 0], `${message}: pins untouched`);
    } finally {
      sim.destroy();
    }
  }
});

test("a full queue refuses more events but accepts a replace", () => {
  const sim = new Simulation({ components: [{ type: 200, outputs: [0] }] });
  const progress = new Uint32Array(1);
  try {
    const events = (n) => Array(n).fill({ state: [true] });
    sim.submitInput([{ comp: 0, frames: events(65_536) }]);
    assert.throws(() => sim.submitInput([{ comp: 0, frames: events(1) }]), /exceed the limit/);
    sim.submitInput([{ comp: 0, frames: [{ state: [false], coalesce: true }] }]);
    sim.submitInput([{ comp: 0, replace: true, frames: events(65_536) }]);
    sim.getOutputs();
    sim.inputProgress(progress);
    assert.equal(progress[0], 65_536);
  } finally {
    sim.destroy();
  }
});
