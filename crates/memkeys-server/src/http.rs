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

#[tracing::instrument(skip(state, envelope), fields(memkeys.tenant.id = %tenant.0))]
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
    match state.service.ingest(&envelope, &caller, now).await {
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

#[tracing::instrument(skip(state, params), fields(memkeys.tenant.id = %tenant.0))]
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
    match state.service.fetch_by_keys(&tenant.0, &query).await {
        Ok(records) => (StatusCode::OK, Json(records)).into_response(),
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
/// Optional: `fact_type`, `limit`, `valid` (`current` | `all`).
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

    let fact_type = match params.get("fact_type") {
        Some(ft) => Some(
            serde_json::from_value(serde_json::Value::String(ft.clone()))
                .map_err(|_| format!("unknown fact_type: {ft}"))?,
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
        as_of: None,
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
    #[serde(default)]
    dir: Direction,
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

#[tracing::instrument(skip(state, body), fields(memkeys.tenant.id = %tenant.0))]
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
        dir: body.dir,
        depth: body.depth,
        deadline: Duration::from_millis(body.deadline_ms),
        hydrate: body.hydrate,
        fanout_cap: DEFAULT_FANOUT_CAP,
        node_budget: DEFAULT_NODE_BUDGET,
    };
    match state.service.traverse(&tenant.0, &req).await {
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
