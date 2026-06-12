#!/usr/bin/env node
// Launcher for the `sim` CLI. The actual binary ships inside the per-platform packages
// (`@logigator/sim-<platform>`); npm's os/cpu/libc constraints install exactly one of them,
// and this shim resolves it and hands over with the caller's argv and stdio.

import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

function platformDir() {
  const { platform, arch } = process;
  if (platform === 'linux') {
    // glibcVersionRuntime is absent from the process report on musl-based systems.
    const musl = !process.report?.getReport()?.header?.glibcVersionRuntime;
    return `linux-${arch}-${musl ? 'musl' : 'gnu'}`;
  }
  if (platform === 'darwin') return `darwin-${arch}`;
  if (platform === 'win32') return `win32-${arch}-msvc`;
  return `${platform}-${arch}`;
}

const exe = process.platform === 'win32' ? 'sim.exe' : 'sim';
let bin;
try {
  bin = require.resolve(`@logigator/sim-${platformDir()}/${exe}`);
} catch {
  console.error(
    `@logigator/sim: no prebuilt sim CLI for ${platformDir()} — ` +
      `is @logigator/sim-${platformDir()} installed?`,
  );
  process.exit(1);
}

const { status, error } = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
if (error) throw error;
process.exit(status ?? 1);