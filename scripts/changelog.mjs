#!/usr/bin/env node
// Reads and stamps CHANGELOG.md (Keep a Changelog format):
//   - `## [Unreleased]` collects the notes for the next release
//   - `## [x.y.z] - YYYY-MM-DD` holds each released version
//   - `[x.y.z]: <compare url>` link references at the bottom
//
// Usage: node scripts/changelog.mjs notes <semver>
//   Prints the body of that version's section (the GitHub release notes) and fails if the
//   section is missing or empty, so a tag can't be released without notes.
//
// `bump-version.mjs` calls `stampRelease` to turn [Unreleased] into the new version's section.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const changelogPath = join(dirname(fileURLToPath(import.meta.url)), '..', 'CHANGELOG.md');
const REPO_URL = 'https://github.com/logigator/logigator-sim';

const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

// The body of `## [name]` up to the next `## ` heading or the link references, or null when
// there is no such section.
function section(text, name) {
  const heading = new RegExp(`^## \\[${escape(name)}\\][^\\n]*\\n`, 'm').exec(text);
  if (!heading) return null;
  const start = heading.index + heading[0].length;
  const end = /^(## |\[[^\]]+\]: )/m.exec(text.slice(start));
  return { start, end: end ? start + end.index : text.length };
}

export function releaseNotes(text, version) {
  const s = section(text, version);
  if (!s) throw new Error(`CHANGELOG.md has no [${version}] section`);
  const body = text.slice(s.start, s.end).trim();
  if (!body) throw new Error(`CHANGELOG.md's [${version}] section is empty`);
  return body;
}

// Moves the [Unreleased] notes under a new `## [version] - date` heading and repoints the
// link references: [Unreleased] compares from the new tag, [version] from the previous one.
export function stampRelease(text, version, date) {
  if (section(text, version)) throw new Error(`CHANGELOG.md already has a [${version}] section`);
  const s = section(text, 'Unreleased');
  if (!s) throw new Error('CHANGELOG.md has no [Unreleased] section');
  if (!text.slice(s.start, s.end).trim()) {
    throw new Error('CHANGELOG.md has nothing under [Unreleased]; write the release notes first');
  }

  const link = /^\[Unreleased\]: .*\/compare\/(\S+)\.\.\.HEAD$/m.exec(text);
  if (!link) throw new Error('CHANGELOG.md has no [Unreleased]: .../compare/<tag>...HEAD link');
  const previousTag = link[1];

  const stamped =
    text.slice(0, s.start) + `\n## [${version}] - ${date}\n` + text.slice(s.start);
  return stamped.replace(
    link[0],
    `[Unreleased]: ${REPO_URL}/compare/v${version}...HEAD\n` +
      `[${version}]: ${REPO_URL}/compare/${previousTag}...v${version}`,
  );
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [command, version] = process.argv.slice(2);
  if (command !== 'notes' || !version) {
    console.error('usage: node scripts/changelog.mjs notes <semver>');
    process.exit(2);
  }
  try {
    console.log(releaseNotes(readFileSync(changelogPath, 'utf8'), version));
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
