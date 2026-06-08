// Generate the golden trace for ONE board fixture and print it as JSON to stdout.
//
// Runs in its own process (see gen-golden.mjs): the C++ engine is a global singleton whose Board
// destructor aborts on the still-joinable worker thread, so we never destroy() — each fixture gets
// a fresh process that exits without tearing the board down, exactly like the old test harness.
//
// Usage: node gen-one.mjs <fixture.json>   (writes JSON trace to stdout, progress to stderr)

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { logicsim } = require('@logigator/logigator-simulation');

const STOPPED = 1; // Board::State::Stopped

function stepOne() {
  logicsim.start(1, 1, Number.MAX_SAFE_INTEGER); // 1 thread, 1 tick: deterministic single-threaded
  while (logicsim.getStatus().currentState !== STOPPED) {
    /* spin until the single background tick settles */
  }
}

function frame(tick) {
  const b = logicsim.getBoard();
  return {
    tick,
    links: b.links.map((x) => (x ? '1' : '0')).join(''),
    outputs: b.components.map((pins) => pins.map((p) => !!p)),
  };
}

const fixture = JSON.parse(readFileSync(process.argv[2], 'utf8'));
logicsim.init(fixture.board);
for (const t of fixture.triggers ?? []) {
  logicsim.triggerInput(t.comp, t.event, t.state);
}

const trace = [frame(0)];
for (let tick = 1; tick <= fixture.ticks; tick++) {
  stepOne();
  trace.push(frame(tick));
}

process.stdout.write(JSON.stringify({ name: fixture.name, ticks: fixture.ticks, trace }, null, 2) + '\n');
