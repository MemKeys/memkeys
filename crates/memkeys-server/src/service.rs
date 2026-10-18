//! The MemKeys service: orchestrates pipeline stages 1-4 with OTel
//! instrumentation, serves op 1 (fetch-by-keys, §7.2), and fronts the M2
//! graph projection (stage-5a worker dispatch + the bounded graph reads
//! behind `GraphView`).

use std::sync::Arc;
use std::time::Instant;

use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::graph::{
    GraphError, GraphView, LinksReq, LinksResult, SupersedesReq, SupersedesResult, TraverseReq,
    TraverseResult,
};
use crate::model::{EntityKey, FactRecord, FactType, KeyedQuery, ProcessedFact, Validity};
use crate::pg_graph::PgGraph;
use crate::pipeline::{
    key_stage, normalize, temporalize, CallerContext, EntityResolver, NoopResolver,
};
use crate::semantic::{
    rrf_fuse, CandidateSet, Embedder, FusedHit, FusionSource, RankedList, VectorIndex,
};
use crate::store::{CommitOutcome, FactStore};
use crate::telemetry::{self, Instruments};
use crate::worker::{GraphProjectionWorker, VectorProjectionWorker};
use opentelemetry::trace::{Span, SpanKind, Status, TraceContextExt};

/// Response of `POST /memory/facts` (§7.1 write-with-keys): the fact id, the
/// keyed flag, and the server-resolved entity keys so the caller can cache
/// them for subsequent keyed reads.
#[derive(Debug, Clone)]
pub struct IngestReceipt {
    pub fact_id: uuid::Uuid,
    pub keyed: bool,
    /// `(entity.* wire name, resolved value)` pairs (§7.1).
    pub entity_keys_resolved: Vec<(&'static str, String)>,
    /// True when this ingest was a retried flush that resolved to an existing
    /// row (no duplicate stored).
    pub duplicate: bool,
}

/// The M1 service. One instance serves many tenants — the tenant is a per-call
/// attribute (WS-4 Invariant T), never process state.
pub struct MemKeysService {
    store: Arc<dyn FactStore>,
    resolver: Arc<dyn EntityResolver>,
    instruments: Instruments,
    /// The M2 graph projection (None = the minimal keyed-only deployment —
    /// the graph is always-on per §5.4.1, but the SERVICE degrades cleanly
    /// when no projection is wired in unit contexts).
    graph: Option<Arc<dyn GraphView>>,
    worker: Option<Arc<GraphProjectionWorker>>,
    /// M5 semantic module (§5.4.2/§7.4). `None` = `memory.semantic` off → the
    /// minimal keyed+graph deployment; op-4 then returns `feature_disabled`
    /// (§7.4) rather than a silent empty result. Both are present together:
    /// the embedder produces query/fact vectors, the index stores+ranks them.
    embedder: Option<Arc<dyn Embedder>>,
    vector: Option<Arc<dyn VectorIndex>>,
    /// M5 stage-5b vector-projection worker (write path). `None` in unit
    /// contexts (no pg pool) — tests seed the index directly. Enqueued OFF the
    /// stage-4 ack path, alongside the stage-5a graph worker (§6/R2).
    vector_worker: Option<Arc<VectorProjectionWorker>>,
}

impl MemKeysService {
    pub fn new(store: Arc<dyn FactStore>) -> Self {
        Self::with_resolver(store, Arc::new(NoopResolver))
    }

    pub fn with_resolver(store: Arc<dyn FactStore>, resolver: Arc<dyn EntityResolver>) -> Self {
        Self {
            store,
            resolver,
            instruments: Instruments::new(),
            graph: None,
            worker: None,
            embedder: None,
            vector: None,
            vector_worker: None,
        }
    }

    /// M2: wire the graph projection. The stage-5a worker is created against
    /// the same Postgres pool; every successful stage-4 commit is enqueued to
    /// it OFF the ack path (§6: the receipt is gated on stage 4 only).
    pub fn with_graph_projection(
        store: Arc<dyn FactStore>,
        resolver: Arc<dyn EntityResolver>,
        graph: Arc<PgGraph>,
    ) -> Self {
        let worker = Arc::new(GraphProjectionWorker::new(graph.clone()));
        Self {
            store,
            resolver,
            instruments: Instruments::new(),
            graph: Some(graph),
            worker: Some(worker),
            embedder: None,
            vector: None,
            vector_worker: None,
        }
    }

    /// M5: enable the semantic module (`memory.semantic` on). Fluent: wire the
    /// embedder + vector index onto an existing service. The fact tier is
    /// always present when the module is on (§5.4.2 R1); the digest sub-flag is
    /// a follow-up. The stage-5b vector-projection worker is wired separately
    /// against the same index.
    pub fn with_semantic(
        mut self,
        embedder: Arc<dyn Embedder>,
        vector: Arc<dyn VectorIndex>,
    ) -> Self {
        self.embedder = Some(embedder);
        self.vector = Some(vector);
        self
    }

    /// M5: wire the stage-5b vector-projection worker (write path). Fluent;
    /// requires [`with_semantic`](Self::with_semantic) to also be set so the
    /// read path is coherent with the index the worker populates. The worker is
    /// enqueued OFF the stage-4 ack path (§6/R2).
    pub fn with_semantic_worker(mut self, worker: Arc<VectorProjectionWorker>) -> Self {
        self.vector_worker = Some(worker);
        self
    }

    /// The wired stage-5a worker (drain/catch-up control for the binary and
    /// tests). `None` when no graph projection is configured.
    pub fn projection_worker(&self) -> Option<Arc<GraphProjectionWorker>> {
        self.worker.clone()
    }

    /// The wired stage-5b vector-projection worker (drain/catch-up control).
    /// `None` when the semantic module is off or in a unit context.
    pub fn vector_worker(&self) -> Option<Arc<VectorProjectionWorker>> {
        self.vector_worker.clone()
    }

    /// Write path (§6): stages 1-4 as one trace, `memkeys.fact.ingest`
    /// (SERVER) → normalize/key/temporalize (INTERNAL) → `memkeys.commit`
    /// (CLIENT, db.*). The receipt — the "ack" — is gated on stage 4 only.
    pub async fn ingest(
        &self,
        envelope: &Envelope,
        caller: &CallerContext,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<IngestReceipt, IngestError> {
        let ingest_span = telemetry::start_span(
            "memkeys.fact.ingest",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller.tenant_id.clone()),
                telemetry::attr("memkeys.entity.kind", anchor_kind(envelope)),
            ],
        );

        // Explicit parent context (not thread-local attach) so the stage
        // spans parent correctly across `.await` boundaries without making
        // the pipeline future `!Send`.
        let cx = opentelemetry::Context::current_with_span(ingest_span);

        let outcome = self.run_pipeline(envelope, caller, now, &cx).await;
        // Attach the ingest span around the sync outcome handling so the
        // bridge log record auto-carries this trace id (no awaits follow).
        let _active = cx.clone().attach();
        let ingest = cx.span();
        match &outcome {
            Ok(_) => {
                ingest.set_status(Status::Ok);
                self.instruments
                    .write_operation(anchor_kind(envelope), telemetry::OUTCOME_SUCCESS);
            }
            Err(IngestError::Rejected { reason }) => {
                // Signal-catalog key event: templated body only — the free-form
                // reject reason is response data, never a log body (deny-list);
                // the bounded reason CODE is the catalog's "with reason enum".
                tracing::warn!(target: "memkeys", "memkeys.write.rejected reason={}", rejection_code(reason));
                self.instruments
                    .write_operation(anchor_kind(envelope), telemetry::OUTCOME_REJECTED);
            }
            Err(_) => {
                self.instruments
                    .write_operation(anchor_kind(envelope), telemetry::OUTCOME_ERROR);
            }
        }
        ingest.end();
        outcome
    }

    async fn run_pipeline(
        &self,
        envelope: &Envelope,
        caller: &CallerContext,
        now: chrono::DateTime<chrono::Utc>,
        parent: &opentelemetry::Context,
    ) -> Result<IngestReceipt, IngestError> {
        // Stage 1 — normalize.
        let t = Instant::now();
        let mut fact = stage_span(telemetry::PIPELINE_STAGE_NORMALIZE, parent, || {
            normalize(envelope, caller, now)
        })?;
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_NORMALIZE,
            telemetry::OUTCOME_SUCCESS,
            t.elapsed().as_secs_f64(),
        );

        // Stage 2 — key. The span wraps the async stage body (enrichment
        // pivots are resolver lookups), so its duration is the real stage time.
        let t = Instant::now();
        let key_result = key_stage_span(anchor_kind(envelope), parent, async {
            key_stage(envelope, caller, &mut fact, self.resolver.as_ref()).await
        })
        .await;
        let key_outcome = match &key_result {
            Ok(_) => telemetry::OUTCOME_SUCCESS,
            Err(IngestError::Rejected { .. }) => telemetry::OUTCOME_REJECTED,
            Err(_) => telemetry::OUTCOME_ERROR,
        };
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_KEY,
            key_outcome,
            t.elapsed().as_secs_f64(),
        );
        key_result?;

        // Stage 3 — temporalize (+ the supersedes-target store check that
        // validates the target shares (entity, subject, type)).
        let t = Instant::now();
        stage_span(telemetry::PIPELINE_STAGE_TEMPORALIZE, parent, || {
            temporalize(&mut fact)
        })?;
        if let Some(target_id) = fact.supersedes {
            let target = self.store.get_fact(&caller.tenant_id, target_id).await?;
            let target = target.ok_or_else(|| IngestError::Rejected {
                reason: "supersedes target fact not found".into(),
            })?;
            validate_supersedes_target(&fact, &target)?;
        }
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_TEMPORALIZE,
            telemetry::OUTCOME_SUCCESS,
            t.elapsed().as_secs_f64(),
        );

        // Stage 4 — commit (the ack point).
        let t = Instant::now();
        let mut commit_span = telemetry::start_child_span(
            "memkeys.commit",
            SpanKind::Client,
            telemetry::db_attrs("insert"),
            parent,
        );
        let commit_result = self.store.commit_fact(&fact).await;
        let commit_outcome = match &commit_result {
            Ok(CommitOutcome::Inserted) => telemetry::OUTCOME_SUCCESS,
            Ok(CommitOutcome::Duplicate) => telemetry::OUTCOME_DUPLICATE,
            Err(IngestError::Rejected { .. }) => telemetry::OUTCOME_REJECTED,
            Err(_) => telemetry::OUTCOME_ERROR,
        };
        if let Err(e) = &commit_result {
            record_error(&mut commit_span, format!("{e}"));
        }
        commit_span.set_status(match &commit_result {
            Ok(_) => Status::Ok,
            Err(_) => Status::Error {
                description: "commit failed".into(),
            },
        });
        commit_span.end();
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_COMMIT,
            commit_outcome,
            t.elapsed().as_secs_f64(),
        );
        let commit_result = commit_result.map_err(|e| {
            parent.span().set_status(Status::Error {
                description: format!("{e}").into(),
            });
            e
        })?;

        // Stage 5a dispatch (async, OFF the ack path — §6/R2): the receipt is
        // already decided above; a projection hiccup can never fail the write.
        if commit_result == CommitOutcome::Inserted {
            if let Some(worker) = &self.worker {
                worker.enqueue(
                    &caller.tenant_id,
                    fact.id,
                    Some(commit_span.span_context().clone()),
                );
            }
            // Stage 5b dispatch (async, OFF the ack path — §6/R2): embed the
            // fact body + upsert the vector tier. Gated on `memory.semantic`
            // (the worker is only wired when the module is on).
            if let Some(vworker) = &self.vector_worker {
                vworker.enqueue(
                    &caller.tenant_id,
                    fact.id,
                    Some(commit_span.span_context().clone()),
                );
            }
        }

        let keyed = fact.keyed;
        Ok(IngestReceipt {
            fact_id: fact.id,
            keyed,
            entity_keys_resolved: fact
                .entity
                .resolved()
                .into_iter()
                .map(|(k, v)| (k, v.to_string()))
                .collect(),
            duplicate: commit_result == CommitOutcome::Duplicate,
        })
    }

    /// Operation 1 — fetch-by-keys (§7.2). Requires ≥1 entity key (ANDed);
    /// `valid=current` (default) collapses supersession at read time. One
    /// trace: `memkeys.retrieve` (SERVER) → `memkeys.store.query` (CLIENT).
    pub async fn fetch_by_keys(
        &self,
        caller_tenant: &str,
        query: &KeyedQuery,
    ) -> Result<Vec<crate::model::FactRecord>, QueryError> {
        if query.keys.is_empty() {
            return Err(QueryError::Invalid {
                reason: "at least one entity.* key is required".into(),
            });
        }
        if query.limit <= 0 || query.limit > crate::model::KeyedQuery::MAX_LIMIT {
            return Err(QueryError::Invalid {
                reason: format!("limit must be in 1..={}", KeyedQuery::MAX_LIMIT),
            });
        }

        let retrieve_span = telemetry::start_span(
            "memkeys.retrieve",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller_tenant.to_string()),
                telemetry::attr("memkeys.retrieval.op", telemetry::RETRIEVAL_OP_KEYED),
            ],
        );
        let t = Instant::now();
        let cx = opentelemetry::Context::current_with_span(retrieve_span);
        let mut store_span = telemetry::start_child_span(
            "memkeys.store.query",
            SpanKind::Client,
            telemetry::db_attrs("select"),
            &cx,
        );
        let retrieve = cx.span();
        let result = self.store.fetch_by_keys(caller_tenant, query).await;
        match &result {
            Ok(rows) => {
                store_span.set_status(Status::Ok);
                retrieve.set_status(Status::Ok);
                self.instruments
                    .candidate_set_size(telemetry::RETRIEVAL_OP_KEYED, rows.len() as u64);
            }
            Err(e) => {
                record_error(&mut store_span, format!("{e}"));
                retrieve.set_status(Status::Error {
                    description: format!("{e}").into(),
                });
                store_span.set_status(Status::Error {
                    description: "query failed".into(),
                });
            }
        }
        store_span.end();
        let secs = t.elapsed().as_secs_f64();
        let outcome = if result.is_ok() {
            telemetry::OUTCOME_SUCCESS
        } else {
            telemetry::OUTCOME_ERROR
        };
        self.instruments
            .retrieve_duration(telemetry::RETRIEVAL_OP_KEYED, outcome, secs);
        retrieve.end();
        result
    }
}

/// The M2 graph reads: one `memkeys.retrieve` (SERVER, op=graph) trace each,
/// delegated to `GraphView` (the substrate records the store-query span and
/// `memkeys.graph.traverse.latency` at its call site, WS-7/ISI-5911). The
/// tenant is SERVER-SET from the verified caller and overwrites any body
/// value — WS-4 Invariant T / ADR-GRAPH tenant grain.
impl MemKeysService {
    fn require_graph(&self) -> Result<&Arc<dyn GraphView>, GraphError> {
        self.graph
            .as_ref()
            .ok_or(GraphError::Invalid("graph projection is not configured"))
    }

    pub async fn traverse(
        &self,
        caller_tenant: &str,
        req: &TraverseReq,
    ) -> Result<TraverseResult, GraphError> {
        let graph = self.require_graph()?;
        let mut req = req.clone();
        req.tenant = caller_tenant.to_string();

        let mut span = telemetry::start_span(
            "memkeys.retrieve",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller_tenant.to_string()),
                telemetry::attr("memkeys.retrieval.op", telemetry::RETRIEVAL_OP_GRAPH),
            ],
        );
        let t = std::time::Instant::now();
        let result = graph.traverse(&req).await;
        match &result {
            Ok(r) => {
                if r.truncated {
                    span.set_attribute(telemetry::attr(
                        "memkeys.graph.truncate.reason",
                        r.reason.as_str(),
                    ));
                }
                span.set_status(Status::Ok);
                self.instruments
                    .candidate_set_size(telemetry::RETRIEVAL_OP_GRAPH, r.nodes.len() as u64);
            }
            Err(_) => span.set_status(Status::Error {
                description: "graph traverse failed".into(),
            }),
        }
        let outcome = if result.is_ok() {
            telemetry::OUTCOME_SUCCESS
        } else {
            telemetry::OUTCOME_ERROR
        };
        self.instruments.retrieve_duration(
            telemetry::RETRIEVAL_OP_GRAPH,
            outcome,
            t.elapsed().as_secs_f64(),
        );
        span.end();
        result
    }

    pub async fn supersedes(
        &self,
        caller_tenant: &str,
        req: &SupersedesReq,
    ) -> Result<SupersedesResult, GraphError> {
        let graph = self.require_graph()?;
        let mut req = req.clone();
        req.tenant = caller_tenant.to_string();

        let mut span = telemetry::start_span(
            "memkeys.retrieve",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller_tenant.to_string()),
                telemetry::attr("memkeys.retrieval.op", telemetry::RETRIEVAL_OP_GRAPH),
            ],
        );
        let t = std::time::Instant::now();
        let result = graph.supersedes(&req).await;
        match &result {
            Ok(_) => span.set_status(Status::Ok),
            Err(_) => span.set_status(Status::Error {
                description: "graph supersedes failed".into(),
            }),
        }
        let outcome = if result.is_ok() {
            telemetry::OUTCOME_SUCCESS
        } else {
            telemetry::OUTCOME_ERROR
        };
        self.instruments.retrieve_duration(
            telemetry::RETRIEVAL_OP_GRAPH,
            outcome,
            t.elapsed().as_secs_f64(),
        );
        span.end();
        result
    }

    pub async fn links(
        &self,
        caller_tenant: &str,
        req: &LinksReq,
    ) -> Result<LinksResult, GraphError> {
        let graph = self.require_graph()?;
        let mut req = req.clone();
        req.tenant = caller_tenant.to_string();

        let mut span = telemetry::start_span(
            "memkeys.retrieve",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller_tenant.to_string()),
                telemetry::attr("memkeys.retrieval.op", telemetry::RETRIEVAL_OP_GRAPH),
            ],
        );
        let t = std::time::Instant::now();
        let result = graph.links(&req).await;
        match &result {
            Ok(_) => span.set_status(Status::Ok),
            Err(_) => span.set_status(Status::Error {
                description: "graph links failed".into(),
            }),
        }
        let outcome = if result.is_ok() {
            telemetry::OUTCOME_SUCCESS
        } else {
            telemetry::OUTCOME_ERROR
        };
        self.instruments.retrieve_duration(
            telemetry::RETRIEVAL_OP_GRAPH,
            outcome,
            t.elapsed().as_secs_f64(),
        );
        span.end();
        result
    }
}

/// Op-4 semantic-search request (§7.4). `scope` is the caller's finer
/// `entity.*` filters, ANDed and translated to a candidate id set; empty scope
/// runs against the whole tenant floor (R6 — `unkeyed` facts included). The
/// tenant floor itself is the authenticated caller's and is never in this
/// struct (§7.4/R6: "never supplied by, nor omittable by, the client").
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub scope: Vec<(EntityKey, String)>,
    pub fact_type: Option<FactType>,
    pub k: usize,
    pub as_of: Option<chrono::DateTime<chrono::Utc>>,
}

impl SearchRequest {
    /// Clamp (§7.4 has no unbounded search): default/ceiling on `k`.
    pub const DEFAULT_K: usize = 10;
    pub const MAX_K: usize = 100;
}

/// One semantic hit (§7.4 response): the authoritative record (hydrated from
/// the FactStore — R1), its fused/similarity score, and `via` provenance (which
/// path surfaced it).
#[derive(Debug, Clone)]
pub struct SemanticHit {
    pub record: FactRecord,
    pub score: f64,
    pub via: Vec<FusionSource>,
}

impl SemanticHit {
    /// The §7.4 response-shape JSON for one hit — the authoritative record
    /// (hydrated from the SoT, R1) + score + `via` provenance. Shared verbatim
    /// by the HTTP and MCP transports (WS-5 §2.3 "byte-for-byte the same").
    pub fn to_wire_json(&self) -> serde_json::Value {
        let entity: serde_json::Map<String, serde_json::Value> = self
            .record
            .entity_keys()
            .resolved()
            .into_iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect();
        serde_json::json!({
            "fact.id": self.record.id,
            "score": self.score,
            "fact.type": self.record.fact_type,
            "fact.subject": self.record.subject,
            "fact.body": self.record.body,
            "entity": entity,
            "event_time": self.record.event_time,
            "provenance": self.record.provenance_principal,
            "via": self.via.first().map(|s| s.as_str()).unwrap_or("fact"),
        })
    }
}

/// M5 retrieval: op-4 semantic search (§7.4) and RRF fusion across the keyed +
/// semantic paths (§7.4 step 3). The tenant floor is SERVER-SET from the
/// verified caller on every call (WS-4 Invariant T / §7.4 R6).
impl MemKeysService {
    /// True when the `memory.semantic` module is wired (embedder + index).
    pub fn semantic_enabled(&self) -> bool {
        self.embedder.is_some() && self.vector.is_some()
    }

    /// Op-4 — semantic search (§7.4). scope → candidate id set → ANN rank (fact
    /// tier) → hydrate authoritative records (R1). Returns
    /// [`QueryError::FeatureDisabled`] (→ 501, §7.4) when the module is off, so
    /// callers degrade gracefully rather than mistaking "disabled" for "empty".
    pub async fn semantic_search(
        &self,
        caller_tenant: &str,
        req: &SearchRequest,
    ) -> Result<Vec<SemanticHit>, QueryError> {
        let (embedder, vector) = match (&self.embedder, &self.vector) {
            (Some(e), Some(v)) => (e, v),
            _ => {
                return Err(QueryError::FeatureDisabled {
                    feature: "memory.semantic",
                })
            }
        };
        let k = req.k.clamp(1, SearchRequest::MAX_K);

        let mut span = telemetry::start_span(
            "memkeys.retrieve",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller_tenant.to_string()),
                telemetry::attr("memkeys.retrieval.op", telemetry::RETRIEVAL_OP_SEMANTIC),
            ],
        );
        let t = Instant::now();
        let result = self
            .semantic_search_inner(caller_tenant, req, k, embedder.as_ref(), vector.as_ref())
            .await;
        match &result {
            Ok(hits) => {
                span.set_status(Status::Ok);
                self.instruments
                    .candidate_set_size(telemetry::RETRIEVAL_OP_SEMANTIC, hits.len() as u64);
            }
            Err(_) => span.set_status(Status::Error {
                description: "semantic search failed".into(),
            }),
        }
        let outcome = if result.is_ok() {
            telemetry::OUTCOME_SUCCESS
        } else {
            telemetry::OUTCOME_ERROR
        };
        self.instruments.retrieve_duration(
            telemetry::RETRIEVAL_OP_SEMANTIC,
            outcome,
            t.elapsed().as_secs_f64(),
        );
        span.end();
        result
    }

    async fn semantic_search_inner(
        &self,
        caller_tenant: &str,
        req: &SearchRequest,
        k: usize,
        embedder: &dyn Embedder,
        vector: &dyn VectorIndex,
    ) -> Result<Vec<SemanticHit>, QueryError> {
        // Step 1 — scope → candidate id set (§7.4). A finer scope runs a keyed
        // fetch to bound the candidate ids; an empty scope is the whole tenant
        // floor (CandidateSet::All). Either way the tenant floor is enforced by
        // `vector.search`, which is the hard boundary.
        // An empty scope is the whole tenant floor (CandidateSet::All, R6 — so
        // `unkeyed` facts stay reachable). `as_of` is NOT dropped here: it is
        // honored on the All path by the vector tier's `ingested_at <= as_of`
        // filter below, so an unscoped `as_of` search is temporally bounded
        // (ISI-6172 / CR ISI-6168 B1), not a silent newer-than-as_of read.
        let candidates = if req.scope.is_empty() {
            CandidateSet::All
        } else {
            let kq = KeyedQuery {
                keys: req.scope.clone(),
                fact_type: req.fact_type,
                as_of: req.as_of,
                valid: Validity::All, // scope is a candidate filter, not a read
                limit: KeyedQuery::MAX_LIMIT,
            };
            let rows = self.store.fetch_by_keys(caller_tenant, &kq).await?;
            CandidateSet::Only(rows.into_iter().map(|r| r.id).collect())
        };

        // Step 2 — embed the query and ANN-rank the fact tier.
        let qvec = embedder
            .embed(&req.query)
            .await
            .map_err(|e| QueryError::Semantic(e.to_string()))?;
        let scored = vector
            .search(caller_tenant, &candidates, &qvec, k, req.as_of)
            .await
            .map_err(|e| QueryError::Semantic(e.to_string()))?;

        // A semantic hit must carry actual signal: drop non-positive cosine
        // similarities (`<= 0` = orthogonal or anti-correlated — no shared
        // direction, not a match). ANN backends return the k nearest rows
        // regardless of similarity, so without this filter a zero-signal fact
        // surfaces as a hit and, worse, earns undeserved RRF credit in
        // `fused_search` — which let a single-path keyed fact tie out a genuine
        // cross-path agreement (ISI-6079 / CR ISI-6168 DB suite). Score is
        // cosine similarity on both backends (MemVectorIndex and PgVectorIndex's
        // `1 - (embedding <=> q)`), so the predicate is backend-agnostic.
        let scored: Vec<_> = scored.into_iter().filter(|s| s.score > 0.0).collect();

        // Step 4 — hydrate authoritative records (R1 resolve-to-ground-truth),
        // preserving ANN rank order. A vanished fact (retention sweep racing
        // the index) is skipped, not an error.
        let mut hits = Vec::with_capacity(scored.len());
        for s in scored {
            if let Some(record) = self.store.get_fact(caller_tenant, s.fact_id).await? {
                hits.push(SemanticHit {
                    record,
                    score: s.score as f64,
                    via: vec![FusionSource::SemanticFact],
                });
            }
        }
        Ok(hits)
    }

    /// Fused retrieval (§7.4 step 3): run the keyed path, the graph-traverse
    /// path, and the semantic path, fuse their ranked id lists with RRF, and
    /// hydrate the top-`k` in fused order. A fact that several paths agree on
    /// ranks highest — the "keyed + traverse + semantic agree" signal. `keyed`,
    /// `traverse`, and `semantic` are each optional; whichever are present
    /// participate. The graph path is skipped when no projection is configured,
    /// and a start node outside the tenant floor (`not_found`) contributes an
    /// empty list rather than failing the fused read; the semantic path is
    /// skipped (not an error) when the module is off — so a keyed-only
    /// deployment still gets a coherent single-path result.
    pub async fn fused_search(
        &self,
        caller_tenant: &str,
        keyed: Option<&KeyedQuery>,
        traverse: Option<&TraverseReq>,
        semantic: Option<&SearchRequest>,
        k: usize,
    ) -> Result<Vec<SemanticHit>, QueryError> {
        let k = k.clamp(1, SearchRequest::MAX_K);
        let mut lists: Vec<RankedList> = Vec::new();
        // Carry hydrated records + per-path score so the fused output needs no
        // second FactStore pass for keyed hits.
        let mut records: std::collections::HashMap<uuid::Uuid, FactRecord> =
            std::collections::HashMap::new();

        if let Some(kq) = keyed {
            let rows = self.fetch_by_keys(caller_tenant, kq).await?;
            lists.push(RankedList::new(
                FusionSource::Keyed,
                rows.iter().map(|r| r.id).collect(),
            ));
            for r in rows {
                records.entry(r.id).or_insert(r);
            }
        }
        if let Some(treq) = traverse {
            if self.graph.is_some() {
                match self.traverse(caller_tenant, treq).await {
                    Ok(result) => {
                        // Fact nodes in BFS order are the graph path's ranked
                        // list; non-fact nodes (entity/a2a) carry no body to
                        // hydrate and don't participate as hits.
                        let ids: Vec<uuid::Uuid> = result
                            .nodes
                            .iter()
                            .filter(|n| n.kind == crate::graph::NodeKind::Fact)
                            .filter_map(|n| crate::graph::parse_node_id(&n.id))
                            .filter_map(|(_, _, local)| uuid::Uuid::parse_str(&local).ok())
                            .collect();
                        lists.push(RankedList::new(FusionSource::Graph, ids));
                        // Opportunistically carry any hydrated records.
                        for n in result.nodes {
                            if let Some(rec) = n.record {
                                records.entry(rec.id).or_insert(rec);
                            }
                        }
                    }
                    // A start node outside the tenant floor is `not_found`
                    // (§3.3) — the graph path contributes nothing, never an
                    // error that would sink the whole fused read.
                    Err(GraphError::NotFound) => {}
                    Err(GraphError::Invalid(reason)) => {
                        return Err(QueryError::Invalid {
                            reason: reason.to_string(),
                        })
                    }
                    Err(e) => return Err(QueryError::Semantic(e.to_string())),
                }
            }
        }
        if let Some(sreq) = semantic {
            if self.semantic_enabled() {
                let hits = self.semantic_search(caller_tenant, sreq).await?;
                lists.push(RankedList::new(
                    FusionSource::SemanticFact,
                    hits.iter().map(|h| h.record.id).collect(),
                ));
                for h in hits {
                    records.entry(h.record.id).or_insert(h.record);
                }
            }
        }

        let fused: Vec<FusedHit> = rrf_fuse(&lists);
        let mut out = Vec::with_capacity(fused.len().min(k));
        for f in fused.into_iter().take(k) {
            let record = match records.get(&f.fact_id) {
                Some(r) => r.clone(),
                // Produced by a path that didn't hydrate it (defensive; both
                // current paths hydrate). Resolve from the store (R1).
                None => match self.store.get_fact(caller_tenant, f.fact_id).await? {
                    Some(r) => r,
                    None => continue,
                },
            };
            out.push(SemanticHit {
                record,
                score: f.score,
                via: f.sources,
            });
        }
        Ok(out)
    }
}

/// Bounded reason enum for the `memkeys.write.rejected` key event
/// (signal-catalog logs.key_events "with reason enum"). The free-text
/// rejection reason is caller response data — only this bounded code is
/// ever emitted into telemetry.
fn rejection_code(reason: &str) -> &'static str {
    if reason.contains("unkeyed") {
        "unkeyed"
    } else if reason.contains("supersedes") {
        "supersedes"
    } else if reason.contains("limit must be") || reason.contains("entity.* key") {
        "query"
    } else {
        "envelope"
    }
}

/// The entity-kind attribute of the envelope's anchor (the kind only — the
/// value is on the deny-list, signal-catalog "never fact content").
fn anchor_kind(envelope: &Envelope) -> &'static str {
    match (
        envelope.source.kind,
        envelope.entity.service.is_some() || envelope.entity.project.is_some(),
    ) {
        (crate::envelope::SourceKind::TelemetryLearning, true)
            if envelope.entity.service.is_some() =>
        {
            "service"
        }
        (crate::envelope::SourceKind::TelemetryLearning, _) => "project",
        (crate::envelope::SourceKind::A2aExchange, _) => "thread",
        _ => "run",
    }
}

fn validate_supersedes_target(
    fact: &ProcessedFact,
    target: &crate::model::FactRecord,
) -> Result<(), IngestError> {
    // §6 stage 3: "validate the target shares (entity, subject, type)". The
    // supersession grain is the *thing the fact is about*: subject and type
    // must match exactly, and the two records must share at least one anchor
    // entity key value (project/issue/pr/run/thread/service). Exact entity
    // equality was considered and rejected: legitimate versioning crosses
    // runs (a new decision in a later run supersedes the old one on the same
    // issue), and the §5.2 collapse is per (entity, subject, type) group.
    let shared_anchor = [
        (&fact.entity.project, &target.entity_project),
        (&fact.entity.issue, &target.entity_issue),
        (&fact.entity.pr, &target.entity_pr),
        (&fact.entity.run, &target.entity_run),
        (&fact.entity.thread, &target.entity_thread),
        (&fact.entity.service, &target.entity_service),
    ]
    .into_iter()
    .any(|(a, b)| a.is_some() && a == b);
    if !shared_anchor {
        return Err(IngestError::Rejected {
            reason: "supersedes target does not share the entity keys".into(),
        });
    }
    if target.subject != fact.subject {
        return Err(IngestError::Rejected {
            reason: "supersedes target does not share the subject".into(),
        });
    }
    if target.fact_type != fact.fact_type.as_str() {
        return Err(IngestError::Rejected {
            reason: "supersedes target does not share the fact type".into(),
        });
    }
    Ok(())
}

fn record_error(span: &mut opentelemetry::global::BoxedSpan, message: String) {
    span.set_status(Status::Error {
        description: message.into(),
    });
}

/// Start a pipeline-stage span around a synchronous stage body, recording the
/// stage attribute (`memkeys.pipeline.stage`) as a child of the ingest span.
fn stage_span<T>(
    stage: &'static str,
    parent: &opentelemetry::Context,
    body: impl FnOnce() -> Result<T, IngestError>,
) -> Result<T, IngestError> {
    let mut span = telemetry::start_child_span(
        stage_span_name(stage),
        SpanKind::Internal,
        vec![telemetry::attr("memkeys.pipeline.stage", stage)],
        parent,
    );
    let result = body();
    match &result {
        Ok(_) => span.set_status(Status::Ok),
        Err(e) => record_error(&mut span, format!("{e}")),
    }
    span.end();
    result
}

/// Like [`stage_span`], but wrapping the async key-stage body (enrichment
/// pivots are resolver lookups). Carries the signal-catalog `memkeys.key`
/// attribute set: `memkeys.pipeline.stage` AND `memkeys.entity.kind`
/// (the kind only — never entity-key values).
async fn key_stage_span<T>(
    entity_kind: &'static str,
    parent: &opentelemetry::Context,
    body: impl std::future::Future<Output = Result<T, IngestError>>,
) -> Result<T, IngestError> {
    let mut span = telemetry::start_child_span(
        stage_span_name(telemetry::PIPELINE_STAGE_KEY),
        SpanKind::Internal,
        vec![
            telemetry::attr("memkeys.pipeline.stage", telemetry::PIPELINE_STAGE_KEY),
            telemetry::attr("memkeys.entity.kind", entity_kind),
        ],
        parent,
    );
    let result = body.await;
    match &result {
        Ok(_) => span.set_status(Status::Ok),
        Err(e) => record_error(&mut span, format!("{e}")),
    }
    span.end();
    result
}

fn stage_span_name(stage: &str) -> &'static str {
    match stage {
        telemetry::PIPELINE_STAGE_NORMALIZE => "memkeys.normalize",
        telemetry::PIPELINE_STAGE_KEY => "memkeys.key",
        telemetry::PIPELINE_STAGE_TEMPORALIZE => "memkeys.temporalize",
        _ => "memkeys.stage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityKey, Validity};

    #[test]
    fn keyed_query_defaults() {
        let q = KeyedQuery {
            keys: vec![(EntityKey::Issue, "i1".into())],
            fact_type: None,
            as_of: None,
            valid: Validity::default(),
            limit: KeyedQuery::DEFAULT_LIMIT,
        };
        assert_eq!(q.valid, Validity::Current);
        assert_eq!(q.limit, 100);
    }

    // --- M5 semantic / fusion ---------------------------------------------

    use crate::memstore::MemStore;
    use crate::model::EntityKeys;
    use crate::semantic::{Embedder, FactVector, HashEmbedder, MemVectorIndex};
    use chrono::Utc;
    use uuid::Uuid;

    fn pf(n: u128, tenant: &str, run: &str, body: &str) -> ProcessedFact {
        ProcessedFact {
            id: Uuid::from_u128(n),
            tenant_id: tenant.into(),
            fact_type: FactType::Decision,
            subject: Some("db".into()),
            body: body.into(),
            body_hash: format!("h{n}"),
            confidence: None,
            supersedes: None,
            event_time: Utc::now(),
            ingested_at: Utc::now(),
            entity: EntityKeys {
                run: Some(run.into()),
                ..Default::default()
            },
            keyed: true,
            provenance_principal: "agent-a".into(),
            provenance_source_kind: "gateway_flush".into(),
            provenance_trace_id: None,
            rejected_hints: vec![],
            attachments: serde_json::Value::Array(vec![]),
        }
    }

    /// Commit a fact to the store AND project its vector (simulates the
    /// stage-5b worker having run; the worker itself is wired in M5 part 2).
    async fn seed(
        store: &MemStore,
        index: &MemVectorIndex,
        embedder: &HashEmbedder,
        f: &ProcessedFact,
    ) {
        store.commit_fact(f).await.unwrap();
        index
            .upsert(&FactVector {
                fact_id: f.id,
                tenant_id: f.tenant_id.clone(),
                embedding: embedder.embed(&f.body).await.unwrap(),
                fact_type: f.fact_type.as_str().into(),
                entity: f.entity.clone(),
                ingested_at: f.ingested_at,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn semantic_search_disabled_returns_feature_disabled() {
        let svc = MemKeysService::new(Arc::new(MemStore::new()));
        let req = SearchRequest {
            query: "postgres".into(),
            scope: vec![],
            fact_type: None,
            k: 10,
            as_of: None,
        };
        let err = svc.semantic_search("t", &req).await.unwrap_err();
        assert!(matches!(
            err,
            QueryError::FeatureDisabled {
                feature: "memory.semantic"
            }
        ));
    }

    #[tokio::test]
    async fn semantic_search_ranks_relevant_fact_first_and_hydrates() {
        let store = Arc::new(MemStore::new());
        let index = Arc::new(MemVectorIndex::new());
        let embedder = Arc::new(HashEmbedder::new());
        seed(
            &store,
            &index,
            &embedder,
            &pf(1, "t", "r", "chose postgres for the durable fact store"),
        )
        .await;
        seed(
            &store,
            &index,
            &embedder,
            &pf(
                2,
                "t",
                "r",
                "the react frontend component library was updated",
            ),
        )
        .await;

        let svc = MemKeysService::new(store.clone()).with_semantic(embedder.clone(), index.clone());
        let req = SearchRequest {
            query: "postgres database durability".into(),
            scope: vec![],
            fact_type: None,
            k: 10,
            as_of: None,
        };
        let hits = svc.semantic_search("t", &req).await.unwrap();
        assert!(!hits.is_empty());
        // R1: the hit carries the authoritative body hydrated from the store.
        assert_eq!(hits[0].record.id, Uuid::from_u128(1));
        assert!(hits[0].record.body.contains("postgres"));
        assert_eq!(hits[0].via, vec![FusionSource::SemanticFact]);
    }

    #[tokio::test]
    async fn fused_search_combines_keyed_and_semantic() {
        let store = Arc::new(MemStore::new());
        let index = Arc::new(MemVectorIndex::new());
        let embedder = Arc::new(HashEmbedder::new());
        seed(
            &store,
            &index,
            &embedder,
            &pf(1, "t", "r", "chose postgres for the durable fact store"),
        )
        .await;
        seed(
            &store,
            &index,
            &embedder,
            &pf(2, "t", "r", "a different decision about caching"),
        )
        .await;

        let svc = MemKeysService::new(store.clone()).with_semantic(embedder.clone(), index.clone());
        let keyed = KeyedQuery {
            keys: vec![(EntityKey::Run, "r".into())],
            fact_type: None,
            as_of: None,
            valid: Validity::All,
            limit: KeyedQuery::DEFAULT_LIMIT,
        };
        let sem = SearchRequest {
            query: "postgres database durability".into(),
            scope: vec![],
            fact_type: None,
            k: 10,
            as_of: None,
        };
        let hits = svc
            .fused_search("t", Some(&keyed), None, Some(&sem), 10)
            .await
            .unwrap();
        // Both facts are keyed on run=r; fact 1 is also the top semantic hit, so
        // cross-path agreement puts it first, surfaced via both paths.
        assert_eq!(hits[0].record.id, Uuid::from_u128(1));
        assert!(hits[0].via.contains(&FusionSource::Keyed));
        assert!(hits[0].via.contains(&FusionSource::SemanticFact));
        assert_eq!(hits.len(), 2, "deduped across the two paths");
    }

    #[tokio::test]
    async fn semantic_search_drops_zero_similarity_hits() {
        let store = Arc::new(MemStore::new());
        let index = Arc::new(MemVectorIndex::new());
        let embedder = Arc::new(HashEmbedder::new());
        // f1 shares tokens with the query; f2 is orthogonal (cosine 0.0 under
        // HashEmbedder — verified: no shared tokens, no hash collision).
        seed(
            &store,
            &index,
            &embedder,
            &pf(1, "t", "r", "chose postgres for the durable fact store"),
        )
        .await;
        seed(
            &store,
            &index,
            &embedder,
            &pf(2, "t", "r", "the standup meeting was moved to Tuesday"),
        )
        .await;

        let svc = MemKeysService::new(store.clone()).with_semantic(embedder.clone(), index.clone());
        let req = SearchRequest {
            query: "postgres database durability".into(),
            scope: vec![],
            fact_type: None,
            k: 10,
            as_of: None,
        };
        let hits = svc.semantic_search("t", &req).await.unwrap();
        // The orthogonal fact must NOT surface — a zero-signal hit would earn
        // undeserved RRF credit in fused_search (ISI-6079 regression guard).
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.id, Uuid::from_u128(1));
    }
}
