#!/usr/bin/env node
// Bumps the release version everywhere it lives:
//   - `[workspace.package] version` in Cargo.toml (every crate inherits it)
//   - crates/sim-node/package.json `version` + the pinned optionalDependencies
//   - Cargo.lock / package-lock.json, via their own tools
//
// Usage: node scripts/bump-version.mjs <semver>   (or: just bump <semver>)
//
// The release workflow refuses to publish unless the pushed tag, package.json, and the
// cargo workspace version all agree, so run this (and commit) before tagging.

import { execSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const version = process.argv[2];
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version ?? '')) {
  console.error('usage: node scripts/bump-version.mjs <semver>');
  process.exit(2);
}

// Cargo: the workspace-level version is the only declaration (crates inherit it), so the
// first `version = "..."` line in the root manifest is the one.
const cargoPath = join(root, 'Cargo.toml');
const cargo = readFileSync(cargoPath, 'utf8');
const updated = cargo.replace(/^version = ".*"$/m, `version = "${version}"`);
if (updated === cargo && !cargo.includes(`version = "${version}"`)) {
  throw new Error('no version line found in [workspace.package]');
}
writeFileSync(cargoPath, updated);

// npm: the package version and the platform packages pinned alongside it.
const pkgPath = join(root, 'crates', 'sim-node', 'package.json');
const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));
pkg.version = version;
pkg.optionalDependencies = Object.fromEntries(
  Object.keys(pkg.optionalDependencies).map((name) => [name, version]),
);
writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + '\n');

// Lockfiles follow their manifests.
execSync('cargo update --workspace --offline', { cwd: root, stdio: 'inherit' });
execSync('npm install --package-lock-only', {
  cwd: join(root, 'crates', 'sim-node'),
  stdio: 'inherit',
});

console.log(`version ${version} set across Cargo.toml, package.json, and the lockfiles`);