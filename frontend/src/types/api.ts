// Wire types for the MemKeys read-only REST surface (crate `memkeys-server`,
// WS-5 §7). These mirror the server's serde shapes exactly — see
// crates/memkeys-server/src/{http,model,graph}.rs. The Inspector UI consumes
// ONLY these read paths; it never writes (no POST /memory/facts).

/** The seven server-resolved entity-key slots (`FactRecord.entity_*`). */
export type EntityKeyName =
  | 'project'
  | 'issue'
  | 'pr'
  | 'agent'
  | 'run'
  | 'thread'
  | 'service';

export const ENTITY_KEY_NAMES: EntityKeyName[] = [
  'project',
  'issue',
  'pr',
  'agent',
  'run',
  'thread',
  'service',
];

/**
 * Authoritative read record returned by `GET /memory/facts` inside
 * `{ facts: [...] }` (model.rs `FactRecord`). `body` is an opaque string;
 * nullable fields come back as `null`.
 */
export interface FactRecord {
  id: string;
  fact_type: string;
  subject: string | null;
  body: string;
  confidence: number | null;
  supersedes: string | null;
  event_time: string;
  ingested_at: string;
  entity_project: string | null;
  entity_issue: string | null;
  entity_pr: string | null;
  entity_agent: string | null;
  entity_run: string | null;
  entity_thread: string | null;
  entity_service: string | null;
  keyed: boolean;
  provenance_principal: string;
}

/** §7.5 response-shape invariant: always wrapped, never a bare array. */
export interface FactsResponse {
  facts: FactRecord[];
}

/** §7.2 `valid` supersession-collapse mode. */
export type Validity = 'current' | 'all';

/** Flat query the facts browser builds; mapped to query params by the client. */
export interface FactsQuery {
  /** At least one entity key is required (keys are ANDed server-side). */
  keys: Partial<Record<EntityKeyName, string>>;
  /** §7.2 canonical `type` filter (a FactType value). */
  type?: string;
  /** §7.2 RFC3339 point-in-time replay; absent = live head. */
  as_of?: string;
  valid: Validity;
  limit: number;
}

// --- graph traverse (graph.rs) ---

/** `NodeKind` (snake_case serde). */
export type NodeKind =
  | 'fact'
  | 'entity_project'
  | 'entity_issue'
  | 'entity_pr'
  | 'entity_agent'
  | 'entity_run'
  | 'a2a_hint';

/** `EdgeType` (snake_case serde). */
export type EdgeType =
  | 'entity_fact'
  | 'supersedes'
  | 'produced_by'
  | 'a2a_participant';

/** `Direction` (lowercase serde). */
export type Direction = 'out' | 'in' | 'both';

/** `TruncReason` (snake_case serde). */
export type TruncReason =
  | 'none'
  | 'depth_clamp'
  | 'fanout'
  | 'node_budget'
  | 'deadline'
  | 'orphan';

/** `{kind, id}` start node (§7.3); tenant is NEVER in the body. */
export interface NodeRef {
  kind: NodeKind;
  id: string;
}

/**
 * Client-settable traverse request — the exact `TraverseReqWire` shape
 * (http.rs). `tenant`, `fanout_cap`, and `node_budget` are server-derived and
 * deliberately absent here; the body can never widen scope.
 */
export interface TraverseRequest {
  start: NodeRef;
  edges: EdgeType[];
  direction: Direction;
  depth: number;
  hydrate: boolean;
  deadline_ms: number;
}

export interface TraverseNode {
  kind: NodeKind;
  id: string;
  record: FactRecord | null;
  unhydratable: boolean;
}

export interface TraverseEdge {
  from: string;
  to: string;
  edge_type: EdgeType;
  event_time: string;
}

export interface TraverseResult {
  nodes: TraverseNode[];
  edges: TraverseEdge[];
  truncated: boolean;
  reason: TruncReason;
  cursor: {
    depth: number;
    frontier: string[];
    visited: string[];
  } | null;
}

/** WS-5 §7 structured error body (`{error, code?, retriable?}`). */
export interface APIError {
  error: string;
  code?: string;
  retriable?: boolean;
}

// --- semantic search (op-4; ISI-6581 Explore tab) ---

/**
 * Semantic search op-4 — the keyless Explore query. 'query' is plain natural
 * language (no entity keys required; the tenant floor is server-set from the
 * verified principal, R6); 'type' maps to the server's scope["fact.type"]
 * AND-filter; 'k' is the result count (server default DEFAULT_K=10, ceiling
 * MAX_K=100).
 */
export interface SearchQuery {
  query: string;
  /** Canonical fact-type filter; empty = any type. */
  type?: string;
  k: number;
}

/**
 * One semantic-search hit (SemanticHit::to_wire_json, service.rs): the
 * authoritative record flattened as fact.* fields plus a resolved entity
 * map, with the fused similarity score and the via provenance path.
 */
export interface SearchHit {
  'fact.id': string;
  score: number;
  'fact.type': string;
  'fact.subject': string | null;
  'fact.body': string;
  entity: Partial<Record<EntityKeyName, string>>;
  event_time: string;
  provenance: string;
  via: string;
}

/** Response-shape invariant: always wrapped, never a bare array. */
export interface SearchResponse {
  hits: SearchHit[];
}
