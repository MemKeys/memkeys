//! M7 integration tests — the reconciler / backfill (ISI-6081) against a real
//! Postgres, gated on TEST_DATABASE_URL (same harness/env guard as the
//! M1/M2/M5/M6 suites; CI points it at `pgvector/pgvector:pg18` so migrations
//! 0003/0004/0007 apply).
//!
//! Coverage per the ISI-6081 acceptance criteria:
//! - the reconciler detects + backfills projection HOLES across every wired
//!   tier (graph 5a, vector 5b, extract 5e, categorize 5d, digest 5c) from the
//!   FactStore — committed facts with no projected artifact are rebuilt;
//! - a model-stamp change triggers a REBUILD without downtime: a fact_vectors
//!   row embedded under a stale embedder `model_id` is re-embedded in place
//!   (overwrite, never a duplicate), mirroring the LLM-tier stale-stamp path;
//! - IDEMPOTENCY: a second pass over a converged tenant is a no-op (0 applied,
//!   no duplicate rows);
//! - RESUME: a pass interrupted after projecting some facts picks up exactly
//!   the remaining ones on the next call (durable stamps, no lost cursor).

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use memkeys_server::generative::{Generator, TemplateGenerator};
use memkeys_server::llm_worker::{CategorizeWorker, DigestWorker, ExtractWorker};
use memkeys_server::model::{EntityKeys, FactType, ProcessedFact};
use memkeys_server::pg::PgStore;
use memkeys_server::pg_graph::PgGraph;
use memkeys_server::pg_vector::PgVectorIndex;
use memkeys_server::reconciler::Reconciler;
use memkeys_server::semantic::{Embedder, HashEmbedder, VectorIndex, HASH_EMBED_MODEL_ID};
use memkeys_server::store::FactStore;
use memkeys_server::worker::{GraphProjectionWorker, VectorProjectionWorker};
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
        "m7test-{}-{}",
        run_nonce(),
        TENANT_SEQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Ctx {
    pool: sqlx::PgPool,
    store: Arc<PgStore>,
    vector_worker: Arc<VectorProjectionWorker>,
    reconciler: Reconciler,
}

/// All five tiers wired against a real Postgres + a single reconciler over them.
/// `None` when TEST_DATABASE_URL is unset (skip). Uses the deterministic
/// TemplateGenerator + HashEmbedder so the suite needs no model server.
async fn ctx() -> Option<Ctx> {
    let url = db_url()?;
    let store = Arc::new(PgStore::connect(&url).await.expect("connect + migrate"));
    let pool = store.pool().clone();
    let graph = Arc::new(PgGraph::from_pool(pool.clone()));
    let embedder = Arc::new(HashEmbedder::new());
    let index: Arc<dyn VectorIndex> = Arc::new(PgVectorIndex::from_pool(pool.clone()));
    let generator: Arc<dyn Generator> = Arc::new(TemplateGenerator::new());

    let graph_worker = Arc::new(GraphProjectionWorker::new(graph.clone()));
    let vector_worker = Arc::new(VectorProjectionWorker::new(
        pool.clone(),
        embedder.clone() as Arc<dyn Embedder>,
        index.clone(),
    ));
    let extract = Arc::new(ExtractWorker::new(pool.clone(), generator.clone()));
    let categorize = Arc::new(CategorizeWorker::new(pool.clone(), generator.clone()));
    let digest = Arc::new(DigestWorker::new(
        pool.clone(),
        generator.clone(),
        embedder.clone() as Arc<dyn Embedder>,
    ));

    let reconciler = Reconciler::new(vec![
        graph_worker,
        vector_worker.clone(),
        extract,
        categorize,
        digest,
    ]);

    Some(Ctx {
        pool,
        store,
        vector_worker,
        reconciler,
    })
}

fn fact(tenant: &str, n: u128, run: &str, body: &str) -> ProcessedFact {
    let now = Utc::now();
    ProcessedFact {
        id: Uuid::from_u128(n ^ (fnv(tenant) as u128)),
        tenant_id: tenant.into(),
        fact_type: FactType::Decision,
        subject: Some("db".into()),
        body: body.into(),
        body_hash: format!("h-{n}-{}", fnv(body)),
        confidence: None,
        supersedes: None,
        event_time: now,
        ingested_at: now,
        entity: EntityKeys {
            run: Some(run.into()),
            project: Some("proj-1".into()),
            thread: Some("thr-1".into()),
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

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

async fn count(pool: &sqlx::PgPool, sql: &str, tenant: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(tenant)
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

/// Every tier's projected-row count for a tenant: (edges, vectors, extract
/// stamps, categorize stamps, digests).
async fn projected_counts(pool: &sqlx::PgPool, tenant: &str) -> (i64, i64, i64, i64, i64) {
    (
        count(pool, "SELECT count(*) FROM edges WHERE tenant = $1", tenant).await,
        count(
            pool,
            "SELECT count(*) FROM fact_vectors WHERE tenant_id = $1",
            tenant,
        )
        .await,
        count(
            pool,
            "SELECT count(*) FROM derivation_runs WHERE kind = 'llm_extract' AND tenant_id = $1",
            tenant,
        )
        .await,
        count(
            pool,
            "SELECT count(*) FROM derivation_runs WHERE kind = 'categorize' AND tenant_id = $1",
            tenant,
        )
        .await,
        count(
            pool,
            "SELECT count(*) FROM digests WHERE tenant_id = $1",
            tenant,
        )
        .await,
    )
}

#[tokio::test]
async fn reconciler_backfills_holes_across_all_tiers() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    // Commit two facts straight to the SoT with NO worker enqueue — every
    // derived tier is a pure hole.
    for (n, body) in [
        (1u128, "chose postgres for durability"),
        (2, "added an index"),
    ] {
        ctx.store
            .commit_fact(&fact(&t, n, "r1", body))
            .await
            .unwrap();
    }
    let before = projected_counts(&ctx.pool, &t).await;
    assert_eq!(before, (0, 0, 0, 0, 0), "all tiers start as holes");

    let report = ctx.reconciler.reconcile_tenant(&t).await;
    assert!(!report.had_errors(), "no tier errored: {:?}", report.tiers);

    let after = projected_counts(&ctx.pool, &t).await;
    assert!(after.0 >= 2, "graph edges backfilled (got {})", after.0);
    assert_eq!(after.1, 2, "both fact vectors backfilled");
    assert_eq!(after.2, 2, "both extract stamps backfilled");
    assert_eq!(after.3, 2, "both categorize stamps backfilled");
    assert!(
        after.4 >= 1,
        "at least one digest scope rolled up (got {})",
        after.4
    );
}

#[tokio::test]
async fn reconciler_is_idempotent() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    ctx.store
        .commit_fact(&fact(&t, 1, "r1", "idempotent reconcile target"))
        .await
        .unwrap();

    let first = ctx.reconciler.reconcile_tenant(&t).await;
    assert!(first.total_reconciled() >= 1, "first pass applies the hole");
    let after_first = projected_counts(&ctx.pool, &t).await;

    // Second pass over the now-converged tenant: a no-op (0 applied) and NOT a
    // single extra row anywhere (upserts/stamps converge).
    let second = ctx.reconciler.reconcile_tenant(&t).await;
    assert_eq!(
        second.total_reconciled(),
        0,
        "converged tenant reconciles nothing: {:?}",
        second.tiers
    );
    assert_eq!(
        projected_counts(&ctx.pool, &t).await,
        after_first,
        "row counts are stable across a repeat pass (idempotent)"
    );
}

#[tokio::test]
async fn vector_tier_re_embeds_on_model_stamp_change() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    let f = fact(&t, 1, "r1", "re-embed me when the model changes");
    ctx.store.commit_fact(&f).await.unwrap();
    ctx.reconciler.reconcile_tenant(&t).await;

    // The vector landed stamped with the active embedder model.
    let stamped: String = sqlx::query_as::<_, (String,)>(
        "SELECT model_id FROM fact_vectors WHERE tenant_id = $1 AND fact_id = $2",
    )
    .bind(&t)
    .bind(f.id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap()
    .0;
    assert_eq!(
        stamped, HASH_EMBED_MODEL_ID,
        "stamped with the active embedder"
    );

    // Simulate a PRIOR embedder having produced this vector: rewrite the stamp
    // to a stale id. The hole-only scan would miss this; the M7 stale-stamp
    // scan must re-embed it.
    sqlx::query("UPDATE fact_vectors SET model_id = 'old-embedder/v0' WHERE tenant_id = $1 AND fact_id = $2")
        .bind(&t)
        .bind(f.id)
        .execute(&ctx.pool)
        .await
        .unwrap();

    let n = ctx.vector_worker.catch_up(&t).await.unwrap();
    assert_eq!(n, 1, "the stale-stamped vector was re-embedded");

    // Still exactly one row (overwrite, not duplicate) and the stamp refreshed
    // to the active model — a model change rebuilt the vector with no downtime.
    let rows: i64 = count(
        &ctx.pool,
        "SELECT count(*) FROM fact_vectors WHERE tenant_id = $1",
        &t,
    )
    .await;
    assert_eq!(rows, 1, "re-embed overwrites, never duplicates");
    let refreshed: String = sqlx::query_as::<_, (String,)>(
        "SELECT model_id FROM fact_vectors WHERE tenant_id = $1 AND fact_id = $2",
    )
    .bind(&t)
    .bind(f.id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap()
    .0;
    assert_eq!(refreshed, HASH_EMBED_MODEL_ID, "stamp refreshed to active");

    // And the tier reports converged (no outstanding lag) afterwards.
    assert!(
        ctx.vector_worker.oldest_hole(&t).await.unwrap().is_none(),
        "no outstanding vector hole after re-embed"
    );
}

#[tokio::test]
async fn reconciler_resumes_after_partial_projection() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    let done = fact(&t, 1, "r1", "already embedded before the interruption");
    let hole = fact(&t, 2, "r1", "left outstanding by the interruption");
    ctx.store.commit_fact(&done).await.unwrap();
    ctx.store.commit_fact(&hole).await.unwrap();

    // Simulate a pass that got interrupted after projecting only `done` (its
    // stage-5b vector lands via the worker; the rest stays a hole).
    ctx.vector_worker.enqueue(&t, done.id, None);
    ctx.vector_worker.drain().await;
    assert_eq!(
        count(
            &ctx.pool,
            "SELECT count(*) FROM fact_vectors WHERE tenant_id = $1",
            &t
        )
        .await,
        1,
        "only the pre-interruption fact is embedded"
    );

    // Resume: the reconciler picks up exactly the remaining hole (driven off
    // durable state — the already-embedded fact is skipped, no lost cursor).
    ctx.reconciler.reconcile_tenant(&t).await;
    assert_eq!(
        count(
            &ctx.pool,
            "SELECT count(*) FROM fact_vectors WHERE tenant_id = $1",
            &t
        )
        .await,
        2,
        "the remaining hole is filled on resume, the done one not re-touched twice"
    );
    // A fresh pass now finds nothing (fully converged).
    assert_eq!(ctx.vector_worker.catch_up(&t).await.unwrap(), 0);
}
