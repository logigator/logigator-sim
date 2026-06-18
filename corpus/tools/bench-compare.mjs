// Interleaved before/after benchmark for one surface — the protocol driver, so no ad-hoc scripts.
//
// Builds the `--before` ref in a throwaway git worktree and the current working tree as "after",
// then runs the surface's bench harness from each tree (each resolves its own built artifact)
// against the same board files, interleaved per board, and prints before/after/Δ. Time-bound by
// default (`--ms`, the protocol's 1 s window); best-of-`--repeat`.
//
// Only the affected surface needs running: a sim-core change → cli; a node-binding change → node;
// a wasm-binding change → wasm + wasm-async.
//
// Both refs must have the time-bound bench (this protocol or later); an older `--before` whose
// `sim bench` / harness lacks `--ms` is reported as an error rather than mismeasured.
//
// Usage:
//   node bench-compare.mjs <cli|node|wasm|wasm-async> --before <ref> \
//        [--ms 1000] [--repeat 5] [--boards a,b,c] [--cores 0,2,4,6,8,10,12,14] [--keep]

import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const ALL_BOARDS = [
  'cpu', 'correlated', 'fanout', 'small_idle', 'medium_idle',
  'large_idle', 'small_active', 'medium_active', 'large_active',
];

// surface → (just build recipes to run in each tree, harness invocation given a tree + board).
const SURFACES = {
  cli: {
    build: ['build-cli'],
    cmd: (tree, board, ms, repeat) =>
      [join(tree, 'target/release/sim'), ['bench', board, '--ms', ms, '--repeat', repeat]],
  },
  node: {
    build: ['setup-node', 'build-node-release'],
    cmd: (tree, board, ms, repeat) =>
      ['node', [join(tree, 'corpus/tools/bench-node.mjs'), board, '--ms', ms, '--repeat', repeat]],
  },
  wasm: {
    build: ['build-wasm'],
    cmd: (tree, board, ms, repeat) =>
      ['node', [join(tree, 'corpus/tools/bench-wasm.mjs'), board, '--ms', ms, '--repeat', repeat]],
  },
  'wasm-async': {
    build: ['build-wasm'],
    cmd: (tree, board, ms, repeat) =>
      ['node', [join(tree, 'corpus/tools/bench-wasm-async.mjs'), board, '--ms', ms, '--repeat', repeat]],
  },
};

function flag(name, dflt) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : dflt;
}
const has = (name) => process.argv.includes(`--${name}`);

const surface = process.argv[2];
const before = flag('before');
if (!SURFACES[surface] || !before) {
  console.error('usage: node bench-compare.mjs <cli|node|wasm|wasm-async> --before <ref> ' +
    '[--ms 1000] [--repeat 5] [--boards a,b,c] [--cores LIST] [--keep]');
  process.exit(2);
}
const ms = flag('ms', '1000');
const repeat = flag('repeat', '5');
const boards = flag('boards') ? flag('boards').split(',') : ALL_BOARDS;
const cores = flag('cores', '0,2,4,6,8,10,12,14');
const pin = process.platform === 'linux' && cores.length > 0;

const root = spawnSync('git', ['rev-parse', '--show-toplevel'], { encoding: 'utf8' }).stdout.trim();
if (!root) { console.error('not inside a git repo'); process.exit(1); }

function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', ...opts });
  if (r.status !== 0) {
    throw new Error(`\`${cmd} ${args.join(' ')}\` failed (exit ${r.status})\n${r.stderr ?? ''}`);
  }
  return r;
}

// Build one tree's artifact for this surface (inherit stdio so the build is visible).
function build(tree, ref) {
  console.error(`\n=== building ${ref} (${tree}) ===`);
  for (const recipe of SURFACES[surface].build) {
    run('just', [recipe], { cwd: tree, stdio: 'inherit' });
  }
}

// best ticks/s for one (tree, board): run the harness, parse the summary line.
function bench(tree, board) {
  const boardFile = join(root, 'corpus/bench', `${board}.json`);
  let [cmd, args] = SURFACES[surface].cmd(tree, boardFile, ms, repeat);
  if (pin) { args = ['-c', cores, cmd, ...args]; cmd = 'taskset'; }
  const r = spawnSync(cmd, args, { encoding: 'utf8' });
  const m = r.stdout.match(/best ([\d_]+) ticks\/s/);
  if (!m) {
    throw new Error(
      `no throughput from \`${cmd} ${args.join(' ')}\` — does the '${before}' build support ` +
      `the time-bound bench (--ms)?\nstdout: ${r.stdout}\nstderr: ${r.stderr}`);
  }
  return Number(m[1].replaceAll('_', ''));
}

const wt = mkdtempSync(join(tmpdir(), 'logigator-bench-'));
try {
  run('git', ['worktree', 'add', '--detach', wt, before]);
  build(wt, before);          // before
  build(root, 'working tree'); // after

  console.error(`\n=== ${surface}: ${before} → working tree, ${ms} ms × ${repeat}, interleaved ===\n`);
  const pad = (s, n) => String(s).padStart(n);
  console.log(`${'board'.padEnd(14)} ${pad('before', 14)} ${pad('after', 14)} ${pad('Δ%', 9)}`);
  for (const b of boards) {
    const before_tps = bench(wt, b);   // interleaved per board: before then after, back-to-back
    const after_tps = bench(root, b);
    const d = ((after_tps - before_tps) * 100 / before_tps).toFixed(1);
    console.log(`${b.padEnd(14)} ${pad(before_tps, 14)} ${pad(after_tps, 14)} ${pad(d + '%', 9)}`);
  }
} finally {
  if (has('keep')) {
    console.error(`\nworktree kept at ${wt}`);
  } else {
    run('git', ['worktree', 'remove', '--force', wt]);
  }
}
