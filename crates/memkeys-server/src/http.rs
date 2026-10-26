//! HTTP transport for `/memory/*` routes (M3, §7; WS-5 §2.2 per-request auth).
//!
//! Routes:
//!   POST /memory/facts            → service.ingest()
//!   GET  /memory/facts            → service.fetch_by_keys()
//!   POST /memory/graph/traverse   → service.traverse()
//!   POST /memory/search           → service.semantic_search() (op-4, §7.4;
//!                                    501 feature_disabled when the module is off)
//!   POST /memory/flush            → service.flush_semantic() (ISI-6677: force
//!                                    the async stage-5b vector projection to
//!                                    converge so a just-stored fact is
//!                                    deterministically searchable; opt-in)
//!
//! Admission (ISI-6148, closes ISI-6134 S1):
//!   1. Concurrency shedding ([`AdmissionControl::try_admit`]) gates every
//!      request — a saturated pool sheds `rate_limited` (429), not unbounded
//!      queueing.
//!   2. When a [`TokenVerifier`] is configured, every request MUST carry a
//!      `Authorization: Bearer <jwt>`; the server verifies it per request and
//!      derives the R6 tenant floor SERVER-SIDE from the principal (§3). The
//!      client-asserted `X-MemKeys-Tenant` header is no longer trusted as a
//!      tenant source — it is honored only as an intersect-only *narrower*
//!      (must equal the principal tenant) and can never widen it. A mismatch
//!      is `not_found` (§3.3 collapse — never an existence oracle).
//!   3. With no verifier (loopback dev only — `main` fails closed on a
//!      non-loopback bind), `X-MemKeys-Tenant` is self-asserted (the documented
//!      S1 dev path).
//!
//! The F14 write-size cap and the F15 boundary-probe throttle are wired into
//! the write and traverse handlers respectively.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, Request, State},
    http::{header, header::AUTHORIZATION, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Extension, Router,
};
use serde::{Deserialize, Serialize};

use crate::admission::AdmissionControl;
use crate::auth::{Principal, TokenVerifier};
use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::graph::{
    Direction, EdgeType, GraphError, NodeRef, TraverseReq, DEFAULT_DEADLINE, DEFAULT_FANOUT_CAP,
    DEFAULT_NODE_BUDGET,
};
use crate::model::{EntityKey, FactType, KeyedQuery, Validity};
use crate::service::{IngestReceipt, MemKeysService, SearchRequest, SemanticHit};
use crate::telemetry::{self, TRANSPORT_HTTP};
use crate::wire::{ErrorCode, WireError};
use opentelemetry::trace::{FutureExt, TraceContextExt};
use opentelemetry::Context;
use tower::Layer;
use tower_http::services::ServeDir;

/// Shared server state injected into every handler via axum `State`.
#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemKeysService>,
    /// Per-request bearer verifier (the Paperclip run-token `JwtVerifier`).
    /// `None` = open loopback dev mode — tenant is self-asserted (S1 dev path);
    /// `main` refuses a non-loopback bind in this mode.
    pub verifier: Option<Arc<dyn TokenVerifier>>,
    /// Self-protection gates (concurrency shedding, F14 size caps, F15 probe
    /// throttle), shared across both transports.
    pub admission: Arc<AdmissionControl>,
    /// Directory of the built Inspector SPA (ISI-6476, parent ISI-6395:
    /// "website without nginx"). When present the memory-transport listener
    /// also serves the SPA at `/` with a history-API fallback. Static UI is
    /// NOT admitted through the auth middleware — it is read-through and
    /// calls the same token-gated `/memory/*` API as any other client.
    /// `None` (or a missing dir) = UI not bundled; `/memory/*` unaffected.
    pub ui_dir: Option<PathBuf>,
}

impl AppState {
    pub fn new(
        service: Arc<MemKeysService>,
        verifier: Option<Arc<dyn TokenVerifier>>,
        admission: Arc<AdmissionControl>,
    ) -> Self {
        Self {
            service,
            verifier,
            admission,
            ui_dir: None,
        }
    }

    /// Bundle the Inspector SPA from `dir` (no-ops when the dir is absent at
    /// request time, so a misconfigured deploy serves `/memory/*` normally).
    pub fn with_ui_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.ui_dir = dir;
        self
    }
}

/// Build the `/memory/*` axum `Router`, optionally serving the Inspector SPA.
///
/// `/memory/*` keeps the full admission stack (verifier + self-protection).
/// When `state.ui_dir` exists the SPA is attached as the router fallback:
/// `/` and any non-API path serve `index.html` (history-API fallback, nginx
/// parity), `/assets/*` serve content-hashed statics with an immutable
/// cache header. The UI router is deliberately outside `admit` — it adds no
/// auth surface; the SPA talks to the token-gated API like any client.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/memory/facts", post(ingest))
        .route("/memory/facts", get(fetch))
        .route("/memory/graph/traverse", post(traverse))
        .route("/memory/search", post(search))
        .route("/memory/flush", post(flush))
        .route_layer(middleware::from_fn_with_state(state.clone(), admit))
        .with_state(state.clone());

    match state.ui_dir.as_ref().filter(|d| d.is_dir()) {
        Some(dir) => {
            let ui = Router::new()
                .route("/", get(index_html))
                .nest_service(
                    "/assets",
                    tower_http::set_header::SetResponseHeaderLayer::overriding(
                        header::CACHE_CONTROL,
                        HeaderValue::from_static(CACHE_ASSETS_IMMUTABLE),
                    )
                    // nest_service strips the /assets prefix; the Vite
                    // bundle serves content-hashed files from dist/assets/.
                    .layer(ServeDir::new(dir.join("assets"))),
                )
                .fallback(spa_fallback)
                .with_state(state);
            api.fallback_service(ui)
        }
        None => api,
    }
}

// ---------------------------------------------------------------------------
// Inspector UI (nginx-free, ISI-6476 / ISI-6395)
// ---------------------------------------------------------------------------

/// Vite content-hashes `/assets/*`, so any change ships a new filename —
/// those can be cached aggressively (nginx parity).
const CACHE_ASSETS_IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `index.html` must always revalidate so deploys roll out on the next load.
const CACHE_INDEX_NO_CACHE: &str = "no-cache";

async fn index_html(State(state): State<AppState>) -> Response {
    serve_index(&state).await
}

/// History-API fallback: the SPA is a single page, so any path that is not a
/// real file or a `/memory/*` route serves `index.html`.
async fn spa_fallback(State(state): State<AppState>) -> Response {
    serve_index(&state).await
}

async fn serve_index(state: &AppState) -> Response {
    let Some(dir) = state.ui_dir.as_deref() else {
        return (StatusCode::NOT_FOUND, "UI not bundled").into_response();
    };
    match tokio::fs::read(dir.join("index.html")).await {
        Ok(body) => (
            [
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(CACHE_INDEX_NO_CACHE),
                ),
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                ),
            ],
            body,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "UI index.html missing").into_response(),
    }
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
    // Gate 1 (concurrency shedding): take an in-flight permit before any work.
    // The guard releases the permit on drop at the end of the request, so the
    // in-flight depth always reflects live work. A saturated pool sheds
    // `rate_limited` (429) rather than queueing unboundedly.
    let _admit = match state.admission.try_admit() {
        Ok(g) => g,
        Err(e) => return wire_response(e),
    };

    let now = chrono::Utc::now().timestamp();

    // Principal derivation (§2.2 / §3). With a verifier configured, the bearer
    // JWT is the ONLY tenant source — the floor is derived server-side and is
    // never client-assertable.
    let principal = match &state.verifier {
        Some(verifier) => {
            let bearer = headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.strip_prefix("Bearer ")
                        .or_else(|| v.strip_prefix("bearer "))
                })
                .unwrap_or("");
            match verifier.verify(bearer, now) {
                Ok(p) => p,
                // Never reveal *why* — one opaque `unauthenticated` (§3/§7).
                Err(e) => return wire_response(e),
            }
        }
        // Open loopback dev mode (S1 dev path): tenant is self-asserted. `main`
        // refuses a non-loopback bind in this mode.
        None => {
            let tenant = match headers
                .get("x-memkeys-tenant")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
            {
                Some(t) => t.to_owned(),
                None => {
                    return wire_response(WireError::new(
                        ErrorCode::Unauthenticated,
                        "X-MemKeys-Tenant header is required in open (no-verifier) mode",
                    ));
                }
            };
            Principal {
                tenant_id: tenant,
                agent_id: "http-dev".to_string(),
                run_id: None,
                expires_at: i64::MAX,
                trace_id: None,
            }
        }
    };

    // `X-MemKeys-Tenant` is an intersect-only *narrower*, never a widener
    // (§3.3): when present it MUST equal the principal's tenant. A different
    // value is a cross-tenant assertion → `not_found`, never an existence
    // oracle and never a `forbidden` that would confirm the tenant exists.
    if let Some(asserted) = headers
        .get("x-memkeys-tenant")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        if state.verifier.is_some() && asserted != principal.tenant_id {
            return wire_response(WireError::new(
                ErrorCode::NotFound,
                "not found within tenant",
            ));
        }
    }

    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// Render a [`WireError`] as the WS-5 §7 HTTP response: the taxonomy status +
/// the structured error body (so clients read a stable `code`/`retriable`).
fn wire_response(err: WireError) -> Response {
    let status = StatusCode::from_u16(err.error_code().http_status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(err)).into_response()
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
    // F14 write-size cap: reject an over-cap body/attachment set as
    // `invalid_envelope` (422) BEFORE it reaches the pipeline.
    let body_bytes = serde_json::to_vec(&envelope.fact.body)
        .map(|v| v.len())
        .unwrap_or(0);
    if let Err(e) = state
        .admission
        .check_write_size(body_bytes, envelope.attachments.len())
    {
        return wire_response(e);
    }

    // The caller context — the trust root for keys (§4 rule 1) — is the VERIFIED
    // principal's tenant/agent, never a client-asserted value.
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
    // The tenant floor is the VERIFIED principal's, never client-asserted.
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
    // The tenant floor is the VERIFIED principal's; `service.traverse` also
    // overwrites `req.tenant` from it (WS-4 Invariant T), so a body value can
    // never widen scope.
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
        // F15: a boundary-crossing lookup is `not_found` (§3.3 — no existence
        // oracle). Record the miss; once the principal is over its probe budget
        // the would-be 404 becomes a `rate_limited` (429) throttle.
        Err(GraphError::NotFound) => {
            let now = chrono::Utc::now().timestamp();
            match state
                .admission
                .record_boundary_miss(&principal.tenant_id, now)
            {
                Ok(()) => wire_response(WireError::new(ErrorCode::NotFound, "not found")),
                Err(throttle) => wire_response(throttle),
            }
        }
        Err(GraphError::Invalid(msg)) => {
            wire_response(WireError::new(ErrorCode::InvalidArgument, msg))
        }
        Err(e) => wire_response(WireError::new(ErrorCode::BackendUnavailable, e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// POST /memory/search — op-4 semantic search (§7.4)
// ---------------------------------------------------------------------------

/// Wire format for op-4 (§7.4). `scope` carries the caller's finer `entity.*`
/// filters + `fact.type`, ANDed and translated to a candidate id set; the
/// tenant floor is NOT here — it is SERVER-SET from the verified principal
/// (§7.4 R6, same as traverse). `tiers` selects fact/digest/auto.
#[derive(Deserialize)]
struct SearchReqWire {
    query: String,
    #[serde(default)]
    scope: HashMap<String, String>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    tiers: Option<String>,
    #[serde(default)]
    as_of: Option<String>,
}

/// The parsed §7.4 scope: ANDed `entity.*` keys + an optional `fact.type`.
type SearchScope = (Vec<(EntityKey, String)>, Option<FactType>);

/// Parse the §7.4 `scope` map → `(entity.* keys, fact.type)`. Unknown scope
/// keys are rejected rather than silently ignored (a mis-typed filter would
/// otherwise widen the result set). `category` is reserved for the M6 facet
/// surface and is accepted-but-unenforced with a note (not yet a candidate
/// pre-filter).
fn parse_search_scope(scope: &HashMap<String, String>) -> Result<SearchScope, String> {
    const KEY_MAP: &[(&str, EntityKey)] = &[
        ("entity.project", EntityKey::Project),
        ("entity.issue", EntityKey::Issue),
        ("entity.pr", EntityKey::Pr),
        ("entity.agent", EntityKey::Agent),
        ("entity.run", EntityKey::Run),
        ("entity.thread", EntityKey::Thread),
        ("entity.service", EntityKey::Service),
    ];
    let mut keys = Vec::new();
    let mut fact_type = None;
    for (k, v) in scope {
        if let Some((_, ek)) = KEY_MAP.iter().find(|(name, _)| name == k) {
            keys.push((*ek, v.clone()));
        } else if k == "fact.type" || k == "type" {
            fact_type = Some(
                serde_json::from_value(serde_json::Value::String(v.clone()))
                    .map_err(|_| format!("unknown fact.type: {v}"))?,
            );
        } else if k == "category" {
            // D4 category facet is an M6 surface; accept but don't pre-filter yet.
            continue;
        } else {
            return Err(format!("unknown scope key: {k}"));
        }
    }
    Ok((keys, fact_type))
}

async fn search(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<SearchReqWire>,
) -> Response {
    let (scope, fact_type) = match parse_search_scope(&body.scope) {
        Ok(s) => s,
        Err(reason) => {
            return wire_response(WireError::new(ErrorCode::InvalidArgument, reason));
        }
    };
    let as_of = match body.as_of.as_deref() {
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(t) => Some(t.with_timezone(&chrono::Utc)),
            Err(_) => {
                return wire_response(WireError::new(
                    ErrorCode::InvalidArgument,
                    format!("as_of must be RFC3339: {ts}"),
                ));
            }
        },
        None => None,
    };
    // `tiers=digest` is an explicit digest-only request; the digest sub-flag is
    // a later milestone, so it degrades to the fact tier with a warning header
    // rather than a 501 (§7.4). `auto`/`fact` run the fact tier silently.
    let digest_requested = matches!(body.tiers.as_deref(), Some("digest"));

    let req = SearchRequest {
        query: body.query,
        scope,
        fact_type,
        k: body.k.unwrap_or(SearchRequest::DEFAULT_K),
        as_of,
    };

    let cx = http_transport_cx();
    let result = state
        .service
        .semantic_search(&principal.tenant_id, &req)
        .with_context(cx.clone())
        .await;
    cx.span().end();
    match result {
        Ok(hits) => {
            let body = serde_json::json!({
                "hits": hits.iter().map(SemanticHit::to_wire_json).collect::<Vec<_>>(),
            });
            let mut resp = (StatusCode::OK, Json(body)).into_response();
            if digest_requested {
                resp.headers_mut().insert(
                    "x-memkeys-warning",
                    axum::http::HeaderValue::from_static(
                        "digest tier unavailable (memory.semantic.digest off); served fact tier",
                    ),
                );
            }
            resp
        }
        // §7.4: the semantic module off is a 501 `feature_disabled`, never a
        // silent empty result — callers degrade gracefully.
        Err(QueryError::FeatureDisabled { feature }) => wire_response(WireError::new(
            ErrorCode::FeatureDisabled,
            format!("semantic module disabled: {feature}"),
        )),
        Err(QueryError::Invalid { reason }) => {
            wire_response(WireError::new(ErrorCode::InvalidArgument, reason))
        }
        Err(e) => wire_response(WireError::new(ErrorCode::BackendUnavailable, e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// POST /memory/flush — fact-vector readiness (ISI-6677)
// ---------------------------------------------------------------------------

/// Force the async stage-5b vector projection to CONVERGE for the caller's
/// tenant, so a just-stored fact is deterministically searchable (a benchmark
/// harness that writes then immediately queries would otherwise race the
/// worker — empty hit set until stage 5b lands). OPT-IN: embedding stays off
/// the default commit path; this is an explicit flush call. No request body;
/// the tenant floor is the VERIFIED principal's (never client-asserted). The
/// response reports how many facts the convergence scan embedded.
async fn flush(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let cx = http_transport_cx();
    let result = state
        .service
        .flush_semantic(&principal.tenant_id)
        .with_context(cx.clone())
        .await;
    cx.span().end();
    match result {
        Ok(embedded) => {
            let body = serde_json::json!({ "tier": "fact_vector", "embedded": embedded });
            (StatusCode::OK, Json(body)).into_response()
        }
        // Same seam as op-4: the module off is a 501 `feature_disabled`, never
        // a silent "converged" that would let a harness query an empty tier.
        Err(QueryError::FeatureDisabled { feature }) => wire_response(WireError::new(
            ErrorCode::FeatureDisabled,
            format!("semantic module disabled: {feature}"),
        )),
        Err(e) => wire_response(WireError::new(ErrorCode::BackendUnavailable, e.to_string())),
    }
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
    fn wire_response_carries_taxonomy_status() {
        // The §7 error body renders at the taxonomy's HTTP status.
        let r = wire_response(WireError::new(ErrorCode::NotFound, "x"));
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = wire_response(WireError::new(ErrorCode::RateLimited, "x"));
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        let r = wire_response(WireError::new(ErrorCode::Unauthenticated, "x"));
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
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

    // -- ISI-6148: per-request principal auth + server-derived tenant floor --

    fn mint_jwt(secret: &[u8], claims: &serde_json::Value) -> String {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let header = b64(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64(serde_json::to_vec(claims).unwrap().as_slice());
        let signing_input = format!("{header}.{payload}");
        let mut mac = <Hmac<Sha256>>::new_from_slice(secret).unwrap();
        mac.update(signing_input.as_bytes());
        let sig = b64(&mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    fn verified_router(secret: &[u8]) -> Router {
        use crate::auth::JwtVerifier;
        use crate::memstore::MemStore;
        use crate::service::MemKeysService;
        let service = Arc::new(MemKeysService::new(Arc::new(MemStore::new())));
        let state = AppState::new(
            service,
            Some(Arc::new(JwtVerifier::new(secret.to_vec()))),
            Arc::new(AdmissionControl::with_defaults()),
        );
        router(state)
    }

    #[tokio::test]
    async fn rejects_request_with_no_bearer() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let app = verified_router(b"s3cr3t");
        // No Authorization header → middleware stops it as `unauthenticated`
        // before the handler ever runs. An X-MemKeys-Tenant header cannot
        // substitute for a verified token.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/memory/facts?entity.issue=I1")
                    .header("x-memkeys-tenant", "tenant-victim")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn x_memkeys_tenant_cannot_widen_past_principal() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let secret = b"s3cr3t";
        let app = verified_router(secret);
        let token = mint_jwt(
            secret,
            &serde_json::json!({"sub":"agent-a","company_id":"tenant-a","exp":9_999_999_999i64}),
        );
        // A valid token for tenant-a, but the client asserts a DIFFERENT tenant
        // in the header → cross-tenant → `not_found` (§3.3), never served and
        // never a `forbidden` existence oracle.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/memory/facts?entity.issue=I1")
                    .header("authorization", format!("Bearer {token}"))
                    .header("x-memkeys-tenant", "tenant-b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn valid_bearer_reaches_handler_with_derived_floor() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let secret = b"s3cr3t";
        let app = verified_router(secret);
        let token = mint_jwt(
            secret,
            &serde_json::json!({"sub":"agent-a","company_id":"tenant-a","exp":9_999_999_999i64}),
        );
        // Valid token, matching (or absent) tenant header → passes admission and
        // the keyed read runs against the derived floor (empty store → 200 with
        // `{facts:[]}`), proving the principal reached the handler.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/memory/facts?entity.issue=I1")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A verified router whose service has the semantic module wired (in-memory
    /// index + hash embedder) — the op-4 "on" path.
    fn verified_router_semantic(secret: &[u8]) -> Router {
        use crate::auth::JwtVerifier;
        use crate::memstore::MemStore;
        use crate::semantic::{HashEmbedder, MemVectorIndex};
        use crate::service::MemKeysService;
        let service = Arc::new(
            MemKeysService::new(Arc::new(MemStore::new())).with_semantic(
                Arc::new(HashEmbedder::new()),
                Arc::new(MemVectorIndex::new()),
            ),
        );
        let state = AppState::new(
            service,
            Some(Arc::new(JwtVerifier::new(secret.to_vec()))),
            Arc::new(AdmissionControl::with_defaults()),
        );
        router(state)
    }

    #[tokio::test]
    async fn search_feature_disabled_is_501_when_module_off() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let secret = b"s3cr3t";
        // Default verified_router has NO semantic module → op-4 is a 501
        // feature_disabled, never a silent empty result (§7.4).
        let app = verified_router(secret);
        let token = mint_jwt(
            secret,
            &serde_json::json!({"sub":"agent-a","company_id":"tenant-a","exp":9_999_999_999i64}),
        );
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/memory/search")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"postgres"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn search_reaches_handler_when_module_on() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let secret = b"s3cr3t";
        // Module on but the in-memory index is empty → 200 with `{hits:[]}`,
        // proving the op-4 handler is wired (not the 501 stub).
        let app = verified_router_semantic(secret);
        let token = mint_jwt(
            secret,
            &serde_json::json!({"sub":"agent-a","company_id":"tenant-a","exp":9_999_999_999i64}),
        );
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/memory/search")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"postgres","k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn search_rejects_unknown_scope_key() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let secret = b"s3cr3t";
        let app = verified_router_semantic(secret);
        let token = mint_jwt(
            secret,
            &serde_json::json!({"sub":"agent-a","company_id":"tenant-a","exp":9_999_999_999i64}),
        );
        // A mis-typed scope key is an invalid_argument (400), never a silently
        // dropped filter that would widen the result set.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/memory/search")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"x","scope":{"entity.bogus":"y"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // -- ISI-6476: nginx-free Inspector UI on the memory-transport listener --

    fn ui_router(dir: &std::path::Path) -> Router {
        use crate::memstore::MemStore;
        let service = Arc::new(MemKeysService::new(Arc::new(MemStore::new())));
        let state = AppState::new(
            service,
            None, // open loopback dev mode: tenant self-asserted
            Arc::new(AdmissionControl::with_defaults()),
        )
        .with_ui_dir(Some(dir.to_path_buf()));
        router(state)
    }

    /// Scratch SPA: index.html with a marker, one hashed asset.
    fn write_ui_fixture(dir: &std::path::Path) {
        use std::fs;
        fs::create_dir_all(dir.join("assets")).expect("mkdir assets");
        fs::write(dir.join("index.html"), "<html>memkeys-ui-fixture</html>").expect("write index");
        fs::write(dir.join("assets").join("app-abc123.js"), "console.log(1)").expect("write asset");
    }

    fn ui_fixture_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("memkeys-ui-test-{tag}-{}", uuid::Uuid::new_v4()));
        write_ui_fixture(&dir);
        dir
    }

    #[tokio::test]
    async fn ui_root_serves_index_html_without_auth() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let dir = ui_fixture_dir("root");
        let app = ui_router(&dir);
        let resp = app
            .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get("cache-control")
            .is_some_and(|v| v == "no-cache"));
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("memkeys-ui-fixture"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ui_history_fallback_serves_index_for_deep_links() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let dir = ui_fixture_dir("spa");
        let app = ui_router(&dir);
        // A client-side route that is not a real file must serve index.html.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/facts/some-deep-link")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("memkeys-ui-fixture"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ui_assets_are_immutable_cached() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let dir = ui_fixture_dir("assets");
        let app = ui_router(&dir);
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/assets/app-abc123.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get("cache-control")
            .is_some_and(|v| { v.to_str().unwrap_or("").contains("immutable") }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ui_serving_does_not_bypass_memory_auth() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;
        let dir = ui_fixture_dir("auth");
        let app = ui_router(&dir);
        // The SPA itself is served freely, but the API behind it is not: a
        // /memory/facts call without the (dev-mode) tenant header must still
        // be rejected — bundling the UI opens no API surface.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/memory/facts?entity.issue=I1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
