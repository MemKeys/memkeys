//! HTTP transport for `/memory/*` routes (M3, §7).
//!
//! Routes:
//!   POST /memory/facts            → service.ingest()
//!   GET  /memory/facts            → service.fetch_by_keys()
//!   POST /memory/graph/traverse   → service.traverse()
//!   POST /memory/search           → 501 feature_disabled
//!
//! Admission + per-tenant authz (ISI-6151, WS-5 §2.2/§3, closes the §7.4/R6
//! deviation): every request is authenticated by a verified bearer token and
//! the tenant floor is DERIVED from its claims — never from a client-supplied
//! `X-MemKeys-Tenant` header (which is now ignored). The admission layer then
//! load-sheds (§7 `rate_limited`), caps write size (F14), and throttles
//! boundary-probing (F15) via the shared [`AdmissionControl`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Extension, Router,
};
use serde::{Deserialize, Serialize};

use crate::admission::AdmissionControl;
use crate::auth::{JwtVerifier, Principal, StaticVerifier, TokenVerifier};
use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::graph::{
    Direction, EdgeType, GraphError, NodeRef, TraverseReq, DEFAULT_DEADLINE, DEFAULT_FANOUT_CAP,
    DEFAULT_NODE_BUDGET,
};
use crate::model::{EntityKey, KeyedQuery, Validity};
use crate::service::{IngestReceipt, MemKeysService};
use crate::telemetry::{self, TRANSPORT_HTTP};
use crate::wire::WireError;
use opentelemetry::trace::{FutureExt, TraceContextExt};
use opentelemetry::Context;

/// Shared server state injected into every handler via axum `State`.
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemKeysService>,
    /// Verifies a presented bearer token into a [`Principal`] (§3). The tenant
    /// floor is read from the verified claims, never a request header.
    pub verifier: Arc<dyn TokenVerifier>,
    /// Shared self-protection layer (load shed / size caps / probe throttle).
    pub admission: Arc<AdmissionControl>,
}

impl AppState {
    /// Explicit constructor — tests inject a verifier + admission directly.
    pub fn new(
        service: Arc<MemKeysService>,
        verifier: Arc<dyn TokenVerifier>,
        admission: Arc<AdmissionControl>,
    ) -> Self {
        Self {
            service,
            verifier,
            admission,
        }
    }

    /// Build HTTP state from the environment, sharing `admission` with the
    /// process's other transports.
    ///
    /// - `MEMKEYS_JWT_SECRET` set → per-request HS256 [`JwtVerifier`]: the
    ///   authenticated path, tenant floor from the token's `company_id` claim.
    /// - unset → an insecure dev [`StaticVerifier`] bound to one fixed,
    ///   server-side tenant (`MEMKEYS_RUN_TENANT`/`MEMKEYS_DEV_TENANT`, default
    ///   `dev`). Still never client-supplied; `main` refuses a non-loopback
    ///   bind in this mode unless the operator opts in.
    pub fn from_env(service: Arc<MemKeysService>, admission: Arc<AdmissionControl>) -> Self {
        let verifier: Arc<dyn TokenVerifier> = match std::env::var("MEMKEYS_JWT_SECRET") {
            Ok(secret) if !secret.is_empty() => Arc::new(JwtVerifier::new(secret.into_bytes())),
            _ => Arc::new(StaticVerifier::new(
                crate::auth::run_token_principal_from_env(),
            )),
        };
        Self::new(service, verifier, admission)
    }
}

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
// Admission + authentication middleware
// ---------------------------------------------------------------------------

async fn admit(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Response {
    // Gate 1 (§7 load-shed): take an in-flight permit before any work. The
    // guard is held across `next.run`, so the in-flight gauge tracks real work
    // and releases on every return path (RAII).
    let _guard = match state.admission.try_admit() {
        Ok(g) => g,
        Err(e) => return wire_error_response(&e),
    };

    // Authenticate: the tenant floor is DERIVED from a verified bearer token,
    // never from a client-supplied header (§7.4/R6, ISI-6151). Any
    // `X-MemKeys-Tenant` header is ignored. A missing/invalid/expired token is
    // `unauthenticated` (401) — never a distinct error that reveals why (§3/§7).
    let now = chrono::Utc::now().timestamp();
    let principal = match state.verifier.verify(bearer_token(&headers), now) {
        Ok(p) => p,
        Err(e) => return wire_error_response(&e),
    };

    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// Extract the raw bearer token: `Authorization: Bearer <t>` (standard), or the
/// legacy `X-MemKeys-Token` header. Empty when neither is present — the
/// verifier decides whether that is acceptable (the dev `StaticVerifier`
/// ignores it; `JwtVerifier` rejects it as `unauthenticated`).
fn bearer_token(headers: &HeaderMap) -> &str {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(rest) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            return rest.trim();
        }
    }
    headers
        .get("x-memkeys-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Render a [`WireError`] as the HTTP response: the WS-5 §7 taxonomy status +
/// the structured error body, with `Retry-After` set when the error carries it.
fn wire_error_response(e: &WireError) -> Response {
    let status = StatusCode::from_u16(e.error_code().http_status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut resp = (status, Json(e)).into_response();
    if let Some(secs) = e.retry_after_secs {
        if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    resp
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
    Extension(principal): Extension<Principal>,
    Json(envelope): Json<Envelope>,
) -> Response {
    // Gate 2 (F14): reject an over-cap write before the pipeline can plant an
    // unbounded body. `fact.body` is arbitrary JSON — size it by its serialized
    // byte length.
    let body_bytes = serde_json::to_vec(&envelope.fact.body)
        .map(|v| v.len())
        .unwrap_or(0);
    if let Err(e) = state
        .admission
        .check_write_size(body_bytes, envelope.attachments.len())
    {
        return wire_error_response(&e);
    }
    // The caller context — tenant floor + producer principal — comes from the
    // verified token, not the request body (ISI-6151).
    let caller = principal.caller_context();
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
    Extension(principal): Extension<Principal>,
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
    // The tenant floor is the verified principal's, not a request header.
    let cx = http_transport_cx();
    let result = state
        .service
        .fetch_by_keys(&principal.tenant_id, &query)
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
    Extension(principal): Extension<Principal>,
    Json(body): Json<TraverseReqWire>,
) -> Response {
    // The tenant floor is the verified principal's — `service.traverse`
    // overwrites `req.tenant` with it, so a request cannot assert another.
    let req = TraverseReq {
        tenant: principal.tenant_id.clone(),
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
        .traverse(&principal.tenant_id, &req)
        .with_context(cx.clone())
        .await;
    cx.span().end();
    match result {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        // A boundary-crossing miss is also the §3.3 existence oracle — record it
        // (F15) and convert to a `rate_limited` throttle once a principal is over
        // its probe budget, so fail-closed `not_found` is not unlimited.
        Err(GraphError::NotFound) => {
            if let Err(throttle) = state
                .admission
                .record_boundary_miss(&principal.tenant_id, chrono::Utc::now().timestamp())
            {
                return wire_error_response(&throttle);
            }
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not found"})),
            )
                .into_response()
        }
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
    fn bearer_token_reads_authorization_then_legacy_header() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer abc.def.ghi".parse().unwrap());
        assert_eq!(bearer_token(&h), "abc.def.ghi");
        // Case-insensitive scheme.
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "bearer tok".parse().unwrap());
        assert_eq!(bearer_token(&h), "tok");
        // Legacy X-MemKeys-Token fallback.
        let mut h = HeaderMap::new();
        h.insert("x-memkeys-token", "legacy".parse().unwrap());
        assert_eq!(bearer_token(&h), "legacy");
        // Neither present → empty (verifier decides).
        assert_eq!(bearer_token(&HeaderMap::new()), "");
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
