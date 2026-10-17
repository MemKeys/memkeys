//! The fact envelope (§3 of docs/architecture.md) — raw in, modeled server-side.
//!
//! The envelope carries *hints, not authority*: the server is the authority on
//! keys, time, and supersession. Parsing is deliberately tolerant of unknown
//! fields (they are hints), but the per-source required fields of §3.1 are
//! enforced in stage 1 (normalize).

use serde::{Deserialize, Serialize};

use crate::model::FactType;

/// How the fact reached the server (§3). Also selects the per-source rules of
/// the §3.1 matrix: required anchor, allowed hints, default fact type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    #[serde(rename = "gateway_flush")]
    GatewayFlush,
    #[serde(rename = "run_settle")]
    RunSettle,
    #[serde(rename = "a2a_exchange")]
    A2aExchange,
    #[serde(rename = "telemetry_learning")]
    TelemetryLearning,
}

impl SourceKind {
    /// Wire form (also the stored `provenance_source_kind` value).
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::GatewayFlush => "gateway_flush",
            SourceKind::RunSettle => "run_settle",
            SourceKind::A2aExchange => "a2a_exchange",
            SourceKind::TelemetryLearning => "telemetry_learning",
        }
    }

    /// Server default for an omitted `fact.type` (§3.1, default column).
    pub fn default_fact_type(self) -> FactType {
        match self {
            SourceKind::GatewayFlush => FactType::Context,
            SourceKind::RunSettle => FactType::Outcome,
            SourceKind::A2aExchange => FactType::Observation,
            SourceKind::TelemetryLearning => FactType::Learning,
        }
    }
}

/// `source` block. `kind` and `emitted_at` are REQUIRED (§3).
#[derive(Debug, Clone, Deserialize)]
pub struct SourceBlock {
    pub kind: SourceKind,
    /// Advisory only — the server never trusts it (verified against the caller
    /// principal instead).
    #[serde(default)]
    pub producer: Option<String>,
    /// REQUIRED, client clock, RFC3339. Default for `fact.event_time`.
    pub emitted_at: String,
    /// Optional caller-supplied idempotency key. Retried flushes that reuse it
    /// resolve to the same fact id (stage 1, normalize).
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// `entity` block — client-asserted key HINTS, validated/enriched server-side
/// (§3: "send AT LEAST this source's anchor"). The anchor itself is per-source
/// per the §3.1 matrix.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EntityHints {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub issue: Option<String>,
    #[serde(default)]
    pub pr: Option<String>,
    /// Advisory; always overwritten by the authenticated producer principal (§4).
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default)]
    pub service: Option<String>,
    /// A2A only; hint, corroborated, never written as a verified key.
    #[serde(default)]
    pub counterpart_agent: Option<String>,
}

/// `fact` block. `body` is REQUIRED — the one thing only the producer can supply.
#[derive(Debug, Clone, Deserialize)]
pub struct FactBlock {
    /// Optional; defaults per `source.kind` (§3.1).
    #[serde(rename = "type", default)]
    pub fact_type: Option<FactType>,
    /// Natural-language text OR structured payload.
    pub body: serde_json::Value,
    /// Supersession grain. REQUIRED when `supersedes` is set (stage 3 enforces).
    #[serde(default)]
    pub subject: Option<String>,
    /// Optional RFC3339; defaults to `source.emitted_at` in stage 3.
    #[serde(default)]
    pub event_time: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub supersedes: Option<uuid::Uuid>,
}

/// The full envelope as it arrives on the wire.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    pub source: SourceBlock,
    #[serde(default)]
    pub entity: EntityHints,
    pub fact: FactBlock,
    /// Optional refs (doc ids, blob uris) — never inlined.
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    /// §3: MUST be absent/empty — a populated block is discarded whole, never
    /// merged. Stage 1 records its presence as a rejected hint so the discard
    /// is observable, per the "no silent key-dropping" rule of §4.
    #[serde(default)]
    pub provenance: Option<serde_json::Value>,
}
