//! Write pipeline stages 1-4 (§6 of docs/architecture.md).
//!
//! Stages 1-3 are pure (backend-agnostic); stage 4 is the only synchronous
//! backend write. The `ack` is gated on stage 4 only — nothing here depends on
//! derived views (those are M+ work, off the hot path).
//!
//! Stage 1 — normalize: validate the envelope, canonicalize the fact type,
//!   stamp `ingested_at`, assign the stable idempotent `fact.id`.
//! Stage 2 — key: resolve entity keys (§4: verify → enrich → accept-low-trust),
//!   mark `keyed`, attach rejected hints.
//! Stage 3 — temporalize: fix the event-time axis; require `subject` when
//!   superseding (the supersession grain).
//! Stage 4 — commit: durable FactStore write (see store.rs / pg.rs).

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::envelope::{Envelope, SourceKind};
use crate::error::IngestError;
use crate::model::{EntityKeys, ProcessedFact};

/// What the server knows about the verified caller — the trust root for keys
/// (§4 rule 1: principal and agent binding are verified, client claims are
/// overwritten on conflict). The transport stamps this from authenticated
/// headers, never from the body.
#[derive(Debug, Clone)]
pub struct CallerContext {
    /// WS-4 Invariant T isolation floor; opaque id.
    pub tenant_id: String,
    /// The authenticated producer agent = `entity.agent` (§3.1).
    pub principal: String,
    /// Custody trace context stamped onto the record.
    pub trace_id: Option<String>,
}

/// Context-binding of a run (§4 rule 2: enrichment source). M1 ships the port;
/// a real run registry is a later milestone. Enrichment is corroborated, never
/// guessed — an unresolvable anchor yields no keys, not invented ones.
#[derive(Debug, Clone, Default)]
pub struct RunBinding {
    pub project: Option<String>,
    pub issue: Option<String>,
    pub pr: Option<String>,
    pub agent: Option<String>,
}

/// Context-binding of an A2A thread (§4 rule 2).
#[derive(Debug, Clone, Default)]
pub struct ThreadBinding {
    pub project: Option<String>,
    pub issue: Option<String>,
    pub pr: Option<String>,
    /// Participant set used to corroborate a `counterpart_agent` hint.
    pub participants: Vec<String>,
}

/// Enrichment pivot of stage 2: resolves run/thread/service anchors into the
/// entity keys the caller did not send. `NoopResolver` is the M1 default
/// (no registries exist yet): it enriches nothing, so only client-asserted
/// keys land on the record.
#[async_trait::async_trait]
pub trait EntityResolver: Send + Sync {
    async fn resolve_run(&self, tenant_id: &str, run: &str) -> Option<RunBinding>;
    async fn resolve_thread(&self, tenant_id: &str, thread: &str) -> Option<ThreadBinding>;
    async fn resolve_service(&self, tenant_id: &str, service: &str) -> Option<String>;
}

/// The M1 default resolver: no registries configured, nothing enriches.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopResolver;

#[async_trait::async_trait]
impl EntityResolver for NoopResolver {
    async fn resolve_run(&self, _tenant_id: &str, _run: &str) -> Option<RunBinding> {
        None
    }
    async fn resolve_thread(&self, _tenant_id: &str, _thread: &str) -> Option<ThreadBinding> {
        None
    }
    async fn resolve_service(&self, _tenant_id: &str, _service: &str) -> Option<String> {
        None
    }
}

fn reject(reason: impl Into<String>) -> IngestError {
    IngestError::Rejected {
        reason: reason.into(),
    }
}

/// Normalize a hinted entity value: trim; empty/whitespace is a rejected hint
/// (recorded, never silently dropped, §4), not a stored key.
fn hint(v: Option<String>, name: &str, rejected: &mut Vec<String>) -> Option<String> {
    let was_some = v.is_some();
    match v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(s) => Some(s),
        None => {
            if was_some {
                rejected.push(name.to_string());
            }
            None
        }
    }
}

/// Stage 1 — normalize. Pure: no I/O, no clock reads (`now` is injected so the
/// transaction axis is testable).
///
/// `fact.id` is principal-salted and stable (§6: "idempotency key so retried
/// flushes don't duplicate"): UUIDv5 over (tenant_id, principal, idempotency
/// input). The idempotency input is the caller's `idempotency_key` when
/// supplied, else a content digest of (body, event_time, anchor) so an
/// identical retry of the same envelope resolves to the same id.
pub fn normalize(
    env: &Envelope,
    caller: &CallerContext,
    now: DateTime<Utc>,
) -> Result<ProcessedFact, IngestError> {
    let kind = env.source.kind;

    // --- required fields (§3.1 matrix: kind, emitted_at, anchor, fact.body) ---
    let emitted_at = chrono::DateTime::parse_from_rfc3339(&env.source.emitted_at)
        .map_err(|_| reject("source.emitted_at is not a valid RFC3339 timestamp"))?
        .with_timezone(&Utc);

    let body_text = body_to_text(&env.fact.body)
        .ok_or_else(|| reject("fact.body is required and must be text or a JSON value"))?;
    if body_text.trim().is_empty() {
        return Err(reject("fact.body must not be empty"));
    }

    if let Some(t) = env.fact.fact_type {
        // Decision facts over A2A require a subject (§3.1).
        if kind == SourceKind::A2aExchange
            && t == crate::model::FactType::Decision
            && hint(env.fact.subject.clone(), "fact.subject", &mut Vec::new()).is_none()
        {
            return Err(reject(
                "fact.subject is required for a2a_exchange decisions",
            ));
        }
    }

    if let Some(c) = env.fact.confidence {
        if !(0.0..=1.0).contains(&c) {
            return Err(reject("fact.confidence must be within [0.0, 1.0]"));
        }
    }

    // --- canonicalize (server defaults; §3.1) ---
    let fact_type = env
        .fact
        .fact_type
        .unwrap_or_else(|| kind.default_fact_type());
    let event_time = match env.fact.event_time.as_deref() {
        None => emitted_at,
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map_err(|_| reject("fact.event_time is not a valid RFC3339 timestamp"))?
            .with_timezone(&Utc),
    };

    let mut rejected_hints = Vec::new();
    if env.provenance.as_ref().is_some_and(|p| !p.is_null()) {
        rejected_hints.push("provenance".to_string());
    }

    // --- fact.id: stable, principal-salted idempotency key (§6 stage 1) ---
    let anchor = first_anchor(env);
    let idem_input = match env
        .source
        .idempotency_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(k) => k.to_string(),
        None => {
            let mut h = Sha256::new();
            h.update(&body_text);
            h.update(event_time.to_rfc3339());
            if let Some(a) = &anchor {
                h.update(a.as_bytes());
            }
            format!("body:{}", hex(&h.finalize()))
        }
    };
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("{}|{}|{}", caller.tenant_id, caller.principal, idem_input).as_bytes(),
    );

    Ok(ProcessedFact {
        id,
        tenant_id: caller.tenant_id.clone(),
        fact_type,
        subject: env
            .fact
            .subject
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        body: body_text.clone(),
        body_hash: {
            let mut h = Sha256::new();
            h.update(&body_text);
            hex(&h.finalize())
        },
        confidence: env.fact.confidence,
        supersedes: env.fact.supersedes,
        event_time,
        ingested_at: now,
        entity: EntityKeys::default(),
        keyed: false,
        provenance_principal: caller.principal.clone(),
        provenance_source_kind: kind.as_str().to_string(),
        provenance_trace_id: caller.trace_id.clone(),
        rejected_hints,
        attachments: serde_json::to_value(&env.attachments)
            .unwrap_or(serde_json::Value::Array(vec![])),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn body_to_text(body: &serde_json::Value) -> Option<String> {
    match body {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The client-asserted anchor hint, per source (§3.1 "Required" column).
fn first_anchor(env: &Envelope) -> Option<String> {
    let e = &env.entity;
    match env.source.kind {
        SourceKind::GatewayFlush | SourceKind::RunSettle => e.run.clone(),
        SourceKind::A2aExchange => e.thread.clone(),
        SourceKind::TelemetryLearning => e.service.clone().or_else(|| e.project.clone()),
    }
}

/// Stage 2 — key (§4: verify → enrich → accept-low-trust). Resolves the entity
/// keys and marks `keyed`. Async because enrichment pivots (run registry,
/// thread registry, service→project map) are lookups.
pub async fn key_stage(
    env: &Envelope,
    caller: &CallerContext,
    fact: &mut ProcessedFact,
    resolver: &dyn EntityResolver,
) -> Result<(), IngestError> {
    let kind = env.source.kind;
    let mut rejected = std::mem::take(&mut fact.rejected_hints);
    let mut k = EntityKeys::default();

    // 1. Verified from the caller (§4 rule 1): the agent binding is the
    //    authenticated principal; a disagreeing client claim is overwritten
    //    and recorded as a rejected hint.
    if let Some(claimed) = hint(env.entity.agent.clone(), "entity.agent", &mut rejected) {
        if claimed != caller.principal {
            rejected.push("entity.agent".to_string());
        }
    }
    k.agent = Some(caller.principal.clone());

    // 2. Accept low-trust, non-custody hints as asserted (§4 rule 3),
    //    validating shape (blank strings become rejected hints).
    k.project = hint(env.entity.project.clone(), "entity.project", &mut rejected);
    k.issue = hint(env.entity.issue.clone(), "entity.issue", &mut rejected);
    k.pr = hint(env.entity.pr.clone(), "entity.pr", &mut rejected);
    k.run = hint(env.entity.run.clone(), "entity.run", &mut rejected);
    k.thread = hint(env.entity.thread.clone(), "entity.thread", &mut rejected);
    k.service = hint(env.entity.service.clone(), "entity.service", &mut rejected);
    k.counterpart_agent = hint(
        env.entity.counterpart_agent.clone(),
        "entity.counterpart_agent",
        &mut rejected,
    );

    // 3. Enrich from the anchor (§4 rule 2, corroborated — never guessed).
    match kind {
        SourceKind::GatewayFlush | SourceKind::RunSettle => {
            if let Some(run) = &k.run {
                if let Some(b) = resolver.resolve_run(&caller.tenant_id, run).await {
                    k.project = k.project.or(b.project);
                    k.issue = k.issue.or(b.issue);
                    k.pr = k.pr.or(b.pr);
                    if let Some(agent) = b.agent {
                        k.agent = Some(agent);
                    }
                }
            }
        }
        SourceKind::A2aExchange => {
            if let Some(thread) = &k.thread {
                if let Some(b) = resolver.resolve_thread(&caller.tenant_id, thread).await {
                    k.project = k.project.or(b.project);
                    k.issue = k.issue.or(b.issue);
                    k.pr = k.pr.or(b.pr);
                    // Counterpart is corroborated against thread membership (§3.1);
                    // an uncorroborated hint is kept as the stored hint-only
                    // value and flagged, never promoted to a verified key.
                    if let Some(cp) = &k.counterpart_agent {
                        if !b.participants.iter().any(|p| p == cp) {
                            rejected.push("entity.counterpart_agent".to_string());
                        }
                    }
                }
            }
        }
        SourceKind::TelemetryLearning => {
            if let Some(service) = &k.service {
                if let Some(project) = resolver.resolve_service(&caller.tenant_id, service).await {
                    k.project = k.project.or(Some(project));
                }
            }
        }
    }

    // Anchor requirement (§3.1). A fact with no resolvable anchor is stored
    // `unkeyed` — tolerated only for telemetry_learning.
    if !k.any_anchor_key() && kind != SourceKind::TelemetryLearning {
        return Err(reject(match kind {
            SourceKind::GatewayFlush | SourceKind::RunSettle => "entity.run anchor is required",
            SourceKind::A2aExchange => "entity.thread anchor is required",
            SourceKind::TelemetryLearning => "entity.service or entity.project anchor is required",
        }));
    }

    fact.keyed = k.any_anchor_key();
    fact.entity = k;
    fact.rejected_hints = rejected;
    Ok(())
}

/// Stage 3 — temporalize. Pure: fixes the event-time axis (already defaulted in
/// normalize) and enforces the supersession grain: `subject` is required
/// whenever `supersedes` is set (§3: "you cannot version a statement without
/// naming it"). The (entity, subject, type) match against the target record is
/// a store read and is enforced by the service before commit.
pub fn temporalize(fact: &mut ProcessedFact) -> Result<(), IngestError> {
    if fact.supersedes.is_some()
        && fact
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
    {
        return Err(reject(
            "fact.subject is required when fact.supersedes is set",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller() -> CallerContext {
        CallerContext {
            tenant_id: "t1".into(),
            principal: "agent-a".into(),
            trace_id: Some("trace-1".into()),
        }
    }

    fn envelope(kind: SourceKind) -> Envelope {
        serde_json::from_value(serde_json::json!({
            "source": { "kind": kind.as_str(), "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": { "run": "r1" },
            "fact": { "body": "chose postgres for the fact store" }
        }))
        .unwrap()
    }

    #[test]
    fn normalize_defaults_type_per_source_and_event_time_to_emitted_at() {
        let c = caller();
        let f = normalize(&envelope(SourceKind::GatewayFlush), &c, Utc::now()).unwrap();
        assert_eq!(f.fact_type.as_str(), "context");
        assert_eq!(
            f.event_time.to_rfc3339(),
            "2026-10-17T10:00:00.000000+00:00"
                .parse::<DateTime<Utc>>()
                .unwrap()
                .to_rfc3339()
        );
        assert_eq!(f.provenance_source_kind, "gateway_flush");
        assert_eq!(f.provenance_principal, "agent-a");
    }

    #[test]
    fn normalize_is_idempotent_on_retries() {
        let c = caller();
        let e = envelope(SourceKind::GatewayFlush);
        let now = Utc::now();
        let a = normalize(&e, &c, now).unwrap();
        let b = normalize(&e, &c, now).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(a.body_hash, b.body_hash);
    }

    #[test]
    fn normalize_rejects_bad_timestamp_and_empty_body() {
        let c = caller();
        let mut e = envelope(SourceKind::GatewayFlush);
        e.source.emitted_at = "not-a-time".into();
        assert!(matches!(
            normalize(&e, &c, Utc::now()),
            Err(IngestError::Rejected { .. })
        ));

        let mut e = envelope(SourceKind::GatewayFlush);
        e.fact.body = serde_json::json!("");
        assert!(matches!(
            normalize(&e, &c, Utc::now()),
            Err(IngestError::Rejected { .. })
        ));
    }

    #[test]
    fn normalize_records_populated_provenance_as_rejected_hint() {
        let c = caller();
        let raw = serde_json::json!({
            "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": { "run": "r1" },
            "fact": { "body": "b" },
            "provenance": { "principal": "spoofed" }
        });
        let e: Envelope = serde_json::from_value(raw).unwrap();
        let f = normalize(&e, &c, Utc::now()).unwrap();
        assert!(f.rejected_hints.iter().any(|h| h == "provenance"));
        assert_eq!(f.provenance_principal, "agent-a");
    }

    #[tokio::test]
    async fn key_stage_overwrites_client_agent_claim() {
        let c = caller();
        let raw = serde_json::json!({
            "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": { "run": "r1", "agent": "someone-else" },
            "fact": { "body": "b" }
        });
        let e: Envelope = serde_json::from_value(raw).unwrap();
        let mut f = normalize(&e, &c, Utc::now()).unwrap();
        key_stage(&e, &c, &mut f, &NoopResolver).await.unwrap();
        assert_eq!(f.entity.agent.as_deref(), Some("agent-a"));
        assert!(f.rejected_hints.iter().any(|h| h == "entity.agent"));
        assert!(f.keyed);
    }

    #[tokio::test]
    async fn key_stage_requires_anchor_except_telemetry_learning() {
        let c = caller();
        let raw = serde_json::json!({
            "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": {},
            "fact": { "body": "b" }
        });
        let e: Envelope = serde_json::from_value(raw).unwrap();
        let mut f = normalize(&e, &c, Utc::now()).unwrap();
        assert!(matches!(
            key_stage(&e, &c, &mut f, &NoopResolver).await,
            Err(IngestError::Rejected { .. })
        ));

        let raw = serde_json::json!({
            "source": { "kind": "telemetry_learning", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": {},
            "fact": { "body": "learned something with no key" }
        });
        let e: Envelope = serde_json::from_value(raw).unwrap();
        let mut f = normalize(&e, &c, Utc::now()).unwrap();
        key_stage(&e, &c, &mut f, &NoopResolver).await.unwrap();
        assert!(!f.keyed);
    }

    #[tokio::test]
    async fn key_stage_enriches_run_from_stub_registry() {
        struct Stub;
        #[async_trait::async_trait]
        impl EntityResolver for Stub {
            async fn resolve_run(&self, _t: &str, run: &str) -> Option<RunBinding> {
                (run == "r1").then(|| RunBinding {
                    project: Some("p1".into()),
                    issue: Some("i1".into()),
                    pr: None,
                    agent: None,
                })
            }
            async fn resolve_thread(&self, _t: &str, _th: &str) -> Option<ThreadBinding> {
                None
            }
            async fn resolve_service(&self, _t: &str, _s: &str) -> Option<String> {
                None
            }
        }
        let c = caller();
        let e = envelope(SourceKind::RunSettle);
        let mut f = normalize(&e, &c, Utc::now()).unwrap();
        key_stage(&e, &c, &mut f, &Stub).await.unwrap();
        assert_eq!(f.entity.project.as_deref(), Some("p1"));
        assert_eq!(f.entity.issue.as_deref(), Some("i1"));
        assert!(f.keyed);
    }

    #[test]
    fn temporalize_requires_subject_when_superseding() {
        let c = caller();
        let raw = serde_json::json!({
            "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
            "entity": { "run": "r1" },
            "fact": { "body": "b", "supersedes": "00000000-0000-0000-0000-000000000001" }
        });
        let e: Envelope = serde_json::from_value(raw).unwrap();
        let mut f = normalize(&e, &c, Utc::now()).unwrap();
        assert!(matches!(
            temporalize(&mut f),
            Err(IngestError::Rejected { .. })
        ));
        f.subject = Some("the chosen database".into());
        temporalize(&mut f).unwrap();
    }
}
