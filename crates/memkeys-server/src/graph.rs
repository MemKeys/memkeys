//! Graph projection domain + `GraphView` port (ADR-GRAPH, WS-2/ISI-5867
//! Accepted rev 4 — docs/architecture.md §10) and the pure, bounded
//! level-synchronous BFS engine.
//!
//! The graph is a derived, rebuildable projection of the FactStore (§5.4.1):
//! it carries node ids + edge metadata only, never body text. `GraphView` is
//! the narrow interface so the substrate (relational edge-rows today) can be
//! swapped without touching callers.
//!
//! Node identity (ADR-GRAPH "Node-id encoding scheme", rev 4): canonical
//! identity is the triple `(tenant, kind, local_id)`; the wire/storage form is
//! the opaque string `"{tenant}|{kind}|{local_id}"` where each segment is
//! percent-encoded for `|` and `%`. The encoding is injective over the triple:
//! no concatenation collision can alias one tenant's node onto another's, and
//! no `local_id` containing the separator can escape its own segment. Callers
//! treat node ids as opaque; only the server decodes.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::model::{EntityKey, FactRecord, ProcessedFact};

/// The ADR-GRAPH structural-rule version stamped on every projected edge
/// (`edges.rule_ver`): structural edges are rule-derived, not model-derived —
/// model swaps never churn them (LOW-14). Bump when the derivation rules below
/// change semantics.
pub const RULE_VER: &str = "adr-graph-r4";

/// Hard interface ceiling for `TraverseReq.depth` (ADR-GRAPH Bounds). Requests
/// beyond this clamp to 5 with `TruncReason::DepthClamp`.
pub const MAX_DEPTH: u8 = 5;

/// Default BFS working envelope (per-level fanout cap and total node budget).
/// Every read is budgeted — these are DoS bounds, not tuning knobs.
pub const DEFAULT_FANOUT_CAP: usize = 256;
pub const DEFAULT_NODE_BUDGET: usize = 1024;

/// Default wall-clock deadline for one bounded read (ADR-GRAPH B3: cardinality
/// bounds alone don't bound a low-fanout deep walk's latency).
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(500);

/// Maximum supersession lineage length gathered per `Supersedes` call before
/// truncation (the lineage walk is budgeted like every other read, B8).
pub const MAX_CHAIN: usize = 64;

/// Edge vocabulary (ADR-GRAPH; the §5.4.1 `temporal` edge was dropped — rev 2,
/// HIGH-6/B4/Gap4: the only query it served is a keyed range filter §5.2/§5.3
/// do strictly better).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    EntityFact,
    Supersedes,
    ProducedBy,
    A2aParticipant,
}

impl EdgeType {
    pub const ALL: [EdgeType; 4] = [
        EdgeType::EntityFact,
        EdgeType::Supersedes,
        EdgeType::ProducedBy,
        EdgeType::A2aParticipant,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeType::EntityFact => "entity_fact",
            EdgeType::Supersedes => "supersedes",
            EdgeType::ProducedBy => "produced_by",
            EdgeType::A2aParticipant => "a2a_participant",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == s)
    }
}

/// Node kinds (ADR-GRAPH node-id scheme): `fact` and `a2a_hint` are their own
/// kinds; entity nodes carry the keyed dimension in the kind string
/// (`entity.project` … `entity.run` — the §10 edge-row schema's `src_kind`
/// comment), which keeps the `(tenant, kind, local_id)` triple injective even
/// when two dimensions hold equal values. Only the five §5.4.1 entity
/// dimensions project graph nodes; `thread`/`service` are keyed dimensions
/// without a graph entity node (same ruling as the ADR's entity list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Fact,
    EntityProject,
    EntityIssue,
    EntityPr,
    EntityAgent,
    EntityRun,
    A2aHint,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Fact => "fact",
            NodeKind::EntityProject => "entity.project",
            NodeKind::EntityIssue => "entity.issue",
            NodeKind::EntityPr => "entity.pr",
            NodeKind::EntityAgent => "entity.agent",
            NodeKind::EntityRun => "entity.run",
            NodeKind::A2aHint => "a2a_hint",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            NodeKind::Fact,
            NodeKind::EntityProject,
            NodeKind::EntityIssue,
            NodeKind::EntityPr,
            NodeKind::EntityAgent,
            NodeKind::EntityRun,
            NodeKind::A2aHint,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }

    /// The graph-projecting keyed dimensions (§5.4.1 entity nodes).
    pub fn for_entity_key(key: EntityKey) -> Option<Self> {
        match key {
            EntityKey::Project => Some(NodeKind::EntityProject),
            EntityKey::Issue => Some(NodeKind::EntityIssue),
            EntityKey::Pr => Some(NodeKind::EntityPr),
            EntityKey::Agent => Some(NodeKind::EntityAgent),
            EntityKey::Run => Some(NodeKind::EntityRun),
            // thread/service are keyed dimensions without a graph entity node.
            EntityKey::Thread | EntityKey::Service => None,
        }
    }
}

/// Percent-encode one node-id segment for `|` and `%` (ADR-GRAPH rev 4: the
/// encoding is injective — no segment can inject the separator).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '|' => out.push_str("%7C"),
            _ => out.push(c),
        }
    }
    out
}

fn decode_segment(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 3 > bytes.len() {
                    return None;
                }
                let hex = &s[i + 1..i + 3];
                let b = u8::from_str_radix(hex, 16).ok()?;
                // Only the two escapes `encode_segment` ever emits are valid.
                if b != b'%' && b != b'|' {
                    return None;
                }
                out.push(b as char);
                i += 3;
            }
            b'|' => return None, // separator must not appear inside a segment
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    Some(out)
}

/// The tenant-namespaced opaque node id: `"{tenant}|{kind}|{local_id}"`.
pub fn node_id(tenant: &str, kind: NodeKind, local_id: &str) -> String {
    format!(
        "{}|{}|{}",
        encode_segment(tenant),
        encode_segment(kind.as_str()),
        encode_segment(local_id)
    )
}

/// Decode an opaque node id into `(tenant, kind, local_id)`. A malformed or
/// forged id is `None` — surfaced as `not_found` by the reads, never a leak
/// (ADR-GRAPH: "a mismatch is not_found, never a leak").
pub fn parse_node_id(id: &str) -> Option<(String, NodeKind, String)> {
    // Exactly three segments; split on the separator with a manual scan so an
    // encoded '|' inside a segment cannot forge an extra field.
    let mut segs: Vec<String> = Vec::with_capacity(3);
    let mut cur = String::new();
    for c in id.chars() {
        match c {
            '|' => {
                segs.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    segs.push(cur);
    if segs.len() != 3 {
        return None;
    }
    let tenant = decode_segment(&segs[0])?;
    let kind = NodeKind::parse(&decode_segment(&segs[1])?)?;
    let local = decode_segment(&segs[2])?;
    Some((tenant, kind, local))
}

/// The fact node id of a committed fact.
pub fn fact_node_id(tenant: &str, fact_id: Uuid) -> String {
    node_id(tenant, NodeKind::Fact, &fact_id.to_string())
}

/// The entity node id for a keyed dimension value.
pub fn entity_node_id(tenant: &str, kind: NodeKind, key_value: &str) -> String {
    node_id(tenant, kind, key_value)
}

/// The a2a counterpart hint node local id (ADR-GRAPH rev 4, deterministic):
/// `base32(SHA-256(tenant_id ‖ ":" ‖ a2a.counterpart_hint))[:26]` — fixed
/// length, non-invertible, and convergent (all facts of one tenant
/// corroborating the same hint project onto the SAME node, so the edge set is
/// replay-stable — H3).
pub fn a2a_hint_local_id(tenant: &str, counterpart_hint: &str) -> String {
    let d = Sha256::digest(format!("{tenant}:{counterpart_hint}"));
    base32_lower(&d)[..26].to_string()
}

/// RFC 4648 lowercase base32, no padding (over the 32-byte SHA-256 digest).
fn base32_lower(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for b in bytes {
        buf = (buf << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// Walk direction (§7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Out,
    In,
    Both,
}

/// Why a bounded read stopped early (ADR-GRAPH rev 2 MED-11 / rev 3 H-C /
/// rev 4 R5): `depth_clamp` = re-ask shallower; `fanout | node_budget |
/// deadline` = too dense / too slow; `orphan` = a node vanished (projection
/// hole or §5.6 age-out) — a DISTINCT code from `node_budget`, never merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncReason {
    #[default]
    None,
    DepthClamp,
    Fanout,
    NodeBudget,
    Deadline,
    Orphan,
}

impl TruncReason {
    /// Wire form (the `memkeys.graph.truncate.reason` attribute values,
    /// ADR-GRAPH rev 4 / ISI-5911).
    pub fn as_str(self) -> &'static str {
        match self {
            TruncReason::None => "none",
            TruncReason::DepthClamp => "depth_clamp",
            TruncReason::Fanout => "fanout",
            TruncReason::NodeBudget => "node_budget",
            TruncReason::Deadline => "deadline",
            TruncReason::Orphan => "orphan",
        }
    }
}

/// A node reference the caller supplies (`{kind, id}` §7.3) — the tenant comes
/// from the authenticated principal, never from the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef {
    pub kind: NodeKind,
    /// The OPAQUE tenant-namespaced node id (§7.3 `start.id`).
    pub id: String,
}

/// One projected edge, as returned by reads (topology only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub edge_type: EdgeType,
    pub event_time: DateTime<Utc>,
}

/// A node in a traversal result (rev 4 — ISI-6032/ISI-5910 R5: the schema home
/// for `Node`, incl. the per-node hydration flag F6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub kind: NodeKind,
    pub id: String,
    /// The keyed record (§5.2) when `hydrate=true` and hydration succeeded;
    /// `None` when `hydrate=false` (topology only) or `unhydratable`.
    pub record: Option<FactRecord>,
    /// Hydrate was requested but the keyed fetch failed (projection hole /
    /// §5.6 age-out, pending reconcile): topology entry KEPT, body omitted —
    /// never served stale (§7.5).
    pub unhydratable: bool,
}

/// Resume token for budget/deadline truncation (bounded reads always carry a
/// continuation path, B8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    /// Depth reached when the walk stopped.
    pub depth: u8,
    /// Node ids still unexpanded (the frontier at truncation).
    pub frontier: Vec<String>,
    /// Node ids already visited (cycle-safety across resumes).
    pub visited: Vec<String>,
}

/// §7.3 traverse request. `tenant` is server-set from the verified principal;
/// `project` is an optional intersect-only scope; `depth` clamps to
/// [`MAX_DEPTH`]; `deadline` clamps to the server maximum.
#[derive(Debug, Clone)]
pub struct TraverseReq {
    pub tenant: String,
    pub project: Option<String>,
    pub start: NodeRef,
    pub edges: Vec<EdgeType>,
    pub dir: Direction,
    pub depth: u8,
    pub deadline: Duration,
    pub hydrate: bool,
    /// Per-level fanout cap (testing knob; defaults to [`DEFAULT_FANOUT_CAP`]).
    pub fanout_cap: usize,
    /// Total node budget (testing knob; defaults to [`DEFAULT_NODE_BUDGET`]).
    pub node_budget: usize,
}

impl TraverseReq {
    pub fn new(tenant: impl Into<String>, start: NodeRef) -> Self {
        Self {
            tenant: tenant.into(),
            project: None,
            start,
            edges: Vec::new(),
            dir: Direction::Out,
            depth: 2,
            deadline: DEFAULT_DEADLINE,
            hydrate: false,
            fanout_cap: DEFAULT_FANOUT_CAP,
            node_budget: DEFAULT_NODE_BUDGET,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraverseResult {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub truncated: bool,
    pub reason: TruncReason,
    pub cursor: Option<Cursor>,
}

/// 1-hop neighborhood read (ADR-GRAPH `Links`): the SAME budget + truncation
/// contract as `Traverse` (rev 2 B8: "an unbounded slice here is the cheapest
/// DoS in the interface").
#[derive(Debug, Clone)]
pub struct LinksReq {
    pub tenant: String,
    pub project: Option<String>,
    pub node: NodeRef,
    pub edges: Vec<EdgeType>,
    pub dir: Direction,
    pub deadline: Duration,
    pub hydrate: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinksResult {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub truncated: bool,
    pub reason: TruncReason,
    pub cursor: Option<Cursor>,
}

/// A lineage member — the node id plus the keyed record when hydration
/// succeeded (`record: None` iff unhydratable; same drop-body-not-topology
/// rule as [`Node`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactRef {
    pub id: String,
    pub record: Option<FactRecord>,
}

/// A historical fork — a prior fact with >1 live successor recorded pre-rule
/// (forks are rejected at ingest since; the reconciler flags leftovers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchPoint {
    pub at: String,
    pub heads: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SupersedesReq {
    pub tenant: String,
    pub project: Option<String>,
    pub fact: NodeRef,
    pub deadline: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SupersedesResult {
    /// Historical lineage, root→head, deterministic (event_time, then id).
    pub chain: Vec<FactRef>,
    pub branches: Vec<BranchPoint>,
    /// Budget/deadline hit — NOT a projection hole (opposite of `complete`).
    pub truncated: bool,
    /// `false` => some lineage edge is deferred (a projection hole, HIGH-9) —
    /// the chain is hole-shortened, not whole.
    pub complete: bool,
    pub cursor: Option<Cursor>,
}

/// The rebuild grain (server-internal). Edges are tenant-grain by identity, so
/// a tenant-scoped rebuild can never leave an edge half-rebuilt (MED-12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub tenant: String,
    /// `None` = whole tenant; `Some(p)` = reconcile edges whose source fact is
    /// in project `p`.
    pub project: Option<String>,
}

/// What `Project` did to the projection (observability + tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProjectOutcome {
    pub edges_upserted: u64,
    /// A `supersedes` edge was DEFERRED (target sits in a projection hole —
    /// HIGH-9: never write a dangling edge to a phantom node; `Rebuild` fills
    /// it once the hole closes).
    pub supersedes_deferred: bool,
}

/// What a reconciling `Rebuild` changed (tests assert convergence with these).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildStats {
    pub inserted: u64,
    pub deleted: u64,
    /// Facts examined in the scope.
    pub facts_scanned: u64,
}

/// Read/derivation failure of the graph projection.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// Start/anchor node not owned by the caller's tenant, nonexistent, or a
    /// malformed/forged id — uniformly `not_found` (WS-5 §3.3 fail-closed:
    /// never a distinct error that would make reads an existence oracle).
    #[error("not found")]
    NotFound,
    /// Invalid request (unknown edge type, depth 0, ...).
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    /// Storage failure.
    #[error("store error: {0}")]
    Store(#[from] sqlx::Error),
}

/// The `GraphView` port (ADR-GRAPH §10): three caller-facing bounded reads +
/// two server-internal maintenance methods. The substrate (PgGraph) stays
/// behind this interface; any promotion (ADR-GRAPH promotion triggers) is an
/// adapter swap + reconciling `Rebuild` backfill — no caller change.
#[async_trait::async_trait]
pub trait GraphView: Send + Sync {
    /// §7.3 traverse: bounded level-synchronous BFS from a root node.
    /// Fails closed: a start not owned by `req.tenant` => [`GraphError::NotFound`].
    async fn traverse(&self, req: &TraverseReq) -> Result<TraverseResult, GraphError>;

    /// Supersession lineage for a fact. The CURRENT head is resolved via the
    /// §5.2 supersedes-JOIN (authoritative — the graph can never disagree with
    /// `valid=current`); the walk returns the historical lineage with branch
    /// structure preserved. `complete=false` signals a projection hole.
    async fn supersedes(&self, req: &SupersedesReq) -> Result<SupersedesResult, GraphError>;

    /// 1-hop neighborhood, optionally edge-type filtered; same budget +
    /// truncation contract as `Traverse`.
    async fn links(&self, req: &LinksReq) -> Result<LinksResult, GraphError>;

    /// Stage 5a: idempotent edge upsert from one processed fact. The
    /// `supersedes` edge is DEFERRED when its target sits in a projection
    /// hole (both-endpoints precondition — HIGH-9).
    async fn project(&self, fact: &ProcessedFact) -> Result<ProjectOutcome, GraphError>;

    /// Reconciling replay for a scope: compute the edge set the current SoT
    /// should produce, DELETE edges absent from it (stale / rule-dropped /
    /// source-expired), fill holes. Atomic per scope (shadow-build + swap:
    /// a concurrent read sees the old-consistent graph, never a half-rebuilt
    /// one — BLOCKER-2/H4).
    async fn rebuild(&self, scope: &Scope) -> Result<RebuildStats, GraphError>;
}

// ---------------------------------------------------------------------------
// The pure BFS engine (ADR-GRAPH Decision: app-side bounding)
// ---------------------------------------------------------------------------
//
// The bounds live HERE, not in SQL (rev 2 / B7: a recursive CTE's cycle guard
// is per-path and LIMIT bounds output, not work — it is not a self-bounding
// primitive). Each level is ONE set-based fetch of the frontier's out/in
// edges; the substrate supplies the fetch.

/// One raw edge row as fetched per level (substrate shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEdge {
    pub src_id: String,
    pub dst_id: String,
    pub edge_type: EdgeType,
    pub event_time: DateTime<Utc>,
}

impl RawEdge {
    /// Deterministic read order (ADR-GRAPH: "a connected BFS frontier up to
    /// the first bound hit, deterministically ordered — by event_time, then
    /// id" — so a truncated lineage never drops a middle link, M1).
    fn sort_key(&self) -> (DateTime<Utc>, &str, &str) {
        (self.event_time, &self.src_id, &self.dst_id)
    }
}

/// The per-level set fetch, substrate-supplied (one indexed query per level).
#[async_trait::async_trait]
pub(crate) trait LevelFetcher {
    async fn fetch_level(
        &self,
        frontier: &[String],
        edges: &[EdgeType],
        dir: Direction,
        project: Option<&str>,
    ) -> Result<Vec<RawEdge>, GraphError>;
}

/// Run the bounded BFS. `start` must be established by the caller (the
/// ownership check). Pure apart from the fetcher — unit-testable with a fake.
pub(crate) async fn run_bfs<F: LevelFetcher>(
    fetcher: &F,
    start: &str,
    req: &TraverseReq,
) -> Result<TraverseResult, GraphError> {
    let mut result = TraverseResult::default();
    if req.depth == 0 {
        return Err(GraphError::Invalid("depth must be >= 1"));
    }
    let effective_depth = req.depth.min(MAX_DEPTH);
    let clamped = req.depth > MAX_DEPTH;

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start.to_string());
    let mut frontier: Vec<String> = vec![start.to_string()];
    let mut emitted: HashSet<(String, String, EdgeType)> = HashSet::new();

    let start_clock = std::time::Instant::now();
    let mut stop: Option<TruncReason> = None;
    let mut levels_done: u8 = 0;
    let mut pending_frontier: Vec<String> = Vec::new();

    for _level in 0..effective_depth {
        // B3: a wall-clock deadline bounds low-fanout deep walks too.
        if start_clock.elapsed() >= req.deadline {
            stop = Some(TruncReason::Deadline);
            pending_frontier = frontier;
            break;
        }
        let mut raw = fetcher
            .fetch_level(&frontier, &req.edges, req.dir, req.project.as_deref())
            .await?;
        raw.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));

        // Candidate expansion set for this level: endpoints not yet visited,
        // in deterministic (sorted-edge) order. Capping happens BEFORE the
        // nodes join the visited set — a node beyond the bound is NOT part of
        // the returned frontier ("a connected BFS frontier up to the first
        // bound hit", M1).
        let mut candidates: Vec<String> = Vec::new();
        for edge in &raw {
            for endpoint in [&edge.src_id, &edge.dst_id] {
                if endpoint != start
                    && !visited.contains(endpoint)
                    && !candidates.contains(endpoint)
                {
                    candidates.push(endpoint.clone());
                }
            }
        }

        // Per-level fanout cap, then the running node budget (whichever
        // binds first; both are deterministic truncations).
        let remaining_budget = req.node_budget.saturating_sub(visited.len());
        if candidates.len() > req.fanout_cap {
            candidates.truncate(req.fanout_cap);
            stop = Some(TruncReason::Fanout);
        } else if candidates.len() > remaining_budget {
            candidates.truncate(remaining_budget);
            stop = Some(TruncReason::NodeBudget);
        }

        for c in &candidates {
            visited.insert(c.clone());
        }

        // Emit this level's edges, deduped across levels, and only where BOTH
        // endpoints are in the returned node set (connectivity invariant).
        for edge in &raw {
            if visited.contains(&edge.src_id)
                && visited.contains(&edge.dst_id)
                && emitted.insert((edge.src_id.clone(), edge.dst_id.clone(), edge.edge_type))
            {
                result.edges.push(Edge {
                    from: edge.src_id.clone(),
                    to: edge.dst_id.clone(),
                    edge_type: edge.edge_type,
                    event_time: edge.event_time,
                });
            }
        }

        if candidates.is_empty() || stop.is_some() {
            pending_frontier = candidates;
            break;
        }
        frontier = candidates;
        levels_done += 1;
    }

    if clamped && stop.is_none() {
        stop = Some(TruncReason::DepthClamp);
    }

    // Emit nodes in deterministic (kind, id) order — stable across equal
    // event times, matching the edge order the caller saw.
    let mut node_ids: Vec<String> = visited.into_iter().collect();
    node_ids.sort();
    result.nodes = node_ids
        .into_iter()
        .map(|id| Node {
            kind: parse_node_id(&id)
                .map(|(_, k, _)| k)
                .unwrap_or(NodeKind::Fact),
            id,
            record: None,
            unhydratable: false,
        })
        .collect();

    result.truncated = stop.is_some();
    result.reason = stop.unwrap_or_default();
    result.cursor = None;
    // A budget/deadline truncation always carries a continuation cursor (B8);
    // orphan is an aggregate hydration signal, not a resumable bound.
    if result.truncated {
        match result.reason {
            TruncReason::Fanout | TruncReason::NodeBudget | TruncReason::Deadline => {
                let mut visited_vec: Vec<String> =
                    result.nodes.iter().map(|n| n.id.clone()).collect();
                visited_vec.sort();
                result.cursor = Some(Cursor {
                    depth: levels_done,
                    frontier: pending_frontier,
                    visited: visited_vec,
                });
            }
            _ => {}
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Edge derivation (stage 5a rules — the single rule set for `Project` AND
// `Rebuild`, so a replay converges by construction)
// ---------------------------------------------------------------------------

/// One derived edge row (pre-upsert shape; `rule_ver` is stamped at write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeRow {
    pub tenant: String,
    /// Functionally determined by the SOURCE FACT (ADR rev 2 MED-10): every
    /// edge a fact derives carries that fact's `entity_project` facet.
    pub project: Option<String>,
    pub src_kind: NodeKind,
    pub src_id: String,
    pub dst_kind: NodeKind,
    pub dst_id: String,
    pub edge_type: EdgeType,
    pub event_time: DateTime<Utc>,
}

/// Derive the edge set one committed fact contributes to the projection
/// (§5.4.1 / ADR-GRAPH edge vocabulary):
/// - `entity_fact` — entity node → fact node, one per graph-projecting keyed
///   dimension (project/issue/pr/agent/run; thread/service have no graph
///   entity node);
/// - `produced_by` — fact node → agent node of `provenance.principal`
///   (always: the principal is NOT NULL on every record);
/// - `supersedes` — fact node → prior fact node (the CALLER guards the
///   HIGH-9 both-endpoints precondition before upserting);
/// - `a2a_participant` — fact node → tenant-local opaque a2a_hint node, only
///   for `a2a_exchange` facts with a persisted counterpart hint (HIGH-7: the
///   target is recorded in the caller's namespace, never a cross-tenant
///   reference).
pub(crate) fn derive_edges(fact: &ProcessedFact) -> Vec<EdgeRow> {
    let tenant = &fact.tenant_id;
    let fnode = fact_node_id(tenant, fact.id);
    let mut out = Vec::new();

    let dims: [(EntityKey, &Option<String>); 5] = [
        (EntityKey::Project, &fact.entity.project),
        (EntityKey::Issue, &fact.entity.issue),
        (EntityKey::Pr, &fact.entity.pr),
        (EntityKey::Agent, &fact.entity.agent),
        (EntityKey::Run, &fact.entity.run),
    ];
    for (key, value) in dims {
        if let (Some(kind), Some(v)) = (NodeKind::for_entity_key(key), value) {
            out.push(EdgeRow {
                tenant: tenant.clone(),
                project: fact.entity.project.clone(),
                src_kind: kind,
                src_id: entity_node_id(tenant, kind, v),
                dst_kind: NodeKind::Fact,
                dst_id: fnode.clone(),
                edge_type: EdgeType::EntityFact,
                event_time: fact.event_time,
            });
        }
    }

    out.push(EdgeRow {
        tenant: tenant.clone(),
        project: fact.entity.project.clone(),
        src_kind: NodeKind::Fact,
        src_id: fnode.clone(),
        dst_kind: NodeKind::EntityAgent,
        dst_id: entity_node_id(tenant, NodeKind::EntityAgent, &fact.provenance_principal),
        edge_type: EdgeType::ProducedBy,
        event_time: fact.event_time,
    });

    if let Some(target) = fact.supersedes {
        out.push(EdgeRow {
            tenant: tenant.clone(),
            project: fact.entity.project.clone(),
            src_kind: NodeKind::Fact,
            src_id: fnode.clone(),
            dst_kind: NodeKind::Fact,
            dst_id: fact_node_id(tenant, target),
            edge_type: EdgeType::Supersedes,
            event_time: fact.event_time,
        });
    }

    if fact.provenance_source_kind == "a2a_exchange" {
        if let Some(hint) = &fact.entity.counterpart_agent {
            out.push(EdgeRow {
                tenant: tenant.clone(),
                project: fact.entity.project.clone(),
                src_kind: NodeKind::Fact,
                src_id: fnode.clone(),
                dst_kind: NodeKind::A2aHint,
                dst_id: node_id(tenant, NodeKind::A2aHint, &a2a_hint_local_id(tenant, hint)),
                edge_type: EdgeType::A2aParticipant,
                event_time: fact.event_time,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityKeys;

    fn fact_fixture() -> ProcessedFact {
        ProcessedFact {
            id: Uuid::new_v4(),
            tenant_id: "t1".into(),
            fact_type: crate::model::FactType::Decision,
            subject: Some("s".into()),
            body: "b".into(),
            body_hash: "h".into(),
            confidence: None,
            supersedes: None,
            event_time: DateTime::parse_from_rfc3339("2026-10-17T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            ingested_at: Utc::now(),
            entity: EntityKeys::default(),
            keyed: true,
            provenance_principal: "agent-a".into(),
            provenance_source_kind: "gateway_flush".into(),
            provenance_trace_id: None,
            rejected_hints: Vec::new(),
            attachments: serde_json::json!([]),
        }
    }

    #[test]
    fn derive_edges_always_includes_produced_by() {
        let mut f = fact_fixture();
        f.entity = EntityKeys::default(); // no entity keys at all
        let e = derive_edges(&f);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].edge_type, EdgeType::ProducedBy);
        assert_eq!(e[0].src_kind, NodeKind::Fact);
        assert_eq!(e[0].dst_kind, NodeKind::EntityAgent);
        assert_eq!(e[0].src_id, fact_node_id("t1", f.id));
        assert_eq!(
            e[0].dst_id,
            entity_node_id("t1", NodeKind::EntityAgent, "agent-a")
        );
    }

    #[test]
    fn derive_edges_one_entity_fact_per_graph_dim() {
        let mut f = fact_fixture();
        f.entity.project = Some("proj".into());
        f.entity.issue = Some("iss".into());
        f.entity.thread = Some("thr".into()); // keyed dim, NO graph node
        let e = derive_edges(&f);
        assert_eq!(
            e.iter()
                .filter(|e| e.edge_type == EdgeType::EntityFact)
                .count(),
            2
        );
        // The project facet rides every derived edge (MED-10).
        assert!(e.iter().all(|e| e.project.as_deref() == Some("proj")));
    }

    #[test]
    fn derive_edges_supersedes_and_a2a_rules() {
        let target = Uuid::new_v4();
        let mut f = fact_fixture();
        f.supersedes = Some(target);
        f.provenance_source_kind = "a2a_exchange".into();
        f.entity.counterpart_agent = Some("counterpart-7".into());
        let e = derive_edges(&f);
        assert_eq!(
            e.iter()
                .find(|e| e.edge_type == EdgeType::Supersedes)
                .map(|e| e.dst_id.clone()),
            Some(fact_node_id("t1", target))
        );
        let a2a = e
            .iter()
            .find(|e| e.edge_type == EdgeType::A2aParticipant)
            .unwrap();
        assert_eq!(a2a.dst_kind, NodeKind::A2aHint);
        assert_eq!(
            a2a.dst_id,
            node_id(
                "t1",
                NodeKind::A2aHint,
                &a2a_hint_local_id("t1", "counterpart-7")
            )
        );

        // A non-a2a fact with a stray hint derives NO a2a edge.
        let mut g = fact_fixture();
        g.entity.counterpart_agent = Some("x".into());
        assert!(derive_edges(&g)
            .iter()
            .all(|e| e.edge_type != EdgeType::A2aParticipant));
    }

    #[test]
    fn node_id_round_trips() {
        for (tenant, kind, local) in [
            (
                "acme",
                NodeKind::Fact,
                "0b0e7cf4-04df-4b0c-8a5a-9bb02ea06f7c",
            ),
            ("acme", NodeKind::EntityProject, "memkeys"),
            ("a|b%c", NodeKind::EntityAgent, "ag|ent%1"),
        ] {
            let id = node_id(tenant, kind, local);
            assert_eq!(
                parse_node_id(&id),
                Some((tenant.to_string(), kind, local.to_string()))
            );
        }
    }

    #[test]
    fn node_id_separator_cannot_be_forged() {
        // A local id containing the separator stays inside its segment.
        let id = node_id("t1", NodeKind::EntityIssue, "i|ss");
        assert_eq!(parse_node_id(&id).unwrap().2, "i|ss");
        assert_ne!(
            id,
            node_id("t1", NodeKind::EntityIssue, "i").replace("i|ss", "i")
        );
        // A raw forged id with a bare 4th segment fails to parse.
        assert!(parse_node_id("t1|fact|abc|extra").is_none());
        // Invalid percent escapes fail closed.
        assert!(parse_node_id("t1|fact|ab%zz").is_none());
        assert!(parse_node_id("t1|fact|ab%2F").is_none()); // '/' is not an emitted escape
    }

    #[test]
    fn distinct_tenants_never_alias() {
        let a = node_id("tenant-a", NodeKind::EntityProject, "proj");
        let b = node_id("tenant-a|proj", NodeKind::Fact, ""); // adversarial concat
        assert_ne!(a, b);
        assert_ne!(
            node_id("a", NodeKind::EntityIssue, "x"),
            node_id("a", NodeKind::EntityPr, "x")
        );
    }

    #[test]
    fn a2a_hint_local_id_is_deterministic_and_convergent() {
        let x = a2a_hint_local_id("t1", "counterpart-7");
        assert_eq!(x.len(), 26);
        assert_eq!(x, a2a_hint_local_id("t1", "counterpart-7"));
        assert_ne!(x, a2a_hint_local_id("t2", "counterpart-7"));
        assert_ne!(x, a2a_hint_local_id("t1", "counterpart-8"));
        // RFC 4648 lowercase alphabet only.
        assert!(x
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    // --- BFS engine bounds (pure; fake fetcher) ---

    struct FakeGraph {
        edges: Vec<RawEdge>,
    }

    #[async_trait::async_trait]
    impl LevelFetcher for FakeGraph {
        async fn fetch_level(
            &self,
            frontier: &[String],
            _edges: &[EdgeType],
            _dir: Direction,
            _project: Option<&str>,
        ) -> Result<Vec<RawEdge>, GraphError> {
            Ok(self
                .edges
                .iter()
                .filter(|e| frontier.contains(&e.src_id) || frontier.contains(&e.dst_id))
                .cloned()
                .collect())
        }
    }

    fn edge(src: &str, dst: &str) -> RawEdge {
        RawEdge {
            src_id: src.to_string(),
            dst_id: dst.to_string(),
            edge_type: EdgeType::Supersedes,
            event_time: chrono::TimeZone::timestamp_opt(&chrono::Utc, 1_700_000_000, 0).unwrap(),
        }
    }

    fn req(depth: u8, fanout: usize, budget: usize) -> TraverseReq {
        TraverseReq {
            tenant: "t".into(),
            project: None,
            start: NodeRef {
                kind: NodeKind::Fact,
                id: "t|fact|a".into(),
            },
            edges: vec![],
            dir: Direction::Both,
            depth,
            deadline: DEFAULT_DEADLINE,
            hydrate: false,
            fanout_cap: fanout,
            node_budget: budget,
        }
    }

    #[tokio::test]
    async fn bfs_walks_chain_and_terminates() {
        // a -> b -> c -> a (cycle) — the visited set terminates the walk.
        let g = FakeGraph {
            edges: vec![
                edge("t|fact|a", "t|fact|b"),
                edge("t|fact|b", "t|fact|c"),
                edge("t|fact|c", "t|fact|a"),
            ],
        };
        let r = run_bfs(&g, "t|fact|a", &req(5, 256, 1024)).await.unwrap();
        assert!(!r.truncated);
        assert_eq!(r.reason, TruncReason::None);
        assert_eq!(r.nodes.len(), 3);
        // Edges dedupe across levels even when re-seen from the far endpoint.
        assert_eq!(r.edges.len(), 3);
    }

    #[tokio::test]
    async fn bfs_depth_clamp_beyond_max() {
        let g = FakeGraph {
            edges: vec![edge("t|fact|a", "t|fact|b")],
        };
        let r = run_bfs(&g, "t|fact|a", &req(9, 256, 1024)).await.unwrap();
        assert!(r.truncated);
        assert_eq!(r.reason, TruncReason::DepthClamp);
    }

    #[tokio::test]
    async fn bfs_fanout_cap_truncates_deterministically() {
        let g = FakeGraph {
            edges: vec![
                edge("t|fact|a", "t|fact|b"),
                edge("t|fact|a", "t|fact|c"),
                edge("t|fact|a", "t|fact|d"),
            ],
        };
        let r = run_bfs(&g, "t|fact|a", &req(2, 2, 1024)).await.unwrap();
        assert!(r.truncated);
        assert_eq!(r.reason, TruncReason::Fanout);
        // start + 2 capped frontier nodes; the dropped node is absent from
        // BOTH the node set and the edge set (connectivity, M1).
        assert_eq!(r.nodes.len(), 3);
        assert_eq!(r.edges.len(), 2);
        assert!(r.nodes.iter().all(|n| n.id != "t|fact|d"));
    }

    #[tokio::test]
    async fn bfs_node_budget_truncates() {
        let g = FakeGraph {
            edges: vec![edge("t|fact|a", "t|fact|b"), edge("t|fact|b", "t|fact|c")],
        };
        let r = run_bfs(&g, "t|fact|a", &req(5, 256, 2)).await.unwrap();
        assert!(r.truncated);
        assert_eq!(r.reason, TruncReason::NodeBudget);
    }

    #[tokio::test]
    async fn bfs_deadline_truncates() {
        let g = FakeGraph {
            edges: vec![edge("t|fact|a", "t|fact|b")],
        };
        let mut q = req(5, 256, 1024);
        q.deadline = Duration::from_nanos(1);
        let r = run_bfs(&g, "t|fact|a", &q).await.unwrap();
        assert!(r.truncated);
        assert_eq!(r.reason, TruncReason::Deadline);
    }

    #[tokio::test]
    async fn bfs_zero_depth_is_rejected() {
        let g = FakeGraph { edges: vec![] };
        assert!(matches!(
            run_bfs(&g, "t|fact|a", &req(0, 256, 1024)).await,
            Err(GraphError::Invalid(_))
        ));
    }
}
