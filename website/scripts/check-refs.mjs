#!/usr/bin/env node
// check-refs.mjs — build-time publication guard.
//
// Scans every emitted HTML page in dist/ for internal references that must
// never be published: internal ticket ids (ISI-<digits>) and internal
// tooling names ("paperclip", case-insensitive). Any hit fails the build
// with the offending file and the matching snippet, so an internal doc can
// never reach the public site unnoticed.
//
// Runs automatically as the `postbuild` step of `npm run build`.

import { promises as fs } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const DIST = path.resolve(__dirname, '../dist');

const PATTERNS = [
  { re: /isi-\d+/i, label: 'internal ticket id' },
  { re: /paperclip/i, label: 'internal tooling name' },
];

async function walk(dir) {
  const out = [];
  let entries;
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(full)));
    else if (entry.isFile() && entry.name.endsWith('.html')) out.push(full);
  }
  return out;
}

async function main() {
  const files = await walk(DIST);
  if (files.length === 0) {
    console.error('check-refs: no HTML found in dist/ — did the build run?');
    process.exit(1);
  }

  const violations = [];
  for (const file of files) {
    const html = await fs.readFile(file, 'utf8');
    for (const { re, label } of PATTERNS) {
      const m = html.match(re);
      if (m) {
        const rel = path.relative(DIST, file);
        const at = Math.max(0, html.indexOf(m[0]) - 40);
        violations.push(
          `  ${rel}: ${label} "${m[0]}" … ${html.slice(at, at + 100).replace(/\s+/g, ' ')}`,
        );
      }
    }
  }

  if (violations.length > 0) {
    console.error(
      `check-refs: FAILED — ${violations.length} internal reference(s) in emitted pages:\n` +
        violations.join('\n'),
    );
    process.exit(1);
  }

  console.log(
    `check-refs: clean — ${files.length} page(s) scanned, zero internal references.`,
  );
}

main().catch((err) => {
  console.error('check-refs failed:', err);
  process.exit(1);
});
