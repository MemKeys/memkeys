// Pure legend/detail data for the Memory Graph (ISI-6582). Kept free of React
// and d3 so the edge-semantics table and the derived helpers stay
// unit-testable without a browser (see legend.test.ts).

import { EdgeType, NodeKind } from '../types/api';

/** Declared display order for edge semantics (mirrors the wire enum order). */
export const EDGE_TYPE_ORDER: EdgeType[] = [
  'entity_fact',
  'supersedes',
  'produced_by',
  'a2a_participant',
];

/**
 * The closed meaning of each edge type (graph.rs `EdgeType`). The legend and
 * the edge-detail popup render exactly these strings — one place to edit.
 */
export const EDGE_MEANINGS: Record<EdgeType, string> = {
  entity_fact: 'Fact is keyed on this entity.',
  supersedes: 'This fact replaces an earlier version (bitemporal close).',
  produced_by: 'Agent/run that produced this fact.',
  a2a_participant: 'Participant in an agent-to-agent thread.',
};

/**
 * Per-type edge color (approved UX mock ISI-6582 §4: edges are not anonymous
 * gray lines). Distinct from every node-kind color ramp entry.
 */
export const EDGE_COLOR: Record<EdgeType, string> = {
  entity_fact: '#f5a623',
  supersedes: '#e74c3c',
  produced_by: '#1abc9c',
  a2a_participant: '#af7aa5',
};

export interface EdgeLegendItem {
  type: EdgeType;
  meaning: string;
}

export const EDGE_LEGEND_ITEMS: EdgeLegendItem[] = EDGE_TYPE_ORDER.map((type) => ({
  type,
  meaning: EDGE_MEANINGS[type],
}));

/** Declared display order for node kinds (matches the start-kind dropdown). */
export const NODE_KIND_ORDER: NodeKind[] = [
  'fact',
  'entity_project',
  'entity_issue',
  'entity_pr',
  'entity_agent',
  'entity_run',
  'a2a_hint',
];

/** Stable color per node kind (color-blind-safe-ish categorical ramp). */
export const KIND_COLOR: Record<NodeKind, string> = {
  fact: '#4f8cff',
  entity_project: '#2ca02c',
  entity_issue: '#ff7f0e',
  entity_pr: '#9467bd',
  entity_agent: '#17becf',
  entity_run: '#bcbd22',
  a2a_hint: '#e377c2',
};

/** Direction of an edge relative to the traversal start node. */
export type StartDirection = 'out' | 'in' | 'both';

/**
 * `out` — the edge leaves the traversal start; `in` — it enters the start;
 * `both` — self-loop on the start; `null` — neither endpoint is the start
 * (or the traversal start is unknown).
 */
export function edgeDirectionRelativeToStart(
  sourceId: string,
  targetId: string,
  startId: string | null,
): StartDirection | null {
  if (!startId) return null;
  const fromStart = sourceId === startId;
  const toStart = targetId === startId;
  if (fromStart && toStart) return 'both';
  if (fromStart) return 'out';
  if (toStart) return 'in';
  return null;
}

/**
 * Node ids superseded by a newer fact version: the target of every
 * `supersedes` edge (the newer fact points at the fact it replaces). Drawn
 * with a dashed ring per the graph legend.
 */
export function supersededNodeIds(
  links: Array<{ source: string; target: string; edge_type: string }>,
): Set<string> {
  const ids = new Set<string>();
  for (const l of links) {
    if (l.edge_type === 'supersedes') ids.add(l.target);
  }
  return ids;
}

/** Detail payload for a clicked edge (endpoint ids resolved from d3 data). */
export interface EdgeDetail {
  source: string;
  target: string;
  edge_type: string;
  event_time: string;
  direction: StartDirection | null;
}
