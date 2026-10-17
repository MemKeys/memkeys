//! Postgres `GraphView` (ADR-GRAPH §10): relational edge-rows co-located with
//! the SoT store, traversed by the app-side bounded BFS (graph.rs).
//!
//! Per-level fetch shape (ADR-GRAPH rev 2 / B7): the bounds live app-side — a
//! recursive CTE's cycle guard is per-path and LIMIT bounds output, not work —
//! so each BFS level is ONE set-based fetch (`WHERE src_id = ANY(:frontier)` /
//! `dst_id = ANY(:frontier)`, the ADR's sanctioned relational form). Recursive
//! CTEs appear where they are the right relational tool: the supersession
//! lineage walk (`supersedes`) — the issue's "recursive-CTE walk" — and the
//! reconciling derivation (`rebuild`), which replays the SoT change-log.
//!
//! Fail-closed discipline (WS-5 §3.3): a start node that is cross-tenant,
//! nonexistent, malformed, or kind-mismatched is uniformly `not_found`.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::graph::{
    derive_edges, parse_node_id, Direction, EdgeType, GraphError, GraphView, LinksReq, LinksResult,
    Node, NodeKind, NodeRef, ProjectOutcome, RawEdge, RebuildStats, Scope, SupersedesReq,
    SupersedesResult, TraverseReq, TraverseResult, TruncReason, MAX_CHAIN, RULE_VER,
};
use crate::model::ProcessedFact;
use crate::telemetry::{self, Instruments};
use opentelemetry::trace::Span;

/// A connected Postgres graph projection over the same pool as the FactStore.
#[derive(Debug, Clone)]
pub struct PgGraph {
    pool: PgPool,
    instruments: Arc<Instruments>,
}

impl PgGraph {
    /// Wrap an existing (already migrated) pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            instruments: Arc::new(Instruments::new()),
        }
    }

    /// Connect, apply migrations, wrap.
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self::from_pool(pool))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The projection-hole scan (worker catch-up): committed facts with no
    /// `entity_fact` edge yet. The full reconciler/backfill is M7; this is the
    /// worker's convergence path ("projection worker builds the graph
    /// projection from FactStore") and the test hook for rebuild semantics.
    pub async fn unprojected_facts(&self, tenant: &str) -> Result<Vec<ProcessedFact>, GraphError> {
        // Node ids are computed in Rust (the encoding is injective but not
        // cheaply recomputable in SQL); the membership checks are two queries.
        let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, supersedes FROM facts WHERE tenant_id = $1 ORDER BY ingested_at, id",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await?;
        let node_ids: Vec<String> = rows
            .iter()
            .map(|(id, _)| crate::graph::fact_node_id(tenant, *id))
            .collect();
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let projected: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT dst_id FROM edges \
             WHERE tenant = $1 AND edge_type = 'entity_fact' AND dst_id = ANY($2)",
        )
        .bind(tenant)
        .bind(&node_ids)
        .fetch_all(&self.pool)
        .await?;
        let projected: HashSet<String> = projected.into_iter().map(|(s,)| s).collect();
        let supersedes_edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT src_id, dst_id FROM edges \
             WHERE tenant = $1 AND edge_type = 'supersedes' AND src_id = ANY($2)",
        )
        .bind(tenant)
        .bind(&node_ids)
        .fetch_all(&self.pool)
        .await?;
        let supersedes_edges: HashSet<(String, String)> = supersedes_edges.into_iter().collect();

        // A fact needs (re-)projection when it has NO entity_fact edge (a
        // hole) OR when a SoT supersedes pointer's edge is missing (a HIGH-9
        // DEFERRED edge whose target has since landed — replaying Project
        // writes it).
        let missing: Vec<Uuid> = rows
            .into_iter()
            .zip(node_ids)
            .filter(|((_, target), node)| {
                if !projected.contains(node) {
                    return true;
                }
                match target {
                    Some(t) => {
                        let expected = (node.clone(), crate::graph::fact_node_id(tenant, *t));
                        !supersedes_edges.contains(&expected)
                    }
                    None => false,
                }
            })
            .map(|((id, _), _)| id)
            .collect();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        let facts: Vec<GraphFactRow> = sqlx::query_as(
            "SELECT id, tenant_id, event_time, ingested_at, entity_project, entity_issue, \
             entity_pr, entity_agent, entity_run, entity_thread, entity_service, \
             counterpart_agent, provenance_principal, provenance_source_kind, supersedes \
             FROM facts WHERE tenant_id = $1 AND id = ANY($2) ORDER BY ingested_at, id",
        )
        .bind(tenant)
        .bind(&missing)
        .fetch_all(&self.pool)
        .await?;
        Ok(facts.into_iter().map(Into::into).collect())
    }

    /// Ownership + presence check: the node must have at least one incident
    /// edge within the tenant (nodes materialize through their edges — there
    /// is no node table). Cross-tenant / nonexistent / forged => NotFound.
    async fn require_node(&self, tenant: &str, node: &NodeRef) -> Result<(), GraphError> {
        let parsed = parse_node_id(&node.id).ok_or(GraphError::NotFound)?;
        let (node_tenant, kind, _local) = parsed;
        if node_tenant != tenant || kind != node.kind {
            // A mismatch is not_found, never a leak (ADR-GRAPH node-id scheme).
            return Err(GraphError::NotFound);
        }
        let present: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM edges WHERE tenant = $1 AND (src_id = $2 OR dst_id = $2) LIMIT 1",
        )
        .bind(tenant)
        .bind(&node.id)
        .fetch_optional(&self.pool)
        .await?;
        if present.is_some() {
            Ok(())
        } else {
            Err(GraphError::NotFound)
        }
    }

    /// The per-level set-based fetch (ADR-GRAPH: `WHERE (src|dst)_id =
    /// ANY(:frontier)` — one indexed query per BFS level).
    async fn fetch_level(
        &self,
        tenant: &str,
        frontier: &[String],
        edges: &[EdgeType],
        dir: crate::graph::Direction,
        project: Option<&str>,
    ) -> Result<Vec<RawEdge>, GraphError> {
        let mut sql = String::from(
            "SELECT src_id, dst_id, edge_type, event_time FROM edges WHERE tenant = $1",
        );
        match dir {
            Direction::Out => sql.push_str(" AND src_id = ANY($2)"),
            Direction::In => sql.push_str(" AND dst_id = ANY($2)"),
            Direction::Both => sql.push_str(" AND (src_id = ANY($2) OR dst_id = ANY($2))"),
        }
        let mut next_param = 3usize;
        if project.is_some() {
            sql.push_str(&format!(" AND project IS NOT DISTINCT FROM ${next_param}"));
            next_param += 1;
        }
        if !edges.is_empty() {
            sql.push_str(&format!(" AND edge_type = ANY(${next_param})"));
        }
        sql.push_str(" ORDER BY event_time, src_id, dst_id");

        let mut q = sqlx::query_as::<_, RawEdgeRow>(&sql)
            .bind(tenant)
            .bind(frontier);
        if let Some(p) = project {
            q = q.bind(p);
        }
        if !edges.is_empty() {
            let names: Vec<String> = edges.iter().map(|e| e.as_str().to_string()).collect();
            q = q.bind(names);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|r| RawEdge {
                src_id: r.src_id,
                dst_id: r.dst_id,
                edge_type: EdgeType::parse(&r.edge_type).unwrap_or(EdgeType::EntityFact),
                event_time: r.event_time,
            })
            .collect())
    }

    /// Hydrate fact nodes of a walk from the keyed store (the single body
    /// authority, §7.5). Missing bodies stay in the topology as
    /// `unhydratable` (drop-body-not-topology — never served stale).
    async fn hydrate_nodes(&self, tenant: &str, nodes: &mut [Node]) -> Result<bool, GraphError> {
        let fact_ids: Vec<Uuid> = nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Fact)
            .filter_map(|n| parse_node_id(&n.id))
            .filter(|(t, k, _)| *t == tenant && *k == NodeKind::Fact)
            .filter_map(|(_, _, local)| Uuid::parse_str(&local).ok())
            .collect();
        if fact_ids.is_empty() {
            return Ok(false);
        }
        let records: Vec<crate::model::FactRecord> = sqlx::query_as(&format!(
            "SELECT {} FROM facts WHERE tenant_id = $1 AND id = ANY($2)",
            crate::pg::FACT_COLUMNS
        ))
        .bind(tenant)
        .bind(&fact_ids)
        .fetch_all(&self.pool)
        .await?;
        let by_id: std::collections::HashMap<Uuid, crate::model::FactRecord> =
            records.into_iter().map(|r| (r.id, r)).collect();
        let mut any_orphan = false;
        for n in nodes.iter_mut() {
            if n.kind != NodeKind::Fact {
                continue;
            }
            let local = parse_node_id(&n.id).map(|(_, _, l)| l);
            let Some(uuid) = local.as_deref().and_then(|s| Uuid::parse_str(s).ok()) else {
                continue;
            };
            match by_id.get(&uuid) {
                Some(rec) => n.record = Some(rec.clone()),
                None => {
                    n.unhydratable = true;
                    any_orphan = true;
                }
            }
        }
        Ok(any_orphan)
    }

    /// Fetch the raw rows of a fact + its SoT supersedes pointer set (used by
    /// the completeness check of `supersedes`).
    async fn sot_supersedes_pointers(
        &self,
        tenant: &str,
        fact_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Option<Uuid>)>, GraphError> {
        let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, supersedes FROM facts WHERE tenant_id = $1 AND id = ANY($2)",
        )
        .bind(tenant)
        .bind(fact_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

/// Raw edge row shape (sqlx).
#[derive(Debug, sqlx::FromRow)]
struct RawEdgeRow {
    src_id: String,
    dst_id: String,
    edge_type: String,
    event_time: DateTime<Utc>,
}

#[async_trait::async_trait]
impl GraphView for PgGraph {
    async fn traverse(&self, req: &TraverseReq) -> Result<TraverseResult, GraphError> {
        if req.depth == 0 {
            return Err(GraphError::Invalid("depth must be >= 1"));
        }
        let started = std::time::Instant::now();
        self.require_node(&req.tenant, &req.start).await?;

        let mut store_span = telemetry::start_span(
            "memkeys.store.query",
            opentelemetry::trace::SpanKind::Client,
            telemetry::db_attrs_edges("select"),
        );
        let tenant = req.tenant.clone();
        let fetcher = LevelFetcherAdapter {
            graph: self,
            tenant: &tenant,
        };
        let walk = crate::graph::run_bfs(&fetcher, &req.start.id, req).await;
        store_span.end();
        let mut result = walk?;

        // Hydration is a SECOND store pass (§7.5: bodies come from the keyed
        // store, never from the graph).
        if req.hydrate {
            let orphaned = self.hydrate_nodes(&req.tenant, &mut result.nodes).await?;
            if orphaned && result.reason == TruncReason::None {
                // Rev 4 R5: orphan is its own aggregate reason — never merged
                // into node_budget ("too big").
                result.reason = TruncReason::Orphan;
            }
        }

        // WS-7 catalog: memkeys.graph.traverse.latency at the GraphView call
        // site, bucketed by the post-clamp depth the walk was budgeted for.
        let depth_reached = req.depth.min(crate::graph::MAX_DEPTH);
        self.instruments
            .graph_traverse_latency(depth_reached, started.elapsed().as_secs_f64());
        self.instruments
            .candidate_set_size(telemetry::RETRIEVAL_OP_GRAPH, result.nodes.len() as u64);
        Ok(result)
    }

    async fn supersedes(&self, req: &SupersedesReq) -> Result<SupersedesResult, GraphError> {
        // Fail-closed: a fact anchor not owned by the tenant => NotFound.
        self.require_node(&req.tenant, &req.fact).await?;
        let tenant = req.tenant.clone();
        let start = req.fact.id.clone();

        // The recursive-CTE lineage walk: UNION (not ALL) is the cycle guard
        // and dedup; the depth bound is the chain budget (MAX_CHAIN).
        let rows: Vec<(String,)> = sqlx::query_as(
            "WITH RECURSIVE \
               anc(node_id, depth) AS ( \
                 SELECT $2::text, 0 \
                 UNION \
                 SELECT e.dst_id, a.depth + 1 FROM anc a \
                   JOIN edges e ON e.tenant = $1 AND e.src_id = a.node_id \
                              AND e.edge_type = 'supersedes' \
                 WHERE a.depth < $3 \
               ), \
               des(node_id, depth) AS ( \
                 SELECT $2::text, 0 \
                 UNION \
                 SELECT e.src_id, d.depth + 1 FROM des d \
                   JOIN edges e ON e.tenant = $1 AND e.dst_id = d.node_id \
                              AND e.edge_type = 'supersedes' \
                 WHERE d.depth < $3 \
               ) \
             SELECT node_id FROM anc WHERE depth > 0 \
             UNION \
             SELECT node_id FROM des WHERE depth > 0",
        )
        .bind(&tenant)
        .bind(&start)
        .bind(MAX_CHAIN as i64)
        .fetch_all(&self.pool)
        .await?;
        let truncated = rows.len() >= MAX_CHAIN;

        // Chain = anchor + walked members, root→head, deterministic
        // (event_time, then id) — a truncated lineage never drops a middle
        // link (M1).
        let mut members: Vec<String> = vec![start.clone()];
        members.extend(rows.into_iter().map(|(s,)| s));
        members.sort();
        members.dedup();

        let fact_ids: Vec<Uuid> = members
            .iter()
            .filter_map(|id| parse_node_id(id))
            .filter(|(t, k, _)| *t == tenant && *k == NodeKind::Fact)
            .filter_map(|(_, _, local)| Uuid::parse_str(&local).ok())
            .collect();
        let records: Vec<crate::model::FactRecord> = sqlx::query_as(&format!(
            "SELECT {} FROM facts WHERE tenant_id = $1 AND id = ANY($2)",
            crate::pg::FACT_COLUMNS
        ))
        .bind(&tenant)
        .bind(&fact_ids)
        .fetch_all(&self.pool)
        .await?;
        let by_id: std::collections::HashMap<Uuid, crate::model::FactRecord> =
            records.into_iter().map(|r| (r.id, r)).collect();

        let mut chain: Vec<crate::graph::FactRef> = Vec::new();
        for id in &members {
            let uuid = parse_node_id(id).and_then(|(_, _, l)| Uuid::parse_str(&l).ok());
            let record = uuid.and_then(|u| by_id.get(&u).cloned());
            chain.push(crate::graph::FactRef {
                id: id.clone(),
                record,
            });
        }
        chain.sort_by(|a, b| {
            let ta = a.record.as_ref().map(|r| (r.event_time, r.id));
            let tb = b.record.as_ref().map(|r| (r.event_time, r.id));
            match (ta, tb) {
                (Some(x), Some(y)) => x.cmp(&y),
                _ => a.id.cmp(&b.id),
            }
        });

        // Historical forks (pre-rule legacy): chain members with >1 successor.
        let mut edges_among: Vec<(String, String)> = sqlx::query_as(
            "SELECT src_id, dst_id FROM edges \
             WHERE tenant = $1 AND edge_type = 'supersedes' \
               AND (src_id = ANY($2) OR dst_id = ANY($2))",
        )
        .bind(&tenant)
        .bind(&members)
        .fetch_all(&self.pool)
        .await?;
        edges_among.sort();
        let mut by_target: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (src, dst) in &edges_among {
            by_target.entry(dst.clone()).or_default().push(src.clone());
        }
        let branches: Vec<crate::graph::BranchPoint> = by_target
            .into_iter()
            .filter(|(_, heads)| heads.len() > 1)
            .map(|(at, mut heads)| {
                heads.sort();
                crate::graph::BranchPoint { at, heads }
            })
            .collect();

        // Completeness (H-D): compare the graph's supersedes edges against the
        // SoT pointers — a hole-shortened chain must not masquerade as whole.
        let mut complete = true;
        let pointers = self.sot_supersedes_pointers(&tenant, &fact_ids).await?;
        let graph_edges: HashSet<(String, String)> = edges_among.into_iter().collect();
        for (id, target) in &pointers {
            if let Some(target) = target {
                let expected = (
                    crate::graph::fact_node_id(&tenant, *id),
                    crate::graph::fact_node_id(&tenant, *target),
                );
                if !graph_edges.contains(&expected) {
                    complete = false;
                    break;
                }
            }
        }

        Ok(SupersedesResult {
            chain,
            branches,
            truncated,
            complete,
            cursor: None,
        })
    }

    async fn links(&self, req: &LinksReq) -> Result<LinksResult, GraphError> {
        self.require_node(&req.tenant, &req.node).await?;
        // 1-hop = the BFS machinery with depth 1 and the caller's limit as the
        // budget family (same truncation contract, B8).
        let t = TraverseReq {
            tenant: req.tenant.clone(),
            project: req.project.clone(),
            start: req.node.clone(),
            edges: req.edges.clone(),
            dir: req.dir,
            depth: 1,
            deadline: req.deadline,
            hydrate: req.hydrate,
            fanout_cap: req.limit,
            node_budget: req.limit.saturating_add(1),
        };
        let r = self.traverse(&t).await?;
        Ok(LinksResult {
            nodes: r.nodes,
            edges: r.edges,
            truncated: r.truncated,
            reason: r.reason,
            cursor: r.cursor,
        })
    }

    /// Stage 5a — idempotent edge upsert from one committed fact. All edges
    /// derive from the same `derive_edges` used by `Rebuild` (one rule set —
    /// replay converges by construction).
    async fn project(&self, fact: &ProcessedFact) -> Result<ProjectOutcome, GraphError> {
        let mut derived = derive_edges(fact);

        // HIGH-9: a supersedes edge whose target sits in a projection hole is
        // DEFERRED to Rebuild — never a dangling edge to a phantom node.
        let mut outcome = ProjectOutcome::default();
        if let Some(pos) = derived
            .iter()
            .position(|e| e.edge_type == EdgeType::Supersedes)
        {
            let target_node = derived[pos].dst_id.clone();
            let projected: Option<(i32,)> = sqlx::query_as(
                "SELECT 1 FROM edges \
                 WHERE tenant = $1 AND dst_id = $2 AND edge_type = 'entity_fact' LIMIT 1",
            )
            .bind(&fact.tenant_id)
            .bind(&target_node)
            .fetch_optional(&self.pool)
            .await?;
            if projected.is_none() {
                derived.remove(pos);
                outcome.supersedes_deferred = true;
            }
        }

        let inserted = upsert_edges(&self.pool, &derived).await?;
        outcome.edges_upserted = inserted;

        // Async-observed promotion-trigger gauge (WS-7 F11): maintained at
        // derivation time, never on the read path.
        self.observe_edge_cardinality(&fact.tenant_id).await?;
        Ok(outcome)
    }

    /// Reconciling replay (ADR-GRAPH BLOCKER-2/H4): derive the expected edge
    /// set from the CURRENT SoT for the scope, then swap atomically in one
    /// transaction — a concurrent read sees the old-consistent graph or the
    /// new-consistent one, never a half-rebuilt one (shadow-build + swap).
    async fn rebuild(&self, scope: &Scope) -> Result<RebuildStats, GraphError> {
        let mut tx = self.pool.begin().await?;

        let rows: Vec<GraphFactRow> = match &scope.project {
            None => {
                sqlx::query_as::<_, GraphFactRow>(
                    "SELECT id, tenant_id, event_time, ingested_at, entity_project, \
                     entity_issue, entity_pr, entity_agent, entity_run, entity_thread, \
                     entity_service, counterpart_agent, provenance_principal, \
                     provenance_source_kind, supersedes \
                     FROM facts WHERE tenant_id = $1 ORDER BY ingested_at, id",
                )
                .bind(&scope.tenant)
                .fetch_all(&mut *tx)
                .await?
            }
            Some(p) => {
                sqlx::query_as::<_, GraphFactRow>(
                    "SELECT id, tenant_id, event_time, ingested_at, entity_project, \
                     entity_issue, entity_pr, entity_agent, entity_run, entity_thread, \
                     entity_service, counterpart_agent, provenance_principal, \
                     provenance_source_kind, supersedes \
                     FROM facts WHERE tenant_id = $1 AND entity_project = $2 ORDER BY ingested_at, id",
                )
                .bind(&scope.tenant)
                .bind(p)
                .fetch_all(&mut *tx)
                .await?
            }
        };
        let facts: Vec<ProcessedFact> = rows.into_iter().map(Into::into).collect();
        let mut expected: Vec<crate::graph::EdgeRow> = Vec::new();
        for f in &facts {
            expected.extend(derive_edges(f));
        }

        // Scope delete: project Some(p) evicts only that project's facet
        // (MED-12); project None evicts the WHOLE tenant — including stale
        // project-carrying edges, so tenant rebuild stays reconciling
        // (ADR-GRAPH "Rebuild determinism & eviction", BLOCKER-2).
        let deleted = sqlx::query(
            "DELETE FROM edges WHERE tenant = $1 \
             AND ($2::text IS NULL OR project IS NOT DISTINCT FROM $2)",
        )
        .bind(&scope.tenant)
        .bind(&scope.project)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        let mut inserted = 0u64;
        if !expected.is_empty() {
            inserted = upsert_edges_tx(&mut tx, &expected).await?;
        }
        tx.commit().await?;

        self.observe_edge_cardinality(&scope.tenant).await?;
        Ok(RebuildStats {
            inserted,
            deleted,
            facts_scanned: facts.len() as u64,
        })
    }
}

impl PgGraph {
    /// `memkeys.graph.edges.cardinality` (WS-7 F11, promotion trigger #3):
    /// observed at maintenance time — never on the read path.
    pub async fn observe_edge_cardinality(&self, tenant: &str) -> Result<(), GraphError> {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM edges WHERE tenant = $1")
            .bind(tenant)
            .fetch_one(&self.pool)
            .await?;
        self.instruments.graph_edges_cardinality(tenant, n as u64);
        Ok(())
    }
}

/// Adapter: the substrate's per-level fetch through the engine's trait.
struct LevelFetcherAdapter<'a> {
    graph: &'a PgGraph,
    tenant: &'a str,
}

#[async_trait::async_trait]
impl crate::graph::LevelFetcher for LevelFetcherAdapter<'_> {
    async fn fetch_level(
        &self,
        frontier: &[String],
        edges: &[EdgeType],
        dir: Direction,
        project: Option<&str>,
    ) -> Result<Vec<RawEdge>, GraphError> {
        self.graph
            .fetch_level(self.tenant, frontier, edges, dir, project)
            .await
    }
}

/// The minimal fact projection row used by the reconciling derivation.
#[derive(Debug, sqlx::FromRow)]
struct GraphFactRow {
    id: Uuid,
    tenant_id: String,
    event_time: DateTime<Utc>,
    ingested_at: DateTime<Utc>,
    entity_project: Option<String>,
    entity_issue: Option<String>,
    entity_pr: Option<String>,
    entity_agent: Option<String>,
    entity_run: Option<String>,
    entity_thread: Option<String>,
    entity_service: Option<String>,
    counterpart_agent: Option<String>,
    provenance_principal: String,
    provenance_source_kind: String,
    supersedes: Option<Uuid>,
}

impl From<GraphFactRow> for ProcessedFact {
    fn from(r: GraphFactRow) -> Self {
        // Body-bearing fields are irrelevant to edge derivation; the shape is
        // restored faithfully where the derivation reads it.
        ProcessedFact {
            id: r.id,
            tenant_id: r.tenant_id,
            fact_type: crate::model::FactType::Observation,
            subject: None,
            body: String::new(),
            body_hash: String::new(),
            confidence: None,
            supersedes: r.supersedes,
            event_time: r.event_time,
            ingested_at: r.ingested_at,
            entity: crate::model::EntityKeys {
                project: r.entity_project,
                issue: r.entity_issue,
                pr: r.entity_pr,
                agent: r.entity_agent,
                run: r.entity_run,
                thread: r.entity_thread,
                service: r.entity_service,
                counterpart_agent: r.counterpart_agent,
            },
            keyed: true,
            provenance_principal: r.provenance_principal,
            provenance_source_kind: r.provenance_source_kind,
            provenance_trace_id: None,
            rejected_hints: Vec::new(),
            attachments: serde_json::Value::Array(Vec::new()),
        }
    }
}

/// Batch idempotent upsert (`ON CONFLICT DO NOTHING` on the PK — replay-safe,
/// D3). Shared by `Project` and `Rebuild`.
async fn upsert_edges(pool: &PgPool, edges: &[crate::graph::EdgeRow]) -> Result<u64, GraphError> {
    let mut tx = pool.begin().await?;
    let n = upsert_edges_tx(&mut tx, edges).await?;
    tx.commit().await?;
    Ok(n)
}

async fn upsert_edges_tx(
    tx: &mut sqlx::PgConnection,
    edges: &[crate::graph::EdgeRow],
) -> Result<u64, GraphError> {
    if edges.is_empty() {
        return Ok(0);
    }
    let tenants: Vec<&str> = edges.iter().map(|e| e.tenant.as_str()).collect();
    let projects: Vec<Option<&str>> = edges.iter().map(|e| e.project.as_deref()).collect();
    let src_kinds: Vec<&str> = edges.iter().map(|e| e.src_kind.as_str()).collect();
    let src_ids: Vec<&str> = edges.iter().map(|e| e.src_id.as_str()).collect();
    let dst_kinds: Vec<&str> = edges.iter().map(|e| e.dst_kind.as_str()).collect();
    let dst_ids: Vec<&str> = edges.iter().map(|e| e.dst_id.as_str()).collect();
    let types: Vec<&str> = edges.iter().map(|e| e.edge_type.as_str()).collect();
    let times: Vec<DateTime<Utc>> = edges.iter().map(|e| e.event_time).collect();
    let rule_vers: Vec<&str> = edges.iter().map(|_| RULE_VER).collect();

    let n = sqlx::query(
        "INSERT INTO edges \
           (tenant, project, src_kind, src_id, dst_kind, dst_id, edge_type, event_time, rule_ver) \
         SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], \
                              $5::text[], $6::text[], $7::text[], $8::timestamptz[], $9::text[]) \
         ON CONFLICT (tenant, src_id, dst_id, edge_type) DO NOTHING",
    )
    .bind(tenants)
    .bind(projects)
    .bind(src_kinds)
    .bind(src_ids)
    .bind(dst_kinds)
    .bind(dst_ids)
    .bind(types)
    .bind(times)
    .bind(rule_vers)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ver_is_pinned() {
        // Structural edges are rule-derived; the version must be explicit.
        assert_eq!(RULE_VER, "adr-graph-r4");
    }
}
