//! M6 — the LLM pipeline workers, stages 5c-5e (ADR-llm-pipeline Decision 2).
//!
//! Three post-commit, off-the-ack-path workers behind the generative
//! [`Generator`](crate::generative::Generator) port (§6/R2: a projection
//! failure never blocks stages 1-4). All idempotent (ADR Decision 6), all
//! rebuildable via `catch_up` SoT scans, all stamped with
//! `derivation_version = (model_id, prompt_version)`:
//!
//! - [`ExtractWorker`] — **W-extract (stage 5e, new).** For a committed,
//!   sparsely-keyed fact, the generative model reads `fact.body` and proposes
//!   additional candidate entity keys. Proposals are validated (allowed key
//!   space, no overwrite of existing keys, verbatim evidence span — the
//!   anti-hallucination guard) and written ONLY as low-trust
//!   `derived_key[]` rows (`source='llm-extract'`). They never overwrite
//!   client/server-stamped keys and never carry provenance (R8). Between `ack`
//!   and 5e landing, the fact stays reachable by its stage-2 keys + the tenant
//!   floor — the async gap costs finer-key precision latency, never
//!   findability.
//! - [`DigestWorker`] — **W-digest (stage 5c).** Debounced + coalesced per
//!   `(scope, scope_id)`: a burst of N facts on a thread marks the scope dirty
//!   once and triggers ONE rollup. The summarizer regenerates the scope's
//!   digest + "story" lineage from its current (non-superseded) member facts,
//!   the embedder embeds the summary, and the result lands in the digest tier
//!   with `member_fact_ids` (the R1 resolve-down path). `scope_version` (the
//!   wall-clock epoch of the member-snapshot read — monotonic across rollups,
//!   unlike the member count, which a supersession-out-of-scope or retention
//!   sweep can shrink; ISI-6192 B1) makes a late rollup for an older snapshot
//!   a no-op.
//! - [`CategorizeWorker`] — **W-categorize (stage 5d).** The schema-constrained
//!   categorizer over `fact.body` → validated emergent labels →
//!   `category_facets` rows + the `categories[]` facet surface on the keyed
//!   record (the derived-attribute projection column provisioned in migration
//!   0001). The controlled-vocabulary consolidation is the M7 reconciler's
//!   periodic job (§5.4.3).
//!
//! OTel (WS-7 signal catalog): each apply/rollup is its own trace root
//! `memkeys.worker.process` (CONSUMER) carrying a SPAN_LINK to the originating
//! `memkeys.commit`, recording `memkeys.projection.lag` and the
//! `memkeys.worker.queue.depth` / `memkeys.poison.queue.depth` gauges. The
//! generative call itself is the gen_ai CLIENT span, emitted by the single
//! helper in `crate::generative` — never prompt/completion content.
//!
//! Failure policy (ADR Decision 8): a transient failure retries once per
//! drain; a second failure (or a deterministic schema-invalid output) is a
//! POISON item — the fact stays committed and keyed, only its derived view is
//! missing, and it is still reachable by keyed + tenant-floor search.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use opentelemetry::trace::{Span, SpanKind, Status};
use sqlx::PgPool;
use uuid::Uuid;

use crate::generative::{
    self, CategorizeInput, CategorizeOutput, ExtractInput, ExtractOutput, GenTask, Generator,
    MemberFact, SummarizeInput, SummarizeOutput,
};
use crate::model::EntityKey;
use crate::semantic::Embedder;
use crate::telemetry::{self, Instruments};
use crate::worker::WorkItem;

/// Failure of an LLM-pipeline worker. `Generator` failures distinguish
/// transient (endpoint unavailable → retried) from poison (schema-invalid
/// output → dead-letter).
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("llm pipeline backend error: {0}")]
    Backend(String),
    #[error("generative call failed: {0}")]
    Generator(#[from] generative::GenError),
}

/// One digest member as the rollup reads it: (id, fact_type, event_time,
/// ingested_at, body).
type MemberRow = (Uuid, String, DateTime<Utc>, DateTime<Utc>, String);

/// The scope axis of a digest (§5.4.2: per thread / run / entity) and the
/// facts column it rolls up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DigestScope {
    Thread,
    Run,
    Entity,
}

impl DigestScope {
    pub fn as_str(self) -> &'static str {
        match self {
            DigestScope::Thread => "thread",
            DigestScope::Run => "run",
            DigestScope::Entity => "entity",
        }
    }

    /// The `facts` column whose value is the `scope_id`.
    fn column(self) -> &'static str {
        match self {
            DigestScope::Thread => "entity_thread",
            DigestScope::Run => "entity_run",
            // The entity scope rolls up by the anchor project key.
            DigestScope::Entity => "entity_project",
        }
    }
}

const ALL_SCOPES: [DigestScope; 3] = [DigestScope::Thread, DigestScope::Run, DigestScope::Entity];

/// The full extract key space — the §4 anchor-able keys the model may propose
/// (`allowed_key_space` in the extract contract).
fn allowed_key_space() -> Vec<String> {
    [
        EntityKey::Project,
        EntityKey::Issue,
        EntityKey::Pr,
        EntityKey::Agent,
        EntityKey::Run,
        EntityKey::Thread,
        EntityKey::Service,
    ]
    .iter()
    .map(|k| k.wire_name().to_string())
    .collect()
}

/// A fact row as the workers read it: the authoritative body (the ONE place
/// LLM workers read body text — it never leaves the worker; only derived
/// artifacts reach the projections) + keys + both time axes.
#[derive(Debug, sqlx::FromRow)]
struct FactRow {
    id: Uuid,
    tenant_id: String,
    body: String,
    ingested_at: DateTime<Utc>,
    entity_project: Option<String>,
    entity_issue: Option<String>,
    entity_pr: Option<String>,
    entity_agent: Option<String>,
    entity_run: Option<String>,
    entity_thread: Option<String>,
    entity_service: Option<String>,
}

const FACT_ROW_SQL: &str = "SELECT id, tenant_id, body, ingested_at, \
     entity_project, entity_issue, entity_pr, entity_agent, entity_run, entity_thread, \
     entity_service FROM facts WHERE tenant_id = $1 AND id = $2";

async fn fetch_fact(pool: &PgPool, tenant: &str, id: Uuid) -> Result<Option<FactRow>, LlmError> {
    sqlx::query_as::<_, FactRow>(FACT_ROW_SQL)
        .bind(tenant)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| LlmError::Backend(e.to_string()))
}

/// The `derivation_runs` stamp kinds (migration 0004; `digest_rollup` added in
/// 0005 for the ISI-6192 B2 scope-level poison quarantine).
const RUN_KIND_EXTRACT: &str = "llm_extract";
const RUN_KIND_CATEGORIZE: &str = "categorize";
const RUN_KIND_DIGEST: &str = "digest_rollup";

/// The stable digest id per `(tenant, scope, scope_id)` — the `digests` upsert
/// target AND the `derivation_runs.ref_id` used to quarantine a poison scope
/// (ISI-6192 B2). Deterministic so a rollup and its poison stamp agree.
fn digest_id(tenant: &str, scope: DigestScope, scope_id: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("memkeys/digest/{tenant}/{}/{scope_id}", scope.as_str()).as_bytes(),
    )
}

/// True when an LLM-worker failure is a DETERMINISTIC poison (schema-invalid
/// generative output, ADR Decision 8) — retrying the identical input cannot
/// succeed, so it is quarantined rather than retried. A transient
/// [`GenError::Unavailable`] or a DB [`LlmError::Backend`] is NOT poison: it is
/// left unstamped so a later drain/`catch_up` retries once the cause clears.
fn is_poison(e: &LlmError) -> bool {
    matches!(
        e,
        LlmError::Generator(generative::GenError::InvalidOutput(_))
    )
}

/// Record the derivation stamp (idempotent upsert) — marks that a generative
/// worker ran for this fact at this derivation version, even with an EMPTY
/// result, so the catch-up hole scan does not re-process facts forever.
async fn stamp_run(
    tx: &mut sqlx::PgConnection,
    kind: &str,
    fact_id: Uuid,
    tenant: &str,
    model_id: &str,
    prompt_version: &str,
) -> Result<(), LlmError> {
    sqlx::query(
        "INSERT INTO derivation_runs (kind, ref_id, tenant_id, model_id, prompt_version) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (kind, ref_id) DO UPDATE \
         SET model_id = EXCLUDED.model_id, prompt_version = EXCLUDED.prompt_version, \
             applied_at = now()",
    )
    .bind(kind)
    .bind(fact_id)
    .bind(tenant)
    .bind(model_id)
    .bind(prompt_version)
    .execute(&mut *tx)
    .await
    .map_err(|e| LlmError::Backend(e.to_string()))?;
    Ok(())
}

/// Durably quarantine a poison derivation (ISI-6192 B2): stamp `derivation_runs`
/// at the CURRENT `(model_id, prompt_version)` so the `catch_up` hole-scan skips
/// this ref until a version bump re-opens it (ADR Decision 8: poison STOPS
/// reprocessing — the fact stays committed + keyed, only its derived view is
/// absent). `ref_id` is the fact id for extract/categorize and the stable
/// [`digest_id`] for a digest scope. Runs on the pool (no surrounding tx —
/// there is no derived-view write to make atomic with the stamp).
async fn stamp_poison(
    pool: &PgPool,
    kind: &str,
    ref_id: Uuid,
    tenant: &str,
    model_id: &str,
    prompt_version: &str,
) -> Result<(), LlmError> {
    sqlx::query(
        "INSERT INTO derivation_runs (kind, ref_id, tenant_id, model_id, prompt_version) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (kind, ref_id) DO UPDATE \
         SET model_id = EXCLUDED.model_id, prompt_version = EXCLUDED.prompt_version, \
             applied_at = now()",
    )
    .bind(kind)
    .bind(ref_id)
    .bind(tenant)
    .bind(model_id)
    .bind(prompt_version)
    .execute(pool)
    .await
    .map_err(|e| LlmError::Backend(e.to_string()))?;
    Ok(())
}

/// `memkeys.projection.lag` — elapsed seconds between the fact's ingest and
/// the projection apply (the D3 observable-lag invariant).
fn lag_seconds(ingested_at: DateTime<Utc>) -> f64 {
    (Utc::now() - ingested_at).num_milliseconds() as f64 / 1000.0
}

// ===========================================================================
// W-extract (stage 5e)
// ===========================================================================

/// The stage-5e worker. Clone-safe handle (the queue is shared).
#[derive(Clone)]
pub struct ExtractWorker {
    pool: PgPool,
    generator: Arc<dyn Generator>,
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    instruments: Arc<Instruments>,
    poison: Arc<Mutex<HashMap<String, i64>>>,
}

impl std::fmt::Debug for ExtractWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtractWorker").finish()
    }
}

impl ExtractWorker {
    pub fn new(pool: PgPool, generator: Arc<dyn Generator>) -> Self {
        Self {
            pool,
            generator,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            instruments: Arc::new(Instruments::new()),
            poison: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enqueue one committed fact (called by the service AFTER the stage-4 ack
    /// decision — never on the ack path itself).
    pub fn enqueue(
        &self,
        tenant: &str,
        fact_id: Uuid,
        commit_span: Option<opentelemetry::trace::SpanContext>,
    ) {
        let mut q = self.queue.lock().expect("extract queue lock");
        q.push_back(WorkItem {
            tenant: tenant.to_string(),
            fact_id,
            commit_span,
        });
        let depth = q.len();
        drop(q);
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_LLM_EXTRACT, depth);
    }

    /// Apply every queued item now (tests + the binary's periodic pass). A
    /// transient failure retries once per drain; a DETERMINISTIC poison
    /// (schema-invalid output) is quarantined on first sight — retrying the
    /// identical input cannot succeed (ADR Decision 8; ISI-6192 B2).
    pub async fn drain(&self) -> usize {
        let mut applied = 0usize;
        let mut retries: Vec<WorkItem> = Vec::new();
        loop {
            let item = { self.queue.lock().expect("extract queue lock").pop_front() };
            let Some(item) = item else { break };
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(e) if is_poison(&e) => self.quarantine(&item).await,
                Err(_) => retries.push(item), // transient → one retry below
            }
        }
        for item in retries {
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(e) if is_poison(&e) => self.quarantine(&item).await,
                // Still transient-failing after a retry: bump the gauge and drop
                // from the live queue UNSTAMPED — the catch_up scan reaps the
                // hole once the endpoint recovers (no durable quarantine).
                Err(_) => self.dead_letter(&item),
            }
        }
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_LLM_EXTRACT, 0);
        applied
    }

    /// Durable poison quarantine (ISI-6192 B2): stamp the fact so `catch_up`
    /// skips it until a `(model_id, prompt_version)` bump re-opens it, and bump
    /// the live poison gauge.
    async fn quarantine(&self, item: &WorkItem) {
        if let Err(e) = stamp_poison(
            &self.pool,
            RUN_KIND_EXTRACT,
            item.fact_id,
            &item.tenant,
            self.generator.model_id(),
            GenTask::Extract.prompt_version(),
        )
        .await
        {
            tracing::warn!(target: "memkeys", error = %e, fact_id = %item.fact_id,
                "extract poison stamp failed; fact will be retried next pass");
        }
        self.dead_letter(item);
    }

    fn dead_letter(&self, item: &WorkItem) {
        let mut p = self.poison.lock().expect("poison lock");
        let n = p.entry(item.tenant.clone()).or_insert(0);
        *n += 1;
        let v = *n;
        drop(p);
        self.instruments
            .poison_queue_depth(telemetry::WORKER_KIND_LLM_EXTRACT, v);
    }

    /// SoT convergence scan: committed facts with no `llm_extract`
    /// derivation stamp — or one stale versus the active `(model_id,
    /// prompt_version)` (ADR Decision 5: a version bump is a backfill event)
    /// — are re-extracted.
    pub async fn catch_up(&self, tenant: &str) -> Result<usize, LlmError> {
        let ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT f.id FROM facts f \
             LEFT JOIN derivation_runs r ON r.kind = $2 AND r.ref_id = f.id \
             WHERE f.tenant_id = $1 \
               AND (r.ref_id IS NULL OR r.model_id <> $3 OR r.prompt_version <> $4) \
             ORDER BY f.ingested_at, f.id",
        )
        .bind(tenant)
        .bind(RUN_KIND_EXTRACT)
        .bind(self.generator.model_id())
        .bind(GenTask::Extract.prompt_version())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        // Collect-and-continue (ISI-6192 B2): a single failing fact must NOT
        // `?`-abort the tenant's backfill — at startup that would propagate to
        // `main()` and wedge the boot. A deterministic poison is quarantined so
        // it is not re-driven every tick; a transient failure is left for the
        // next pass.
        let mut applied = 0usize;
        for (id,) in ids {
            let item = WorkItem {
                tenant: tenant.to_string(),
                fact_id: id,
                commit_span: None,
            };
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(e) if is_poison(&e) => self.quarantine(&item).await,
                Err(e) => tracing::warn!(target: "memkeys", error = %e, fact_id = %id,
                    "extract catch_up transient failure; left for next pass"),
            }
        }
        Ok(applied)
    }

    async fn apply(&self, item: &WorkItem) -> Result<(), LlmError> {
        let row = match fetch_fact(&self.pool, &item.tenant, item.fact_id).await? {
            Some(row) => row,
            None => {
                // The fact vanished (retention sweep racing the queue):
                // nothing to derive — not an error, not a poison item.
                return Ok(());
            }
        };
        let mut span = telemetry::start_span_with_links(
            "memkeys.worker.process",
            SpanKind::Consumer,
            vec![
                telemetry::attr("memkeys.tenant.id", row.tenant_id.clone()),
                telemetry::attr("memkeys.worker.kind", telemetry::WORKER_KIND_LLM_EXTRACT),
                telemetry::attr(
                    "memkeys.projection.kind",
                    telemetry::PROJECTION_KIND_DERIVED_KEYS,
                ),
            ],
            item.commit_span.as_ref(),
        );
        let outcome = self.extract_and_write(&row).await;
        match &outcome {
            Ok(()) => {
                span.set_status(Status::Ok);
                self.instruments.projection_lag(
                    telemetry::PROJECTION_KIND_DERIVED_KEYS,
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

    /// The stage-5e derivation: assemble the extract input from the committed
    /// record, call the generative port (through the single OTel helper),
    /// validate the proposals, and write accepted ones as low-trust derived
    /// keys + the derivation stamp in ONE transaction (atomic projection).
    async fn extract_and_write(&self, row: &FactRow) -> Result<(), LlmError> {
        let mut existing_keys = std::collections::HashMap::new();
        for (k, v) in [
            (EntityKey::Project, &row.entity_project),
            (EntityKey::Issue, &row.entity_issue),
            (EntityKey::Pr, &row.entity_pr),
            (EntityKey::Agent, &row.entity_agent),
            (EntityKey::Run, &row.entity_run),
            (EntityKey::Thread, &row.entity_thread),
            (EntityKey::Service, &row.entity_service),
        ] {
            if let Some(v) = v {
                existing_keys.insert(k.wire_name().to_string(), v.clone());
            }
        }
        let input = serde_json::to_value(ExtractInput {
            fact_body: row.body.clone(),
            existing_keys: existing_keys.clone(),
            allowed_key_space: allowed_key_space(),
        })
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        let raw = generative::generate(
            self.generator.as_ref(),
            GenTask::Extract,
            &input,
            &self.instruments,
        )
        .await?;
        // Schema guard (ADR Decision 8): an unparseable output is POISON —
        // mapped to GenError::InvalidOutput, never a silent bad write.
        let output: ExtractOutput = serde_json::from_value(raw)
            .map_err(|e| generative::GenError::InvalidOutput(e.to_string()))?;
        let accepted =
            generative::validate_extract(&output, &row.body, &existing_keys, &allowed_key_space());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        // Idempotent re-run (ADR Decision 6: upsert the derived-key set for
        // (fact.id, model_id, prompt_version)): the deterministic generator
        // reproduces the same set, so delete+insert converges.
        sqlx::query("DELETE FROM derived_keys WHERE fact_id = $1")
            .bind(row.id)
            .execute(&mut *tx)
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        for (p, confidence) in &accepted {
            sqlx::query(
                "INSERT INTO derived_keys \
                 (fact_id, tenant_id, key, value, confidence, source, model_id, prompt_version) \
                 VALUES ($1, $2, $3, $4, $5, 'llm-extract', $6, $7)",
            )
            .bind(row.id)
            .bind(&row.tenant_id)
            .bind(&p.key)
            .bind(&p.value)
            .bind(confidence)
            .bind(self.generator.model_id())
            .bind(GenTask::Extract.prompt_version())
            .execute(&mut *tx)
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        }
        stamp_run(
            &mut tx,
            RUN_KIND_EXTRACT,
            row.id,
            &row.tenant_id,
            self.generator.model_id(),
            GenTask::Extract.prompt_version(),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        Ok(())
    }
}

// ===========================================================================
// W-digest (stage 5c)
// ===========================================================================

/// The stage-5c worker. Clone-safe handle (the dirty-scope set is shared).
///
/// Debounce/coalescing (ADR Decision 2b): enqueued facts mark their owning
/// scopes `(scope, scope_id)` dirty; one drain performs ONE rollup per dirty
/// scope regardless of how many facts marked it — a burst of N facts on a
/// thread triggers one summarization, not N.
#[derive(Clone)]
pub struct DigestWorker {
    pool: PgPool,
    generator: Arc<dyn Generator>,
    embedder: Arc<dyn Embedder>,
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    instruments: Arc<Instruments>,
    poison: Arc<Mutex<HashMap<String, i64>>>,
}

impl std::fmt::Debug for DigestWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestWorker").finish()
    }
}

impl DigestWorker {
    pub fn new(pool: PgPool, generator: Arc<dyn Generator>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            pool,
            generator,
            embedder,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            instruments: Arc::new(Instruments::new()),
            poison: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enqueue one committed fact; its owning scopes (thread / run / entity —
    /// the fact's anchor project) are marked dirty on drain. A fact with no
    /// scope key belongs to no digest scope (nothing to roll up).
    pub fn enqueue(
        &self,
        tenant: &str,
        fact_id: Uuid,
        commit_span: Option<opentelemetry::trace::SpanContext>,
    ) {
        let mut q = self.queue.lock().expect("digest queue lock");
        q.push_back(WorkItem {
            tenant: tenant.to_string(),
            fact_id,
            commit_span,
        });
        let depth = q.len();
        drop(q);
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_DIGEST_ROLLUP, depth);
    }

    /// Apply the queued facts: coalesce into the dirty-scope set, then roll
    /// each dirty scope up ONCE. Retries/dead-letters are per scope.
    pub async fn drain(&self) -> usize {
        // 1. Coalesce: pop all items, fetch scope keys, build the dirty set.
        let mut dirty: BTreeSet<(String, DigestScope, String)> = BTreeSet::new();
        let mut spans: Vec<opentelemetry::trace::SpanContext> = Vec::new();
        let mut pending: Vec<WorkItem> = Vec::new();
        loop {
            let item = { self.queue.lock().expect("digest queue lock").pop_front() };
            let Some(item) = item else { break };
            match fetch_fact(&self.pool, &item.tenant, item.fact_id).await {
                Ok(Some(row)) => {
                    if let Some(ctx) = &item.commit_span {
                        spans.push(ctx.clone());
                    }
                    let mut scopes = Vec::new();
                    if let Some(t) = &row.entity_thread {
                        scopes.push((DigestScope::Thread, t.clone()));
                    }
                    if let Some(r) = &row.entity_run {
                        scopes.push((DigestScope::Run, r.clone()));
                    }
                    if let Some(p) = &row.entity_project {
                        scopes.push((DigestScope::Entity, p.clone()));
                    }
                    if scopes.is_empty() {
                        continue; // no owning scope → nothing to roll up
                    }
                    for (scope, id) in scopes {
                        dirty.insert((row.tenant_id.clone(), scope, id));
                    }
                }
                Ok(None) => {} // fact vanished — retention sweep racing the queue
                Err(_) => pending.push(item), // fetch failure → retry round below
            }
        }
        // 2. One rollup per dirty scope (the coalesced debounce). A transient
        //    failure is retried once below; a DETERMINISTIC poison is
        //    quarantined on first sight — no identical retry (ISI-6192 B2).
        let mut rolled = 0usize;
        let mut failed: Vec<(String, DigestScope, String)> = Vec::new();
        for (tenant, scope, scope_id) in &dirty {
            match self.rollup(tenant, *scope, scope_id, spans.first()).await {
                Ok(()) => rolled += 1,
                Err(e) if is_poison(&e) => self.quarantine_scope(tenant, *scope, scope_id).await,
                Err(_) => failed.push((tenant.clone(), *scope, scope_id.clone())),
            }
        }
        // 3. One retry round for transient failures; a still-failing transient
        //    bumps the gauge only (the catch_up scan reaps the hole later), a
        //    poison is quarantined durably so catch_up skips it.
        for key in failed {
            match self.rollup(&key.0, key.1, &key.2, spans.first()).await {
                Ok(()) => rolled += 1,
                Err(e) if is_poison(&e) => self.quarantine_scope(&key.0, key.1, &key.2).await,
                Err(_) => self.poison_gauge(&key.0),
            }
        }
        // Fetch-failure retries (rare — transient DB): one more pass; a second
        // failure bumps the gauge (the catch-up scan reaps the hole later).
        let mut pending_poison: Vec<WorkItem> = Vec::new();
        for item in pending {
            let mut scopes: BTreeSet<(String, DigestScope, String)> = BTreeSet::new();
            match fetch_fact(&self.pool, &item.tenant, item.fact_id).await {
                Ok(Some(row)) => {
                    for (scope, value) in [
                        (DigestScope::Thread, &row.entity_thread),
                        (DigestScope::Run, &row.entity_run),
                        (DigestScope::Entity, &row.entity_project),
                    ] {
                        if let Some(v) = value {
                            scopes.insert((row.tenant_id.clone(), scope, v.clone()));
                        }
                    }
                    for (tenant, scope, scope_id) in scopes {
                        if self
                            .rollup(&tenant, scope, &scope_id, spans.first())
                            .await
                            .is_ok()
                        {
                            rolled += 1;
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => pending_poison.push(item),
            }
        }
        for item in pending_poison {
            self.poison_gauge(&item.tenant);
        }
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_DIGEST_ROLLUP, 0);
        rolled
    }

    /// Bump the live poison gauge for a tenant (dead-letter without a durable
    /// stamp — a still-failing transient the catch_up scan will retry later).
    fn poison_gauge(&self, tenant: &str) {
        let mut p = self.poison.lock().expect("poison lock");
        let n = p.entry(tenant.to_string()).or_insert(0);
        *n += 1;
        let v = *n;
        drop(p);
        self.instruments
            .poison_queue_depth(telemetry::WORKER_KIND_DIGEST_ROLLUP, v);
    }

    /// Durable poison quarantine for a SCOPE (ISI-6192 B2): stamp
    /// `derivation_runs(kind='digest_rollup', ref_id=digest_id)` at the current
    /// version so the `catch_up` hole-scan skips this scope until a version bump
    /// re-opens it, and bump the live gauge. A scope has no `derivation_runs`
    /// success stamp (the `digests` row IS its success marker), so the stamp is
    /// unambiguously a quarantine.
    async fn quarantine_scope(&self, tenant: &str, scope: DigestScope, scope_id: &str) {
        if let Err(e) = stamp_poison(
            &self.pool,
            RUN_KIND_DIGEST,
            digest_id(tenant, scope, scope_id),
            tenant,
            self.generator.model_id(),
            GenTask::Summarize.prompt_version(),
        )
        .await
        {
            tracing::warn!(target: "memkeys", error = %e, scope = scope.as_str(), scope_id,
                "digest poison stamp failed; scope will be retried next pass");
        }
        self.poison_gauge(tenant);
    }

    /// SoT convergence scan: scopes (per kind) with member facts but no
    /// current digest — or one stale versus the active derivation version —
    /// are re-rolled. Scopes durably quarantined at the current version
    /// (ISI-6192 B2) are skipped, and a per-scope failure NEVER `?`-aborts the
    /// backfill (which at boot would wedge `main()`): a poison is quarantined,
    /// a transient is left for the next pass.
    pub async fn catch_up(&self, tenant: &str) -> Result<usize, LlmError> {
        // The scopes already quarantined at the CURRENT version — their
        // digest_ids. A version bump empties this set → re-opens them.
        let quarantined: std::collections::HashSet<Uuid> = sqlx::query_as::<_, (Uuid,)>(
            "SELECT ref_id FROM derivation_runs \
             WHERE kind = $1 AND tenant_id = $2 AND model_id = $3 AND prompt_version = $4",
        )
        .bind(RUN_KIND_DIGEST)
        .bind(tenant)
        .bind(self.generator.model_id())
        .bind(GenTask::Summarize.prompt_version())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| LlmError::Backend(e.to_string()))?
        .into_iter()
        .map(|(id,)| id)
        .collect();

        let mut n = 0usize;
        for scope in ALL_SCOPES {
            let ids: Vec<(String,)> = sqlx::query_as(&format!(
                "SELECT DISTINCT f.{col} FROM facts f \
                 LEFT JOIN digests d ON d.tenant_id = f.tenant_id \
                     AND d.scope = $2 AND d.scope_id = f.{col} \
                 WHERE f.tenant_id = $1 AND f.{col} IS NOT NULL \
                   AND (d.digest_id IS NULL OR d.model_id <> $3 OR d.prompt_version <> $4)",
                col = scope.column()
            ))
            .bind(tenant)
            .bind(scope.as_str())
            .bind(self.generator.model_id())
            .bind(GenTask::Summarize.prompt_version())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
            for (scope_id,) in ids {
                if quarantined.contains(&digest_id(tenant, scope, &scope_id)) {
                    continue; // durably poison at this version → do not re-drive
                }
                match self.rollup(tenant, scope, &scope_id, None).await {
                    Ok(()) => n += 1,
                    Err(e) if is_poison(&e) => {
                        self.quarantine_scope(tenant, scope, &scope_id).await
                    }
                    Err(e) => tracing::warn!(target: "memkeys", error = %e,
                        scope = scope.as_str(), scope_id,
                        "digest catch_up transient failure; left for next pass"),
                }
            }
        }
        Ok(n)
    }

    /// One per-scope rollup (§6 stage 5c): fetch current (non-superseded)
    /// members, summarize + story via the generative port, embed the summary,
    /// upsert the digest tier row.
    async fn rollup(
        &self,
        tenant: &str,
        scope: DigestScope,
        scope_id: &str,
        commit_span: Option<&opentelemetry::trace::SpanContext>,
    ) -> Result<(), LlmError> {
        // Members: current on the transaction axis (read-time collapse, §5.1).
        let members: Vec<MemberRow> = sqlx::query_as(&format!(
            "SELECT f.id, f.fact_type, f.event_time, f.ingested_at, f.body FROM facts f \
             WHERE f.tenant_id = $1 AND f.{col} = $2 \
               AND NOT EXISTS (SELECT 1 FROM facts s \
                     WHERE s.supersedes = f.id AND s.tenant_id = f.tenant_id) \
             ORDER BY f.event_time ASC, f.id ASC",
            col = scope.column()
        ))
        .bind(tenant)
        .bind(scope_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        if members.is_empty() {
            return Ok(()); // every member superseded/swept — nothing to roll up
        }
        // The newest member's ingest is the digest's freshness point (lag).
        let newest_ingest = members.iter().map(|m| m.3).max().unwrap_or_else(Utc::now);

        let mut span = telemetry::start_span_with_links(
            "memkeys.worker.process",
            SpanKind::Consumer,
            vec![
                telemetry::attr("memkeys.tenant.id", tenant.to_string()),
                telemetry::attr("memkeys.worker.kind", telemetry::WORKER_KIND_DIGEST_ROLLUP),
                telemetry::attr("memkeys.projection.kind", telemetry::PROJECTION_KIND_DIGEST),
            ],
            commit_span,
        );
        let outcome = self.rollup_inner(tenant, scope, scope_id, &members).await;
        match &outcome {
            Ok(()) => {
                span.set_status(Status::Ok);
                self.instruments.projection_lag(
                    telemetry::PROJECTION_KIND_DIGEST,
                    lag_seconds(newest_ingest),
                );
            }
            Err(e) => span.set_status(Status::Error {
                description: e.to_string().into(),
            }),
        }
        span.end();
        outcome
    }

    async fn rollup_inner(
        &self,
        tenant: &str,
        scope: DigestScope,
        scope_id: &str,
        members: &[MemberRow],
    ) -> Result<(), LlmError> {
        // `scope_version` = the wall-clock microsecond epoch at which this
        // rollup read its member snapshot (captured here, immediately after the
        // `rollup` SELECT). It is MONOTONIC across rollups — a later rollup read
        // a later state of the scope — so the `scope_version <= EXCLUDED`
        // late-rollup guard still rejects a stale coalesced rollup landing late,
        // while a correct newer rollup ALWAYS wins. The member COUNT is NOT
        // monotonic (ISI-6192 B1): a supersession whose superseder leaves the
        // scope, or a retention sweep, shrinks it — a count-based version would
        // then permanently reject the correct smaller rollup and freeze the
        // digest on a stale summary.
        // ponytail: cross-process clock skew (microservice profile, >1 worker
        // pod) could misorder two near-simultaneous rollups of the SAME scope;
        // bounded by NTP + per-scope coalescing. Upgrade path if ever hit: a
        // DB-assigned per-scope sequence.
        let scope_version = Utc::now().timestamp_micros();
        let input = serde_json::to_value(SummarizeInput::new(
            scope.as_str(),
            scope_id,
            members
                .iter()
                .map(|(id, ty, event_time, _, body)| MemberFact {
                    fact_id: *id,
                    fact_type: ty.clone(),
                    event_time: event_time.to_rfc3339(),
                    body: body.clone(),
                })
                .collect(),
        ))
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        let raw = generative::generate(
            self.generator.as_ref(),
            GenTask::Summarize,
            &input,
            &self.instruments,
        )
        .await?;
        let output: SummarizeOutput = serde_json::from_value(raw)
            .map_err(|e| generative::GenError::InvalidOutput(e.to_string()))?;
        generative::validate_summarize(&output)?;
        // The story is a SERVER artifact (§5.5) — when the adapter omits it,
        // the server stamps its own lineage note.
        let story = output.story.unwrap_or_else(|| {
            format!(
                "rolled up {} facts on {} {} by {} ({})",
                members.len(),
                scope.as_str(),
                scope_id,
                self.generator.model_id(),
                GenTask::Summarize.prompt_version()
            )
        });
        // One embedder, both tiers (ADR Decision 3): the same model embeds
        // fact bodies and digest summaries so the two vector spaces are
        // RRF-fusable (§7.4).
        let embedding = self
            .embedder
            .embed(&output.summary)
            .await
            .map_err(|e| LlmError::Backend(format!("digest summary embed failed: {e}")))?;

        // Stable digest id per (tenant, scope, scope_id) — the upsert target.
        let digest_id = digest_id(tenant, scope, scope_id);
        let member_ids: Vec<Uuid> = members.iter().map(|m| m.0).collect();
        let encoded = crate::pg_vector::encode_vector(&embedding);
        // Late-rollup guard: only overwrite when the incoming scope_version is
        // >= the stored one (a stale coalesced rollup landing late is a no-op).
        sqlx::query(
            "INSERT INTO digests (digest_id, tenant_id, scope, scope_id, scope_version, \
                 summary, story, member_fact_ids, embedding, model_id, prompt_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::vector, $10, $11) \
             ON CONFLICT (tenant_id, scope, scope_id) DO UPDATE SET \
                 digest_id = EXCLUDED.digest_id, scope_version = EXCLUDED.scope_version, \
                 summary = EXCLUDED.summary, story = EXCLUDED.story, \
                 member_fact_ids = EXCLUDED.member_fact_ids, embedding = EXCLUDED.embedding, \
                 model_id = EXCLUDED.model_id, prompt_version = EXCLUDED.prompt_version, \
                 ingested_at = now() \
             WHERE digests.scope_version <= EXCLUDED.scope_version",
        )
        .bind(digest_id)
        .bind(tenant)
        .bind(scope.as_str())
        .bind(scope_id)
        .bind(scope_version)
        .bind(&output.summary)
        .bind(&story)
        .bind(&member_ids)
        .bind(&encoded)
        .bind(self.generator.model_id())
        .bind(GenTask::Summarize.prompt_version())
        .execute(&self.pool)
        .await
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        Ok(())
    }
}

// ===========================================================================
// W-categorize (stage 5d)
// ===========================================================================

/// The stage-5d worker. Clone-safe handle (the queue is shared).
#[derive(Clone)]
pub struct CategorizeWorker {
    pool: PgPool,
    generator: Arc<dyn Generator>,
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    instruments: Arc<Instruments>,
    poison: Arc<Mutex<HashMap<String, i64>>>,
}

impl std::fmt::Debug for CategorizeWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CategorizeWorker").finish()
    }
}

impl CategorizeWorker {
    pub fn new(pool: PgPool, generator: Arc<dyn Generator>) -> Self {
        Self {
            pool,
            generator,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            instruments: Arc::new(Instruments::new()),
            poison: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enqueue one committed fact (service calls this AFTER the stage-4 ack).
    pub fn enqueue(
        &self,
        tenant: &str,
        fact_id: Uuid,
        commit_span: Option<opentelemetry::trace::SpanContext>,
    ) {
        let mut q = self.queue.lock().expect("categorize queue lock");
        q.push_back(WorkItem {
            tenant: tenant.to_string(),
            fact_id,
            commit_span,
        });
        let depth = q.len();
        drop(q);
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_CATEGORIZE, depth);
    }

    /// Apply every queued item now (tests + the binary's periodic pass). A
    /// transient failure retries once per drain; a DETERMINISTIC poison is
    /// quarantined on first sight (ADR Decision 8; ISI-6192 B2).
    pub async fn drain(&self) -> usize {
        let mut applied = 0usize;
        let mut retries: Vec<WorkItem> = Vec::new();
        loop {
            let item = {
                self.queue
                    .lock()
                    .expect("categorize queue lock")
                    .pop_front()
            };
            let Some(item) = item else { break };
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(e) if is_poison(&e) => self.quarantine(&item).await,
                Err(_) => retries.push(item),
            }
        }
        for item in retries {
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(e) if is_poison(&e) => self.quarantine(&item).await,
                Err(_) => self.dead_letter(&item),
            }
        }
        self.instruments
            .worker_queue_depth(telemetry::WORKER_KIND_CATEGORIZE, 0);
        applied
    }

    /// Durable poison quarantine (ISI-6192 B2) — see [`ExtractWorker::quarantine`].
    async fn quarantine(&self, item: &WorkItem) {
        if let Err(e) = stamp_poison(
            &self.pool,
            RUN_KIND_CATEGORIZE,
            item.fact_id,
            &item.tenant,
            self.generator.model_id(),
            GenTask::Categorize.prompt_version(),
        )
        .await
        {
            tracing::warn!(target: "memkeys", error = %e, fact_id = %item.fact_id,
                "categorize poison stamp failed; fact will be retried next pass");
        }
        self.dead_letter(item);
    }

    fn dead_letter(&self, item: &WorkItem) {
        let mut p = self.poison.lock().expect("poison lock");
        let n = p.entry(item.tenant.clone()).or_insert(0);
        *n += 1;
        let v = *n;
        drop(p);
        self.instruments
            .poison_queue_depth(telemetry::WORKER_KIND_CATEGORIZE, v);
    }

    /// SoT convergence scan: facts with no `categorize` stamp — or one stale
    /// versus the active derivation version — are re-categorized.
    pub async fn catch_up(&self, tenant: &str) -> Result<usize, LlmError> {
        let ids: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT f.id FROM facts f \
             LEFT JOIN derivation_runs r ON r.kind = $2 AND r.ref_id = f.id \
             WHERE f.tenant_id = $1 \
               AND (r.ref_id IS NULL OR r.model_id <> $3 OR r.prompt_version <> $4) \
             ORDER BY f.ingested_at, f.id",
        )
        .bind(tenant)
        .bind(RUN_KIND_CATEGORIZE)
        .bind(self.generator.model_id())
        .bind(GenTask::Categorize.prompt_version())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        // Collect-and-continue (ISI-6192 B2): never `?`-abort the backfill on a
        // single poison fact (would wedge the boot-time scan).
        let mut applied = 0usize;
        for (id,) in ids {
            let item = WorkItem {
                tenant: tenant.to_string(),
                fact_id: id,
                commit_span: None,
            };
            match self.apply(&item).await {
                Ok(()) => applied += 1,
                Err(e) if is_poison(&e) => self.quarantine(&item).await,
                Err(e) => tracing::warn!(target: "memkeys", error = %e, fact_id = %id,
                    "categorize catch_up transient failure; left for next pass"),
            }
        }
        Ok(applied)
    }

    async fn apply(&self, item: &WorkItem) -> Result<(), LlmError> {
        let row = match fetch_fact(&self.pool, &item.tenant, item.fact_id).await? {
            Some(row) => row,
            None => return Ok(()), // retention sweep racing the queue
        };
        let mut span = telemetry::start_span_with_links(
            "memkeys.worker.process",
            SpanKind::Consumer,
            vec![
                telemetry::attr("memkeys.tenant.id", row.tenant_id.clone()),
                telemetry::attr("memkeys.worker.kind", telemetry::WORKER_KIND_CATEGORIZE),
                telemetry::attr(
                    "memkeys.projection.kind",
                    telemetry::PROJECTION_KIND_CATEGORY_FACETS,
                ),
            ],
            item.commit_span.as_ref(),
        );
        let outcome = self.categorize_and_write(&row).await;
        match &outcome {
            Ok(()) => {
                span.set_status(Status::Ok);
                self.instruments.projection_lag(
                    telemetry::PROJECTION_KIND_CATEGORY_FACETS,
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

    /// The stage-5d derivation: categorize the body, validate the emergent
    /// labels, write them as `category_facets` rows AND mirror onto the
    /// `facts.categories` derived-attribute column (the facet surface
    /// provisioned in migration 0001 "written back onto this record"). The
    /// controlled-vocabulary consolidation stays an M7 reconciler job (§5.4.3).
    async fn categorize_and_write(&self, row: &FactRow) -> Result<(), LlmError> {
        let input = serde_json::to_value(CategorizeInput {
            fact_body: row.body.clone(),
        })
        .map_err(|e| LlmError::Backend(e.to_string()))?;
        let raw = generative::generate(
            self.generator.as_ref(),
            GenTask::Categorize,
            &input,
            &self.instruments,
        )
        .await?;
        let output: CategorizeOutput = serde_json::from_value(raw)
            .map_err(|e| generative::GenError::InvalidOutput(e.to_string()))?;
        let labels = generative::validate_categorize(&output);

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        // Idempotent re-run (ADR Decision 6): delete+insert converges for the
        // deterministic generator; the stamp prevents hole-scan re-processing.
        sqlx::query("DELETE FROM category_facets WHERE fact_id = $1")
            .bind(row.id)
            .execute(&mut *tx)
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        for (label, confidence) in &labels {
            sqlx::query(
                "INSERT INTO category_facets \
                 (fact_id, tenant_id, label, confidence, model_id, prompt_version) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(row.id)
            .bind(&row.tenant_id)
            .bind(label)
            .bind(confidence)
            .bind(self.generator.model_id())
            .bind(GenTask::Categorize.prompt_version())
            .execute(&mut *tx)
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        }
        // The facet surface on the keyed record (§5.4.3 "as facets" — the
        // derived-attribute projection column from migration 0001).
        let names: Vec<String> = labels.iter().map(|(l, _)| l.clone()).collect();
        sqlx::query("UPDATE facts SET categories = $2 WHERE id = $1 AND tenant_id = $3")
            .bind(row.id)
            .bind(&names)
            .bind(&row.tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        stamp_run(
            &mut tx,
            RUN_KIND_CATEGORIZE,
            row.id,
            &row.tenant_id,
            self.generator.model_id(),
            GenTask::Categorize.prompt_version(),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| LlmError::Backend(e.to_string()))?;
        Ok(())
    }
}
