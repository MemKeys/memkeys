import { describe, it, expect } from 'vitest';
import {
  buildQueryParams,
  hasEntityKey,
  recordEntityKeys,
  toGraphData,
  nodeLabel,
  DEFAULT_LIMIT,
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
