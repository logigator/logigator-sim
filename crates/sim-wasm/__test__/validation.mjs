// Malformed boards, run configs and link ids: refused with the same messages as the Node addon
// (the cases are shared, in corpus/validation-cases.mjs).

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import init, { Simulation } from "../pkg/sim_wasm.js";
import {
  BOARD,
  BOARDS,
  LINK_OUT_OF_RANGE,
  RUN_CONFIGS,
  UNKNOWN_FIELD,
} from "../../../corpus/validation-cases.mjs";

await init({ module_or_path: readFileSync(new URL("../pkg/sim_wasm_bg.wasm", import.meta.url)) });

for (const { name, board, error } of BOARDS) {
  test(`board: ${name}`, () => {
    assert.throws(() => new Simulation(board), { message: error });
  });
}

test("board: unknown fields are ignored", () => {
  new Simulation(UNKNOWN_FIELD).free();
});

for (const { name, config, error } of RUN_CONFIGS) {
  test(`run config: ${name}`, () => {
    const sim = new Simulation(BOARD);
    try {
      assert.throws(() => sim.runAsync(config), { message: error });
      assert.throws(() => sim.run(config), { message: error });
      assert.equal(sim.getStatus().running, false, "nothing started");
    } finally {
      sim.free();
    }
  });
}

test("link: an id past the last link", () => {
  const sim = new Simulation(BOARD);
  try {
    assert.throws(() => sim.link(LINK_OUT_OF_RANGE.id), { message: LINK_OUT_OF_RANGE.error });
  } finally {
    sim.free();
  }
});
