//! M2 integration tests — the graph projection (stage 5a) and op 2 traverse
//! against a real Postgres, gated on TEST_DATABASE_URL (CI sets it to the
//! postgres service container, same harness as the M1 suite).
//!
//! Coverage per the ISI-6076 acceptance criteria:
//! - projection worker builds the graph projection from FactStore
//!   (post-commit apply + catch-up hole scan + reconciling rebuild),
//! - traverse semantics (bounded BFS: direction, filters, depth clamp,
//!   budgets, tenant fail-closed, hydration + orphan),
//! - supersession lineage (recursive CTE walk, current-head agreement with
//!   `valid=current`, hole signaling via `complete`, legacy forks),
//! - idempotent replay + deterministic a2a hint convergence.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use memkeys_server::envelope::Envelope;
use memkeys_server::graph::{
    a2a_hint_local_id, node_id, Direction, EdgeType, GraphError, NodeKind, NodeRef, Scope,
    TruncReason,
};
use memkeys_server::pg::PgStore;
use memkeys_server::pg_graph::PgGraph;
use memkeys_server::pipeline::{CallerContext, EntityResolver, NoopResolver};
use memkeys_server::service::MemKeysService;
use memkeys_server::worker::GraphProjectionWorker;
use memkeys_server::GraphView;
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

/// Unique tenant per test (nonce + index) — full isolation, no cleanup.
fn caller() -> (CallerContext, String) {
    let tenant = format!(
        "m2test-{}-{}",
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

struct Ctx {
    service: MemKeysService,
    graph: Arc<PgGraph>,
    worker: Arc<GraphProjectionWorker>,
    tenant: String,
    caller: CallerContext,
}

async fn ctx() -> Ctx {
    let url = db_url().expect("TEST_DATABASE_URL must be set for integration tests");
    let store = PgStore::connect(&url).await.expect("connect + migrate");
    let graph = Arc::new(PgGraph::from_pool(store.pool().clone()));
    let service = MemKeysService::with_graph_projection(
        Arc::new(store),
        Arc::new(NoopResolver) as Arc<dyn EntityResolver>,
        graph.clone(),
    );
    let worker = service.projection_worker().expect("worker wired");
    let (c, tenant) = caller();
    Ctx {
        service,
        graph,
        worker,
        tenant,
        caller: c,
    }
}

fn envelope(raw: serde_json::Value) -> Envelope {
    serde_json::from_value(raw).expect("valid envelope")
}

fn gw_flush(subject: Option<&str>, body: &str, entity: serde_json::Value) -> serde_json::Value {
    let mut fact = serde_json::json!({ "body": body });
    if let Some(s) = subject {
        fact["subject"] = serde_json::json!(s);
    }
    serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": entity,
        "fact": fact
    })
}

fn a2a(body: &str, entity: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "source": { "kind": "a2a_exchange", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": entity,
        "fact": { "body": body }
    })
}

async fn ingest(x: &Ctx, env: &Envelope) -> memkeys_server::IngestReceipt {
    x.service
        .ingest(env, &x.caller, Utc::now())
        .await
        .expect("ingest succeeds")
}

async fn drain(x: &Ctx) {
    x.worker.drain().await;
}

fn proj_entity(proj: &str) -> serde_json::Value {
    serde_json::json!({ "project": proj, "issue": "iss-1", "agent": "agent-a", "run": "run-1" })
}

fn entity_node(tenant: &str, kind: NodeKind, v: &str) -> NodeRef {
    NodeRef {
        kind,
        id: node_id(tenant, kind, v),
    }
}

async fn edge_rows(x: &Ctx) -> Vec<(String, String, String)> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT src_id, dst_id, edge_type FROM edges WHERE tenant = $1 ORDER BY src_id, dst_id, edge_type",
    )
    .bind(&x.tenant)
    .fetch_all(x.graph.pool())
    .await
    .expect("edge rows");
    rows
}

// ---------------------------------------------------------------------------
// Projection (stage 5a)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn projection_after_ingest_builds_the_graph() {
    let x = ctx().await;
    let e1 = ingest(
        &x,
        &envelope(gw_flush(Some("status"), "v1", proj_entity("proj-1"))),
    )
    .await;
    let mut v2 = gw_flush(Some("status"), "v2", proj_entity("proj-1"));
    v2["fact"]["supersedes"] = serde_json::json!(e1.fact_id.to_string());
    let e2 = ingest(&x, &envelope(v2)).await;
    let _ = e2;
    drain(&x).await;

    let edges = edge_rows(&x).await;
    let fact1 = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    // entity_fact: project, issue, agent nodes -> fact nodes
    for (kind, v) in [
        (NodeKind::EntityProject, "proj-1"),
        (NodeKind::EntityIssue, "iss-1"),
        (NodeKind::EntityAgent, "agent-a"),
    ] {
        let ent = node_id(&x.tenant, kind, v);
        assert!(
            edges
                .iter()
                .any(|(s, d, t)| s == &ent && d == &fact1 && t == "entity_fact"),
            "missing entity_fact edge for {v}"
        );
    }
    // produced_by: fact -> agent(principal)
    let principal_node = node_id(&x.tenant, NodeKind::EntityAgent, "agent-a");
    assert!(
        edges
            .iter()
            .any(|(s, d, t)| s == &fact1 && d == &principal_node && t == "produced_by"),
        "missing produced_by edge"
    );
    // No a2a edge from a gateway_flush fact even with a stray hint.
    assert!(!edges.iter().any(|(_, _, t)| t == "a2a_participant"));
}

#[tokio::test]
async fn supersedes_edge_written_and_deferred_until_target_lands() {
    let x = ctx().await;
    // v1 ingested but its projection LOST (simulated: never drained — clear queue).
    let e1 = ingest(
        &x,
        &envelope(gw_flush(Some("status"), "v1", proj_entity("proj-1"))),
    )
    .await;
    // v2 supersedes v1 (superseding facts carry the shared subject — stage 3).
    let mut j = gw_flush(Some("status"), "v2", proj_entity("proj-1"));
    j["fact"]["supersedes"] = serde_json::json!(e1.fact_id.to_string());
    let e2 = ingest(&x, &envelope(j)).await;

    // Drain: worker applies v1 first (queue order), then v2 — both present.
    drain(&x).await;
    let f1 = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    let f2 = node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string());
    let edges = edge_rows(&x).await;
    assert!(
        edges
            .iter()
            .any(|(s, d, t)| s == &f2 && d == &f1 && t == "supersedes"),
        "supersedes edge present when both endpoints projected"
    );

    // HIGH-9 defer path: a NEW fact v3 superseding a fact whose projection is
    // a hole (target edges deleted underneath the projection).
    let mut j3 = gw_flush(Some("status"), "v3", proj_entity("proj-1"));
    j3["fact"]["supersedes"] = serde_json::json!(e2.fact_id.to_string());
    let e3 = ingest(&x, &envelope(j3)).await;
    sqlx::query(
        "DELETE FROM edges WHERE tenant = $1 AND dst_id = $2 AND edge_type = 'entity_fact'",
    )
    .bind(&x.tenant)
    .bind(&f2)
    .execute(x.graph.pool())
    .await
    .expect("punch hole");
    drain(&x).await;
    let f3 = node_id(&x.tenant, NodeKind::Fact, &e3.fact_id.to_string());
    let edges = edge_rows(&x).await;
    assert!(
        !edges
            .iter()
            .any(|(s, d, t)| s == &f3 && d == &f2 && t == "supersedes"),
        "supersedes edge DEFERRED while target sits in a projection hole"
    );

    // The hole closes via catch-up (worker builds from FactStore).
    let filled = x.worker.catch_up(&x.tenant).await.expect("catch up");
    assert!(filled > 0);
    let edges = edge_rows(&x).await;
    assert!(
        edges
            .iter()
            .any(|(s, d, t)| s == &f3 && d == &f2 && t == "supersedes"),
        "deferred supersedes edge lands once the hole closes"
    );
}

#[tokio::test]
async fn a2a_hint_nodes_are_convergent_and_deterministic() {
    let x = ctx().await;
    let ent = serde_json::json!({ "project": "proj-1", "thread": "th-1", "counterpart_agent": "counterpart-7" });
    let e1 = ingest(&x, &envelope(a2a("hello", ent.clone()))).await;
    let e2 = ingest(&x, &envelope(a2a("reply", ent))).await;
    drain(&x).await;

    let hint_node = node_id(
        &x.tenant,
        NodeKind::A2aHint,
        &a2a_hint_local_id(&x.tenant, "counterpart-7"),
    );
    let f1 = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    let f2 = node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string());
    let edges = edge_rows(&x).await;
    for f in [&f1, &f2] {
        assert!(
            edges
                .iter()
                .any(|(s, d, t)| s == f && d == &hint_node && t == "a2a_participant"),
            "convergent a2a_hint node for corroborating facts"
        );
    }
}

#[tokio::test]
async fn projection_is_idempotent_on_replay() {
    let x = ctx().await;
    let env = envelope(gw_flush(None, "body-1", proj_entity("proj-1")));
    ingest(&x, &env).await;
    drain(&x).await;
    let before = edge_rows(&x).await;
    // Same envelope again (same idempotency inputs -> same fact id) — the
    // store dedups the commit; a replayed Project must not add edges either.
    let dup = x
        .service
        .ingest(&env, &x.caller, Utc::now())
        .await
        .expect("replay ok");
    assert!(dup.duplicate, "replayed flush resolves to the existing row");
    drain(&x).await;
    let after = edge_rows(&x).await;
    assert_eq!(before, after, "replay is edge-idempotent");
}

// ---------------------------------------------------------------------------
// Traverse semantics (op 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn traverse_walks_from_entity_over_directions_and_filters() {
    let x = ctx().await;
    let e1 = ingest(
        &x,
        &envelope(gw_flush(Some("status"), "f1", proj_entity("proj-1"))),
    )
    .await;
    let mut j = gw_flush(Some("status"), "f2", proj_entity("proj-1"));
    j["fact"]["supersedes"] = serde_json::json!(e1.fact_id.to_string());
    let e2 = ingest(&x, &envelope(j)).await;
    drain(&x).await;

    let start = entity_node(&x.tenant, NodeKind::EntityProject, "proj-1");
    let mut req = memkeys_server::graph::TraverseReq::new(&x.tenant, start);
    req.depth = 2;
    req.dir = Direction::Out;
    let r = x.graph.traverse(&req).await.expect("traverse");
    assert!(!r.truncated);
    assert!(r.nodes.len() >= 3, "project node + 2 fact nodes");
    let f2 = node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string());
    let f1 = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    assert!(r.nodes.iter().any(|n| n.id == f2));

    // Edge filter: supersedes-only from the newer fact reaches the older one.
    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        NodeRef {
            kind: NodeKind::Fact,
            id: f2.clone(),
        },
    );
    req.edges = vec![EdgeType::Supersedes];
    let r = x.graph.traverse(&req).await.expect("filtered traverse");
    assert!(r.edges.iter().all(|e| e.edge_type == EdgeType::Supersedes));
    assert!(r.nodes.iter().any(|n| n.id == f1));

    // Direction=in from the older fact reaches the newer one via supersedes.
    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        NodeRef {
            kind: NodeKind::Fact,
            id: f1.clone(),
        },
    );
    req.edges = vec![EdgeType::Supersedes];
    req.dir = Direction::In;
    let r = x.graph.traverse(&req).await.expect("in traverse");
    assert!(r.nodes.iter().any(|n| n.id == f2));

    // Default hydrate=false: topology only, no bodies.
    assert!(r.nodes.iter().all(|n| n.record.is_none()));
}

#[tokio::test]
async fn traverse_is_tenant_fail_closed() {
    let x = ctx().await;
    let e1 = ingest(&x, &envelope(gw_flush(None, "f1", proj_entity("proj-1")))).await;
    drain(&x).await;

    // Another tenant's caller asking for the SAME node id -> not_found.
    let intruder_tenant = format!("{}-intruder", x.tenant);
    let mut req = memkeys_server::graph::TraverseReq::new(
        &intruder_tenant,
        entity_node(&x.tenant, NodeKind::EntityProject, "proj-1"),
    );
    req.depth = 1;
    let err = x.graph.traverse(&req).await.unwrap_err();
    assert!(
        matches!(err, GraphError::NotFound),
        "cross-tenant start fails closed"
    );

    // Forged node id (tenant segment lies) -> not_found, never a leak.
    let forged = format!("{}|fact|{}", x.tenant, Uuid::new_v4());
    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        NodeRef {
            kind: NodeKind::Fact,
            id: forged,
        },
    );
    req.depth = 1;
    assert!(matches!(
        x.graph.traverse(&req).await.unwrap_err(),
        GraphError::NotFound
    ));

    // Kind mismatch on the id -> not_found (ADR node-id scheme).
    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        NodeRef {
            kind: NodeKind::EntityIssue,
            id: node_id(&x.tenant, NodeKind::EntityProject, "proj-1"),
        },
    );
    req.depth = 1;
    assert!(matches!(
        x.graph.traverse(&req).await.unwrap_err(),
        GraphError::NotFound
    ));
    let _ = e1;
}

#[tokio::test]
async fn traverse_depth_clamp_and_budgets() {
    let x = ctx().await;
    for i in 0..3 {
        ingest(
            &x,
            &envelope(gw_flush(None, &format!("f{i}"), proj_entity("proj-1"))),
        )
        .await;
    }
    drain(&x).await;

    // depth 9 clamps to 5 with depth_clamp signal.
    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        entity_node(&x.tenant, NodeKind::EntityProject, "proj-1"),
    );
    req.depth = 9;
    let r = x.graph.traverse(&req).await.expect("clamp");
    assert!(r.truncated);
    assert_eq!(r.reason, TruncReason::DepthClamp);
    assert_eq!(r.reason.as_str(), "depth_clamp");

    // Fanout cap: one entity node with a tiny cap.
    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        entity_node(&x.tenant, NodeKind::EntityProject, "proj-1"),
    );
    req.depth = 1;
    req.fanout_cap = 1;
    let r = x.graph.traverse(&req).await.expect("fanout");
    assert!(r.truncated);
    assert_eq!(r.reason, TruncReason::Fanout);
    assert!(r.cursor.is_some(), "budget truncation carries a cursor");
}

#[tokio::test]
async fn traverse_hydrates_from_the_keyed_store_and_flags_orphans() {
    let x = ctx().await;
    let e1 = ingest(
        &x,
        &envelope(gw_flush(None, "the-body", proj_entity("proj-1"))),
    )
    .await;
    drain(&x).await;

    let mut req = memkeys_server::graph::TraverseReq::new(
        &x.tenant,
        entity_node(&x.tenant, NodeKind::EntityProject, "proj-1"),
    );
    req.depth = 2;
    req.hydrate = true;
    let r = x.graph.traverse(&req).await.expect("hydrate");
    let f1 = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    let fact_node = r
        .nodes
        .iter()
        .find(|n| n.id == f1)
        .expect("fact node present");
    let rec = fact_node.record.as_ref().expect("hydrated record");
    assert_eq!(rec.body, "the-body");

    // §5.6-style age-out: the fact row disappears; topology KEPT, body
    // dropped, orphan reason (never node_budget).
    sqlx::query("DELETE FROM facts WHERE tenant_id = $1 AND id = $2")
        .bind(&x.tenant)
        .bind(e1.fact_id)
        .execute(x.graph.pool())
        .await
        .expect("age out");
    let r = x.graph.traverse(&req).await.expect("orphan walk");
    let fact_node = r.nodes.iter().find(|n| n.id == f1).expect("topology kept");
    assert!(fact_node.unhydratable);
    assert!(fact_node.record.is_none(), "never served stale");
    assert_eq!(r.reason, TruncReason::Orphan);
}

#[tokio::test]
async fn links_is_the_budgeted_one_hop_read() {
    let x = ctx().await;
    for i in 0..4 {
        ingest(
            &x,
            &envelope(gw_flush(None, &format!("f{i}"), proj_entity("proj-1"))),
        )
        .await;
    }
    drain(&x).await;

    let req = memkeys_server::graph::LinksReq {
        tenant: x.tenant.clone(),
        project: None,
        node: entity_node(&x.tenant, NodeKind::EntityProject, "proj-1"),
        edges: vec![],
        dir: Direction::Out,
        deadline: std::time::Duration::from_millis(500),
        hydrate: false,
        limit: 2,
    };
    let r = x.graph.links(&req).await.expect("links");
    assert!(r.truncated, "hot node with limit=2 truncates");
    assert!(matches!(
        r.reason,
        TruncReason::Fanout | TruncReason::NodeBudget
    ));
    assert!(r.nodes.len() <= 3, "start + limit neighbors");
}

// ---------------------------------------------------------------------------
// Supersession lineage (recursive CTE)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supersedes_lineage_complete_holes_and_heads() {
    let x = ctx().await;
    let ent = serde_json::json!({ "project": "proj-1", "issue": "iss-1", "agent": "agent-a", "run": "run-1" });
    let mut ids = Vec::new();
    let mut prev: Option<Uuid> = None;
    for i in 0..3 {
        let mut j = gw_flush(Some("status"), &format!("v{i}"), ent.clone());
        // Distinct event times per version — the ADR chain order is
        // (event_time, then id); a real supersession chain is time-ordered.
        j["fact"]["event_time"] = serde_json::json!(format!("2026-10-1{}T10:00:00Z", 4 + i));
        if let Some(p) = prev {
            j["fact"]["supersedes"] = serde_json::json!(p.to_string());
        }
        let e = ingest(&x, &envelope(j)).await;
        prev = Some(e.fact_id);
        ids.push(e.fact_id);
    }
    drain(&x).await;
    let [v1, v2, v3] = [ids[0], ids[1], ids[2]];

    let req = memkeys_server::graph::SupersedesReq {
        tenant: x.tenant.clone(),
        project: None,
        fact: NodeRef {
            kind: NodeKind::Fact,
            id: node_id(&x.tenant, NodeKind::Fact, &v3.to_string()),
        },
        deadline: std::time::Duration::from_millis(500),
    };
    let r = x.graph.supersedes(&req).await.expect("lineage");
    assert!(!r.truncated);
    assert!(r.complete, "no projection holes");
    assert!(r.branches.is_empty());
    // Chain: root→head, all three versions, hydrated bodies.
    assert_eq!(r.chain.len(), 3);
    assert_eq!(
        r.chain[0].record.as_ref().map(|x| x.body.clone()),
        Some("v0".into())
    );

    // Hole: delete the v2→v1 supersedes edge — the walk shortens and MUST
    // signal incomplete (H-D), while current-head stays JOIN-authoritative.
    sqlx::query(
        "DELETE FROM edges WHERE tenant = $1 AND edge_type = 'supersedes' \
         AND src_id = $2 AND dst_id = $3",
    )
    .bind(&x.tenant)
    .bind(node_id(&x.tenant, NodeKind::Fact, &v2.to_string()))
    .bind(node_id(&x.tenant, NodeKind::Fact, &v1.to_string()))
    .execute(x.graph.pool())
    .await
    .expect("punch lineage hole");
    let r = x.graph.supersedes(&req).await.expect("lineage with hole");
    assert!(!r.complete, "hole-shortened chain signals incomplete");
    assert_eq!(r.chain.len(), 2);

    // Rebuild converges the projection back to the SoT and the chain heals.
    let stats = x
        .graph
        .rebuild(&Scope {
            tenant: x.tenant.clone(),
            project: None,
        })
        .await
        .expect("rebuild");
    assert_eq!(stats.facts_scanned, 3);
    let r = x.graph.supersedes(&req).await.expect("healed lineage");
    assert!(r.complete);
    assert_eq!(r.chain.len(), 3);
}

#[tokio::test]
async fn supersedes_reports_legacy_forks_as_branch_points() {
    let x = ctx().await;
    let ent = serde_json::json!({ "project": "proj-1", "issue": "iss-1", "agent": "agent-a", "run": "run-1" });
    let e1 = ingest(&x, &envelope(gw_flush(Some("status"), "v1", ent.clone()))).await;
    // The first successor of v1 is admitted normally: v2a names the current
    // head (e1), so the T3 CAS gate accepts it and advances the head to e2.
    let mut j2 = gw_flush(Some("status"), "v2a", ent.clone());
    j2["fact"]["supersedes"] = serde_json::json!(e1.fact_id.to_string());
    let e2 = ingest(&x, &envelope(j2)).await;
    drain(&x).await;

    // A SECOND successor of v1 is a legacy fork. v2b also supersedes e1, but
    // e1 is no longer the live head (e2 is), so today's stage-3 admission — the
    // T3 CAS gate this branch implements — rejects it as a `Conflict`. Such
    // forks therefore only exist in pre-rule / reconciler-pending history, so
    // we inject one directly at the storage layer (exactly as the reconciling
    // rebuild test injects drift) to prove `supersedes` still REPORTS it as a
    // branch point rather than hiding it.
    let e3_id = Uuid::new_v4();
    let e3_node = node_id(&x.tenant, NodeKind::Fact, &e3_id.to_string());
    let e1_node = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    sqlx::query(
        "INSERT INTO edges (tenant, project, src_kind, src_id, dst_kind, dst_id, edge_type, event_time, rule_ver) \
         VALUES ($1, 'proj-1', 'fact', $2, 'fact', $3, 'supersedes', now(), 'legacy-fork')",
    )
    .bind(&x.tenant)
    .bind(&e3_node)
    .bind(&e1_node)
    .execute(x.graph.pool())
    .await
    .expect("inject legacy fork edge");

    let req = memkeys_server::graph::SupersedesReq {
        tenant: x.tenant.clone(),
        project: None,
        fact: NodeRef {
            kind: NodeKind::Fact,
            id: node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string()),
        },
        deadline: std::time::Duration::from_millis(500),
    };
    let r = x.graph.supersedes(&req).await.expect("fork lineage");
    assert_eq!(r.branches.len(), 1, "the fork is reported, not hidden");
    let b = &r.branches[0];
    assert_eq!(
        b.at,
        node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string())
    );
    let mut heads = b.heads.clone();
    heads.sort();
    let mut expected = vec![
        node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string()),
        e3_node.clone(),
    ];
    expected.sort();
    assert_eq!(heads, expected);
}

// ---------------------------------------------------------------------------
// Reconciling rebuild
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rebuild_converges_after_drift_and_respects_scope() {
    let x = ctx().await;
    let e1 = ingest(&x, &envelope(gw_flush(None, "p1", proj_entity("proj-1")))).await;
    let e2 = ingest(&x, &envelope(gw_flush(None, "p2", proj_entity("proj-2")))).await;
    drain(&x).await;

    // Drift: delete a real edge + insert a phantom one.
    let f1 = node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string());
    sqlx::query("DELETE FROM edges WHERE tenant = $1 AND dst_id = $2 AND edge_type = 'entity_fact' AND project = 'proj-1'")
        .bind(&x.tenant)
        .bind(&f1)
        .execute(x.graph.pool())
        .await
        .expect("delete real");
    sqlx::query(
        "INSERT INTO edges (tenant, project, src_kind, src_id, dst_kind, dst_id, edge_type, event_time, rule_ver) \
         VALUES ($1, 'proj-1', 'fact', $2, 'fact', $3, 'supersedes', now(), 'stale-rule')",
    )
    .bind(&x.tenant)
    .bind(node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string()))
    .bind(node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string()))
    .execute(x.graph.pool())
    .await
    .expect("insert phantom");

    // Project-scoped rebuild heals proj-1 only.
    let stats = x
        .graph
        .rebuild(&Scope {
            tenant: x.tenant.clone(),
            project: Some("proj-1".into()),
        })
        .await
        .expect("scoped rebuild");
    assert_eq!(stats.facts_scanned, 1);
    let edges = edge_rows(&x).await;
    assert!(
        !edges.iter().any(|(s, _d, t)| s == &f1 && t == "supersedes"),
        "phantom edge evicted"
    );
    assert!(
        edges.iter().any(|(s, d, t)| d == &f1
            && s == &node_id(&x.tenant, NodeKind::EntityProject, "proj-1")
            && t == "entity_fact"),
        "deleted real edge healed"
    );

    // Tenant-wide drift: a phantom edge LIVING IN A PROJECT facet. The
    // whole-tenant rebuild (project: None) must evict it too — BLOCKER-2
    // keeps the tenant scope reconciling, not additive.
    let f2 = node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string());
    sqlx::query(
        "INSERT INTO edges (tenant, project, src_kind, src_id, dst_kind, dst_id, edge_type, event_time, rule_ver) \
         VALUES ($1, 'proj-2', 'fact', $2, 'fact', $3, 'supersedes', now(), 'stale-rule')",
    )
    .bind(&x.tenant)
    .bind(node_id(&x.tenant, NodeKind::Fact, &e2.fact_id.to_string()))
    .bind(node_id(&x.tenant, NodeKind::Fact, &e1.fact_id.to_string()))
    .execute(x.graph.pool())
    .await
    .expect("insert tenant-wide phantom");

    // Full tenant rebuild converges the whole projection to the SoT-derived
    // set: exactly the edges of the two facts.
    let stats = x
        .graph
        .rebuild(&Scope {
            tenant: x.tenant.clone(),
            project: None,
        })
        .await
        .expect("tenant rebuild");
    assert_eq!(stats.facts_scanned, 2);
    let edges = edge_rows(&x).await;
    // e1: 4 entity dims (incl. the run anchor) + produced_by = 5; e2 likewise.
    assert_eq!(
        edges.len(),
        10,
        "converged edge set, nothing stale, nothing missing"
    );
    assert!(
        !edges
            .iter()
            .any(|(s, _d, t)| (s == &f1 || s == &f2) && t == "supersedes"),
        "phantom edges evicted, including the project-carrying one"
    );
}

#[tokio::test]
async fn worker_catch_up_builds_projection_from_factstore() {
    let x = ctx().await;
    // Ingest directly through the store (queue never sees these) — the
    // projection is then built purely FROM the FactStore.
    for i in 0..3 {
        ingest(
            &x,
            &envelope(gw_flush(None, &format!("f{i}"), proj_entity("proj-1"))),
        )
        .await;
    }
    let n = x.worker.catch_up(&x.tenant).await.expect("catch up");
    assert_eq!(n, 3, "all committed facts projected from the SoT");
    let edges = edge_rows(&x).await;
    assert_eq!(
        edges.len(),
        3 * 5,
        "entity_fact x4 dims (incl. run anchor) + produced_by per fact"
    );
    // Idempotent: a second catch-up finds no holes.
    let n = x.worker.catch_up(&x.tenant).await.expect("catch up 2");
    assert_eq!(n, 0);
}
