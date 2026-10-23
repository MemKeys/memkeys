#!/usr/bin/env node
// sync-docs.mjs — publish the curated public docs from the repo-root docs/
// tree into the website's content collection, and guard the publication
// boundary at build time.
//
// Publication boundary (overhaul plan §5):
//   - PUBLIC_DOCS is the allowlist: the ONLY repo docs published to the site.
//     Everything else in docs/ (internal process, CI, observability, design,
//     ADRs, work products, the copy deck, ...) is never synced. No repo file
//     is moved or deleted — repo hygiene stays with the P1 cleanup ticket.
//   - KEEP protects the hand-authored site pages (landing, why, quickstart)
//     so the sync never overwrites or deletes them.
//   - Synced pages are nested under docs/ in the content collection so they
//     publish at /docs/<slug>/ per the approved IA. Absolute links to the
//     pre-IA flat routes are rewritten to /docs/ in the generated copy.
//   - The build-time guard (assertClean below) scans the whole content set
//     that will be published and FAILS the build if any page carries an
//     internal ticket ref (ISI-<digits>) or the internal tooling name
//     ("paperclip", case-insensitive). scripts/check-refs.mjs adds a second
//     net over the emitted dist/ HTML as the postbuild step.
//
// Output under src/content/docs/ is git-ignored and regenerated on every
// `npm run dev` / `npm run build` (predev/prebuild hooks in package.json).
//
// Usage:
//   node scripts/sync-docs.mjs           sync the allowlist + run the guard
//   node scripts/sync-docs.mjs --check   run the guard only (scan the
//                                        current content set, no sync)

import { promises as fs } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '../..');
const SRC_DOCS = path.join(REPO_ROOT, 'docs');
const OUT_DOCS = path.resolve(__dirname, '../src/content/docs');
// Synced pages are nested under docs/ so they publish at /docs/<slug>/.
const SECTION = 'docs';
const EDIT_BASE = 'https://github.com/MemKeys/memkeys/edit/main/docs';

// The public documentation set — the ONLY repo docs published to the site.
// Slugs match the curated sidebar in astro.config.mjs (plan §3.4).
const PUBLIC_DOCS = [
  'what-is-memkeys.md',
  'architecture.md',
  'connect-your-agents.md',
  'agent-mcp-transport.md',
  'k8s-operator.md',
  'retrieval-contract.md',
  'configuration.md',
];

// Hand-authored site pages that live in the site itself (not synced from
// docs/). The sync never writes or deletes these.
const KEEP = new Set(['index.mdx', 'why.mdx', 'quickstart.mdx']);

// Internal references that must never reach the published site.
const FORBIDDEN = [
  { re: /isi-\d+/i, label: 'internal ticket id' },
  { re: /paperclip/i, label: 'internal tooling name' },
];

// Absolute links to the pre-IA flat doc routes are rewritten to the /docs/
// IA in the generated copy (anchors preserved). The /memkeys/ prefix is the
// site base, matching how the source docs author their absolute links.
const SLUGS = PUBLIC_DOCS.map((name) => path.basename(name, '.md'));
const FLAT_LINK_RE = new RegExp(
  `(\\]\\(/memkeys/)(${SLUGS.join('|')})(/|#|\\))`,
  'g',
);

/**
 * Recursively collect every Markdown page under `dir`.
 * @param {string} dir
 * @returns {Promise<string[]>}
 */
async function walk(dir) {
  /** @type {string[]} */
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
    else if (entry.isFile()) out.push(full);
  }
  return out;
}

/**
 * @param {string} body
 * @param {string} relPath
 * @returns {string}
 */
function deriveTitle(body, relPath) {
  const match = body.match(/^#\s+(.+?)\s*$/m);
  if (match) return match[1].trim();
  // Fallback: humanize the file name.
  const base = path.basename(relPath, '.md');
  return base.replace(/[-_]/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

/**
 * @param {string} body
 * @param {string} title
 * @returns {string}
 */
function stripFirstH1(body, title) {
  // Drop the first H1 — Starlight renders the frontmatter title as the page H1,
  // so keeping the Markdown H1 would duplicate it.
  const lines = body.split('\n');
  const idx = lines.findIndex((l) => /^#\s+/.test(l));
  if (idx !== -1 && lines[idx].replace(/^#\s+/, '').trim() === title) {
    lines.splice(idx, 1);
    // Also drop an immediately-following blank line for tidiness.
    if (lines[idx] === '') lines.splice(idx, 1);
  }
  return lines.join('\n');
}

/**
 * @param {string} s
 * @returns {string}
 */
function yamlQuote(s) {
  return `"${s.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

// Rewrite absolute links to the pre-IA flat doc routes so the published
// pages point at the curated /docs/ locations.
function rewriteFlatLinks(out) {
  return out.replace(
    FLAT_LINK_RE,
    (_match, prefix, slug, tail) => `${prefix}${SECTION}/${slug}${tail}`,
  );
}

async function transformDoc(name) {
  const raw = await fs.readFile(path.join(SRC_DOCS, name), 'utf8');
  let out;
  if (raw.startsWith('---\n') || raw.startsWith('---\r\n')) {
    // Already authored with Starlight frontmatter — pass it through verbatim
    // so we never double-inject a frontmatter block.
    out = raw;
  } else {
    // Plain engineering Markdown — inject the minimal frontmatter Starlight
    // needs, deriving the title from the first H1. The edit link points at
    // the real source under repo-root docs/, not the generated copy.
    const title = deriveTitle(raw, name);
    const body = stripFirstH1(raw, title);
    out = [
      '---',
      `title: ${yamlQuote(title)}`,
      `editUrl: ${EDIT_BASE}/${name}`,
      '---',
      '',
    ].join('\n') + body;
  }
  return rewriteFlatLinks(out);
}

// Build-time guard: scan the whole content set that will be published
// (synced docs + hand-authored KEEP pages) and fail the build on any
// internal reference.
async function assertClean() {
  const files = (await walk(OUT_DOCS)).filter((f) =>
    /\.(md|mdx)$/.test(f),
  );
  if (files.length === 0) {
    console.error(
      'sync-docs guard: no content found in src/content/docs/ — did the sync run?',
    );
    process.exit(1);
  }
  const violations = [];
  for (const file of files) {
    const rel = path.relative(OUT_DOCS, file);
    const body = await fs.readFile(file, 'utf8');
    for (const { re, label } of FORBIDDEN) {
      const m = body.match(re);
      if (m) {
        const at = Math.max(0, body.indexOf(m[0]) - 40);
        violations.push(
          `  ${rel}: ${label} "${m[0]}" … ${body.slice(at, at + 100).replace(/\s+/g, ' ')}`,
        );
      }
    }
  }
  if (violations.length > 0) {
    console.error(
      `sync-docs guard: FAILED — ${violations.length} internal reference(s) in the content set that will be published:\n` +
        violations.join('\n') +
        '\nRemove the internal reference from the source doc (docs/) or the hand-authored page; internal refs must never be emitted.',
    );
    process.exit(1);
  }
  console.log(
    `sync-docs guard: clean — ${files.length} page(s) scanned, zero internal references.`,
  );
}

// Remove directories left empty by the stale-file sweep (never OUT_DOCS).
async function pruneEmptyDirs(dir) {
  let entries;
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const full = path.join(dir, entry.name);
    await pruneEmptyDirs(full);
    if ((await fs.readdir(full)).length === 0) await fs.rmdir(full);
  }
}

async function main() {
  if (process.argv.includes('--check')) {
    await assertClean();
    return;
  }

  await fs.mkdir(path.join(OUT_DOCS, SECTION), { recursive: true });

  // Transform the allowlisted docs. A missing allowlisted doc is a hard
  // error: the curated sidebar references it, so the published set would be
  // incomplete.
  const targets = new Map(); // posix rel path from OUT_DOCS -> content
  for (const name of PUBLIC_DOCS) {
    let out;
    try {
      out = await transformDoc(name);
    } catch {
      console.error(
        `sync-docs: allowlisted doc not found: docs/${name} — the curated sidebar references it; the public set is incomplete.`,
      );
      process.exit(1);
    }
    targets.set(`${SECTION}/${name}`, out);
  }

  // Sweep stale generated output: everything that is neither a KEEP page nor
  // a current sync target (covers deleted docs and the pre-IA flat layout).
  const keep = new Set([...KEEP, ...targets.keys()]);
  for (const file of await walk(OUT_DOCS)) {
    const rel = path.relative(OUT_DOCS, file).split(path.sep).join('/');
    if (!keep.has(rel)) await fs.rm(file);
  }
  await pruneEmptyDirs(OUT_DOCS);

  // Write targets, skipping unchanged files so dev rebuilds stay quiet.
  let written = 0;
  for (const [rel, out] of targets) {
    const dest = path.join(OUT_DOCS, rel);
    await fs.mkdir(path.dirname(dest), { recursive: true });
    let prev;
    try {
      prev = await fs.readFile(dest, 'utf8');
    } catch {
      // New file.
    }
    if (prev !== out) {
      await fs.writeFile(dest, out, 'utf8');
      written += 1;
    }
  }

  // Internal docs excluded by the allowlist (for the log line).
  const excluded = (await walk(SRC_DOCS)).filter(
    (f) => f.endsWith('.md') && !PUBLIC_DOCS.includes(path.basename(f)),
  ).length;

  console.log(
    `sync-docs: published ${targets.size} public doc(s) -> src/content/docs/${SECTION}/ ` +
      `(${written} written, ${excluded} internal doc(s) excluded by the allowlist)`,
  );

  await assertClean();
}

main().catch((err) => {
  console.error('sync-docs failed:', err);
  process.exit(1);
});
