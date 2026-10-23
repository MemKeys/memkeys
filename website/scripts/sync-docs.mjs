#!/usr/bin/env node
// sync-docs.mjs — ingest the repo-root docs/ Markdown as the website's docs
// source of truth so docs and code stay versioned together.
//
// Publication boundary: only the PUBLIC_DOCS allowlist below is synced into
// the site. The repo docs/ tree also contains internal engineering notes
// (process, CI, observability planning, design notes, ADRs) that must never
// be published — the allowlist is the boundary, and a build-time guard
// (check-refs.mjs) fails the build if any internal ticket reference slips
// into the emitted pages.
//
// Hand-authored site pages (landing, why, quickstart) live in the site
// itself and are protected by the KEEP set.

import { promises as fs } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, '../..');
const SRC_DOCS = path.join(REPO_ROOT, 'docs');
const OUT_DOCS = path.resolve(__dirname, '../src/content/docs');
const EDIT_BASE = 'https://github.com/MemKeys/memkeys/edit/main/docs';

// The public documentation set — the ONLY repo docs published to the site.
// Everything else in docs/ stays internal to the repository.
const PUBLIC_DOCS = new Set([
  'what-is-memkeys.md',
  'architecture.md',
  'connect-your-agents.md',
  'agent-mcp-transport.md',
  'k8s-operator.md',
  'retrieval-contract.md',
  'configuration.md',
]);

// Hand-authored pages that live in the site itself (not synced from docs/).
const KEEP = new Set(['index.mdx', 'why.mdx', 'quickstart.mdx']);

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
  const base = path.basename(relPath, '.md');
  return base.replace(/[-_]/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}

function stripFirstH1(body, title) {
  const lines = body.split('\n');
  const idx = lines.findIndex((l) => /^#\s+/.test(l));
  if (idx !== -1 && lines[idx].replace(/^#\s+/, '').trim() === title) {
    lines.splice(idx, 1);
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

  const files = await fs.readdir(SRC_DOCS);
  const publicFiles = files.filter(
    (f) => f.endsWith('.md') && PUBLIC_DOCS.has(f),
  );
  const skipped = files.filter((f) => f.endsWith('.md')).length - publicFiles.length;

  for (const name of publicFiles) {
    const file = path.join(SRC_DOCS, name);
    const raw = await fs.readFile(file, 'utf8');
    const dest = path.join(OUT_DOCS, name);
    await fs.mkdir(path.dirname(dest), { recursive: true });

    let out;
    if (raw.startsWith('---\n') || raw.startsWith('---\r\n')) {
      out = raw;
    } else {
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

    await fs.writeFile(dest, out, 'utf8');
  }

  console.log(
    `sync-docs: published ${publicFiles.length} public doc(s) -> src/content/docs/ ` +
      `(${skipped} internal doc(s) excluded by the allowlist)`,
  );
}

main().catch((err) => {
  console.error('sync-docs failed:', err);
  process.exit(1);
});
