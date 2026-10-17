// Component state through the Node binding: every method round-trips, `writeState` is checked on
// the JS thread and lands in order with `submitInput`, and a recorder read during `runAsync` gets
// its records and its count from the same tick boundary.
//
// Every async test is bounded (an explicit `stop()` then `await`) so the suite can never hang.

import test from "node:test";
import assert from "node:assert/strict";

import { Simulation } from "../index.js";

// Input 0 drives data links 0–2 and a trigger (link 3). Component 1 records them (capacity 4, with
// ticks); component 2 is a RAM of two 2-bit words (address = link 0, data = links 1–2,
// write-enable = link 4, never driven, clock = link 3) outputting on links 5–6.
const BOARD = {
  links: 7,
  components: [
    { type: 200, inputs: [], outputs: [0, 1, 2, 3] },
    { type: 201, inputs: [0, 1, 2, 3], outputs: [], ops: [4, 1] },
    { type: 17, inputs: [0, 1, 2, 4, 3], outputs: [5, 6] },
  ],
};

const COMPS = Uint32Array.of(0, 1, 2);

function versions(sim) {
  const out = new Uint32Array(3);
  sim.stateVersions(COMPS, out);
  return [...out];
}

/// A trigger pulse per value, carrying it on the data pins.
function pulses(values) {
  return values.flatMap((d) => {
    const data = [0, 1, 2].map((b) => ((d >> b) & 1) === 1);
    return [{ state: [...data, true] }, { state: [...data, false] }];
  });
}

test("state methods round-trip", async () => {
  const sim = new Simulation(BOARD);
  try {
    assert.deepEqual(versions(sim), [0, 0, 0]);
    assert.deepEqual([...COMPS].map((c) => sim.stateLength(c)), [0, 0, 1]);

    // Word 0 = 0b10, word 1 = 0b11; the padding nibble is dropped.
    sim.writeState(2, 0, Uint8Array.of(0xfe));
    assert.deepEqual([...(await sim.readState(2, 0, 1))], [0b1110]);
    const ramVersion = versions(sim)[2];
    assert.notEqual(ramVersion, 0, "the write moved the RAM's version");
    assert.equal((await sim.readState(1, 0, 0)).length, 0);

    sim.submitInput([{ comp: 0, frames: pulses([5, 2, 7, 1, 6]) }]);
    sim.run({ ticks: 12 });
    assert.deepEqual([...sim.getOutputs().subarray(4)], [0, 1], "the last pulse read word 0");

    const r = await sim.readRecords(1, 0);
    assert.equal(r.count, 5);
    assert.equal(r.first, 1, "record 0 was overwritten");
    assert.ok(r.values instanceof Uint32Array);
    assert.deepEqual([...r.values], [2, 7, 1, 6]);
    assert.ok(r.ticks instanceof Float64Array);
    assert.deepEqual([...r.ticks], [3, 5, 7, 9], "pulse k rises in tick 2k + 1");
    assert.deepEqual(versions(sim), [0, 5, ramVersion], "versions are exact once run() returns");

    const page = await sim.readRecords(1, 3, 1);
    assert.equal(page.first, 3);
    assert.deepEqual([...page.values], [1]);
  } finally {
    sim.destroy();
  }
});

test("state errors: writes refused synchronously, reads rejected", async () => {
  const sim = new Simulation(BOARD);
  try {
    assert.throws(() => sim.writeState(2, 0, new Uint8Array(2)), /state range 0\+2 exceeds its 1 state bytes/);
    assert.throws(() => sim.writeState(2, 2 ** 32 - 1, new Uint8Array(1)), /exceeds/);
    assert.throws(() => sim.writeState(1, 0, new Uint8Array(0)), /component 1: state is read-only/);
    assert.throws(() => sim.writeState(0, 0, new Uint8Array(0)), /read-only/);
    assert.throws(() => sim.writeState(3, 0, new Uint8Array(0)), /component 3 out of range/);
    assert.throws(() => sim.stateLength(3), /out of range/);
    assert.throws(() => sim.stateVersions(Uint32Array.of(0, 3), new Uint32Array(2)), /component 3 out of range/);
    assert.throws(() => sim.stateVersions(Uint32Array.of(0, 1), new Uint32Array(1)), /out has 1 entries/);

    await assert.rejects(sim.readState(2, 0, 2), /exceeds/);
    await assert.rejects(sim.readState(2, 2 ** 32 - 1, 2 ** 32 - 1), /exceeds/);
    await assert.rejects(sim.readRecords(1, 1), /since 1 is past its 0 records/);
    await assert.rejects(sim.readRecords(1, -1), /since must be a non-negative integer/);
    await assert.rejects(sim.readRecords(1, 0.5), /since must be a non-negative integer/);
    await assert.rejects(sim.readRecords(0, 0), /component 0 is not a recorder/);
    await assert.rejects(sim.readRecords(9, 0), /component 9 out of range/);

    // A refused write changed nothing.
    assert.deepEqual([...(await sim.readState(2, 0, 1))], [0]);
    assert.deepEqual(versions(sim), [0, 0, 0]);
  } finally {
    sim.destroy();
  }
});

// BOARD with its RAM in the legacy clocked-read mode, where a read clock is the one moment it reads.
const CLOCKED = {
  ...BOARD,
  components: BOARD.components.map((c) => (c.type === 17 ? { ...c, ops: [1] } : c)),
};

test("writeState lands no later than an input request submitted after it", async () => {
  const sim = new Simulation(CLOCKED);
  try {
    // A read clock at address 1 is the input's first frame: it must see the written word.
    sim.writeState(2, 0, Uint8Array.of(0b1100)); // word 1 = 0b11
    sim.submitInput([{ comp: 0, frames: [{ state: [true, false, false, true] }] }]);
    sim.tick();
    sim.tick();
    assert.deepEqual([...sim.getOutputs().subarray(4)], [1, 1]);
  } finally {
    sim.destroy();
  }
});

// Link 0 is a clock high for one tick in every 17; link 1 is never driven. Bit i of a 12-bit
// ripple counter is a JK flip-flop with J = K = high (negated reads of link 1) toggling on the
// falling edge of bit i − 1 (bit 0: the clock's rising edge); its Q is link 2 + 2i. The recorder
// (component 1, capacity 64) samples the counter on the clock, so record k holds k mod 4096.
const BITS = 12;
const CAPACITY = 64;
const COUNTER = {
  links: 2 + 2 * BITS,
  components: [
    { type: 6, inputs: [1], outputs: [0], ops: [16, 0] }, // pulse mode
    {
      type: 201,
      inputs: [...Array.from({ length: BITS }, (_, i) => 2 + 2 * i), 0],
      outputs: [],
      ops: [CAPACITY, 0],
    },
    ...Array.from({ length: BITS }, (_, i) => ({
      type: 14,
      inputs: [1, i === 0 ? 0 : 2 * i, 1],
      outputs: [2 + 2 * i, 3 + 2 * i],
      negInputs: i === 0 ? [0, 2] : [0, 1, 2],
    })),
  ],
};

test("readRecords during runAsync returns records and count from one boundary", async () => {
  const sim = new Simulation(COUNTER);
  const version = new Uint32Array(1);
  try {
    const run = sim.runAsync({});
    let since = 0;
    let lost = 0;
    for (let i = 0; i < 20; i++) {
      const r = await sim.readRecords(1, since);
      assert.ok(r.first >= since && r.first >= r.count - CAPACITY, `first ${r.first} of ${r.count}`);
      assert.equal(r.first + r.values.length, r.count, "everything up to the count");
      r.values.forEach((v, j) => assert.equal(v, (r.first + j) % 4096, `record ${r.first + j}`));
      lost += r.first - since;
      since = r.count;
    }
    sim.stateVersions(Uint32Array.of(1), version);
    assert.ok(version[0] > 0, "the version moves during the run");
    assert.ok(lost > 0, "a batch is long enough to overwrite records between reads");
    sim.stop();
    await run;
    const r = await sim.readRecords(1, 0, 0);
    sim.stateVersions(Uint32Array.of(1), version);
    assert.equal(version[0], r.count, "exact once runAsync resolves");
  } finally {
    sim.destroy();
  }
});
