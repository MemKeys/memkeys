import { describe, it, expect } from 'vitest';
import {
  buildNodeId,
  buildQueryParams,
  encodeSegment,
  hasEntityKey,
  KIND_ID_SEGMENT,
  recordEntityKeys,
  toGraphData,
  nodeLabel,
  DEFAULT_LIMIT,
  buildSearchBody,
  DEFAULT_SEARCH_K,
  hasSearchText,
  hitToRecord,
  isFeatureDisabled,
} from './transform';
import { FactRecord, FactsQuery, TraverseResult } from '../types/api';

const baseQuery: FactsQuery = { keys: {}, valid: 'current', limit: DEFAULT_LIMIT };

function record(over: Partial<FactRecord>): FactRecord {
  return {
    id: 'f1',
    fact_type: 'decision',
    subject: null,
    body: 'hello',
    confidence: null,
    supersedes: null,
    event_time: '2026-10-18T00:00:00Z',
    ingested_at: '2026-10-18T00:00:00Z',
    entity_project: null,
    entity_issue: null,
    entity_pr: null,
    entity_agent: null,
    entity_run: null,
    entity_thread: null,
    entity_service: null,
    keyed: true,
    provenance_principal: 'agent-a',
    ...over,
  };
}

describe('buildQueryParams', () => {
  it('maps entity keys to entity.<name> params and trims', () => {
    const p = buildQueryParams({ ...baseQuery, keys: { issue: ' I1 ', project: 'P1' } });
    expect(p['entity.issue']).toBe('I1');
    expect(p['entity.project']).toBe('P1');
    expect(p['valid']).toBe('current');
  });

  it('drops blank keys and omits default limit', () => {
    const p = buildQueryParams({ ...baseQuery, keys: { issue: '', agent: '   ' } });
    expect(p['entity.issue']).toBeUndefined();
    expect(p['entity.agent']).toBeUndefined();
    expect(p['limit']).toBeUndefined();
  });

  it('emits non-default limit, type and as_of', () => {
    const p = buildQueryParams({
      ...baseQuery,
      keys: { issue: 'I1' },
      type: 'decision',
      as_of: '2026-01-01T00:00:00Z',
      limit: 10,
    });
    expect(p['limit']).toBe('10');
    expect(p['type']).toBe('decision');
    expect(p['as_of']).toBe('2026-01-01T00:00:00Z');
  });
});

describe('hasEntityKey', () => {
  it('is false for no keys and true for a non-blank key', () => {
    expect(hasEntityKey(baseQuery)).toBe(false);
    expect(hasEntityKey({ ...baseQuery, keys: { pr: '  ' } })).toBe(false);
    expect(hasEntityKey({ ...baseQuery, keys: { pr: '42' } })).toBe(true);
  });
});

describe('recordEntityKeys', () => {
  it('returns only present keys in canonical order', () => {
    const r = record({ entity_project: 'P', entity_issue: 'I' });
    expect(recordEntityKeys(r)).toEqual([
      ['project', 'P'],
      ['issue', 'I'],
    ]);
  });
});

describe('toGraphData', () => {
  it('drops edges that dangle to an unknown node id', () => {
    const result: TraverseResult = {
      nodes: [
        { kind: 'fact', id: 'a', record: null, unhydratable: false },
        { kind: 'fact', id: 'b', record: null, unhydratable: false },
      ],
      edges: [
        { from: 'a', to: 'b', edge_type: 'entity_fact', event_time: '2026-10-18T00:00:00Z' },
        { from: 'a', to: 'ghost', edge_type: 'supersedes', event_time: '2026-10-18T00:00:00Z' },
      ],
      truncated: false,
      reason: 'none',
      cursor: null,
    };
    const g = toGraphData(result);
    expect(g.links).toHaveLength(1);
    expect(g.droppedEdges).toBe(1);
    expect(g.nodes).toHaveLength(2);
  });
});

describe('nodeLabel', () => {
  it('prefers subject, falls back to body, then id tail', () => {
    expect(nodeLabel({ kind: 'fact', id: 'x', unhydratable: false, record: record({ subject: 'S' }) })).toBe('S');
    expect(
      nodeLabel({ kind: 'fact', id: 'x', unhydratable: false, record: record({ body: 'short body' }) }),
    ).toBe('short body');
    const topo = nodeLabel({ kind: 'entity_issue', id: 'a'.repeat(30), unhydratable: false, record: null });
    expect(topo.startsWith('…')).toBe(true);
  });
});

describe('buildNodeId / encodeSegment', () => {
  it('builds tenant|kind|value with the dotted id spelling for entity kinds', () => {
    expect(buildNodeId('615e9564', 'entity_project', 'memkeys')).toBe(
      '615e9564|entity.project|memkeys',
    );
  });

  it('percent-encodes % and | inside segments', () => {
    expect(encodeSegment('a|b%c')).toBe('a%7Cb%25c');
    expect(buildNodeId('a|b', 'fact', 'x%y')).toBe('a%7Cb|fact|x%25y');
  });

  it('trims tenant and value before encoding', () => {
    expect(buildNodeId('  t  ', 'fact', ' v ')).toBe('t|fact|v');
  });

  it('keeps the snake_case wire kinds distinct from the dotted id segments', () => {
    expect(KIND_ID_SEGMENT.entity_issue).toBe('entity.issue');
    expect(KIND_ID_SEGMENT.entity_pr).toBe('entity.pr');
    expect(KIND_ID_SEGMENT.entity_agent).toBe('entity.agent');
    expect(KIND_ID_SEGMENT.entity_run).toBe('entity.run');
    expect(KIND_ID_SEGMENT.fact).toBe('fact');
  });
});

// --- ISI-6581: Explore tab query transforms (POST /memory/search) ---

describe('buildSearchBody', () => {
  it('trims the query and stays keyless (no entity scope)', () => {
    const b = buildSearchBody({ query: '  what did we decide about run tokens?  ', type: '', k: DEFAULT_SEARCH_K });
    expect(b).toEqual({ query: 'what did we decide about run tokens?' });
  });

  it('maps the type filter to scope["fact.type"]', () => {
    const b = buildSearchBody({ query: 'q', type: ' decision ', k: DEFAULT_SEARCH_K });
    expect(b['scope']).toEqual({ 'fact.type': 'decision' });
  });

  it('emits k only when it differs from the server default', () => {
    expect(buildSearchBody({ query: 'q', type: '', k: 25 })['k']).toBe(25);
    expect(buildSearchBody({ query: 'q', type: '', k: DEFAULT_SEARCH_K })['k']).toBeUndefined();
  });
});

describe('hasSearchText', () => {
  it('requires non-blank text — the only Explore requirement', () => {
    expect(hasSearchText({ query: '   ', type: '', k: 10 })).toBe(false);
    expect(hasSearchText({ query: ' hi ', type: 'decision', k: 10 })).toBe(true);
  });
});

describe('hitToRecord', () => {
  it('flattens a semantic hit onto a FactRecord with entity_* fields', () => {
    const r = hitToRecord({
      'fact.id': 'f9',
      score: 0.875,
      'fact.type': 'decision',
      'fact.subject': 'Use run-token JWTs',
      'fact.body': 'decided: run tokens gate /memory/*',
      entity: { project: 'memkeys', issue: 'ISI-6541' },
      event_time: '2026-10-20T00:00:00Z',
      provenance: 'agent-a',
      via: 'semantic',
    });
    expect(r.id).toBe('f9');
    expect(r.fact_type).toBe('decision');
    expect(r.subject).toBe('Use run-token JWTs');
    expect(r.entity_project).toBe('memkeys');
    expect(r.entity_issue).toBe('ISI-6541');
    expect(r.entity_agent).toBeNull();
    expect(r.ingested_at).toBe('2026-10-20T00:00:00Z');
    expect(r.keyed).toBe(true);
    expect(r.provenance_principal).toBe('agent-a');
  });
});

describe('isFeatureDisabled', () => {
  it('matches the 501 feature_disabled in both code spellings', () => {
    expect(isFeatureDisabled({ error: 'semantic module disabled', code: 'feature_disabled' })).toBe(true);
    expect(isFeatureDisabled({ error: 'x', code: '501' })).toBe(true);
    expect(isFeatureDisabled({ error: 'x', code: 'invalid_argument' })).toBe(false);
    expect(isFeatureDisabled({ error: 'x' })).toBe(false);
  });
});
