// Throughput spot-check for the WASM engine's *async* path under Node: drive `runAsync` for a fixed
// wall-clock window and measure how many ticks land. Unlike the blocking `run()` (bench-wasm.mjs),
// `runAsync` ticks in batches and yields to the event loop between them, so its throughput depends
// on how much work each batch does relative to the per-yield `setTimeout(0)` cost — exactly what the
// batch-sizing strategy governs.
//
// Time-bounded (not tick-bounded) so every board takes the same wall-clock per repeat regardless of
// speed, and so old/new batch strategies compare apples-to-apples under the same `ms` bound.
//
// A Node-hosted wasm number is a stand-in for the browser (same engine, same JIT family); re-measure
// in a real browser if a decision ever hinges on it.
//
// Time-bound by default; `--ticks N` forces a fixed step count.
//
// Usage: node bench-wasm-async.mjs <board.json> [--ms N | --ticks N] [--repeat R]

import { readFileSync } from 'node:fs';

import init, { Simulation } from '../../crates/sim-wasm/pkg/sim_wasm.js';

await init({
  module_or_path: readFileSync(
    new URL('../../crates/sim-wasm/pkg/sim_wasm_bg.wasm', import.meta.url),
  ),
});

function arg(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? Number(process.argv[i + 1]) : dflt;
}

const boardPath = process.argv[2];
if (!boardPath) {
  console.error('usage: node bench-wasm-async.mjs <board.json> [--ms N | --ticks N] [--repeat R]');
  process.exit(2);
}
const ticksArg = arg('ticks', null); // null → time-bound by --ms
const ms = arg('ms', 1000);
const repeat = arg('repeat', 5);
const bound = ticksArg != null ? { ticks: ticksArg } : { ms };
const label = ticksArg != null ? `${ticksArg} ticks` : `${ms} ms window`;

const fixture = JSON.parse(readFileSync(boardPath, 'utf8'));
const board = fixture.board ?? fixture;
const name = fixture.name ?? boardPath;

console.error(`benching ${name} via wasm runAsync — ${label} × ${repeat} repeats`);

let best = 0;
let sum = 0;
for (let r = 1; r <= repeat; r++) {
  const sim = new Simulation(board);
  const start = process.hrtime.bigint();
  await sim.runAsync(bound);
  const secs = Number(process.hrtime.bigint() - start) / 1e9;
  const ticks = sim.getStatus().tick; // fresh sim starts at 0, so this is the ticks this repeat ran
  sim.destroy();

  const tps = ticks / Math.max(secs, 1e-12);
  best = Math.max(best, tps);
  sum += tps;
  console.error(`  run ${r}: ${(secs * 1e3).toFixed(3)} ms, ${ticks} ticks -> ${Math.round(tps)} ticks/s`);
}

console.log(
  `${name}: best ${Math.round(best)} ticks/s, mean ${Math.round(sum / repeat)} ticks/s (${repeat} repeats)`,
);