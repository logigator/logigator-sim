// Throughput bench for the OLD C++ engine (`@logigator/logigator-simulation`, the golden oracle
// gen-one.mjs uses) — the reference point the Rust rewrite is measured against.
//
// Differences from bench-node.mjs forced by the old engine's shape:
//  - it is a global singleton whose Board destructor aborts on the still-joinable worker thread
//    (see gen-one.mjs), so we `init` ONCE and never `destroy()` — repeats continue from the
//    current board state instead of a fresh power-on init. For the steady-state oscillator/clock
//    bench boards that measures the same thing.
//  - `start(threads, ticks, ms, synchronized: true)` blocks until the bound is reached, so timing
//    needs no polling loop.
//
// Usage: node bench-cpp-node.mjs <board.json> [--ticks N] [--repeat R] [--threads T]

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { logicsim } = require('@logigator/logigator-simulation');

function arg(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? Number(process.argv[i + 1]) : dflt;
}

const boardPath = process.argv[2];
if (!boardPath) {
  console.error('usage: node bench-cpp-node.mjs <board.json> [--ticks N] [--repeat R] [--threads T]');
  process.exit(2);
}
const ticks = arg('ticks', 1_000_000);
const repeat = arg('repeat', 10);
const threads = arg('threads', 1);

const fixture = JSON.parse(readFileSync(boardPath, 'utf8'));
const board = fixture.board ?? fixture;
const name = fixture.name ?? boardPath;

console.error(`benching ${name} via C++ logicsim.start(sync) — ${ticks} ticks × ${repeat} repeats (threads ${threads})`);

logicsim.init(board);

let best = 0;
let sum = 0;
for (let r = 1; r <= repeat; r++) {
  const before = logicsim.getStatus().tick;
  const start = process.hrtime.bigint();
  logicsim.start(threads, ticks, Number.MAX_SAFE_INTEGER, true);
  const secs = Number(process.hrtime.bigint() - start) / 1e9;

  const ran = logicsim.getStatus().tick - before;
  if (ran !== ticks) console.error(`  warning: ran ${ran} ticks, expected ${ticks}`);
  const tps = ticks / Math.max(secs, 1e-12);
  best = Math.max(best, tps);
  sum += tps;
  console.error(`  run ${r}: ${(secs * 1e3).toFixed(3)} ms -> ${Math.round(tps)} ticks/s`);
}

console.log(
  `${name}: best ${Math.round(best)} ticks/s, mean ${Math.round(sum / repeat)} ticks/s (${ticks} ticks x ${repeat} repeats, threads ${threads})`,
);

// No logicsim.destroy(): the C++ Board destructor aborts the process (see gen-one.mjs).
process.exit(0);