#!/usr/bin/env node
// sync-docs.mjs — ingest the repo-root docs/ Markdown as the website's docs
// source of truth so docs and code stay versioned together.
//
// Why a sync step (and not a direct glob loader over ../docs)?
//   The engineering docs under docs/ are authored as plain Markdown with no
//   Starlight frontmatter (no `title:`), so a raw docsLoader() glob would fail
//   schema validation. Rather than edit every source doc, we copy each file
//   into src/content/docs/ and inject the minimal frontmatter Starlight needs,
//   deriving the title from the first H1. The source of truth stays docs/.
//
// Output under src/content/docs/ is git-ignored and regenerated on every
// `npm run dev` / `npm run build` (see predev/prebuild hooks in package.json).

import { promises as fs } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '../..');
const SRC_DOCS = path.join(REPO_ROOT, 'docs');
const OUT_DOCS = path.resolve(__dirname, '../src/content/docs');
const EDIT_BASE = 'https://github.com/MemKeys/memkeys/edit/main/docs';

// Hand-authored pages that live in the site itself (not synced from docs/).
const KEEP = new Set(['index.mdx']);

async function walk(dir) {
  const out = [];
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(full)));
    else if (entry.isFile() && entry.name.endsWith('.md')) out.push(full);
  }
  return out;
}

// Remove previously-synced files (everything except the hand-authored KEEP set)
// so deletions in docs/ propagate and stale pages never linger.
async function clean(dir) {
  let entries;
  try {
    entries = await fs.readdir(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      await clean(full);
      if ((await fs.readdir(full)).length === 0) await fs.rmdir(full);
    } else if (!KEEP.has(entry.name)) {
      await fs.rm(full);
    }
  }
}

function deriveTitle(body, relPath) {
  const match = body.match(/^#\s+(.+?)\s*$/m);
  if (match) return match[1].trim();
  // Fallback: humanize the file name.
  const base = path.basename(relPath, '.md');
  return base.replace(/[-_]/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

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

function yamlQuote(s) {
  return `"${s.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

async function main() {
  await fs.mkdir(OUT_DOCS, { recursive: true });
  await clean(OUT_DOCS);

  const files = await walk(SRC_DOCS);
  let count = 0;
  for (const file of files) {
    const rel = path.relative(SRC_DOCS, file); // e.g. adr/ADR-0001-....md
    const raw = await fs.readFile(file, 'utf8');
    const title = deriveTitle(raw, rel);
    const body = stripFirstH1(raw, title);
    const frontmatter = [
      '---',
      `title: ${yamlQuote(title)}`,
      `editUrl: ${EDIT_BASE}/${rel}`,
      '---',
      '',
    ].join('\n');

    const dest = path.join(OUT_DOCS, rel);
    await fs.mkdir(path.dirname(dest), { recursive: true });
    await fs.writeFile(dest, frontmatter + body, 'utf8');
    count += 1;
  }

  console.log(`sync-docs: ingested ${count} Markdown file(s) from docs/ -> src/content/docs/`);
}

main().catch((err) => {
  console.error('sync-docs failed:', err);
  process.exit(1);
});
