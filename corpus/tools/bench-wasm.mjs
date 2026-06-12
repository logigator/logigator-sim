// Throughput spot-check for the WASM engine under Node: time the *blocking* `run()` — the path
// that pays a `performance.now()` read every tick. A Node-hosted wasm number is a stand-in for the
// browser (same engine, same JIT family); re-measure in a real browser if a decision ever hinges
// on it.
//
// Requires a nodejs-target build first:
//   RUSTFLAGS="-C target-feature=+simd128" wasm-pack build crates/sim-wasm --release \
//     --target nodejs --out-dir pkg-node -- --no-default-features --features serde
//
// Usage: node bench-wasm.mjs <board.json> [--ticks N] [--repeat R]

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { Simulation } = require(join(
  dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  'crates',
  'sim-wasm',
  'pkg-node',
  'sim_wasm.js',
));

function arg(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? Number(process.argv[i + 1]) : dflt;
}

const boardPath = process.argv[2];
if (!boardPath) {
  console.error('usage: node bench-wasm.mjs <board.json> [--ticks N] [--repeat R]');
  process.exit(2);
}
const ticks = arg('ticks', 1_000_000);
const repeat = arg('repeat', 10);

const fixture = JSON.parse(readFileSync(boardPath, 'utf8'));
const board = fixture.board ?? fixture;
const name = fixture.name ?? boardPath;

console.error(`benching ${name} via wasm blocking run() — ${ticks} ticks × ${repeat} repeats`);

let best = 0;
let sum = 0;
for (let r = 1; r <= repeat; r++) {
  const sim = new Simulation(board);
  const start = process.hrtime.bigint();
  sim.run({ ticks });
  const secs = Number(process.hrtime.bigint() - start) / 1e9;
  sim.destroy();

  const tps = ticks / Math.max(secs, 1e-12);
  best = Math.max(best, tps);
  sum += tps;
  console.error(`  run ${r}: ${(secs * 1e3).toFixed(3)} ms -> ${Math.round(tps)} ticks/s`);
}

console.log(
  `${name}: best ${Math.round(best)} ticks/s, mean ${Math.round(sum / repeat)} ticks/s (${ticks} ticks x ${repeat} repeats)`,
);