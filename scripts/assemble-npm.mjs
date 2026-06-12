#!/usr/bin/env node
// Assembles the publishable @logigator/sim package tree out of the CI build artifacts:
//   1. copies the wasm-pack web build into crates/sim-node/wasm/ (the `./wasm` export),
//   2. drops each platform's `sim` CLI binary into its napi platform package under
//      crates/sim-node/npm/ and lists it in that package's `files`,
//   3. pins every optionalDependency to the umbrella package's own version.
//
// Run after `napi create-npm-dirs` and `napi artifacts` have populated npm/ with the
// `.node` bindings.
//
// Usage: node scripts/assemble-npm.mjs <cli-artifacts-dir>
//   <cli-artifacts-dir> holds one `cli-<rust-triple>` directory per target, each
//   containing the built `sim` / `sim.exe`.

import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const pkgDir = join(root, 'crates', 'sim-node');
const wasmSrc = join(root, 'crates', 'sim-wasm', 'pkg');

const cliDir = process.argv[2] && resolve(process.argv[2]);
if (!cliDir) {
  console.error('usage: node scripts/assemble-npm.mjs <cli-artifacts-dir>');
  process.exit(2);
}

// 1. wasm web build → wasm/. Only the runtime files; wasm-pack's own package.json,
//    .gitignore and README would otherwise leak into (or get excluded from) the tarball.
const wasmDst = join(pkgDir, 'wasm');
mkdirSync(wasmDst, { recursive: true });
for (const f of readdirSync(wasmSrc)) {
  if (f === 'package.json' || f === '.gitignore' || f === 'README.md') continue;
  cpSync(join(wasmSrc, f), join(wasmDst, f), { recursive: true });
}
console.log(`wasm: ${wasmSrc} -> ${wasmDst}`);

// The registry page renders the package README; npm auto-includes README.md and LICENSE
// from the package root regardless of the `files` allowlist.
cpSync(join(root, 'README.md'), join(pkgDir, 'README.md'));
cpSync(join(root, 'LICENSE'), join(pkgDir, 'LICENSE'));
console.log('README.md + LICENSE copied into the package');

// 2. CLI binaries → platform packages. Platform dir (npm/<name>) ↔ rust target triple,
//    matching the napi target list in package.json.
const TRIPLES = {
  'linux-x64-gnu': 'x86_64-unknown-linux-gnu',
  'linux-x64-musl': 'x86_64-unknown-linux-musl',
  'linux-arm64-gnu': 'aarch64-unknown-linux-gnu',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64-msvc': 'x86_64-pc-windows-msvc',
};

const npmDir = join(pkgDir, 'npm');
for (const dir of readdirSync(npmDir)) {
  const triple = TRIPLES[dir];
  if (!triple) throw new Error(`no rust triple mapped for npm/${dir}`);
  // `napi pre-publish` silently skips what's missing, so an absent binding has to fail
  // here — a platform package without its .node would install broken.
  const binding = join(npmDir, dir, `sim-node.${dir}.node`);
  if (!existsSync(binding)) throw new Error(`missing Node binding: ${binding}`);
  const exe = dir.startsWith('win32') ? 'sim.exe' : 'sim';
  const src = join(cliDir, `cli-${triple}`, exe);
  if (!existsSync(src)) throw new Error(`missing CLI binary: ${src}`);
  const dst = join(npmDir, dir, exe);
  cpSync(src, dst);
  chmodSync(dst, 0o755);

  const pkgJsonPath = join(npmDir, dir, 'package.json');
  const pkg = JSON.parse(readFileSync(pkgJsonPath, 'utf8'));
  pkg.files = [...new Set([...(pkg.files ?? []), exe])];
  writeFileSync(pkgJsonPath, JSON.stringify(pkg, null, 2) + '\n');
  console.log(`cli: ${src} -> ${dst}`);
}

// 3. optionalDependencies pinned to this release's version, so the umbrella always pulls
//    the platform packages published alongside it.
const rootPkgPath = join(pkgDir, 'package.json');
const rootPkg = JSON.parse(readFileSync(rootPkgPath, 'utf8'));
rootPkg.optionalDependencies = Object.fromEntries(
  Object.keys(rootPkg.optionalDependencies).map((name) => [name, rootPkg.version]),
);
writeFileSync(rootPkgPath, JSON.stringify(rootPkg, null, 2) + '\n');
console.log(`optionalDependencies pinned to ${rootPkg.version}`);