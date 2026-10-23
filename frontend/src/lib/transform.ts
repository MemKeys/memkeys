// Pure, side-effect-free mappings between the UI's form state and the wire
// contract. Isolated here so they can be unit-tested without React or a live
// server (see transform.test.ts).

import {
  ENTITY_KEY_NAMES,
  EntityKeyName,
  FactRecord,
  FactsQuery,
  NodeKind,
  TraverseNode,
  TraverseResult,
} from '../types/api';

export const DEFAULT_LIMIT = 100;

/**
 * The id-segment spelling of each node kind (graph.rs `NodeKind::as_str`).
 * Entity kinds use a DOT in ids (`entity.project`) even though the wire enum
 * and the kind dropdown are snake_case (`entity_project`) — the mismatch that
 * made hand-built ids return `not_found`.
 */
export const KIND_ID_SEGMENT: Record<NodeKind, string> = {
  fact: 'fact',
  entity_project: 'entity.project',
  entity_issue: 'entity.issue',
  entity_pr: 'entity.pr',
  entity_agent: 'entity.agent',
  entity_run: 'entity.run',
  a2a_hint: 'a2a_hint',
};

/** Percent-encode one node-id segment for `|` and `%` (graph.rs `encode_segment`). */
export function encodeSegment(s: string): string {
  // split/join over replaceAll: the tsconfig lib target predates ES2021.
  return s.split('%').join('%25').split('|').join('%7C');
}

/**
 * Build the tenant-qualified opaque start id `"{tenant}|{kind}|{value}"`
 * client-side (graph.rs `node_id`), so users never hand-assemble it.
 */
export function buildNodeId(tenant: string, kind: NodeKind, value: string): string {
  return [
    encodeSegment(tenant.trim()),
    KIND_ID_SEGMENT[kind],
    encodeSegment(value.trim()),
  ].join('|');
}

/**
 * Flatten a {@link FactsQuery} into the flat query-param map the server's
 * `build_keyed_query` expects (`entity.project=…`, `type=…`, `valid=…`, …).
 * Empty/blank entity values are dropped. `limit` is emitted only when it
 * differs from the server default, to keep URLs clean.
 */
export function buildQueryParams(query: FactsQuery): Record<string, string> {
  const params: Record<string, string> = {};
  for (const name of ENTITY_KEY_NAMES) {
    const v = query.keys[name];
    if (v && v.trim() !== '') params[`entity.${name}`] = v.trim();
  }
  if (query.type && query.type.trim() !== '') params['type'] = query.type.trim();
  if (query.as_of && query.as_of.trim() !== '') params['as_of'] = query.as_of.trim();
  params['valid'] = query.valid;
  if (query.limit !== DEFAULT_LIMIT) params['limit'] = String(query.limit);
  return params;
}

/** True when the query carries at least one non-blank entity key (required). */
export function hasEntityKey(query: FactsQuery): boolean {
  return ENTITY_KEY_NAMES.some((n) => {
    const v = query.keys[n];
    return !!v && v.trim() !== '';
  });
}

/** The server-resolved entity keys present on a record, for display. */
export function recordEntityKeys(r: FactRecord): Array<[EntityKeyName, string]> {
  const out: Array<[EntityKeyName, string]> = [];
  const map: Record<EntityKeyName, string | null> = {
    project: r.entity_project,
    issue: r.entity_issue,
    pr: r.entity_pr,
    agent: r.entity_agent,
    run: r.entity_run,
    thread: r.entity_thread,
    service: r.entity_service,
  };
  for (const name of ENTITY_KEY_NAMES) {
    const v = map[name];
    if (v) out.push([name, v]);
  }
  return out;
}

export interface GraphNode {
  id: string;
  kind: NodeKind;
  label: string;
  unhydratable: boolean;
  record: FactRecord | null;
}

export interface GraphLink {
  source: string;
  target: string;
  edge_type: string;
  /** Bitemporal event time carried on the wire edge (`TraverseEdge.event_time`). */
  event_time: string;
}

export interface GraphData {
  nodes: GraphNode[];
  links: GraphLink[];
}

/** A short human label for a node id (fact bodies are truncated). */
export function nodeLabel(n: TraverseNode): string {
  if (n.record) {
    const subj = n.record.subject?.trim();
    if (subj) return subj;
    const body = n.record.body?.trim();
    if (body) return body.length > 40 ? body.slice(0, 40) + '…' : body;
    return n.record.fact_type;
  }
  // Topology-only node: show the opaque id tail.
  return n.id.length > 24 ? '…' + n.id.slice(-22) : n.id;
}

/**
 * Transform a {@link TraverseResult} into d3-friendly `{nodes, links}`. Edges
 * that reference an unknown node id are dropped (a projection hole should not
 * crash the force layout); dropped edges are reported in `droppedEdges` so
 * the UI can surface the gap rather than silently hide it. The wire edge's
 * `event_time` is carried through for the edge-detail popup (ISI-6582).
 */
export function toGraphData(result: TraverseResult): GraphData & { droppedEdges: number } {
  const nodes: GraphNode[] = result.nodes.map((n) => ({
    id: n.id,
    kind: n.kind,
    label: nodeLabel(n),
    unhydratable: n.unhydratable,
    record: n.record,
  }));
  const ids = new Set(nodes.map((n) => n.id));
  const links: GraphLink[] = [];
  let droppedEdges = 0;
  for (const e of result.edges) {
    if (ids.has(e.from) && ids.has(e.to)) {
      links.push({
        source: e.from,
        target: e.to,
        edge_type: e.edge_type,
        event_time: e.event_time,
      });
    } else {
      droppedEdges += 1;
    }
  }
  return { nodes, links, droppedEdges };
}
