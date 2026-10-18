//! M3 S1 per-tenant authz integration tests (ISI-6151): the HTTP transport
//! derives the tenant floor + producer principal from a verified bearer token
//! and NEVER from a client-supplied header — closing the §7.4/R6 deviation.
//!
//! DB-free: these drive the real axum router (`router` + `admit` middleware +
//! handlers) over an injected [`FactStore`] via `tower::ServiceExt::oneshot`,
//! so they run under a plain `cargo test` with no Postgres. Cross-tenant
//! *isolation* is proven against the real Postgres store in `integration.rs`
//! (`tenant_floor_isolates_reads`); here we prove the transport hands the store
//! the tenant it derived from the token, which is the part this PR owns. A
//! spy store captures exactly that floor.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tower::ServiceExt; // oneshot

use memkeys_server::admission::{AdmissionConfig, AdmissionControl};
use memkeys_server::auth::{JwtVerifier, Principal, StaticVerifier};
use memkeys_server::error::{IngestError, QueryError};
use memkeys_server::http::{router, AppState};
use memkeys_server::memstore::MemStore;
use memkeys_server::model::{FactRecord, KeyedQuery, ProcessedFact};
use memkeys_server::service::MemKeysService;
use memkeys_server::store::{CommitOutcome, FactStore};

type HmacSha256 = Hmac<Sha256>;

const SECRET: &[u8] = b"integration-secret";

/// Mint an HS256 JWT for `tenant` (the `company_id` claim) with `exp`.
fn mint(tenant: &str, exp: i64) -> String {
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header = b64(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = serde_json::json!({"sub":"agent-from-token","company_id":tenant,"run_id":"run-1","exp":exp});
    let payload = b64(serde_json::to_vec(&claims).unwrap().as_slice());
    let signing_input = format!("{header}.{payload}");
    let mut mac = HmacSha256::new_from_slice(SECRET).unwrap();
    mac.update(signing_input.as_bytes());
    let sig = b64(&mac.finalize().into_bytes());
    format!("{signing_input}.{sig}")
}

// ---------------------------------------------------------------------------
// Spy store: records the tenant floor + principal the transport hands down.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Spy {
    fetch_tenant: Mutex<Option<String>>,
    commit_tenant: Mutex<Option<String>>,
    commit_principal: Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl FactStore for Spy {
    async fn commit_fact(&self, fact: &ProcessedFact) -> Result<CommitOutcome, IngestError> {
        *self.commit_tenant.lock().unwrap() = Some(fact.tenant_id.clone());
        *self.commit_principal.lock().unwrap() = Some(fact.provenance_principal.clone());
        Ok(CommitOutcome::Inserted)
    }
    async fn get_fact(
        &self,
        _tenant_id: &str,
        _fact_id: uuid::Uuid,
    ) -> Result<Option<FactRecord>, QueryError> {
        Ok(None)
    }
    async fn fetch_by_keys(
        &self,
        tenant_id: &str,
        _query: &KeyedQuery,
    ) -> Result<Vec<FactRecord>, QueryError> {
        *self.fetch_tenant.lock().unwrap() = Some(tenant_id.to_string());
        Ok(vec![])
    }
}

fn jwt_app_with(store: Arc<Spy>) -> AppState {
    let service = Arc::new(MemKeysService::new(store));
    AppState::new(
        service,
        Arc::new(JwtVerifier::new(SECRET.to_vec())),
        Arc::new(AdmissionControl::with_defaults()),
    )
}

fn envelope_json(issue: &str, body: &str) -> serde_json::Value {
    serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-18T10:00:00Z" },
        "entity": { "issue": issue, "run": "r1" },
        "fact": { "body": body }
    })
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn post_fact(
    app: &AppState,
    bearer: Option<&str>,
    extra_tenant_header: Option<&str>,
    env: serde_json::Value,
) -> axum::response::Response {
    let mut b = Request::builder()
        .method("POST")
        .uri("/memory/facts")
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(t) = extra_tenant_header {
        b = b.header("x-memkeys-tenant", t);
    }
    router(app.clone())
        .oneshot(b.body(Body::from(env.to_string())).unwrap())
        .await
        .unwrap()
}

async fn get_fact(
    app: &AppState,
    bearer: Option<&str>,
    extra_tenant_header: Option<&str>,
    issue: &str,
) -> axum::response::Response {
    let mut b = Request::builder()
        .method("GET")
        .uri(format!("/memory/facts?entity.issue={issue}"));
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(t) = extra_tenant_header {
        b = b.header("x-memkeys-tenant", t);
    }
    router(app.clone())
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn missing_token_is_unauthenticated() {
    let app = jwt_app_with(Arc::new(Spy::default()));
    let resp = get_fact(&app, None, None, "I1").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // The structured WS-5 §7 body: stable code, not a bespoke string.
    assert_eq!(body_json(resp).await["code"], "unauthenticated");
}

#[tokio::test]
async fn expired_token_is_unauthenticated() {
    let app = jwt_app_with(Arc::new(Spy::default()));
    let token = mint("tenant-a", 1); // exp in the distant past
    let resp = get_fact(&app, Some(&token), None, "I1").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn read_floor_is_derived_from_token_claim() {
    let spy = Arc::new(Spy::default());
    let app = jwt_app_with(spy.clone());
    let resp = get_fact(&app, Some(&mint("tenant-a", 9_999_999_999)), None, "I1").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        spy.fetch_tenant.lock().unwrap().as_deref(),
        Some("tenant-a"),
        "the store is handed the token's tenant as the read floor"
    );
}

#[tokio::test]
async fn client_tenant_header_is_ignored_on_read_and_write() {
    // THE core guarantee: a valid tenant-a token plus a forged
    // `X-MemKeys-Tenant: tenant-b` header → the floor handed to the store is
    // tenant-a (the token), never tenant-b (the header).
    let spy = Arc::new(Spy::default());
    let app = jwt_app_with(spy.clone());
    let tok_a = mint("tenant-a", 9_999_999_999);

    // Write: the committed fact is stamped tenant-a + the token's principal.
    let resp = post_fact(
        &app,
        Some(&tok_a),
        Some("tenant-b"),
        envelope_json("I1", "lands under A"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        spy.commit_tenant.lock().unwrap().as_deref(),
        Some("tenant-a")
    );
    assert_eq!(
        spy.commit_principal.lock().unwrap().as_deref(),
        Some("agent-from-token"),
        "producer principal comes from the token's sub claim, not \"http\""
    );

    // Read: same — the header is ignored, the token's tenant is the floor.
    let resp = get_fact(&app, Some(&tok_a), Some("tenant-b"), "I1").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        spy.fetch_tenant.lock().unwrap().as_deref(),
        Some("tenant-a")
    );
}

#[tokio::test]
async fn oversized_body_is_rejected_before_the_pipeline() {
    // F14: a body over the admission cap is `invalid_envelope` (422) and never
    // reaches the store. StaticVerifier + a tiny cap.
    let spy = Arc::new(Spy::default());
    let service = Arc::new(MemKeysService::new(spy.clone()));
    let app = AppState::new(
        service,
        Arc::new(StaticVerifier::new(Principal {
            tenant_id: "tenant-a".into(),
            agent_id: "a".into(),
            run_id: None,
            expires_at: i64::MAX,
            trace_id: None,
        })),
        Arc::new(AdmissionControl::new(AdmissionConfig {
            max_body_bytes: 16,
            ..Default::default()
        })),
    );
    let big = "x".repeat(1000);
    let resp = post_fact(&app, Some("ignored"), None, envelope_json("I1", &big)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(resp).await["code"], "invalid_envelope");
    assert!(
        spy.commit_tenant.lock().unwrap().is_none(),
        "an over-cap write never reached the store"
    );
}

#[tokio::test]
async fn static_verifier_serves_without_a_token_under_a_fixed_tenant() {
    // The insecure dev fallback: a StaticVerifier binds one server-fixed tenant;
    // no bearer token is needed and a forged header does nothing — the floor is
    // still server-side, not a request header. Uses MemStore (full write path).
    let service = Arc::new(MemKeysService::new(Arc::new(MemStore::new())));
    let app = AppState::new(
        service,
        Arc::new(StaticVerifier::new(Principal {
            tenant_id: "dev".into(),
            agent_id: "dev".into(),
            run_id: None,
            expires_at: i64::MAX,
            trace_id: None,
        })),
        Arc::new(AdmissionControl::with_defaults()),
    );
    let resp = post_fact(
        &app,
        None,
        Some("attacker-tenant"),
        envelope_json("I1", "dev fact"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // Readable back on the fixed dev tenant; the forged header did nothing.
    let seen = get_fact(&app, None, None, "I1").await;
    assert_eq!(body_json(seen).await["facts"].as_array().unwrap().len(), 1);
}
