//! HTTP transport for `/memory/*` routes (M3, §7).
//!
//! Routes:
//!   POST /memory/facts            → service.ingest()
//!   GET  /memory/facts            → service.fetch_by_keys()
//!   POST /memory/graph/traverse   → service.traverse()
//!   POST /memory/search           → 501 feature_disabled
//!
//! Admission: `X-MemKeys-Tenant` is required on every request. When
//! `MEMKEYS_AUTH_TOKEN` is set, `X-MemKeys-Token` must match it
//! (constant-time comparison).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Extension, Router,
};
use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::graph::{
    Direction, EdgeType, GraphError, NodeRef, TraverseReq, DEFAULT_DEADLINE, DEFAULT_FANOUT_CAP,
    DEFAULT_NODE_BUDGET,
};
use crate::model::{EntityKey, KeyedQuery, Validity};
use crate::pipeline::CallerContext;
use crate::service::{IngestReceipt, MemKeysService};
use crate::telemetry::{self, TRANSPORT_HTTP};
use opentelemetry::trace::{FutureExt, TraceContextExt};
use opentelemetry::Context;

/// Shared server state injected into every handler via axum `State`.
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemKeysService>,
    /// Bearer token loaded from `MEMKEYS_AUTH_TOKEN`; `None` = open (dev/test).
    pub auth_token: Option<String>,
}

impl AppState {
    pub fn new(service: Arc<MemKeysService>) -> Self {
        let auth_token = std::env::var("MEMKEYS_AUTH_TOKEN").ok();
        Self {
            service,
            auth_token,
        }
    }
}

/// Tenant id extracted by admission middleware; forwarded via `Extension`.
#[derive(Clone, Debug)]
pub struct TenantId(pub String);

/// Build the `/memory/*` axum `Router`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/memory/facts", post(ingest))
        .route("/memory/facts", get(fetch))
        .route("/memory/graph/traverse", post(traverse))
        .route("/memory/search", post(search_501))
        .layer(middleware::from_fn_with_state(state.clone(), admit))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Admission middleware
// ---------------------------------------------------------------------------

async fn admit(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Response {
    // 1. Tenant header — required on every request.
    let tenant = match headers
        .get("x-memkeys-tenant")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
    {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "X-MemKeys-Tenant header is required"})),
            )
                .into_response();
        }
    };

    // 2. Optional bearer token check (constant-time XOR-fold over same-length
    //    bytes — short-circuits only on length mismatch, which is acceptable
    //    for a bearer-token gate where length is not a secret).
    if let Some(expected) = &state.auth_token {
        let provided = headers
            .get("x-memkeys-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !ct_eq(expected, provided) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid or missing X-MemKeys-Token"})),
            )
                .into_response();
        }
    }

    req.extensions_mut().insert(TenantId(tenant));
    next.run(req).await
}

/// The HTTP transport-boundary span context (ISI-6134 B1). Attach the returned
/// context around a handler body so the request emits `memkeys.transport=http`
/// through the registry-guarded `attr()` path and the service's own
/// ingest/retrieve spans nest beneath it. The span ends when the context is
/// dropped at the end of the handler — covering every early-return branch.
fn http_transport_cx() -> Context {
    Context::current_with_span(telemetry::transport_span(
        "memkeys.http.request",
        TRANSPORT_HTTP,
        None,
    ))
}

/// XOR-fold constant-time equality (same length, no short-circuit on content).
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ---------------------------------------------------------------------------
// POST /memory/facts
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IngestResponse {
    fact_id: uuid::Uuid,
    keyed: bool,
    entity_keys_resolved: HashMap<String, String>,
    duplicate: bool,
}

impl From<IngestReceipt> for IngestResponse {
    fn from(r: IngestReceipt) -> Self {
        Self {
            fact_id: r.fact_id,
            keyed: r.keyed,
            entity_keys_resolved: r
                .entity_keys_resolved
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            duplicate: r.duplicate,
        }
    }
}

async fn ingest(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(envelope): Json<Envelope>,
) -> Response {
    let caller = CallerContext {
        tenant_id: tenant.0.clone(),
        principal: "http".to_string(),
        trace_id: None,
    };
    let now = chrono::Utc::now();
    // `with_context` attaches the transport span's context for each poll without
    // holding a `!Send` `ContextGuard` across `.await` (which would make the
    // handler future `!Send` and fail axum's `Handler` bound). Service spans
    // nest beneath it via the active context on their first synchronous poll.
    let cx = http_transport_cx();
    let result = state
        .service
        .ingest(&envelope, &caller, now)
        .with_context(cx.clone())
        .await;
    cx.span().end();
    match result {
        Ok(receipt) => (StatusCode::OK, Json(IngestResponse::from(receipt))).into_response(),
        Err(IngestError::Rejected { reason }) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": reason})),
        )
            .into_response(),
        Err(IngestError::Conflict { existing_id }) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "conflict", "existing_id": existing_id})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// GET /memory/facts
// ---------------------------------------------------------------------------

async fn fetch(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let query = match build_keyed_query(&params) {
        Ok(q) => q,
        Err(reason) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": reason})),
            )
                .into_response();
        }
    };
    // Transport span created on the served path (after argument validation),
    // attached via `with_context` to stay `Send` across `.await` (see `ingest`).
    let cx = http_transport_cx();
    let result = state
        .service
        .fetch_by_keys(&tenant.0, &query)
        .with_context(cx.clone())
        .await;
    cx.span().end();
    match result {
        // §7.2/§7.5 response-shape invariant: wrap in `{ facts: [ … ] }`,
        // never a bare array (ISI-6134 B3).
        Ok(records) => (
            StatusCode::OK,
            Json(serde_json::json!({ "facts": records })),
        )
            .into_response(),
        Err(QueryError::Invalid { reason }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": reason})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Parse flat query-param map → `KeyedQuery`.
///
/// Entity keys: `entity.project=foo`, `entity.issue=bar`, etc.
/// Optional: `type` (§7.2; `fact_type` accepted as a legacy alias),
/// `as_of` (RFC3339 point-in-time replay), `limit`, `valid` (`current` | `all`).
fn build_keyed_query(params: &HashMap<String, String>) -> Result<KeyedQuery, String> {
    const KEY_MAP: &[(&str, EntityKey)] = &[
        ("entity.project", EntityKey::Project),
        ("entity.issue", EntityKey::Issue),
        ("entity.pr", EntityKey::Pr),
        ("entity.agent", EntityKey::Agent),
        ("entity.run", EntityKey::Run),
        ("entity.thread", EntityKey::Thread),
        ("entity.service", EntityKey::Service),
    ];

    let keys: Vec<(EntityKey, String)> = KEY_MAP
        .iter()
        .filter_map(|(name, ek)| params.get(*name).map(|v| (*ek, v.clone())))
        .collect();

    // §7.2 documents `type`; accept `fact_type` as a legacy alias (ISI-6134 B1).
    let fact_type = match params.get("type").or_else(|| params.get("fact_type")) {
        Some(ft) => Some(
            serde_json::from_value(serde_json::Value::String(ft.clone()))
                .map_err(|_| format!("unknown type: {ft}"))?,
        ),
        None => None,
    };

    // §7.2 `as_of=<RFC3339>` point-in-time replay (S1); absent = live head.
    let as_of = match params.get("as_of") {
        Some(ts) => Some(
            chrono::DateTime::parse_from_rfc3339(ts)
                .map_err(|_| format!("as_of must be RFC3339: {ts}"))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };

    let limit = match params.get("limit") {
        Some(l) => l
            .parse::<i64>()
            .map_err(|_| "limit must be an integer".to_string())?,
        None => KeyedQuery::DEFAULT_LIMIT,
    };

    let valid = match params.get("valid").map(|s| s.as_str()) {
        Some("all") => Validity::All,
        Some("current") | None => Validity::Current,
        Some(other) => return Err(format!("unknown valid value: {other}")),
    };

    Ok(KeyedQuery {
        keys,
        fact_type,
        as_of,
        valid,
        limit,
    })
}

// ---------------------------------------------------------------------------
// POST /memory/graph/traverse
// ---------------------------------------------------------------------------

/// Wire format for a traverse request (`Duration` fields use milliseconds).
#[derive(Deserialize)]
struct TraverseReqWire {
    start: NodeRef,
    #[serde(default)]
    edges: Vec<EdgeType>,
    // §7.3 documents `direction`; accept `dir` as a legacy alias (ISI-6134 B2).
    #[serde(default, alias = "dir")]
    direction: Direction,
    #[serde(default = "default_depth")]
    depth: u8,
    #[serde(default)]
    hydrate: bool,
    #[serde(default = "default_deadline_ms")]
    deadline_ms: u64,
}

fn default_depth() -> u8 {
    2
}
fn default_deadline_ms() -> u64 {
    DEFAULT_DEADLINE.as_millis() as u64
}

async fn traverse(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantId>,
    Json(body): Json<TraverseReqWire>,
) -> Response {
    let req = TraverseReq {
        tenant: tenant.0.clone(),
        project: None,
        start: body.start,
        edges: body.edges,
        dir: body.direction,
        depth: body.depth,
        deadline: Duration::from_millis(body.deadline_ms),
        hydrate: body.hydrate,
        fanout_cap: DEFAULT_FANOUT_CAP,
        node_budget: DEFAULT_NODE_BUDGET,
    };
    // Attached via `with_context` to stay `Send` across `.await` (see `ingest`).
    let cx = http_transport_cx();
    let result = state
        .service
        .traverse(&tenant.0, &req)
        .with_context(cx.clone())
        .await;
    cx.span().end();
    match result {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(GraphError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
        Err(GraphError::Invalid(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /memory/search → 501
// ---------------------------------------------------------------------------

async fn search_501() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "feature_disabled",
            "feature": "semantic_search",
            "message": "Semantic search is not available in M3; use GET /memory/facts for keyed retrieval."
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn ct_eq_matches_only_identical_strings() {
        assert!(ct_eq("secret", "secret"));
        assert!(!ct_eq("secret", "Secret"));
        assert!(!ct_eq("secret", "secre")); // length mismatch
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn build_keyed_query_reads_type_and_fact_type_alias() {
        // §7.2 canonical spelling (ISI-6134 B1).
        let q =
            build_keyed_query(&params(&[("entity.issue", "I1"), ("type", "decision")])).unwrap();
        assert!(q.fact_type.is_some());
        // Legacy alias still accepted.
        let q = build_keyed_query(&params(&[
            ("entity.issue", "I1"),
            ("fact_type", "decision"),
        ]))
        .unwrap();
        assert!(q.fact_type.is_some());
        // Unknown value is a hard error, not a silently dropped filter.
        assert!(build_keyed_query(&params(&[("type", "bogus")])).is_err());
    }

    #[test]
    fn build_keyed_query_wires_as_of() {
        // §7.2 as_of (S1): a valid RFC3339 timestamp lands in the query.
        let q = build_keyed_query(&params(&[("as_of", "2026-10-18T00:00:00Z")])).unwrap();
        assert!(q.as_of.is_some());
        // Garbage is rejected rather than ignored.
        assert!(build_keyed_query(&params(&[("as_of", "last-tuesday")])).is_err());
        // Absent = live head.
        assert!(build_keyed_query(&params(&[])).unwrap().as_of.is_none());
    }

    #[test]
    fn traverse_wire_accepts_direction_and_dir_alias() {
        // §7.3 canonical spelling + legacy alias both parse to the same value
        // (ISI-6134 B2) — neither silently defaults to `out`.
        let canonical: TraverseReqWire = serde_json::from_value(
            serde_json::json!({"start": {"kind":"fact","id":"F1"}, "direction": "in"}),
        )
        .unwrap();
        assert_eq!(canonical.direction, Direction::In);
        let alias: TraverseReqWire = serde_json::from_value(
            serde_json::json!({"start": {"kind":"fact","id":"F1"}, "dir": "both"}),
        )
        .unwrap();
        assert_eq!(alias.direction, Direction::Both);
    }
}
