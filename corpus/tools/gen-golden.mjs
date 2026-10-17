// Regenerate every per-tick golden trace from the Rust engine CLI (`sim trace`).
//
// Each board fixture in ../boards/*.json is traced by running `sim trace <fixture>`.
// Build the CLI first (`just build-cli`) — `just gen-golden` does this automatically.
// Output goes to ../golden/<name>.json, one frame per line, so a behavior change diffs as the
// ticks it touched.
//
// Regenerate with:  just gen-golden

import { mkdirSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';
import { spawnSync } from 'node:child_process';

const here = dirname(fileURLToPath(import.meta.url));
const boardsDir = join(here, '..', 'boards');
const goldenDir = join(here, '..', 'golden');
const simBin = join(here, '..', '..', 'target', 'release', 'sim');
mkdirSync(goldenDir, { recursive: true });

/** The trace with its header fields one per line and one frame per line. */
function render(golden) {
  const { trace, ...header } = golden;
  const fields = Object.entries(header).map(([k, v]) => `  ${JSON.stringify(k)}: ${JSON.stringify(v)},`);
  const frames = trace.map((f) => '    ' + JSON.stringify(f)).join(',\n');
  return `{\n${fields.join('\n')}\n  "trace": [\n${frames}\n  ]\n}\n`;
}

let count = 0;
for (const file of readdirSync(boardsDir).filter((f) => f.endsWith('.json')).sort()) {
  const fixturePath = join(boardsDir, file);
  const res = spawnSync(simBin, ['trace', fixturePath], {
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
  writeFileSync(out, render(golden));
  console.log(`wrote golden/${golden.name}.json (${golden.trace.length} frames)`);
  count++;
}
console.log(`done: ${count} golden trace(s)`);