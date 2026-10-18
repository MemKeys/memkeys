//! M5 integration tests — the fact-vector tier (pgvector), the stage-5b
//! vector-projection worker, and op-4 / RRF fusion against a real Postgres,
//! gated on TEST_DATABASE_URL (same harness/env guard as the M1/M2 suites; CI
//! points it at the `pgvector/pgvector:pg18` service container so migration
//! 0003's `CREATE EXTENSION vector` applies).
//!
//! Coverage per the ISI-6167 acceptance criteria:
//! - fusion ranking: a fact both keyed AND semantic agree on ranks above
//!   single-path hits (the "keyed + semantic agree" signal), incl. the graph
//!   path (keyed + traverse + semantic),
//! - tenant isolation on the pg vector index (§7.4 R6 hard boundary),
//! - stage-5b idempotency (re-embed overwrites, never duplicates) + catch_up
//!   (SoT hole scan builds the projection from the FactStore),
//! - op-4 feature-disabled (501 at the service seam) when the module is off,
//! - as_of scoping (a fact unknown as-of the query time is excluded) on BOTH
//!   the scoped and the unscoped/whole-tenant candidate paths (ISI-6172).

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{Duration as ChronoDuration, Utc};
use memkeys_server::graph::{Direction, NodeRef, TraverseReq};
use memkeys_server::model::{EntityKey, EntityKeys, FactType, KeyedQuery, ProcessedFact, Validity};
use memkeys_server::pg::PgStore;
use memkeys_server::pg_graph::PgGraph;
use memkeys_server::pg_vector::PgVectorIndex;
use memkeys_server::pipeline::NoopResolver;
use memkeys_server::semantic::{CandidateSet, Embedder, HashEmbedder, VectorIndex};
use memkeys_server::service::{MemKeysService, SearchRequest};
use memkeys_server::store::FactStore;
use memkeys_server::worker::VectorProjectionWorker;
use memkeys_server::QueryError;
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
        "m5test-{}-{}",
        run_nonce(),
        TENANT_SEQ.fetch_add(1, Ordering::SeqCst)
    )
}

struct Ctx {
    service: MemKeysService,
    store: Arc<PgStore>,
    index: Arc<PgVectorIndex>,
    worker: Arc<VectorProjectionWorker>,
    embedder: Arc<HashEmbedder>,
}

/// A connected service with the graph projection + semantic module wired
/// against a real Postgres. `None` when TEST_DATABASE_URL is unset (skip).
async fn ctx() -> Option<Ctx> {
    let url = db_url()?;
    let store = Arc::new(PgStore::connect(&url).await.expect("connect + migrate"));
    let pool = store.pool().clone();
    let graph = Arc::new(PgGraph::from_pool(pool.clone()));
    let embedder = Arc::new(HashEmbedder::new());
    let index = Arc::new(PgVectorIndex::from_pool(pool.clone()));
    let vindex: Arc<dyn VectorIndex> = index.clone();
    let worker = Arc::new(VectorProjectionWorker::new(
        pool.clone(),
        embedder.clone() as Arc<dyn Embedder>,
        vindex.clone(),
    ));
    let service =
        MemKeysService::with_graph_projection(store.clone(), Arc::new(NoopResolver), graph.clone())
            .with_semantic(embedder.clone() as Arc<dyn Embedder>, vindex)
            .with_semantic_worker(worker.clone());
    Some(Ctx {
        service,
        store,
        index,
        worker,
        embedder,
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

/// Commit a fact AND enqueue+drain the stage-5b worker (the post-commit path).
async fn commit_and_project(ctx: &Ctx, f: &ProcessedFact) {
    ctx.store.commit_fact(f).await.unwrap();
    ctx.worker.enqueue(&f.tenant_id, f.id, None);
    ctx.worker.drain().await;
}

#[tokio::test]
async fn pg_index_enforces_tenant_isolation() {
    let Some(ctx) = ctx().await else { return };
    let ta = next_tenant();
    let tb = next_tenant();
    let fa = fact(&ta, 1, "r", "chose postgres for the durable fact store");
    let fb = fact(&tb, 1, "r", "chose postgres for the durable fact store");
    commit_and_project(&ctx, &fa).await;
    commit_and_project(&ctx, &fb).await;

    let q = ctx.embedder.embed("postgres durable store").await.unwrap();
    // team A's search must never see team B's identically-bodied fact (R6).
    let hits = ctx
        .index
        .search(&ta, &CandidateSet::All, &q, 10, None)
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "only team A's vector is in team A's partition"
    );
    assert_eq!(hits[0].fact_id, fa.id);
}

#[tokio::test]
async fn stage_5b_is_idempotent_and_catch_up_fills_holes() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    let f = fact(&t, 1, "r", "idempotent embedding target");

    // Commit + project twice: the upsert overwrites, never duplicates.
    ctx.store.commit_fact(&f).await.unwrap();
    ctx.worker.enqueue(&t, f.id, None);
    ctx.worker.drain().await;
    ctx.worker.enqueue(&t, f.id, None);
    ctx.worker.drain().await;
    let q = ctx
        .embedder
        .embed("idempotent embedding target")
        .await
        .unwrap();
    let hits = ctx
        .index
        .search(&t, &CandidateSet::All, &q, 10, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "replayed upsert is idempotent on fact_id");

    // A second fact committed with NO enqueue is a projection hole; catch_up
    // builds the projection from the FactStore.
    let f2 = fact(&t, 2, "r", "a hole that catch_up must fill");
    ctx.store.commit_fact(&f2).await.unwrap();
    let before = ctx
        .index
        .search(&t, &CandidateSet::Only(vec![f2.id]), &q, 10, None)
        .await
        .unwrap();
    assert!(before.is_empty(), "hole: f2 not embedded yet");
    let projected = ctx.worker.catch_up(&t).await.unwrap();
    assert_eq!(projected, 1, "exactly the one hole is embedded");
    // catch_up is convergent: a second pass finds no holes.
    assert_eq!(ctx.worker.catch_up(&t).await.unwrap(), 0);
}

#[tokio::test]
async fn semantic_search_disabled_is_feature_disabled() {
    let Some(ctx) = ctx().await else { return };
    // A service with the SAME store but no semantic module wired → op-4 returns
    // feature_disabled (the 501 seam), never a silent empty result.
    let bare = MemKeysService::new(ctx.store.clone());
    let req = SearchRequest {
        query: "anything".into(),
        scope: vec![],
        fact_type: None,
        k: 10,
        as_of: None,
    };
    let err = bare.semantic_search("t", &req).await.unwrap_err();
    assert!(matches!(
        err,
        QueryError::FeatureDisabled {
            feature: "memory.semantic"
        }
    ));
}

#[tokio::test]
async fn fused_search_ranks_cross_path_agreement_first() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    // f1: keyed on run=r AND semantically matches the query → both paths.
    let f1 = fact(&t, 1, "r", "chose postgres for the durable fact store");
    // f2: keyed on run=r but semantically unrelated → keyed path only.
    let f2 = fact(&t, 2, "r", "the standup meeting was moved to Tuesday");
    // f3: semantically matches but keyed on a DIFFERENT run → semantic only.
    let f3 = fact(
        &t,
        3,
        "other",
        "postgres durability was the deciding factor",
    );
    for f in [&f1, &f2, &f3] {
        commit_and_project(&ctx, f).await;
    }

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
    let hits = ctx
        .service
        .fused_search(&t, Some(&keyed), None, Some(&sem), 10)
        .await
        .unwrap();
    // f1 is the only fact BOTH paths surface → it must rank first, above the
    // single-path f2 (keyed only) and f3 (semantic only).
    assert_eq!(hits[0].record.id, f1.id, "cross-path agreement ranks first");
    assert!(hits[0].via.len() >= 2, "f1 surfaced via keyed + semantic");
}

#[tokio::test]
async fn fused_search_includes_graph_path() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    // f1 keyed on run=r + semantic match + reachable via a graph hop from its
    // own run node → all three paths agree on it.
    let f1 = fact(&t, 1, "r", "chose postgres for the durable fact store");
    commit_and_project(&ctx, &f1).await;
    // Project the graph edges so traverse has a start node.
    let gw = ctx.service.projection_worker().expect("graph worker wired");
    gw.catch_up(&t).await.unwrap();

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
    // Traverse from the fact node itself (depth 1, both directions) — f1 is in
    // its own BFS result, contributing the FusionSource::Graph list.
    let traverse = TraverseReq {
        tenant: t.clone(),
        project: None,
        start: NodeRef {
            kind: memkeys_server::graph::NodeKind::Fact,
            id: memkeys_server::graph::fact_node_id(&t, f1.id),
        },
        edges: vec![],
        dir: Direction::Both,
        depth: 2,
        deadline: std::time::Duration::from_millis(500),
        hydrate: true,
        fanout_cap: 100,
        node_budget: 100,
    };
    let hits = ctx
        .service
        .fused_search(&t, Some(&keyed), Some(&traverse), Some(&sem), 10)
        .await
        .unwrap();
    assert_eq!(hits[0].record.id, f1.id);
    // f1 surfaced via all three paths (keyed + graph + semantic).
    assert!(
        hits[0].via.len() >= 3,
        "expected keyed+graph+semantic, got {:?}",
        hits[0].via
    );
}

#[tokio::test]
async fn as_of_scopes_out_facts_unknown_at_query_time() {
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    // An "old" fact known well before the as_of cutoff.
    let old_time = Utc::now() - ChronoDuration::hours(2);
    let mut f_old = fact(&t, 1, "r", "postgres was chosen early in the project");
    f_old.event_time = old_time;
    f_old.ingested_at = old_time;
    // A "new" fact ingested AFTER the cutoff.
    let mut f_new = fact(&t, 2, "r", "postgres tuning happened much later on");
    f_new.event_time = Utc::now();
    f_new.ingested_at = Utc::now();
    commit_and_project(&ctx, &f_old).await;
    commit_and_project(&ctx, &f_new).await;

    // as_of between the two, with a scope (so the candidate set is time-bounded
    // via the keyed fetch); the new fact is unknown as-of then → excluded.
    let cutoff = Utc::now() - ChronoDuration::hours(1);
    let req = SearchRequest {
        query: "postgres".into(),
        scope: vec![(EntityKey::Run, "r".into())],
        fact_type: None,
        k: 10,
        as_of: Some(cutoff),
    };
    let hits = ctx.service.semantic_search(&t, &req).await.unwrap();
    let ids: Vec<Uuid> = hits.iter().map(|h| h.record.id).collect();
    assert!(
        ids.contains(&f_old.id),
        "the old fact is in scope as-of cutoff"
    );
    assert!(
        !ids.contains(&f_new.id),
        "the later fact is unknown as-of the cutoff and must be excluded"
    );
}

#[tokio::test]
async fn as_of_scopes_out_facts_on_the_unscoped_whole_tenant_path() {
    // Regression for ISI-6172 (CR ISI-6168 B1): with NO scope the candidate set
    // is CandidateSet::All, which used to drop `as_of` entirely — returning
    // facts ingested after as_of with no signal. The vector tier now carries
    // `ingested_at` and bounds the ANN pass, so the unscoped path is temporally
    // correct, exactly like the scoped path above.
    let Some(ctx) = ctx().await else { return };
    let t = next_tenant();
    let old_time = Utc::now() - ChronoDuration::hours(2);
    let mut f_old = fact(&t, 1, "r", "postgres was chosen early in the project");
    f_old.event_time = old_time;
    f_old.ingested_at = old_time;
    let mut f_new = fact(&t, 2, "r", "postgres tuning happened much later on");
    f_new.event_time = Utc::now();
    f_new.ingested_at = Utc::now();
    commit_and_project(&ctx, &f_old).await;
    commit_and_project(&ctx, &f_new).await;

    let cutoff = Utc::now() - ChronoDuration::hours(1);
    // No `scope` → whole-tenant floor (R6). `as_of` must still bound it.
    let req = SearchRequest {
        query: "postgres".into(),
        scope: vec![],
        fact_type: None,
        k: 10,
        as_of: Some(cutoff),
    };
    let hits = ctx.service.semantic_search(&t, &req).await.unwrap();
    let ids: Vec<Uuid> = hits.iter().map(|h| h.record.id).collect();
    assert!(
        ids.contains(&f_old.id),
        "the old fact is known as-of the cutoff on the unscoped path"
    );
    assert!(
        !ids.contains(&f_new.id),
        "the later fact must be excluded on the unscoped path too (ISI-6172)"
    );

    // Sanity: with no `as_of` the live-head read surfaces both.
    let live = SearchRequest { as_of: None, ..req };
    let all: Vec<Uuid> = ctx
        .service
        .semantic_search(&t, &live)
        .await
        .unwrap()
        .iter()
        .map(|h| h.record.id)
        .collect();
    assert!(all.contains(&f_old.id) && all.contains(&f_new.id));
}
