// Regenerate every per-tick golden trace from the *published* C++ engine
// (@logigator/logigator-simulation), the reference oracle of plan §10.1.
//
// Each board fixture in ../boards/*.json is processed in its own child process (gen-one.mjs),
// because the C++ engine is a global singleton that cannot be safely torn down to load a second
// board. The single-threaded capture (1 thread, 1 tick at a time) is the only reproducible mode
// (§10.1). Output goes to ../golden/<name>.json.
//
// Regenerate with:  cd corpus/tools && npm install && npm run gen

import { readFileSync, writeFileSync, readdirSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const here = dirname(fileURLToPath(import.meta.url));
const boardsDir = join(here, '..', 'boards');
const goldenDir = join(here, '..', 'golden');
mkdirSync(goldenDir, { recursive: true });

let count = 0;
for (const file of readdirSync(boardsDir).filter((f) => f.endsWith('.json')).sort()) {
  const fixturePath = join(boardsDir, file);
  const res = spawnSync(process.execPath, [join(here, 'gen-one.mjs'), fixturePath], {
    cwd: here,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });
  if (res.status !== 0) {
    console.error(`FAILED on ${file} (exit ${res.status})`);
    if (res.stderr) console.error(res.stderr);
    process.exit(1);
  }
  const golden = JSON.parse(res.stdout);
  const out = join(goldenDir, `${golden.name}.json`);
  writeFileSync(out, JSON.stringify(golden, null, 2) + '\n');
  console.log(`wrote golden/${golden.name}.json (${golden.trace.length} frames)`);
  count++;
}
console.log(`done: ${count} golden trace(s)`);
