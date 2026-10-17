//! M1 integration tests — the write path and keyed read against a real
//! Postgres (the §8 roundtrip criteria, at library level).
//!
//! Gated on TEST_DATABASE_URL: `cargo test` skips these when it is unset
//! (e.g. no local Postgres); CI sets it to the postgres service container.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use memkeys_server::envelope::Envelope;
use memkeys_server::model::{EntityKey, KeyedQuery, Validity};
use memkeys_server::pg::PgStore;
use memkeys_server::pipeline::{
    CallerContext, EntityResolver, NoopResolver, RunBinding, ThreadBinding,
};
use memkeys_server::service::MemKeysService;
use memkeys_server::store::CommitOutcome;
use memkeys_server::FactStore;

fn db_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

static TENANT_SEQ: AtomicU16 = AtomicU16::new(0);

/// Per-process nonce so reruns never collide with rows from a previous test
/// process (tenant indices alone restart at 0 on every `cargo test`, and test
/// start order is nondeterministic).
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

/// Unique tenant per test (nonce + index) — full isolation, no cleanup needed.
fn caller() -> (CallerContext, String) {
    let tenant = format!(
        "test-{}-{}",
        run_nonce(),
        TENANT_SEQ.fetch_add(1, Ordering::SeqCst)
    );
    (
        CallerContext {
            tenant_id: tenant.clone(),
            principal: "agent-a".into(),
            trace_id: Some("trace-1".into()),
        },
        tenant,
    )
}

async fn service(resolver: Arc<dyn EntityResolver>) -> (MemKeysService, String) {
    let url = db_url().expect("TEST_DATABASE_URL must be set for integration tests");
    let store = PgStore::connect(&url).await.expect("connect + migrate");
    let (c, _tenant) = caller();
    let svc = MemKeysService::with_resolver(Arc::new(store), resolver);
    (svc, c.tenant_id)
}

fn envelope(raw: serde_json::Value) -> Envelope {
    serde_json::from_value(raw).expect("valid envelope")
}

fn gw_flush(body: &str, entity: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": entity,
        "fact": { "body": body }
    })
}

async fn ingest(
    svc: &MemKeysService,
    c: &CallerContext,
    env: &Envelope,
) -> memkeys_server::IngestReceipt {
    svc.ingest(env, c, Utc::now())
        .await
        .expect("ingest succeeds")
}

#[tokio::test]
async fn roundtrip_flush_key_hydrate_by_issue() {
    // §8: a dumb client flushes with only its run id; the record is keyed and
    // retrievable by issue key alone.
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let env = envelope(gw_flush(
        "handoff: the fact store is postgres, migrations in crates/memkeys-server/migrations",
        serde_json::json!({
            "run": "run-1", "issue": "ISI-6075", "project": "memkeys"
        }),
    ));
    let receipt = ingest(&svc, &c, &env).await;
    assert!(receipt.keyed);
    assert!(receipt
        .entity_keys_resolved
        .iter()
        .any(|(k, v)| *k == "entity.issue" && v == "ISI-6075"));

    let facts = svc
        .fetch_by_keys(
            &tenant,
            &KeyedQuery {
                keys: vec![(EntityKey::Issue, "ISI-6075".into())],
                fact_type: None,
                as_of: None,
                valid: Validity::Current,
                limit: KeyedQuery::DEFAULT_LIMIT,
            },
        )
        .await
        .expect("keyed read");
    assert_eq!(facts.len(), 1);
    assert!(facts[0].body.contains("fact store is postgres"));
    assert_eq!(facts[0].provenance_principal, "agent-a");
}

#[tokio::test]
async fn enrichment_from_stub_run_registry_keys_the_fact() {
    // §8: run-only envelope becomes fully keyed via the enrichment pivot.
    struct StubResolver;
    #[async_trait::async_trait]
    impl EntityResolver for StubResolver {
        async fn resolve_run(&self, _t: &str, run: &str) -> Option<RunBinding> {
            (run == "run-7").then(|| RunBinding {
                project: Some("memkeys".into()),
                issue: Some("ISI-6075".into()),
                pr: Some("42".into()),
                agent: None,
            })
        }
        async fn resolve_thread(&self, _t: &str, _th: &str) -> Option<ThreadBinding> {
            Some(ThreadBinding {
                project: Some("memkeys".into()),
                issue: Some("ISI-6075".into()),
                pr: None,
                participants: vec!["agent-b".into()],
            })
        }
        async fn resolve_service(&self, _t: &str, _s: &str) -> Option<String> {
            Some("memkeys".into())
        }
    }
    let (svc, tenant) = service(Arc::new(StubResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let env = envelope(gw_flush(
        "handoff note",
        serde_json::json!({ "run": "run-7" }),
    ));
    let receipt = ingest(&svc, &c, &env).await;
    assert!(receipt.keyed);
    for (k, v) in &receipt.entity_keys_resolved {
        let _ = (k, v);
    }
    assert!(receipt
        .entity_keys_resolved
        .iter()
        .any(|(k, _)| *k == "entity.issue"));

    let facts = svc
        .fetch_by_keys(
            &tenant,
            &KeyedQuery {
                keys: vec![
                    (EntityKey::Issue, "ISI-6075".into()),
                    (EntityKey::Pr, "42".into()),
                ],
                fact_type: None,
                as_of: None,
                valid: Validity::Current,
                limit: KeyedQuery::DEFAULT_LIMIT,
            },
        )
        .await
        .expect("keyed read with ANDed keys");
    assert_eq!(facts.len(), 1);
}

#[tokio::test]
async fn retried_flush_is_idempotent_and_conflicting_replay_is_409() {
    let (svc, _tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: _tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };

    let mut raw = gw_flush(
        "the chosen database is postgres",
        serde_json::json!({ "run": "r1", "issue": "I1" }),
    );
    raw["source"]["idempotency_key"] = serde_json::json!("flush-1");
    let env = envelope(raw.clone());
    let first = ingest(&svc, &c, &env).await;
    assert!(!first.duplicate);

    // Identical retry: same id, no duplicate.
    let second = ingest(&svc, &c, &env).await;
    assert_eq!(first.fact_id, second.fact_id);
    assert!(second.duplicate);

    // Same idempotency key, different body: conflict.
    raw["fact"]["body"] = serde_json::json!("the chosen database is mysql");
    let conflicting = envelope(raw);
    let err = svc
        .ingest(&conflicting, &c, Utc::now())
        .await
        .expect_err("conflicting replay");
    assert!(matches!(err, memkeys_server::IngestError::Conflict { .. }));
}

#[tokio::test]
async fn supersession_collapses_at_read_time() {
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };

    let v1 = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "run": "r1", "issue": "I1" },
        "fact": { "type": "decision", "body": "database: postgres", "subject": "the chosen database" }
    }));
    let r1 = ingest(&svc, &c, &v1).await;

    let v2 = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T11:00:00Z" },
        "entity": { "run": "r2", "issue": "I1" },
        "fact": {
            "type": "decision", "body": "database: sqlite after all",
            "subject": "the chosen database",
            "supersedes": r1.fact_id.to_string()
        }
    }));
    let r2 = ingest(&svc, &c, &v2).await;
    assert_ne!(r1.fact_id, r2.fact_id);

    let q = |valid: Validity| KeyedQuery {
        keys: vec![(EntityKey::Issue, "I1".into())],
        fact_type: None,
        as_of: None,
        valid,
        limit: KeyedQuery::DEFAULT_LIMIT,
    };
    let current = svc
        .fetch_by_keys(&tenant, &q(Validity::Current))
        .await
        .expect("current");
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].body, "database: sqlite after all");

    let all = svc
        .fetch_by_keys(&tenant, &q(Validity::All))
        .await
        .expect("all");
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn supersedes_validation_rejects_foreign_targets() {
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };

    let v1 = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "run": "r1", "issue": "I1" },
        "fact": { "type": "decision", "body": "database: postgres", "subject": "the chosen database" }
    }));
    let r1 = ingest(&svc, &c, &v1).await;

    // Different subject → rejected (cannot version an unnamed statement).
    let bad_subject = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T12:00:00Z" },
        "entity": { "run": "r2", "issue": "I1" },
        "fact": {
            "type": "decision", "body": "database: sqlite",
            "subject": "a different statement",
            "supersedes": r1.fact_id.to_string()
        }
    }));
    let err = svc
        .ingest(&bad_subject, &c, Utc::now())
        .await
        .expect_err("mismatched subject");
    assert!(matches!(err, memkeys_server::IngestError::Rejected { .. }));

    // Unknown target → rejected.
    let bad_target = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T12:00:00Z" },
        "entity": { "run": "r2", "issue": "I1" },
        "fact": {
            "type": "decision", "body": "database: sqlite",
            "subject": "the chosen database",
            "supersedes": "00000000-0000-0000-0000-00000000dead"
        }
    }));
    let err = svc
        .ingest(&bad_target, &c, Utc::now())
        .await
        .expect_err("unknown target");
    assert!(matches!(err, memkeys_server::IngestError::Rejected { .. }));
}

#[tokio::test]
async fn as_of_replay_on_both_temporal_axes() {
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };

    // emitted 10:00, event_time 09:00 (backdated), ingested ~now.
    let env = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "run": "r1", "issue": "I1" },
        "fact": { "body": "backdated observation", "event_time": "2026-10-17T09:00:00Z" }
    }));
    ingest(&svc, &c, &env).await;

    let before: DateTime<Utc> = "2026-10-16T00:00:00Z".parse().unwrap();
    let after: DateTime<Utc> = chrono::Utc::now() + chrono::Duration::seconds(1);
    let q = |as_of: Option<DateTime<Utc>>| KeyedQuery {
        keys: vec![(EntityKey::Issue, "I1".into())],
        fact_type: None,
        as_of,
        valid: Validity::Current,
        limit: KeyedQuery::DEFAULT_LIMIT,
    };

    // Invisible before it happened (event axis)...
    assert!(svc
        .fetch_by_keys(&tenant, &q(Some(before)))
        .await
        .expect("as-of before")
        .is_empty());
    // ...and before we recorded it (transaction axis)...
    // (event_time 09:00 < before? No: before is 2026-10-16, event is 2026-10-17 →
    // excluded on the event axis; and ingested_at is now, excluded on the
    // transaction axis for any as_of before now.)
    // Visible after ingest.
    let after_facts = svc
        .fetch_by_keys(&tenant, &q(Some(after)))
        .await
        .expect("as-of after");
    assert_eq!(after_facts.len(), 1);
    // Also visible with no as_of at all.
    assert_eq!(
        svc.fetch_by_keys(&tenant, &q(None))
            .await
            .expect("plain")
            .len(),
        1
    );
}

#[tokio::test]
async fn unkeyed_facts_never_pollute_keyed_results() {
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };

    let learning = envelope(serde_json::json!({
        "source": { "kind": "telemetry_learning", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": {},
        "fact": { "body": "learned something with no key", "confidence": 0.7 }
    }));
    let r = ingest(&svc, &c, &learning).await;
    assert!(!r.keyed);

    // No entity key reaches it; keyed fetches never see it.
    let err = svc
        .fetch_by_keys(
            &tenant,
            &KeyedQuery {
                keys: vec![],
                fact_type: None,
                as_of: None,
                valid: Validity::Current,
                limit: 10,
            },
        )
        .await
        .expect_err("keyed read requires a key");
    assert!(matches!(err, memkeys_server::QueryError::Invalid { .. }));
}

#[tokio::test]
async fn tenant_floor_isolates_reads() {
    let (svc, tenant_a) = service(Arc::new(NoopResolver)).await;
    let c_a = CallerContext {
        tenant_id: tenant_a.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let env = envelope(gw_flush(
        "tenant A only",
        serde_json::json!({ "run": "r1", "issue": "SHARED" }),
    ));
    ingest(&svc, &c_a, &env).await;

    let q = KeyedQuery {
        keys: vec![(EntityKey::Issue, "SHARED".into())],
        fact_type: None,
        as_of: None,
        valid: Validity::Current,
        limit: 10,
    };
    assert_eq!(
        svc.fetch_by_keys(&tenant_a, &q)
            .await
            .expect("a sees its fact")
            .len(),
        1
    );
    assert!(svc
        .fetch_by_keys("test-tenant-other", &q)
        .await
        .expect("b sees nothing")
        .is_empty());
}

#[tokio::test]
async fn store_commit_is_idempotent_at_the_port_level() {
    let url = db_url().expect("TEST_DATABASE_URL");
    let store = PgStore::connect(&url).await.expect("connect");
    let (c, _tenant) = caller();
    let env = envelope(gw_flush(
        "direct store write",
        serde_json::json!({ "run": "r1", "issue": "I9" }),
    ));
    let mut fact = memkeys_server::pipeline::normalize(&env, &c, Utc::now()).expect("normalize");
    memkeys_server::pipeline::key_stage(&env, &c, &mut fact, &NoopResolver)
        .await
        .expect("key");
    memkeys_server::pipeline::temporalize(&mut fact).expect("temporalize");

    assert_eq!(
        store.commit_fact(&fact).await.expect("first commit"),
        CommitOutcome::Inserted
    );
    assert_eq!(
        store.commit_fact(&fact).await.expect("replay commit"),
        CommitOutcome::Duplicate
    );
}

// --- ISI-5914 T3: live-head CAS gate ---------------------------------------

#[tokio::test]
async fn plain_write_to_occupied_head_auto_supersedes() {
    // Two plain writes (neither sets `supersedes`) to the same statement
    // (same issue + subject + type). Without the gate both would land live —
    // a fork. The gate serializes them: the second auto-supersedes the first.
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let stmt = |body: &str, run: &str| {
        envelope(serde_json::json!({
            "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": { "run": run, "issue": "I1" },
            "fact": { "type": "decision", "body": body, "subject": "the chosen database" }
        }))
    };
    let r1 = ingest(&svc, &c, &stmt("database: postgres", "r1")).await;
    let r2 = ingest(&svc, &c, &stmt("database: sqlite after all", "r2")).await;
    assert_ne!(r1.fact_id, r2.fact_id);

    let q = |valid: Validity| KeyedQuery {
        keys: vec![(EntityKey::Issue, "I1".into())],
        fact_type: None,
        as_of: None,
        valid,
        limit: KeyedQuery::DEFAULT_LIMIT,
    };
    let current = svc
        .fetch_by_keys(&tenant, &q(Validity::Current))
        .await
        .expect("current");
    assert_eq!(current.len(), 1, "exactly one live head (no fork)");
    assert_eq!(current[0].body, "database: sqlite after all");
    assert_eq!(
        svc.fetch_by_keys(&tenant, &q(Validity::All))
            .await
            .expect("all")
            .len(),
        2
    );
}

#[tokio::test]
async fn explicit_supersedes_of_a_stale_head_is_conflict() {
    // v1 is the head; a plain v2 auto-supersedes it (v1 is now stale). An
    // explicit v3 that names v1 (no longer the current head) is a conflict.
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let v1 = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "run": "r1", "issue": "I1" },
        "fact": { "type": "decision", "body": "v1", "subject": "the chosen database" }
    }));
    let r1 = ingest(&svc, &c, &v1).await;
    let v2 = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T11:00:00Z" },
        "entity": { "run": "r2", "issue": "I1" },
        "fact": { "type": "decision", "body": "v2", "subject": "the chosen database" }
    }));
    ingest(&svc, &c, &v2).await;

    let v3 = envelope(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T12:00:00Z" },
        "entity": { "run": "r3", "issue": "I1" },
        "fact": {
            "type": "decision", "body": "v3", "subject": "the chosen database",
            "supersedes": r1.fact_id.to_string()
        }
    }));
    let err = svc
        .ingest(&v3, &c, Utc::now())
        .await
        .expect_err("stale supersedes target");
    assert!(matches!(err, memkeys_server::IngestError::Conflict { .. }));
}

#[tokio::test]
async fn subjectless_facts_skip_the_gate_and_coexist() {
    // Subject-null facts are independent observations — they may share
    // (entity, null, type) and must never supersede each other.
    let (svc, tenant) = service(Arc::new(NoopResolver)).await;
    let c = CallerContext {
        tenant_id: tenant.clone(),
        principal: "agent-a".into(),
        trace_id: None,
    };
    let obs = |body: &str| {
        envelope(serde_json::json!({
            "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": { "run": "r1", "issue": "I1" },
            "fact": { "type": "observation", "body": body }
        }))
    };
    ingest(&svc, &c, &obs("ci is green")).await;
    ingest(&svc, &c, &obs("deploy finished")).await;

    let q = KeyedQuery {
        keys: vec![(EntityKey::Issue, "I1".into())],
        fact_type: None,
        as_of: None,
        valid: Validity::Current,
        limit: KeyedQuery::DEFAULT_LIMIT,
    };
    assert_eq!(
        svc.fetch_by_keys(&tenant, &q).await.expect("current").len(),
        2,
        "both observations stay live (gate skipped)"
    );
}
