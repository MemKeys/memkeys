//! M6 integration conformance tests — the LLM pipeline workers (stages
//! 5c-5e) against a real Postgres, gated on TEST_DATABASE_URL (same
//! harness/env guard as the M1/M2/M5 suites; CI points it at the
//! `pgvector/pgvector:pg18` service container so migrations 0003/0004 apply).
//!
//! Coverage per the ISI-6080 acceptance criteria (ADR-llm-pipeline):
//! - **5e (W-extract):** validated proposals land as low-trust `derived_keys`
//!   (allowed key space, no overwrite of existing keys, verbatim evidence
//!   span — invalid ones are dropped, never errors); schema-invalid
//!   generative output is a POISON item (no write; the fact stays committed
//!   and keyed); idempotent re-run; `catch_up` re-derives holes and stale
//!   stamps (ADR Decision 5: a version bump is a backfill event).
//! - **5c (W-digest):** a burst of N facts on one scope coalesces into ONE
//!   rollup (debounce/coalesce, ADR Decision 2b) with `member_fact_ids` (the
//!   R1 resolve-down path), story lineage, and a summary embedding; a re-roll
//!   bumps `scope_version`; a late rollup for an older version is a no-op
//!   (ADR Decision 6).
//! - **5d (W-categorize):** validated emergent labels land as
//!   `category_facets` rows AND on the `facts.categories` facet surface;
//!   poison on schema-invalid output; idempotent.
//! - **Pluggability:** the workers run against BOTH the deterministic
//!   `TemplateGenerator` default and a scripted test-double `Generator` (the
//!   port is the seam — no model server anywhere in the suite).
//! - **OTel (WS-7 signal catalog):** worker applies are `memkeys.worker.process`
//!   CONSUMER spans linked to the commit, and generative calls are the gen_ai
//!   CLIENT span `chat <model>` with the pinned gen_ai.* attributes and NO
//!   prompt/completion content.

// The global-provider lock is HELD across awaits on purpose (same as the
// otel_signals.rs suite): the awaits under it are exactly what must be
// serialized against other provider-swapping tests.
#![allow(clippy::await_holding_lock)]
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::Utc;
use memkeys_server::envelope::Envelope;
use memkeys_server::generative::{GenError, GenTask, Generator, TemplateGenerator};
use memkeys_server::llm_worker::{CategorizeWorker, DigestWorker, ExtractWorker};
use memkeys_server::pg::PgStore;
use memkeys_server::pg_graph::PgGraph;
use memkeys_server::pipeline::{CallerContext, NoopResolver};
use memkeys_server::semantic::{Embedder, HashEmbedder};
use memkeys_server::service::MemKeysService;
use uuid::Uuid;

fn db_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

static TENANT_SEQ: AtomicU16 = AtomicU16::new(0);

fn run_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("run-{nanos:x}")
    })
}

/// Unique tenant per test (nonce + index) — full isolation on the shared DB.
fn next_tenant() -> String {
    format!(
        "m6test-{}-{}",
        run_nonce(),
        TENANT_SEQ.fetch_add(1, Ordering::SeqCst)
    )
}

/// A scripted test-double `Generator` (the pluggability seam: the workers are
/// driven through the PORT, and any endpoint behavior — valid output, empty
/// output, schema-invalid poison — is scriptable without a model server).
#[derive(Default)]
struct ScriptedGenerator {
    extract: Mutex<Option<serde_json::Value>>,
    categorize: Mutex<Option<serde_json::Value>>,
    summarize: Mutex<Option<serde_json::Value>>,
    summarize_calls: AtomicUsize,
}

impl ScriptedGenerator {
    fn with_extract(v: serde_json::Value) -> Self {
        Self {
            extract: Mutex::new(Some(v)),
            ..Self::default()
        }
    }

    fn with_categorize(v: serde_json::Value) -> Self {
        Self {
            categorize: Mutex::new(Some(v)),
            ..Self::default()
        }
    }

    /// Override the summarize output (e.g. an empty summary → POISON) to drive
    /// the digest poison path (ISI-6192 B2).
    fn with_summarize(v: serde_json::Value) -> Self {
        Self {
            summarize: Mutex::new(Some(v)),
            ..Self::default()
        }
    }
}

#[async_trait::async_trait]
impl Generator for ScriptedGenerator {
    fn model_id(&self) -> &str {
        "scripted-test-double"
    }

    fn system(&self) -> &'static str {
        "test-double"
    }

    async fn generate(
        &self,
        task: GenTask,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, GenError> {
        match task {
            GenTask::Extract => Ok(self
                .extract
                .lock()
                .expect("scripted lock")
                .clone()
                .unwrap_or(serde_json::json!({ "proposed_keys": [] }))),
            GenTask::Categorize => Ok(self
                .categorize
                .lock()
                .expect("scripted lock")
                .clone()
                .unwrap_or(serde_json::json!({ "labels": [] }))),
            GenTask::Summarize => {
                self.summarize_calls.fetch_add(1, Ordering::SeqCst);
                if let Some(v) = self.summarize.lock().expect("scripted lock").clone() {
                    return Ok(v);
                }
                let members = input["member_facts"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0);
                Ok(serde_json::json!({
                    "summary": format!("scripted summary of {members} facts"),
                    "story": format!("rolled up {members} facts by the scripted double")
                }))
            }
        }
    }
}

struct Ctx {
    service: MemKeysService,
    pool: sqlx::PgPool,
    extract: Arc<ExtractWorker>,
    digest: Arc<DigestWorker>,
    categorize: Arc<CategorizeWorker>,
}

/// A connected service with the graph projection + all three M6 workers wired
/// against a real Postgres. `None` when TEST_DATABASE_URL is unset (skip).
async fn ctx(generator: Arc<dyn Generator>) -> Option<Ctx> {
    let url = db_url()?;
    let store = Arc::new(PgStore::connect(&url).await.expect("connect + migrate"));
    let pool = store.pool().clone();
    let graph = Arc::new(PgGraph::from_pool(pool.clone()));
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new());
    let extract = Arc::new(ExtractWorker::new(pool.clone(), generator.clone()));
    let categorize = Arc::new(CategorizeWorker::new(pool.clone(), generator.clone()));
    let digest = Arc::new(DigestWorker::new(pool.clone(), generator.clone(), embedder));
    let service =
        MemKeysService::with_graph_projection(store.clone(), Arc::new(NoopResolver), graph)
            .with_extract_worker(extract.clone())
            .with_categorize_worker(categorize.clone())
            .with_digest_worker(digest.clone());
    Some(Ctx {
        service,
        pool,
        extract,
        digest,
        categorize,
    })
}

async fn ctx_template() -> Option<Ctx> {
    ctx(Arc::new(TemplateGenerator::new())).await
}

/// Ingest one fact through the REAL write path (stages 1-4 + the post-commit
/// stage-5 dispatch) and return its id. `body`-distinct facts get distinct
/// ids (the body-hash idempotency fallback, §6 stage 1).
async fn ingest(ctx: &Ctx, tenant: &str, body: &str, run: &str, thread: Option<&str>) -> Uuid {
    let mut entity = serde_json::json!({ "run": run });
    if let Some(t) = thread {
        entity["thread"] = serde_json::json!(t);
    }
    let env: Envelope = serde_json::from_value(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-18T10:00:00Z" },
        "entity": entity,
        "fact": { "body": body }
    }))
    .expect("valid envelope");
    let caller = CallerContext {
        tenant_id: tenant.into(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let receipt = ctx
        .service
        .ingest(&env, &caller, Utc::now())
        .await
        .expect("ingest ok");
    assert!(!receipt.duplicate);
    receipt.fact_id
}

async fn derived_keys(ctx: &Ctx, fact_id: Uuid) -> Vec<(String, String, String)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT key, value, source FROM derived_keys WHERE fact_id = $1 ORDER BY key",
    )
    .bind(fact_id)
    .fetch_all(&ctx.pool)
    .await
    .expect("derived_keys query");
    rows
}

async fn digest_row(
    ctx: &Ctx,
    tenant: &str,
    scope: &str,
    scope_id: &str,
) -> Option<(i64, Vec<Uuid>, String, String)> {
    sqlx::query_as::<_, (i64, Vec<Uuid>, String, String)>(
        "SELECT scope_version, member_fact_ids, summary, story FROM digests \
         WHERE tenant_id = $1 AND scope = $2 AND scope_id = $3",
    )
    .bind(tenant)
    .bind(scope)
    .bind(scope_id)
    .fetch_optional(&ctx.pool)
    .await
    .expect("digest query")
}

async fn categories_of(ctx: &Ctx, fact_id: Uuid) -> Vec<String> {
    let row: (Vec<String>,) = sqlx::query_as("SELECT categories FROM facts WHERE id = $1")
        .bind(fact_id)
        .fetch_one(&ctx.pool)
        .await
        .expect("fact query");
    row.0
}

// ---------------------------------------------------------------------------
// Stage 5e — W-extract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stage_5e_extracts_validated_derived_keys_with_the_default_generator() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(
        &ctx,
        &tenant,
        "We decided in ISI-6080 to ship PR 19 for the memkeys rollout",
        "run-1",
        Some("t-1"),
    )
    .await;

    let applied = ctx.extract.drain().await;
    assert!(applied >= 1, "extract worker applied the enqueued fact");

    let keys = derived_keys(&ctx, fact).await;
    let by_key: std::collections::HashMap<&str, &str> = keys
        .iter()
        .map(|(k, v, _s)| (k.as_str(), v.as_str()))
        .collect();
    // The template heuristic proposes issue + pr from verbatim evidence.
    assert_eq!(by_key.get("entity.issue"), Some(&"ISI-6080"));
    assert_eq!(by_key.get("entity.pr"), Some(&"19"));
    // R8: proposals are low-trust, source-stamped, never authoritative.
    for (_, _, source) in &keys {
        assert_eq!(source, "llm-extract");
    }
    // The SoT record itself is untouched — derived keys never overwrite the
    // client/server-stamped columns.
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT entity_issue, entity_pr FROM facts WHERE id = $1")
            .bind(fact)
            .fetch_one(&ctx.pool)
            .await
            .expect("fact query");
    assert_eq!(row, (None, None));
}

#[tokio::test]
async fn stage_5e_drops_unknown_keys_overwrites_and_bad_evidence() {
    let scripted = serde_json::json!({ "proposed_keys": [
        // Unknown key space → dropped.
        { "key": "entity.tenant", "value": "acme", "confidence": 1.0,
          "evidence_span": "acme" },
        // Already-present key (entity.run came from the envelope) → dropped.
        { "key": "entity.run", "value": "run-evil", "confidence": 1.0,
          "evidence_span": "run" },
        // Evidence NOT a verbatim substring → dropped (anti-hallucination).
        { "key": "entity.pr", "value": "19", "confidence": 1.0,
          "evidence_span": "PR 19 exists somewhere else" },
        // Valid proposal with substring evidence → accepted.
        { "key": "entity.pr", "value": "19", "confidence": 1.0,
          "evidence_span": "PR 19" }
    ]});
    let Some(ctx) = ctx(Arc::new(ScriptedGenerator::with_extract(scripted))).await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(&ctx, &tenant, "see PR 19 merged", "run-1", None).await;

    ctx.extract.drain().await;

    let keys = derived_keys(&ctx, fact).await;
    assert_eq!(keys.len(), 1, "only the valid proposal persists: {keys:?}");
    assert_eq!(keys[0].0, "entity.pr");
    assert_eq!(keys[0].1, "19");
    // And the client-stamped key was NOT overwritten by the proposal.
    let run: (String,) = sqlx::query_as("SELECT entity_run FROM facts WHERE id = $1")
        .bind(fact)
        .fetch_one(&ctx.pool)
        .await
        .expect("fact query");
    assert_eq!(run.0, "run-1");
}

#[tokio::test]
async fn stage_5e_poison_on_schema_invalid_output_keeps_the_fact_committed() {
    // A schema-invalid generative output is a POISON condition (ADR Decision 8)
    // — never a silent bad write, never a write-path failure.
    let garbage = serde_json::json!({ "proposed_keys": "not-an-array" });
    let Some(ctx) = ctx(Arc::new(ScriptedGenerator::with_extract(garbage))).await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(
        &ctx,
        &tenant,
        "valid body for a poisoned extraction",
        "run-1",
        None,
    )
    .await;

    ctx.extract.drain().await;
    ctx.extract.drain().await; // second drain: the poison item dead-letters

    // No derived keys were written...
    assert!(derived_keys(&ctx, fact).await.is_empty());
    // ...and the fact stays committed and keyed (R2: a projection failure
    // never blocks stages 1-4).
    let row: (bool,) = sqlx::query_as("SELECT keyed FROM facts WHERE id = $1")
        .bind(fact)
        .fetch_one(&ctx.pool)
        .await
        .expect("fact query");
    assert!(row.0, "the fact remains committed and keyed");
}

#[tokio::test]
async fn stage_5e_is_idempotent_and_catch_up_fills_holes_and_stale_stamps() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(
        &ctx,
        &tenant,
        "Decided to track ISI-6080 as the M6 plan; PR 19 carries it",
        "run-1",
        None,
    )
    .await;

    ctx.extract.drain().await;
    let first = derived_keys(&ctx, fact).await;
    assert!(!first.is_empty());

    // Idempotent re-run (ADR Decision 6): a replay converges, no duplicates.
    ctx.extract.drain().await;
    let second = derived_keys(&ctx, fact).await;
    assert_eq!(
        first, second,
        "re-running the worker must not duplicate rows"
    );

    // Hole: the projection rows are lost (simulated) — catch_up re-derives
    // FROM the FactStore (D3: the projection is rebuildable).
    sqlx::query("DELETE FROM derived_keys WHERE fact_id = $1")
        .bind(fact)
        .execute(&ctx.pool)
        .await
        .expect("delete rows");
    sqlx::query("DELETE FROM derivation_runs WHERE kind = 'llm_extract' AND ref_id = $1")
        .bind(fact)
        .execute(&ctx.pool)
        .await
        .expect("delete stamp");
    let n = ctx.extract.catch_up(&tenant).await.expect("catch_up");
    assert!(n >= 1, "the hole fact was re-processed");
    assert_eq!(
        derived_keys(&ctx, fact).await,
        first,
        "re-derived identically"
    );

    // Stale stamp (ADR Decision 5: a version bump is a backfill event): a
    // stamp recorded under an OLD prompt version marks the artifact stale.
    sqlx::query("UPDATE derivation_runs SET prompt_version = 'extract/v0' WHERE ref_id = $1")
        .bind(fact)
        .execute(&ctx.pool)
        .await
        .expect("age the stamp");
    let n = ctx.extract.catch_up(&tenant).await.expect("catch_up");
    assert!(n >= 1, "the stale-stamped fact was re-derived");
    let stamp: (String,) = sqlx::query_as(
        "SELECT prompt_version FROM derivation_runs WHERE kind = 'llm_extract' AND ref_id = $1",
    )
    .bind(fact)
    .fetch_one(&ctx.pool)
    .await
    .expect("stamp query");
    assert_eq!(
        stamp.0, "extract/v1",
        "the stamp was refreshed to the active version"
    );
}

// ---------------------------------------------------------------------------
// Stage 5c — W-digest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stage_5c_coalesces_a_burst_into_one_rollup_with_members_and_story() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant = next_tenant();
    // A burst of 3 facts on ONE thread + 1 on another.
    ingest(&ctx, &tenant, "first: chose postgres", "run-1", Some("t-1")).await;
    ingest(
        &ctx,
        &tenant,
        "second: p95 under 150ms",
        "run-1",
        Some("t-1"),
    )
    .await;
    ingest(
        &ctx,
        &tenant,
        "third: rollout blocked by a bug",
        "run-1",
        Some("t-1"),
    )
    .await;
    ingest(
        &ctx,
        &tenant,
        "elsewhere: security token",
        "run-2",
        Some("t-2"),
    )
    .await;

    let rolled = ctx.digest.drain().await;
    assert!(rolled >= 2, "one rollup per dirty scope, got {rolled}");

    // The t-1 thread digest: ONE row, all 3 members (R1 resolve-down), the
    // story lineage, and a summary embedding were written.
    let (version, members, summary, story) = digest_row(&ctx, &tenant, "thread", "t-1")
        .await
        .expect("thread digest exists");
    assert!(
        version > 0,
        "scope_version is the member-read wall-clock epoch"
    );
    assert_eq!(members.len(), 3, "member_fact_ids carries every member");
    assert!(
        summary.contains("3 facts"),
        "summary reflects the rollup: {summary}"
    );
    assert!(
        story.contains("rolled up 3 facts"),
        "story lineage present: {story}"
    );

    // Exactly ONE thread-scope digest per scope_id (the coalesced debounce:
    // a burst of N facts triggers ONE rollup, not N).
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT scope_id FROM digests WHERE tenant_id = $1 AND scope = 'thread'")
            .bind(&tenant)
            .fetch_all(&ctx.pool)
            .await
            .expect("digest scope query");
    assert_eq!(
        rows.len(),
        2,
        "one digest per (scope, scope_id), got {rows:?}"
    );

    // The run scope rolled up too (every fact shares run-1 / run-2).
    let (run_v, run_members, _, _) = digest_row(&ctx, &tenant, "run", "run-1")
        .await
        .expect("run digest exists");
    assert!(run_v > 0 && run_members.len() == 3);
}

#[tokio::test]
async fn stage_5c_rerolls_on_new_members_and_a_late_old_rollup_is_a_noop() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant = next_tenant();
    ingest(
        &ctx,
        &tenant,
        "first decision on the digest thread",
        "run-1",
        Some("t-1"),
    )
    .await;
    ctx.digest.drain().await;
    let (v1, m1, _, _) = digest_row(&ctx, &tenant, "thread", "t-1")
        .await
        .expect("first rollup");
    assert_eq!(m1.len(), 1);
    assert!(v1 > 0, "scope_version is the member-read wall-clock epoch");

    // A new member arrives → the next drain re-rolls the scope at a STRICTLY
    // LATER version (monotonic read-time epoch; ISI-6192 B1) with more members.
    ingest(
        &ctx,
        &tenant,
        "second decision on the digest thread",
        "run-1",
        Some("t-1"),
    )
    .await;
    ctx.digest.drain().await;
    let (v2, m2, _, _) = digest_row(&ctx, &tenant, "thread", "t-1")
        .await
        .expect("second rollup");
    assert_eq!(m2.len(), 2);
    assert!(v2 > v1, "a later rollup read a later snapshot: {v2} > {v1}");
    assert!(m2.len() > m1.len());

    // Late-rollup guard (ADR Decision 6): a stored rollup at a HIGHER
    // scope_version makes an incoming older rollup a no-op. Simulate a
    // far-future stored version (beyond any wall-clock epoch), enqueue one more
    // fact, drain: the incoming rollup's now()-epoch is <= the stored future
    // value → no overwrite.
    let future = 9_000_000_000_000_000_000i64; // ~year 287396 in micros, < i64::MAX
    sqlx::query("UPDATE digests SET scope_version = $2 WHERE tenant_id = $1 AND scope = 'thread' AND scope_id = 't-1'")
        .bind(&tenant)
        .bind(future)
        .execute(&ctx.pool)
        .await
        .expect("bump stored version");
    ingest(
        &ctx,
        &tenant,
        "third decision on the digest thread",
        "run-1",
        Some("t-1"),
    )
    .await;
    ctx.digest.drain().await;
    let (v3, m3, _, _) = digest_row(&ctx, &tenant, "thread", "t-1")
        .await
        .expect("guarded rollup");
    assert_eq!(v3, future, "the older incoming rollup was a no-op");
    assert_eq!(
        m3.len(),
        2,
        "members were not overwritten by the older rollup"
    );
}

#[tokio::test]
async fn stage_5c_catch_up_fills_missing_scope_rollups() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant = next_tenant();
    ingest(
        &ctx,
        &tenant,
        "orphan fact nobody drained",
        "run-1",
        Some("t-1"),
    )
    .await;
    // No drain — the queue item is "lost" (crash between commit and worker).
    let n = ctx.digest.catch_up(&tenant).await.expect("catch_up");
    assert!(n >= 1, "the hole scope was re-rolled from the SoT");
    let (v, m, _, _) = digest_row(&ctx, &tenant, "thread", "t-1")
        .await
        .expect("catch-up rollup exists");
    assert!(v > 0, "scope_version is the member-read wall-clock epoch");
    assert_eq!(m.len(), 1);
}

// ---------------------------------------------------------------------------
// Stage 5d — W-categorize
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stage_5d_writes_facets_and_mirrors_onto_the_keyed_record() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(
        &ctx,
        &tenant,
        "The rollout is blocked by a security token bug",
        "run-1",
        None,
    )
    .await;

    ctx.categorize.drain().await;

    let facets: Vec<(String,)> =
        sqlx::query_as("SELECT label FROM category_facets WHERE fact_id = $1 ORDER BY label")
            .bind(fact)
            .fetch_all(&ctx.pool)
            .await
            .expect("facet query");
    let labels: Vec<&str> = facets.iter().map(|(l,)| l.as_str()).collect();
    assert!(labels.contains(&"blocker"), "emergent labels: {labels:?}");
    assert!(labels.contains(&"security"));
    assert!(labels.contains(&"rollout"));

    // The facet surface on the keyed record (§5.4.3 "as facets"): the
    // derived-attribute projection column provisioned in migration 0001.
    let mirrored = categories_of(&ctx, fact).await;
    let mirrored: Vec<&str> = mirrored.iter().map(|s| s.as_str()).collect();
    assert!(
        mirrored.contains(&"blocker"),
        "mirrored categories: {mirrored:?}"
    );
    assert_eq!(mirrored.len(), labels.len());

    // Idempotent re-run: converges, no duplicates.
    ctx.categorize.drain().await;
    let facets2: Vec<(String,)> =
        sqlx::query_as("SELECT label FROM category_facets WHERE fact_id = $1 ORDER BY label")
            .bind(fact)
            .fetch_all(&ctx.pool)
            .await
            .expect("facet query");
    assert_eq!(facets, facets2);
}

#[tokio::test]
async fn stage_5d_poison_on_schema_invalid_output_keeps_the_fact_committed() {
    let garbage = serde_json::json!({ "labels": { "not": "an array" } });
    let Some(ctx) = ctx(Arc::new(ScriptedGenerator::with_categorize(garbage))).await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(
        &ctx,
        &tenant,
        "body of a poisoned categorization",
        "run-1",
        None,
    )
    .await;

    ctx.categorize.drain().await;
    ctx.categorize.drain().await; // second drain: dead-letter

    let facets: Vec<(String,)> =
        sqlx::query_as("SELECT label FROM category_facets WHERE fact_id = $1")
            .bind(fact)
            .fetch_all(&ctx.pool)
            .await
            .expect("facet query");
    assert!(facets.is_empty(), "no hallucinated structure was persisted");
    assert!(categories_of(&ctx, fact).await.is_empty());
    let row: (bool,) = sqlx::query_as("SELECT keyed FROM facts WHERE id = $1")
        .bind(fact)
        .fetch_one(&ctx.pool)
        .await
        .expect("fact query");
    assert!(row.0, "the fact remains committed and keyed");
}

// ---------------------------------------------------------------------------
// Multi-tenancy (R6) — the derived views inherit the owning fact's floor.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn derived_views_are_tenant_partitioned() {
    let Some(ctx) = ctx_template().await else {
        return;
    };
    let tenant_a = next_tenant();
    let tenant_b = next_tenant();
    let fact_a = ingest(
        &ctx,
        &tenant_a,
        "tenant A works on ISI-6080 for the rollout",
        "run-a",
        Some("t-a"),
    )
    .await;
    let fact_b = ingest(
        &ctx,
        &tenant_b,
        "tenant B also mentions ISI-6080 in a rollout",
        "run-b",
        Some("t-b"),
    )
    .await;
    ctx.extract.drain().await;
    ctx.digest.drain().await;
    ctx.categorize.drain().await;

    // Tenant A's derived keys/digests never carry tenant B's fact, and every
    // derived row inherits its owning fact's tenant floor (R6) — a tenant
    // column mismatch would be a projection leak.
    let a_keys = derived_keys(&ctx, fact_a).await;
    let b_keys = derived_keys(&ctx, fact_b).await;
    assert!(!a_keys.is_empty() && !b_keys.is_empty());
    for (fact, expected_t) in [(fact_a, &tenant_a), (fact_b, &tenant_b)] {
        let (dk_t,): (String,) = sqlx::query_as(
            "SELECT tenant_id FROM derived_keys WHERE fact_id = $1 ORDER BY key LIMIT 1",
        )
        .bind(fact)
        .fetch_one(&ctx.pool)
        .await
        .expect("derived_keys tenant query");
        assert_eq!(&dk_t, expected_t);
        let (cf_t,): (String,) =
            sqlx::query_as("SELECT tenant_id FROM category_facets WHERE fact_id = $1 LIMIT 1")
                .bind(fact)
                .fetch_one(&ctx.pool)
                .await
                .expect("category_facets tenant query");
        assert_eq!(&cf_t, expected_t);
    }
    let (v, members, _, _) = digest_row(&ctx, &tenant_a, "thread", "t-a")
        .await
        .expect("digest");
    assert!(v > 0, "scope_version is the member-read wall-clock epoch");
    assert_eq!(
        members,
        vec![fact_a],
        "only tenant A's own fact is a member"
    );
}

// ---------------------------------------------------------------------------
// OTel (WS-7 signal catalog) — worker + gen_ai spans.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn workers_emit_catalog_spans_and_gen_ai_client_spans() {
    use opentelemetry::global;
    use opentelemetry_sdk::trace::InMemorySpanExporterBuilder;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(exporter.clone())
        .build();
    global::set_tracer_provider(provider.clone());

    let Some(ctx) = ctx_template().await else {
        drop(provider);
        return;
    };
    let tenant = next_tenant();
    ingest(
        &ctx,
        &tenant,
        "Decided in ISI-6080 that the rollout ships with PR 19",
        "run-1",
        Some("t-1"),
    )
    .await;
    ctx.extract.drain().await;
    ctx.digest.drain().await;
    ctx.categorize.drain().await;

    let spans = exporter.get_finished_spans().expect("spans");
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();

    // The gen_ai CLIENT span (semconv convention "{operation} {model}").
    let gen = spans
        .iter()
        .find(|s| s.name == "chat memkeys-template-v1")
        .unwrap_or_else(|| panic!("gen_ai span not found in {names:?}"));
    assert_eq!(
        gen.span_kind,
        opentelemetry::trace::SpanKind::Client,
        "the signal catalog kinds the LLM call as CLIENT"
    );
    let has = |k: &str| gen.attributes.iter().any(|kv| kv.key.as_str() == k);
    assert!(has("gen_ai.operation.name"));
    assert!(has("gen_ai.request.model"));
    assert!(has("gen_ai.usage.input_tokens"));
    // Forbidden content: no prompt/completion payload attributes, ever.
    for kv in &gen.attributes {
        let k = kv.key.as_str();
        assert!(
            !k.contains("prompt") && !k.contains("completion") && k != "gen_ai.prompt",
            "forbidden gen_ai attribute {k}"
        );
    }

    // The derived-view worker spans: CONSUMER, own trace root, linked to the
    // originating commit, worker/projection kinded.
    for (worker_kind, projection_kind) in [
        ("llm_extract", "derived_keys"),
        ("digest_rollup", "digest"),
        ("categorize", "category_facets"),
    ] {
        // Scope to THIS test's tenant: parallel tests in the same process
        // also emit worker spans (e.g. catch-up applies, which legitimately
        // carry no commit link) into the shared global provider.
        let span = spans
            .iter()
            .find(|s| {
                s.name == "memkeys.worker.process"
                    && s.attributes.iter().any(|kv| {
                        kv.key.as_str() == "memkeys.tenant.id" && kv.value.as_str() == tenant
                    })
                    && s.attributes.iter().any(|kv| {
                        kv.key.as_str() == "memkeys.worker.kind" && kv.value.as_str() == worker_kind
                    })
            })
            .unwrap_or_else(|| panic!("worker span {worker_kind} not found in {names:?}"));
        assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Consumer);
        assert!(
            span.parent_span_id == opentelemetry::trace::SpanId::INVALID,
            "the worker is its own trace root (temporally decoupled)"
        );
        assert!(
            !span.links.is_empty(),
            "the worker carries a SPAN_LINK to the originating commit"
        );
        assert!(span.attributes.iter().any(|kv| {
            kv.key.as_str() == "memkeys.projection.kind" && kv.value.as_str() == projection_kind
        }));
    }

    provider.shutdown().expect("provider shutdown");
}

// ===========================================================================
// ISI-6192 follow-up regressions (CR ISI-6190)
// ===========================================================================

/// B1: a NET member-count DECREASE (a retention sweep here; equivalently a
/// supersession whose superseder leaves the scope) must still roll up. The old
/// `scope_version = member count` wedged the `scope_version <= EXCLUDED` guard:
/// a smaller count was rejected as "stale" and the digest froze on an outdated
/// summary + superseded member_fact_ids. The monotonic read-time epoch fixes it.
#[tokio::test]
async fn stage_5c_member_count_decrease_still_rolls_up() {
    let Some(ctx) = ctx(Arc::new(ScriptedGenerator::default())).await else {
        return;
    };
    let tenant = next_tenant();
    let a = ingest(&ctx, &tenant, "decrease fact A", "run-dec", Some("t-dec")).await;
    let b = ingest(&ctx, &tenant, "decrease fact B", "run-dec", Some("t-dec")).await;
    let c = ingest(&ctx, &tenant, "decrease fact C", "run-dec", Some("t-dec")).await;
    ctx.digest.drain().await;

    let (v1, m1, summary1, _) = digest_row(&ctx, &tenant, "thread", "t-dec")
        .await
        .expect("first rollup");
    assert_eq!(m1.len(), 3);
    assert!(summary1.contains("3 facts"), "summary at peak: {summary1}");

    // Retention sweep removes A and B (no FKs reference facts); the live scope
    // shrinks 3 → 1. Re-mark the scope dirty via the surviving member C (does
    // NOT add a member) and drain.
    sqlx::query("DELETE FROM facts WHERE tenant_id = $1 AND id = ANY($2)")
        .bind(&tenant)
        .bind(vec![a, b])
        .execute(&ctx.pool)
        .await
        .expect("retention sweep");
    ctx.digest.enqueue(&tenant, c, None);
    ctx.digest.drain().await;

    let (v2, m2, summary2, _) = digest_row(&ctx, &tenant, "thread", "t-dec")
        .await
        .expect("post-decrease rollup");
    assert_eq!(
        m2,
        vec![c],
        "the digest regenerated from the 1 surviving member"
    );
    assert!(
        summary2.contains("1 facts"),
        "summary reflects the smaller current scope: {summary2}"
    );
    assert!(
        v2 > v1,
        "the smaller rollup still won (monotonic read-time version): {v2} > {v1}"
    );
}

/// B2: a deterministic poison (schema-invalid extract output) is durably
/// quarantined — a `derivation_runs` stamp is written so `catch_up` SKIPS it
/// instead of re-driving the generator every tick (real cost on HttpGenerator).
#[tokio::test]
async fn stage_5e_poison_is_quarantined_and_catch_up_skips_it() {
    let garbage = serde_json::json!({ "proposed_keys": "not-an-array" });
    let Some(ctx) = ctx(Arc::new(ScriptedGenerator::with_extract(garbage))).await else {
        return;
    };
    let tenant = next_tenant();
    let fact = ingest(
        &ctx,
        &tenant,
        "a fact whose extraction poisons",
        "run-1",
        None,
    )
    .await;

    ctx.extract.drain().await; // first sight of the deterministic poison → quarantine

    assert!(derived_keys(&ctx, fact).await.is_empty());
    let stamps: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM derivation_runs WHERE kind = 'llm_extract' AND ref_id = $1",
    )
    .bind(fact)
    .fetch_one(&ctx.pool)
    .await
    .expect("stamp count");
    assert_eq!(stamps.0, 1, "the poison fact carries a quarantine stamp");

    // catch_up now treats the fact as processed-at-this-version → no re-drive.
    let reaped = ctx.extract.catch_up(&tenant).await.expect("catch_up ok");
    assert_eq!(reaped, 0, "the quarantined fact is not re-extracted");
    assert!(derived_keys(&ctx, fact).await.is_empty());
}

/// B2: a poison SCOPE must not `?`-abort the digest `catch_up` backfill — at
/// boot that propagates to `main()` and the server never starts. Every scope
/// here poisons; `catch_up` must return Ok (collect-and-continue), quarantine
/// each scope, and a second pass must SKIP them (no re-drive).
#[tokio::test]
async fn stage_5c_catch_up_survives_and_quarantines_poison_scopes() {
    let gen = Arc::new(ScriptedGenerator::with_summarize(
        serde_json::json!({ "summary": "" }), // empty summary → InvalidOutput poison
    ));
    let Some(ctx) = ctx(gen.clone()).await else {
        return;
    };
    let tenant = next_tenant();
    // Two threads sharing one run → scopes {t-1, t-2, run-1}: 3 poison scopes.
    ingest(&ctx, &tenant, "poison scope fact one", "run-1", Some("t-1")).await;
    ingest(&ctx, &tenant, "poison scope fact two", "run-1", Some("t-2")).await;

    // catch_up reads scopes straight from the SoT (not the drained queue); every
    // rollup poisons. It must NOT abort — Ok with 0 successful rollups.
    let applied = ctx
        .digest
        .catch_up(&tenant)
        .await
        .expect("catch_up must not abort");
    assert_eq!(applied, 0, "no scope rolled up (all poison)");
    let after_first = gen.summarize_calls.load(Ordering::SeqCst);
    assert_eq!(
        after_first, 3,
        "all 3 scopes were attempted (collect-and-continue)"
    );

    let quarantined: (i64,) = sqlx::query_as(
        "SELECT count(*) FROM derivation_runs WHERE kind = 'digest_rollup' AND tenant_id = $1",
    )
    .bind(&tenant)
    .fetch_one(&ctx.pool)
    .await
    .expect("stamp count");
    assert_eq!(quarantined.0, 3, "each poison scope is durably quarantined");
    assert!(
        digest_row(&ctx, &tenant, "thread", "t-1").await.is_none(),
        "no digest row for a poison scope"
    );

    // Second pass: all scopes quarantined at the current version → skipped, no
    // further generator calls.
    let applied2 = ctx
        .digest
        .catch_up(&tenant)
        .await
        .expect("second catch_up ok");
    assert_eq!(applied2, 0);
    assert_eq!(
        gen.summarize_calls.load(Ordering::SeqCst),
        after_first,
        "quarantined scopes are not re-driven"
    );
}
