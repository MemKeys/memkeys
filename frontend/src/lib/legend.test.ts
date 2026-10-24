import { describe, it, expect } from 'vitest';
import {
  EDGE_COLOR,
  EDGE_LEGEND_ITEMS,
  EDGE_MEANINGS,
  EDGE_TYPE_ORDER,
  KIND_COLOR,
  NODE_KIND_ORDER,
  edgeDirectionRelativeToStart,
  supersededNodeIds,
} from './legend';
import { EdgeType, NodeKind } from '../types/api';

// The closed wire enums, alphabetized — the legend must cover exactly these.
const ALL_EDGE_TYPES: EdgeType[] = ['a2a_participant', 'entity_fact', 'produced_by', 'supersedes'];
const ALL_NODE_KINDS: NodeKind[] = [
  'a2a_hint',
  'entity_agent',
  'entity_issue',
  'entity_pr',
  'entity_project',
  'entity_run',
  'fact',
];

describe('edge legend data', () => {
  it('defines a meaning for exactly the closed EdgeType enum', () => {
    expect(Object.keys(EDGE_MEANINGS).sort()).toEqual(ALL_EDGE_TYPES);
  });

  it('gives every edge type a non-empty human meaning', () => {
    for (const t of ALL_EDGE_TYPES) {
      expect(typeof EDGE_MEANINGS[t]).toBe('string');
      expect(EDGE_MEANINGS[t].trim().length).toBeGreaterThan(0);
    }
  });

  it('colors every edge type exactly once with distinct colors', () => {
    expect(Object.keys(EDGE_COLOR).sort()).toEqual(ALL_EDGE_TYPES);
    for (const t of ALL_EDGE_TYPES) {
      expect(EDGE_COLOR[t]).toMatch(/^#[0-9a-f]{6}$/i);
    }
    expect(new Set(Object.values(EDGE_COLOR)).size).toBe(ALL_EDGE_TYPES.length);
  });

  it('keeps legend items in the declared wire-enum order with matching meanings', () => {
    expect(EDGE_LEGEND_ITEMS.map((i) => i.type)).toEqual(EDGE_TYPE_ORDER);
    expect(EDGE_LEGEND_ITEMS.map((i) => i.meaning)).toEqual(
      EDGE_TYPE_ORDER.map((t) => EDGE_MEANINGS[t]),
    );
  });
});

describe('node-kind legend data', () => {
  it('colors every node kind exactly once with distinct colors', () => {
    expect(Object.keys(KIND_COLOR).sort()).toEqual(ALL_NODE_KINDS);
    for (const k of ALL_NODE_KINDS) {
      expect(KIND_COLOR[k]).toMatch(/^#[0-9a-f]{6}$/i);
    }
    expect(new Set(Object.values(KIND_COLOR)).size).toBe(ALL_NODE_KINDS.length);
  });

  it('lists every node kind once in the dropdown order', () => {
    expect([...NODE_KIND_ORDER].sort()).toEqual(ALL_NODE_KINDS);
    expect(new Set(NODE_KIND_ORDER).size).toBe(ALL_NODE_KINDS.length);
  });
});

describe('edgeDirectionRelativeToStart', () => {
  it('classifies out, in, both and unrelated edges', () => {
    expect(edgeDirectionRelativeToStart('start', 'other', 'start')).toBe('out');
    expect(edgeDirectionRelativeToStart('other', 'start', 'start')).toBe('in');
    expect(edgeDirectionRelativeToStart('start', 'start', 'start')).toBe('both');
    expect(edgeDirectionRelativeToStart('a', 'b', 'start')).toBeNull();
  });

  it('returns null when the traversal start is unknown', () => {
    expect(edgeDirectionRelativeToStart('a', 'b', null)).toBeNull();
  });
});

describe('supersededNodeIds', () => {
  it('collects only the targets of supersedes edges', () => {
    const ids = supersededNodeIds([
      { source: 'new', target: 'old', edge_type: 'supersedes' },
      { source: 'fact', target: 'entity', edge_type: 'entity_fact' },
      { source: 'newer', target: 'new', edge_type: 'supersedes' },
    ]);
    expect([...ids].sort()).toEqual(['new', 'old']);
  });

  it('is empty when no supersedes edges exist', () => {
    expect(supersededNodeIds([{ source: 'a', target: 'b', edge_type: 'produced_by' }]).size).toBe(0);
  });
});
