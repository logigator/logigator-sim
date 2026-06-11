// Throughput bench for the Node binding: time `runAsync` over a bench board.
//
// Mirrors the CLI's `sim bench` protocol — each repeat builds a fresh Simulation (power-on init),
// times one bounded `runAsync`, and we report best/mean ticks per second. Requires a *release*
// addon build (`cd crates/sim-node && npx napi build --platform --release`); a debug addon
// measures the wrong thing.
//
// Usage: node bench-node.mjs <board.json> [--ticks N] [--repeat R] [--threads T]

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
  'sim-node',
));

function arg(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? Number(process.argv[i + 1]) : dflt;
}

const boardPath = process.argv[2];
if (!boardPath) {
  console.error('usage: node bench-node.mjs <board.json> [--ticks N] [--repeat R] [--threads T]');
  process.exit(2);
}
const ticks = arg('ticks', 1_000_000);
const repeat = arg('repeat', 10);
const threads = arg('threads', 1);

const fixture = JSON.parse(readFileSync(boardPath, 'utf8'));
const board = fixture.board ?? fixture; // fixture wrapper or bare descriptor
const name = fixture.name ?? boardPath;

console.error(`benching ${name} via runAsync — ${ticks} ticks × ${repeat} repeats (threads ${threads})`);

let best = 0;
let sum = 0;
for (let r = 1; r <= repeat; r++) {
  const sim = new Simulation(board);
  const start = process.hrtime.bigint();
  await sim.runAsync({ ticks, threads });
  const secs = Number(process.hrtime.bigint() - start) / 1e9;
  sim.destroy();

  const tps = ticks / Math.max(secs, 1e-12);
  best = Math.max(best, tps);
  sum += tps;
  console.error(`  run ${r}: ${(secs * 1e3).toFixed(3)} ms -> ${Math.round(tps)} ticks/s`);
}

console.log(
  `${name}: best ${Math.round(best)} ticks/s, mean ${Math.round(sum / repeat)} ticks/s (${ticks} ticks x ${repeat} repeats, threads ${threads})`,
);
