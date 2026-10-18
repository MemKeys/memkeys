//! Stage 5a — the graph-projection worker (ADR-GRAPH §10 / §6 stage 5a).
//!
//! Derived-view maintenance is ASYNC off the hot path: the ingest `ack` is
//! gated on stage 4 only (§6; R2 — a projection failure never blocks stages
//! 1-4). The worker owns a bounded in-process queue; durability of projection
//! convergence is NOT the queue's job — the reconciler/backfill (M7) and this
//! worker's [`GraphProjectionWorker::catch_up`] scan close holes by replaying
//! the SoT, so a lost queue item costs freshness, never correctness (D3).
//!
//! OTel (WS-7 signal catalog): each apply runs as its own trace root
//! `memkeys.worker.process` (CONSUMER) carrying a SPAN_LINK to the originating
//! `memkeys.commit` span context — temporally decoupled, not a parent edge.
//! On apply it records `memkeys.projection.lag` (the D3 observable-lag
//! invariant) and maintains the `memkeys.worker.queue.depth` /
//! `memkeys.poison.queue.depth` (dead-letter) and
//! `memkeys.graph.edges.cardinality` gauges.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use opentelemetry::trace::{Span, SpanContext, SpanKind};
use uuid::Uuid;

use crate::graph::{GraphError, GraphView};
use crate::model::ProcessedFact;
use crate::pg_graph::PgGraph;
use crate::telemetry::{self, Instruments};

/// One queued projection work item: the committed fact identity + the span
/// context of the commit that produced it (for the CONSUMER link).
/// One queued projection work item: the committed fact identity + the span
/// context of the commit that produced it (for the CONSUMER link). Shared by
/// the graph / vector (M5) and LLM pipeline (M6) workers.
#[derive(Debug, Clone)]
pub(crate) struct WorkItem {
    pub(crate) tenant: String,
    pub(crate) fact_id: Uuid,
    pub(crate) commit_span: Option<SpanContext>,
}

/// The stage-5a worker. Clone-safe handle (the queue is shared).
#[derive(Clone)]
pub struct GraphProjectionWorker {
    graph: Arc<PgGraph>,
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    notify: Arc<tokio::sync::Notify>,
    instruments: Arc<Instruments>,
    poison: Arc<Mutex<std::collections::HashMap<String, i64>>>,
}

impl std::fmt::Debug for GraphProjectionWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphProjectionWorker").finish()
    }
}

impl GraphProjectionWorker {
    /// The underlying graph store pool (maintenance scans).
    pub fn graph_pool(&self) -> &sqlx::PgPool {
        self.graph.pool()
    }

    pub fn new(graph: Arc<PgGraph>) -> Self {
        Self {
            graph,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(tokio::sync::Notify::new()),
            instruments: Arc::new(Instruments::new()),
            poison: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Enqueue one committed fact (called by the service AFTER the stage-4
    /// ack decision — never on the ack path itself). The commit span context
    /// becomes the CONSUMER link (WS-7).
    pub fn enqueue(&self, tenant: &str, fact_id: Uuid, commit_span: Option<SpanContext>) {
        let mut q = self.queue.lock().expect("projection queue lock");
        q.push_back(WorkItem {
            tenant: tenant.to_string(),
            fact_id,
            commit_span,
        });
        let depth = q.len();
        drop(q);
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_GRAPH_PROJECTION, depth);
        self.notify.notify_one();
    }

    /// Apply every queued item now (tests + the binary's periodic pass). A
    /// transient failure is retried once (re-queued); a second failure
    /// dead-letters into the poison gauge (reconciler/M7 territory reaps it).
    pub async fn drain(&self) -> usize {
        let mut applied = 0usize;
        let mut retries: Vec<WorkItem> = Vec::new();
        loop {
            let item = {
                self.queue
                    .lock()
                    .expect("projection queue lock")
                    .pop_front()
            };
            let Some(item) = item else { break };
            match self.apply(&item, true).await {
                Ok(()) => applied += 1,
                Err(_) => retries.push(item),
            }
        }
        // One retry round for transient failures.
        let mut poison_now: Vec<WorkItem> = Vec::new();
        for item in retries {
            if self.apply(&item, true).await.is_err() {
                poison_now.push(item);
            } else {
                applied += 1;
            }
        }
        for item in poison_now {
            self.dead_letter(&item);
        }
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_GRAPH_PROJECTION, 0);
        applied
    }

    /// The SoT convergence scan: project committed facts that have no
    /// `entity_fact` edge yet (projection holes — lost queue items, deferred
    /// supersedes edges whose target later landed, drift). This is the
    /// "projection worker builds the graph projection from FactStore" path.
    pub async fn catch_up(&self, tenant: &str) -> Result<usize, GraphError> {
        let facts = self.graph.unprojected_facts(tenant).await?;
        let n = facts.len();
        for fact in &facts {
            self.apply_fact(fact, None).await?;
        }
        Ok(n)
    }

    /// Ingest time of the OLDEST outstanding graph-projection hole for a tenant
    /// (the reconciler's `memkeys.projection.lag.current` input; M7). Reuses the
    /// same hole scan as [`catch_up`](Self::catch_up) — the graph tier is
    /// deterministic (no model stamp; edges carry a structural `rule_ver`), so a
    /// hole is strictly a missing/deferred edge. `None` when converged.
    pub async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, GraphError> {
        let facts = self.graph.unprojected_facts(tenant).await?;
        Ok(facts.iter().map(|f| f.ingested_at).min())
    }

    async fn apply(&self, item: &WorkItem, _retryable: bool) -> Result<(), GraphError> {
        let fact = fetch_fact(self.graph.as_ref(), &item.tenant, item.fact_id).await?;
        let Some(fact) = fact else {
            // The fact vanished (§5.6 retention sweep racing the queue):
            // nothing to project — not an error, not a poison item.
            return Ok(());
        };
        self.apply_fact(&fact, item.commit_span.as_ref()).await
    }

    async fn apply_fact(
        &self,
        fact: &ProcessedFact,
        commit_span: Option<&SpanContext>,
    ) -> Result<(), GraphError> {
        // WS-7: own trace root, CONSUMER kind, LINKED (not parented) to the
        // originating commit span.
        let mut span = telemetry::start_span_with_links(
            "memkeys.worker.process",
            SpanKind::Consumer,
            vec![
                telemetry::attr("memkeys.tenant.id", fact.tenant_id.clone()),
                telemetry::attr(
                    "memkeys.worker.kind",
                    telemetry::WORKER_KIND_GRAPH_PROJECTION,
                ),
                telemetry::attr("memkeys.projection.kind", telemetry::PROJECTION_KIND_GRAPH),
            ],
            commit_span,
        );
        let outcome = self.graph.project(fact).await;
        match &outcome {
            Ok(_) => {
                span.set_status(opentelemetry::trace::Status::Ok);
                // D3 observable-lag invariant: apply_time - commit_time.
                self.instruments.projection_lag(
                    telemetry::PROJECTION_KIND_GRAPH,
                    lag_seconds(fact.ingested_at),
                );
            }
            Err(e) => {
                span.set_status(opentelemetry::trace::Status::Error {
                    description: e.to_string().into(),
                });
            }
        }
        span.end();
        outcome.map(|_| ())
    }

    fn dead_letter(&self, item: &WorkItem) {
        let key = item.tenant.clone();
        let mut p = self.poison.lock().expect("poison lock");
        let n = p.entry(key).or_insert(0);
        *n += 1;
        let v = *n;
        drop(p);
        self.instruments
            .poison_queue_depth(telemetry::WORKER_KIND_GRAPH_PROJECTION, v);
    }
}

fn lag_seconds(ingested_at: DateTime<Utc>) -> f64 {
    // Wall-clock lag: ingest stamp is DB time; apply is now.
    let now = Utc::now();
    (now - ingested_at).num_milliseconds() as f64 / 1000.0
}

/// Fetch one committed fact (full derivation shape) within the tenant floor.
async fn fetch_fact(
    graph: &PgGraph,
    tenant: &str,
    fact_id: Uuid,
) -> Result<Option<ProcessedFact>, GraphError> {
    let row: Option<FactRow> = sqlx::query_as(
        "SELECT id, tenant_id, event_time, ingested_at, entity_project, entity_issue, \
         entity_pr, entity_agent, entity_run, entity_thread, entity_service, \
         counterpart_agent, provenance_principal, provenance_source_kind, supersedes \
         FROM facts WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(fact_id)
    .fetch_optional(graph.pool())
    .await?;
    Ok(row.map(|r| r.into()))
}

#[derive(Debug, sqlx::FromRow)]
struct FactRow {
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

impl From<FactRow> for ProcessedFact {
    fn from(r: FactRow) -> Self {
        // Body-bearing fields are irrelevant to edge derivation (the graph
        // never carries body text); everything the rules read is restored.
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

// ===========================================================================
// Stage 5b — the vector-projection worker (§6 stage 5b; M5, ISI-6079/ISI-6167).
// ===========================================================================
//
// Mirrors [`GraphProjectionWorker`]: a bounded in-process queue, enqueued by
// the service AFTER the stage-4 ack (OFF the ack path — §6/R2: a projection
// failure never blocks stages 1-4), gated on `memory.semantic`. Unlike the
// graph worker it reads the fact BODY (to embed it) — but only the embedding +
// keys are ever written to the index (§5.4.2 R1: the body stays in the SoT).
// Idempotent (upsert on fact_id) with a [`VectorProjectionWorker::catch_up`]
// SoT scan that closes holes (lost queue items, disabled→re-enabled, drift).
//
// OTel (WS-7): each apply is its own trace root `memkeys.worker.process`
// (CONSUMER) LINKED to the commit span, recording `memkeys.projection.lag` and
// the `memkeys.worker.queue.depth` / `memkeys.poison.queue.depth` gauges under
// `WORKER_KIND_VECTOR_PROJECTION` / `PROJECTION_KIND_VECTOR`.

use crate::semantic::{Embedder, FactVector, VectorError, VectorIndex};
use opentelemetry::trace::Status;
use sqlx::PgPool;

/// The stage-5b worker. Clone-safe handle (the queue/index are shared).
#[derive(Clone)]
pub struct VectorProjectionWorker {
    pool: PgPool,
    embedder: Arc<dyn Embedder>,
    index: Arc<dyn VectorIndex>,
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    notify: Arc<tokio::sync::Notify>,
    instruments: Arc<Instruments>,
    poison: Arc<Mutex<std::collections::HashMap<String, i64>>>,
}

impl std::fmt::Debug for VectorProjectionWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorProjectionWorker").finish()
    }
}

impl VectorProjectionWorker {
    pub fn new(pool: PgPool, embedder: Arc<dyn Embedder>, index: Arc<dyn VectorIndex>) -> Self {
        Self {
            pool,
            embedder,
            index,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(tokio::sync::Notify::new()),
            instruments: Arc::new(Instruments::new()),
            poison: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Enqueue one committed fact (called by the service AFTER the stage-4 ack
    /// decision — never on the ack path itself).
    pub fn enqueue(&self, tenant: &str, fact_id: Uuid, commit_span: Option<SpanContext>) {
        let mut q = self.queue.lock().expect("vector queue lock");
        q.push_back(WorkItem {
            tenant: tenant.to_string(),
            fact_id,
            commit_span,
        });
        let depth = q.len();
        drop(q);
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_VECTOR_PROJECTION, depth);
        self.notify.notify_one();
    }

    /// Apply every queued item now (tests + the binary's periodic pass). One
    /// retry round for transient failures; a second failure dead-letters.
    pub async fn drain(&self) -> usize {
        let mut applied = 0usize;
        let mut retries: Vec<WorkItem> = Vec::new();
        loop {
            let item = { self.queue.lock().expect("vector queue lock").pop_front() };
            let Some(item) = item else { break };
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(_) => retries.push(item),
            }
        }
        let mut poison_now: Vec<WorkItem> = Vec::new();
        for item in retries {
            if self.apply(&item).await.is_err() {
                poison_now.push(item);
            } else {
                applied += 1;
            }
        }
        for item in poison_now {
            self.dead_letter(&item);
        }
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_VECTOR_PROJECTION, 0);
        applied
    }

    /// SoT convergence scan: committed facts with no row in `fact_vectors` yet
    /// (holes — lost queue items, module just-enabled, drift) OR one embedded
    /// under a STALE embedder build (`v.model_id <> active` — M7: an embedding
    /// model change is a re-embed/backfill event, mirroring the LLM tiers'
    /// `(model_id, prompt_version)` staleness; ISI-6081). The projection is
    /// built FROM the FactStore, same contract as the graph worker's catch_up;
    /// the upsert overwrites the stale vector + its stamp in place, so a model
    /// change rebuilds with no downtime (D3 — a stale vector costs freshness,
    /// never correctness).
    pub async fn catch_up(&self, tenant: &str) -> Result<usize, VectorError> {
        let model_id = self.embedder.model_id();
        let ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT f.id FROM facts f \
             LEFT JOIN fact_vectors v ON v.fact_id = f.id AND v.tenant_id = f.tenant_id \
             WHERE f.tenant_id = $1 AND (v.fact_id IS NULL OR v.model_id <> $2) \
             ORDER BY f.ingested_at, f.id",
        )
        .bind(tenant)
        .bind(model_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VectorError::Backend(e.to_string()))?;
        let n = ids.len();
        for (id,) in ids {
            if let Some(row) = self.fetch_fact_body(tenant, id).await? {
                self.apply_fact(&row, None).await?;
            }
        }
        Ok(n)
    }

    /// Ingest time of the OLDEST outstanding stage-5b hole/stale vector for a
    /// tenant (the reconciler's `memkeys.projection.lag.current` input — the
    /// age of the longest-un-applied committed fact for this tier; M7). `None`
    /// when the projection is converged (no holes, no stale embeddings).
    pub async fn oldest_hole(&self, tenant: &str) -> Result<Option<DateTime<Utc>>, VectorError> {
        let row: Option<(Option<DateTime<Utc>>,)> = sqlx::query_as(
            "SELECT min(f.ingested_at) FROM facts f \
             LEFT JOIN fact_vectors v ON v.fact_id = f.id AND v.tenant_id = f.tenant_id \
             WHERE f.tenant_id = $1 AND (v.fact_id IS NULL OR v.model_id <> $2)",
        )
        .bind(tenant)
        .bind(self.embedder.model_id())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VectorError::Backend(e.to_string()))?;
        Ok(row.and_then(|(ts,)| ts))
    }

    async fn apply(&self, item: &WorkItem) -> Result<(), VectorError> {
        let Some(row) = self.fetch_fact_body(&item.tenant, item.fact_id).await? else {
            // The fact vanished (retention sweep racing the queue): nothing to
            // project — not an error, not a poison item.
            return Ok(());
        };
        self.apply_fact(&row, item.commit_span.as_ref()).await
    }

    async fn apply_fact(
        &self,
        row: &FactBodyRow,
        commit_span: Option<&SpanContext>,
    ) -> Result<(), VectorError> {
        let mut span = telemetry::start_span_with_links(
            "memkeys.worker.process",
            SpanKind::Consumer,
            vec![
                telemetry::attr("memkeys.tenant.id", row.tenant_id.clone()),
                telemetry::attr(
                    "memkeys.worker.kind",
                    telemetry::WORKER_KIND_VECTOR_PROJECTION,
                ),
                telemetry::attr("memkeys.projection.kind", telemetry::PROJECTION_KIND_VECTOR),
            ],
            commit_span,
        );
        let outcome = self.embed_and_upsert(row).await;
        match &outcome {
            Ok(()) => {
                span.set_status(Status::Ok);
                self.instruments.projection_lag(
                    telemetry::PROJECTION_KIND_VECTOR,
                    lag_seconds(row.ingested_at),
                );
            }
            Err(e) => span.set_status(Status::Error {
                description: e.to_string().into(),
            }),
        }
        span.end();
        outcome
    }

    async fn embed_and_upsert(&self, row: &FactBodyRow) -> Result<(), VectorError> {
        let embedding = self
            .embedder
            .embed(&row.body)
            .await
            .map_err(|e| VectorError::Backend(e.to_string()))?;
        self.index
            .upsert(&FactVector {
                fact_id: row.id,
                tenant_id: row.tenant_id.clone(),
                embedding,
                fact_type: row.fact_type.clone(),
                entity: crate::model::EntityKeys {
                    project: row.entity_project.clone(),
                    issue: row.entity_issue.clone(),
                    pr: row.entity_pr.clone(),
                    agent: row.entity_agent.clone(),
                    run: row.entity_run.clone(),
                    thread: row.entity_thread.clone(),
                    service: row.entity_service.clone(),
                    counterpart_agent: row.counterpart_agent.clone(),
                },
                ingested_at: row.ingested_at,
                // M7: stamp the active embedder build so the reconciler can
                // detect a stale embedding and re-embed on a model change.
                model_id: self.embedder.model_id().to_string(),
            })
            .await
    }

    async fn fetch_fact_body(
        &self,
        tenant: &str,
        fact_id: Uuid,
    ) -> Result<Option<FactBodyRow>, VectorError> {
        sqlx::query_as::<_, FactBodyRow>(
            "SELECT id, tenant_id, body, fact_type, ingested_at, entity_project, entity_issue, \
             entity_pr, entity_agent, entity_run, entity_thread, entity_service, counterpart_agent \
             FROM facts WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(fact_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VectorError::Backend(e.to_string()))
    }

    fn dead_letter(&self, item: &WorkItem) {
        let mut p = self.poison.lock().expect("poison lock");
        let n = p.entry(item.tenant.clone()).or_insert(0);
        *n += 1;
        let v = *n;
        drop(p);
        self.instruments
            .poison_queue_depth(telemetry::WORKER_KIND_VECTOR_PROJECTION, v);
    }
}

/// The body-bearing fact row the stage-5b worker embeds (the ONE place body
/// text is read for the vector tier; it never leaves this worker — only the
/// embedding + keys reach the index, §5.4.2 R1).
#[derive(Debug, sqlx::FromRow)]
struct FactBodyRow {
    id: Uuid,
    tenant_id: String,
    body: String,
    fact_type: String,
    ingested_at: DateTime<Utc>,
    entity_project: Option<String>,
    entity_issue: Option<String>,
    entity_pr: Option<String>,
    entity_agent: Option<String>,
    entity_run: Option<String>,
    entity_thread: Option<String>,
    entity_service: Option<String>,
    counterpart_agent: Option<String>,
}
