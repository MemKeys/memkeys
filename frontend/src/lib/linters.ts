// Pure, side-effect-free mappings for the Linters management view (Golden
// Path F, ISI-7816). Body-JSON parsing, envelope building, slug validation and
// client-side filtering live here — next to the other wire transforms — so
// they can be unit-tested without React or a live server (see linters.test.ts).
//
// The write path is op-1 `POST /memory/facts`; the read path is a keyed
// `type=linter` fetch (project scope) plus the `applicable_linters` resolver
// block (global scope). See UX spec docs/design/inspector-linters-ux-spec.md
// and ADR-0004 for the native linter rule shape.

import {
  ApplicableLinter,
  EntityKeyName,
  FactRecord,
  LinterDraft,
  LinterMatch,
  LinterRecord,
  LinterRule,
  Severity,
} from '../types/api';

/** The `linter:` subject prefix (ADR-0004 §3: the subject IS the rule_id). */
export const LINTER_SUBJECT_PREFIX = 'linter:';

/** Slug grammar for a linter name (mock 6B inline validation). */
export const LINTER_SLUG_RE = /^[a-z0-9][a-z0-9-]*$/;

/** Server rule-body length bound mirrored client-side (mock 6A counter). */
export const RULE_MAX_LEN = 4000;

export const SEVERITIES: Severity[] = ['info', 'warn', 'error'];

/** `linter:<slug>` — the subject a name maps to. Idempotent if already prefixed. */
export function linterSubject(name: string): string {
  const n = name.trim();
  return n.startsWith(LINTER_SUBJECT_PREFIX) ? n : `${LINTER_SUBJECT_PREFIX}${n}`;
}

/** The slug behind a subject (`linter:foo` → `foo`); trims the prefix once. */
export function nameFromSubject(subject: string | null | undefined): string {
  if (!subject) return '';
  return subject.startsWith(LINTER_SUBJECT_PREFIX)
    ? subject.slice(LINTER_SUBJECT_PREFIX.length)
    : subject;
}

/** True when `sev` is one of the three grammar values. */
export function isSeverity(sev: unknown): sev is Severity {
  return sev === 'info' || sev === 'warn' || sev === 'error';
}

/**
 * Parse an opaque `fact.body` into the native rule shape. A non-JSON or
 * non-object body degrades to `{rule: <raw text>}` with unknown severity rather
 * than throwing (UX spec §2: "raw text + unknown severity"). `enabled` defaults
 * to true when absent (a stored linter with no flag is live).
 */
export function parseLinterBody(body: string): {
  rule: string;
  severity: Severity | null;
  enabled: boolean;
  match: LinterMatch | null;
  bodyJson: LinterRule | null;
} {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return { rule: body, severity: null, enabled: true, match: null, bodyJson: null };
  }
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
    // A bare JSON string/number is still "raw text".
    const raw = typeof parsed === 'string' ? parsed : body;
    return { rule: raw, severity: null, enabled: true, match: null, bodyJson: null };
  }
  const obj = parsed as Record<string, unknown>;
  const rule = typeof obj.rule === 'string' ? obj.rule : body;
  const severity = isSeverity(obj.severity) ? obj.severity : null;
  const enabled = typeof obj.enabled === 'boolean' ? obj.enabled : true;
  const match =
    typeof obj.match === 'object' && obj.match !== null && !Array.isArray(obj.match)
      ? (obj.match as LinterMatch)
      : null;
  const bodyJson: LinterRule = { rule };
  if (severity) bodyJson.severity = severity;
  bodyJson.enabled = enabled;
  if (match) bodyJson.match = match;
  return { rule, severity, enabled, match, bodyJson };
}

/** A compact one-line summary of a `match` block for the table's match cell. */
export function matchSummary(match: LinterMatch | null): string {
  if (!match) return '—';
  const parts: string[] = [];
  if (match.language) parts.push(`language=${match.language}`);
  if (match.path_glob) {
    const globs = Array.isArray(match.path_glob) ? match.path_glob.join(', ') : match.path_glob;
    if (globs.trim() !== '') parts.push(`path=${globs}`);
  }
  return parts.length > 0 ? parts.join(' · ') : '—';
}

const ENTITY_KEY_ORDER: EntityKeyName[] = [
  'project',
  'issue',
  'pr',
  'agent',
  'run',
  'thread',
  'service',
];

/** Normalize a keyed `type=linter` FactRecord into a {@link LinterRecord}. */
export function factRecordToLinter(r: FactRecord): LinterRecord {
  const parsed = parseLinterBody(r.body);
  const entityKeys: Array<[string, string]> = [];
  const map: Record<EntityKeyName, string | null> = {
    project: r.entity_project,
    issue: r.entity_issue,
    pr: r.entity_pr,
    agent: r.entity_agent,
    run: r.entity_run,
    thread: r.entity_thread,
    service: r.entity_service,
  };
  for (const name of ENTITY_KEY_ORDER) {
    const v = map[name];
    if (v) entityKeys.push([name, v]);
  }
  return {
    factId: r.id,
    name: nameFromSubject(r.subject),
    subject: r.subject ?? '',
    scopeKind: r.entity_project ? 'project' : 'global',
    project: r.entity_project,
    rule: parsed.rule,
    severity: parsed.severity,
    enabled: parsed.enabled,
    match: parsed.match,
    bodyJson: parsed.bodyJson,
    rawBody: r.body,
    eventTime: r.event_time,
    ingestedAt: r.ingested_at,
    supersedes: r.supersedes,
    provenance: r.provenance_principal,
    entityKeys,
  };
}

/**
 * Normalize an `applicable_linters` resolver entry into a {@link LinterRecord}.
 * The entry carries the body already parsed (`fact.body` is JSON when it parsed
 * server-side, else a raw string), and lacks the record-level timestamps /
 * provenance — those degrade to null.
 */
export function applicableToLinter(entry: ApplicableLinter): LinterRecord {
  const raw =
    typeof entry['fact.body'] === 'string'
      ? (entry['fact.body'] as string)
      : JSON.stringify(entry['fact.body']);
  const parsed = parseLinterBody(raw);
  const entityKeys: Array<[string, string]> = [];
  for (const name of ENTITY_KEY_ORDER) {
    const v = entry.entity[name];
    if (v) entityKeys.push([name, v]);
  }
  return {
    factId: entry['fact.id'],
    name: nameFromSubject(entry['fact.subject']),
    subject: entry['fact.subject'] ?? '',
    scopeKind: entry.scope === 'context' ? 'project' : 'global',
    project: entry.entity.project ?? null,
    rule: parsed.rule,
    severity: isSeverity(entry.severity) ? entry.severity : parsed.severity,
    enabled: parsed.enabled,
    match: parsed.match,
    bodyJson: parsed.bodyJson,
    rawBody: raw,
    eventTime: null,
    ingestedAt: null,
    supersedes: null,
    provenance: null,
    entityKeys,
  };
}

/**
 * Merge project-scoped FactRecords with global resolver entries into one
 * de-duplicated list (by fact id, FactRecords winning for their fuller detail),
 * ordered global-first then by name — mirroring the resolver's own ordering so
 * the table is stable regardless of which source a row came from.
 */
export function mergeLinters(
  projectRecords: LinterRecord[],
  globalRecords: LinterRecord[],
): LinterRecord[] {
  const byId = new Map<string, LinterRecord>();
  for (const g of globalRecords) byId.set(g.factId, g);
  // FactRecords (fuller detail) overwrite resolver entries with the same id.
  for (const p of projectRecords) byId.set(p.factId, p);
  return [...byId.values()].sort((a, b) => {
    if (a.scopeKind !== b.scopeKind) return a.scopeKind === 'global' ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

/** Scope filter value for the toolbar pills. */
export type ScopeFilter = 'all' | 'global' | 'project';

export interface LinterFilter {
  scope: ScopeFilter;
  /** '' = any severity. */
  severity: Severity | '';
  /** Case-insensitive substring match over name + rule text; '' = no filter. */
  text: string;
}

/** Apply the toolbar filters (scope pill, severity select, text box) purely. */
export function filterLinters(linters: LinterRecord[], f: LinterFilter): LinterRecord[] {
  const needle = f.text.trim().toLowerCase();
  return linters.filter((l) => {
    if (f.scope !== 'all' && l.scopeKind !== f.scope) return false;
    if (f.severity !== '' && l.severity !== f.severity) return false;
    if (needle !== '') {
      const hay = `${l.name} ${l.rule}`.toLowerCase();
      if (!hay.includes(needle)) return false;
    }
    return true;
  });
}

/** Counts shown in the toolbar pills + summary line. */
export function linterCounts(linters: LinterRecord[]): {
  total: number;
  enabled: number;
  global: number;
  project: number;
} {
  let enabled = 0;
  let global = 0;
  let project = 0;
  for (const l of linters) {
    if (l.enabled) enabled += 1;
    if (l.scopeKind === 'global') global += 1;
    else project += 1;
  }
  return { total: linters.length, enabled, global, project };
}

export interface DraftErrors {
  name?: string;
  rule?: string;
  project?: string;
}

/** Client-side validation (mock 6): run before any write so a bad draft never
 *  reaches the server. Returns the per-field errors; empty object = valid. */
export function validateDraft(draft: LinterDraft): DraftErrors {
  const errors: DraftErrors = {};
  const name = draft.name.trim();
  if (name === '') {
    errors.name = 'Name is required.';
  } else if (!LINTER_SLUG_RE.test(name)) {
    errors.name = 'Name must be a lowercase slug (letters, digits, dashes).';
  }
  const rule = draft.rule.trim();
  if (rule === '') {
    errors.rule = 'Rule text is required (1–4000 chars).';
  } else if (rule.length > RULE_MAX_LEN) {
    errors.rule = `Rule text is too long (${rule.length}/${RULE_MAX_LEN}).`;
  }
  if (draft.scopeKind === 'project' && draft.project.trim() === '') {
    errors.project = 'Project is required for project scope.';
  }
  return errors;
}

export function isDraftValid(draft: LinterDraft): boolean {
  return Object.keys(validateDraft(draft)).length === 0;
}

/** Build the `body.match` block from the draft, or omit it when empty. */
export function draftMatch(draft: LinterDraft): LinterMatch | undefined {
  const match: LinterMatch = {};
  const lang = draft.language.trim();
  if (lang !== '') match.language = lang;
  const globs = draft.pathGlobs
    .split(',')
    .map((g) => g.trim())
    .filter((g) => g !== '');
  if (globs.length === 1) match.path_glob = globs[0];
  else if (globs.length > 1) match.path_glob = globs;
  return Object.keys(match).length > 0 ? match : undefined;
}

/**
 * Build the exact op-1 `POST /memory/facts` envelope for a draft. `now` is the
 * caller's RFC3339 clock (the server re-stamps authoritatively); `idemSuffix`
 * makes retries of the same logical write collapse to one fact id. Global scope
 * OMITS the entity block; project scope keys on `entity.project`. `supersedes`
 * rides on edit / enable-disable (the subject satisfies stage 3's requirement).
 */
export function buildLinterEnvelope(
  draft: LinterDraft,
  now: string,
  idemSuffix: string,
): Record<string, unknown> {
  const name = draft.name.trim();
  const match = draftMatch(draft);
  const body: LinterRule = {
    rule: draft.rule.trim(),
    severity: draft.severity,
    enabled: draft.enabled,
  };
  if (match) body.match = match;

  const source: Record<string, unknown> = {
    kind: 'telemetry_learning',
    producer: 'memkeys-inspector',
    emitted_at: now,
    idempotency_key: `${linterSubject(name)}:${idemSuffix}`,
  };
  const fact: Record<string, unknown> = {
    type: 'linter',
    subject: linterSubject(name),
    body,
  };
  if (draft.supersedes) fact.supersedes = draft.supersedes;

  const envelope: Record<string, unknown> = { source, fact };
  if (draft.scopeKind === 'project') {
    envelope.entity = { project: draft.project.trim() };
  }
  return envelope;
}

/** Pretty-printed envelope for the editor's collapsible wire preview (mock 6A). */
export function wirePreview(draft: LinterDraft, now: string, idemSuffix: string): string {
  return `POST /memory/facts\n${JSON.stringify(
    buildLinterEnvelope(draft, now, idemSuffix),
    null,
    2,
  )}`;
}

/** Seed an editor draft from an existing linter (edit); name becomes immutable. */
export function draftFromLinter(l: LinterRecord): LinterDraft {
  const globs = l.match?.path_glob;
  return {
    name: l.name,
    scopeKind: l.scopeKind,
    project: l.project ?? '',
    severity: l.severity ?? 'warn',
    enabled: l.enabled,
    rule: l.rule,
    language: l.match?.language ?? '',
    pathGlobs: Array.isArray(globs) ? globs.join(', ') : (globs ?? ''),
    supersedes: l.factId,
  };
}

/** A blank create draft for a given default project context. */
export function emptyDraft(defaultProject: string): LinterDraft {
  return {
    name: '',
    scopeKind: 'global',
    project: defaultProject,
    severity: 'warn',
    enabled: true,
    rule: '',
    language: '',
    pathGlobs: '',
    supersedes: null,
  };
}
