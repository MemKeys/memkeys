import { describe, it, expect } from 'vitest';
import {
  linterSubject,
  nameFromSubject,
  isSeverity,
  parseLinterBody,
  matchSummary,
  factRecordToLinter,
  applicableToLinter,
  mergeLinters,
  filterLinters,
  linterCounts,
  validateDraft,
  isDraftValid,
  draftMatch,
  buildLinterEnvelope,
  wirePreview,
  draftFromLinter,
  emptyDraft,
  LINTER_SLUG_RE,
  RULE_MAX_LEN,
} from './linters';
import { ApplicableLinter, FactRecord, LinterDraft } from '../types/api';

function record(over: Partial<FactRecord>): FactRecord {
  return {
    id: 'f1',
    fact_type: 'linter',
    subject: 'linter:no-new-rust-code',
    body: JSON.stringify({ rule: 'Never add Rust.', severity: 'error', enabled: true }),
    confidence: null,
    supersedes: null,
    event_time: '2026-11-20T14:03:00Z',
    ingested_at: '2026-11-20T14:03:01Z',
    entity_project: null,
    entity_issue: null,
    entity_pr: null,
    entity_agent: null,
    entity_run: null,
    entity_thread: null,
    entity_service: null,
    keyed: false,
    provenance_principal: 'acme/inspector-ui',
    ...over,
  };
}

function draft(over: Partial<LinterDraft>): LinterDraft {
  return { ...emptyDraft('memkeys'), ...over };
}

describe('subject <-> name', () => {
  it('prefixes a bare name and is idempotent', () => {
    expect(linterSubject('no-new-rust-code')).toBe('linter:no-new-rust-code');
    expect(linterSubject('linter:no-new-rust-code')).toBe('linter:no-new-rust-code');
    expect(linterSubject('  spaced  ')).toBe('linter:spaced');
  });
  it('strips the prefix once and tolerates null/absent subjects', () => {
    expect(nameFromSubject('linter:foo')).toBe('foo');
    expect(nameFromSubject('bare')).toBe('bare');
    expect(nameFromSubject(null)).toBe('');
    expect(nameFromSubject(undefined)).toBe('');
  });
});

describe('isSeverity', () => {
  it('accepts only the three grammar values', () => {
    expect(isSeverity('info')).toBe(true);
    expect(isSeverity('warn')).toBe(true);
    expect(isSeverity('error')).toBe(true);
    expect(isSeverity('critical')).toBe(false);
    expect(isSeverity(null)).toBe(false);
    expect(isSeverity(3)).toBe(false);
  });
});

describe('parseLinterBody', () => {
  it('parses the native rule shape', () => {
    const p = parseLinterBody(
      JSON.stringify({
        rule: 'All handlers authed.',
        severity: 'error',
        enabled: false,
        match: { language: 'rust', path_glob: 'crates/**' },
      }),
    );
    expect(p.rule).toBe('All handlers authed.');
    expect(p.severity).toBe('error');
    expect(p.enabled).toBe(false);
    expect(p.match).toEqual({ language: 'rust', path_glob: 'crates/**' });
    expect(p.bodyJson?.rule).toBe('All handlers authed.');
  });
  it('defaults enabled to true when the flag is absent', () => {
    expect(parseLinterBody(JSON.stringify({ rule: 'x', severity: 'info' })).enabled).toBe(true);
  });
  it('degrades a non-JSON body to raw text + unknown severity', () => {
    const p = parseLinterBody('just some prose the agent reads');
    expect(p.rule).toBe('just some prose the agent reads');
    expect(p.severity).toBeNull();
    expect(p.enabled).toBe(true);
    expect(p.bodyJson).toBeNull();
  });
  it('degrades a JSON array/number to raw text (not an object)', () => {
    expect(parseLinterBody('[1,2,3]').bodyJson).toBeNull();
    expect(parseLinterBody('42').bodyJson).toBeNull();
  });
  it('ignores an out-of-grammar severity', () => {
    expect(parseLinterBody(JSON.stringify({ rule: 'x', severity: 'nope' })).severity).toBeNull();
  });
});

describe('matchSummary', () => {
  it('summarizes language + single glob + list, and dashes on empty', () => {
    expect(matchSummary(null)).toBe('—');
    expect(matchSummary({ language: 'rust' })).toBe('language=rust');
    expect(matchSummary({ path_glob: 'crates/**' })).toBe('path=crates/**');
    expect(matchSummary({ language: 'rust', path_glob: ['a/**', 'b/**'] })).toBe(
      'language=rust · path=a/**, b/**',
    );
    expect(matchSummary({})).toBe('—');
  });
});

describe('factRecordToLinter', () => {
  it('maps a global (unkeyed) linter', () => {
    const l = factRecordToLinter(record({}));
    expect(l.name).toBe('no-new-rust-code');
    expect(l.scopeKind).toBe('global');
    expect(l.project).toBeNull();
    expect(l.severity).toBe('error');
    expect(l.enabled).toBe(true);
    expect(l.eventTime).toBe('2026-11-20T14:03:00Z');
    expect(l.provenance).toBe('acme/inspector-ui');
  });
  it('maps a project (keyed) linter and collects entity keys', () => {
    const l = factRecordToLinter(record({ entity_project: 'memkeys', keyed: true }));
    expect(l.scopeKind).toBe('project');
    expect(l.project).toBe('memkeys');
    expect(l.entityKeys).toEqual([['project', 'memkeys']]);
  });
});

describe('applicableToLinter', () => {
  const entry: ApplicableLinter = {
    scope: 'global',
    severity: 'info',
    'fact.id': 'g1',
    'fact.subject': 'linter:prefer-adr',
    'fact.body': { rule: 'Record ADRs.', severity: 'info', enabled: false },
    entity: {},
  };
  it('maps a resolver entry (lean, no timestamps)', () => {
    const l = applicableToLinter(entry);
    expect(l.factId).toBe('g1');
    expect(l.name).toBe('prefer-adr');
    expect(l.scopeKind).toBe('global');
    expect(l.severity).toBe('info');
    expect(l.enabled).toBe(false);
    expect(l.eventTime).toBeNull();
  });
  it('maps context scope to project and handles a raw-string body', () => {
    const l = applicableToLinter({
      ...entry,
      scope: 'context',
      'fact.body': 'raw rule text',
      entity: { project: 'memkeys' },
    });
    expect(l.scopeKind).toBe('project');
    expect(l.project).toBe('memkeys');
    expect(l.rule).toBe('raw rule text');
    expect(l.bodyJson).toBeNull();
  });
});

describe('mergeLinters', () => {
  it('de-dups by fact id (FactRecord wins) and orders global-first then name', () => {
    const globalG = applicableToLinter({
      scope: 'global',
      severity: 'error',
      'fact.id': 'g1',
      'fact.subject': 'linter:zeta',
      'fact.body': { rule: 'z', severity: 'error', enabled: true },
      entity: {},
    });
    // Same id as a project FactRecord — the record (fuller) must win.
    const projDup = factRecordToLinter(
      record({ id: 'p1', subject: 'linter:alpha', entity_project: 'memkeys', keyed: true }),
    );
    const resolverDup = applicableToLinter({
      scope: 'context',
      severity: 'error',
      'fact.id': 'p1',
      'fact.subject': 'linter:alpha',
      'fact.body': { rule: 'a', severity: 'error', enabled: true },
      entity: { project: 'memkeys' },
    });
    const merged = mergeLinters([projDup], [globalG, resolverDup]);
    expect(merged.map((l) => l.factId)).toEqual(['g1', 'p1']);
    // p1 came from the FactRecord (has provenance), not the resolver entry.
    expect(merged[1].provenance).toBe('acme/inspector-ui');
  });
});

describe('filterLinters + linterCounts', () => {
  const list = [
    factRecordToLinter(record({ id: 'a', subject: 'linter:aaa', body: JSON.stringify({ rule: 'rust rule', severity: 'error', enabled: true }) })),
    factRecordToLinter(record({ id: 'b', subject: 'linter:bbb', entity_project: 'memkeys', keyed: true, body: JSON.stringify({ rule: 'auth handlers', severity: 'warn', enabled: false }) })),
  ];
  it('counts total / enabled / scope', () => {
    expect(linterCounts(list)).toEqual({ total: 2, enabled: 1, global: 1, project: 1 });
  });
  it('filters by scope', () => {
    expect(filterLinters(list, { scope: 'global', severity: '', text: '' })).toHaveLength(1);
    expect(filterLinters(list, { scope: 'project', severity: '', text: '' })[0].name).toBe('bbb');
  });
  it('filters by severity', () => {
    expect(filterLinters(list, { scope: 'all', severity: 'warn', text: '' })).toHaveLength(1);
  });
  it('filters by case-insensitive text over name + rule', () => {
    expect(filterLinters(list, { scope: 'all', severity: '', text: 'AUTH' })[0].name).toBe('bbb');
    expect(filterLinters(list, { scope: 'all', severity: '', text: 'aaa' })).toHaveLength(1);
    expect(filterLinters(list, { scope: 'all', severity: '', text: 'zzz' })).toHaveLength(0);
  });
});

describe('validateDraft / isDraftValid', () => {
  it('accepts a well-formed global draft', () => {
    const d = draft({ name: 'no-new-rust', rule: 'Never add Rust.' });
    expect(validateDraft(d)).toEqual({});
    expect(isDraftValid(d)).toBe(true);
  });
  it('rejects a blank or non-slug name', () => {
    expect(validateDraft(draft({ name: '', rule: 'x' })).name).toMatch(/required/);
    expect(validateDraft(draft({ name: 'Not A Slug', rule: 'x' })).name).toMatch(/slug/);
    expect(validateDraft(draft({ name: '-leading', rule: 'x' })).name).toMatch(/slug/);
  });
  it('rejects blank and over-long rule text', () => {
    expect(validateDraft(draft({ name: 'ok', rule: '' })).rule).toMatch(/required/);
    expect(
      validateDraft(draft({ name: 'ok', rule: 'x'.repeat(RULE_MAX_LEN + 1) })).rule,
    ).toMatch(/too long/);
  });
  it('requires a project only for project scope', () => {
    expect(validateDraft(draft({ name: 'ok', rule: 'r', scopeKind: 'project', project: '' })).project).toMatch(/required/);
    expect(validateDraft(draft({ name: 'ok', rule: 'r', scopeKind: 'project', project: 'memkeys' })).project).toBeUndefined();
    expect(validateDraft(draft({ name: 'ok', rule: 'r', scopeKind: 'global', project: '' })).project).toBeUndefined();
  });
  it('slug regex sanity', () => {
    expect(LINTER_SLUG_RE.test('a1-b2')).toBe(true);
    expect(LINTER_SLUG_RE.test('UPPER')).toBe(false);
  });
});

describe('draftMatch', () => {
  it('omits an empty match, keeps language, and single vs list globs', () => {
    expect(draftMatch(draft({ language: '', pathGlobs: '' }))).toBeUndefined();
    expect(draftMatch(draft({ language: 'rust', pathGlobs: '' }))).toEqual({ language: 'rust' });
    expect(draftMatch(draft({ language: '', pathGlobs: 'crates/**' }))).toEqual({ path_glob: 'crates/**' });
    expect(draftMatch(draft({ language: 'rust', pathGlobs: 'a/**, b/**' }))).toEqual({
      language: 'rust',
      path_glob: ['a/**', 'b/**'],
    });
  });
});

describe('buildLinterEnvelope', () => {
  const now = '2026-11-25T13:40:11Z';
  it('builds a global create envelope (no entity block)', () => {
    const env = buildLinterEnvelope(
      draft({ name: 'no-new-rust', rule: 'Never add Rust.', severity: 'error' }),
      now,
      'ui-1',
    ) as any;
    expect(env.source.kind).toBe('telemetry_learning');
    expect(env.source.producer).toBe('memkeys-inspector');
    expect(env.source.emitted_at).toBe(now);
    expect(env.source.idempotency_key).toBe('linter:no-new-rust:ui-1');
    expect(env.entity).toBeUndefined();
    expect(env.fact.type).toBe('linter');
    expect(env.fact.subject).toBe('linter:no-new-rust');
    expect(env.fact.body).toEqual({ rule: 'Never add Rust.', severity: 'error', enabled: true });
    expect(env.fact.supersedes).toBeUndefined();
  });
  it('keys a project envelope on entity.project', () => {
    const env = buildLinterEnvelope(
      draft({ name: 'authed', rule: 'r', scopeKind: 'project', project: 'memkeys' }),
      now,
      'ui-2',
    ) as any;
    expect(env.entity).toEqual({ project: 'memkeys' });
  });
  it('sets supersedes on edit/disable and embeds a match block', () => {
    const env = buildLinterEnvelope(
      draft({ name: 'f', rule: 'r', supersedes: 'old-id', language: 'rust', pathGlobs: 'crates/**', enabled: false }),
      now,
      'ui-3',
    ) as any;
    expect(env.fact.supersedes).toBe('old-id');
    expect(env.fact.body.enabled).toBe(false);
    expect(env.fact.body.match).toEqual({ language: 'rust', path_glob: 'crates/**' });
  });
});

describe('wirePreview', () => {
  it('prefixes the POST line and pretty-prints the envelope', () => {
    const txt = wirePreview(draft({ name: 'x', rule: 'r' }), '2026-11-25T13:40:11Z', 'ui-1');
    expect(txt.startsWith('POST /memory/facts\n{')).toBe(true);
    expect(txt).toContain('"type": "linter"');
  });
});

describe('draftFromLinter', () => {
  it('seeds an edit draft with supersedes = current fact id and immutable name', () => {
    const l = factRecordToLinter(
      record({
        id: 'cur',
        subject: 'linter:auth',
        entity_project: 'memkeys',
        keyed: true,
        body: JSON.stringify({ rule: 'r', severity: 'warn', enabled: true, match: { language: 'typescript', path_glob: ['a', 'b'] } }),
      }),
    );
    const d = draftFromLinter(l);
    expect(d.name).toBe('auth');
    expect(d.scopeKind).toBe('project');
    expect(d.project).toBe('memkeys');
    expect(d.severity).toBe('warn');
    expect(d.language).toBe('typescript');
    expect(d.pathGlobs).toBe('a, b');
    expect(d.supersedes).toBe('cur');
  });
});
