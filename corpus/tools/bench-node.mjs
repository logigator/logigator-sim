// Throughput bench for the Node binding: time `runAsync` over a bench board.
//
// Mirrors the CLI's `sim bench` protocol — each repeat builds a fresh Simulation (power-on init),
// times one bounded `runAsync`, and we report best/mean ticks per second. Time-bound by default
// (count the ticks that land in `--ms`); `--ticks N` forces a fixed step count. Requires a *release*
// addon build (`just build-node-release`); a debug addon measures the wrong thing.
//
// Usage: node bench-node.mjs <board.json> [--ms N | --ticks N] [--repeat R] [--threads T]

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
  console.error('usage: node bench-node.mjs <board.json> [--ms N | --ticks N] [--repeat R] [--threads T]');
  process.exit(2);
}
const ticksArg = arg('ticks', null); // null → time-bound by --ms
const ms = arg('ms', 1000);
const repeat = arg('repeat', 5);
const threads = arg('threads', 1);
const bound = ticksArg != null ? { ticks: ticksArg } : { ms };
const label = ticksArg != null ? `${ticksArg} ticks` : `${ms} ms window`;

const fixture = JSON.parse(readFileSync(boardPath, 'utf8'));
const board = fixture.board ?? fixture; // fixture wrapper or bare descriptor
const name = fixture.name ?? boardPath;

console.error(`benching ${name} via runAsync — ${label} × ${repeat} repeats (threads ${threads})`);

let best = 0;
let sum = 0;
for (let r = 1; r <= repeat; r++) {
  const sim = new Simulation(board);
  const start = process.hrtime.bigint();
  await sim.runAsync({ ...bound, threads });
  const secs = Number(process.hrtime.bigint() - start) / 1e9;
  const ticks = sim.getStatus().tick; // fresh sim starts at 0, so this is the ticks this repeat ran
  sim.destroy();

  const tps = ticks / Math.max(secs, 1e-12);
  best = Math.max(best, tps);
  sum += tps;
  console.error(`  run ${r}: ${(secs * 1e3).toFixed(3)} ms, ${ticks} ticks -> ${Math.round(tps)} ticks/s`);
}

console.log(
  `${name}: best ${Math.round(best)} ticks/s, mean ${Math.round(sum / repeat)} ticks/s (${repeat} repeats, threads ${threads})`,
);
