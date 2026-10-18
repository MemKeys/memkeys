//! Domain model: the processed fact record (mirrors the FactStore schema) and
//! the keyed-read query (§7.2).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Canonical fact types (§5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum FactType {
    #[serde(rename = "decision")]
    Decision,
    #[serde(rename = "outcome")]
    Outcome,
    #[serde(rename = "observation")]
    Observation,
    #[serde(rename = "learning")]
    Learning,
    #[serde(rename = "context")]
    Context,
}

impl FactType {
    pub fn as_str(self) -> &'static str {
        match self {
            FactType::Decision => "decision",
            FactType::Outcome => "outcome",
            FactType::Observation => "observation",
            FactType::Learning => "learning",
            FactType::Context => "context",
        }
    }
}

/// The keyed entity dimensions (§5.1 "first-class keyed dimensions"). The wire
/// name is the `entity.*` key of the envelope; the column is the Postgres
/// projection in migrations/0001_factstore.sql.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKey {
    Project,
    Issue,
    Pr,
    Agent,
    Run,
    Thread,
    Service,
}

impl EntityKey {
    /// `entity.*` wire name (§7.2 query parameter form).
    pub fn wire_name(self) -> &'static str {
        match self {
            EntityKey::Project => "entity.project",
            EntityKey::Issue => "entity.issue",
            EntityKey::Pr => "entity.pr",
            EntityKey::Agent => "entity.agent",
            EntityKey::Run => "entity.run",
            EntityKey::Thread => "entity.thread",
            EntityKey::Service => "entity.service",
        }
    }

    /// FactStore column (migrations/0001_factstore.sql).
    pub fn column(self) -> &'static str {
        match self {
            EntityKey::Project => "entity_project",
            EntityKey::Issue => "entity_issue",
            EntityKey::Pr => "entity_pr",
            EntityKey::Agent => "entity_agent",
            EntityKey::Run => "entity_run",
            EntityKey::Thread => "entity_thread",
            EntityKey::Service => "entity_service",
        }
    }
}

/// Server-resolved entity keys for one processed fact (§4). `None` = not
/// resolved for this fact. Values are opaque ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityKeys {
    pub project: Option<String>,
    pub issue: Option<String>,
    pub pr: Option<String>,
    pub agent: Option<String>,
    pub run: Option<String>,
    pub thread: Option<String>,
    pub service: Option<String>,
    /// A2A counterpart — hint-only, stored separately, never a verified key.
    pub counterpart_agent: Option<String>,
}

impl EntityKeys {
    /// True when at least one durable entity key resolved (§4). The producer
    /// agent alone does not make a fact keyed — the anchor-able keys do
    /// (project/issue/pr/run/thread/service); otherwise every fact would be
    /// keyed and the unkeyed tier of §4 could not exist.
    pub fn any_anchor_key(&self) -> bool {
        [
            &self.project,
            &self.issue,
            &self.pr,
            &self.run,
            &self.thread,
            &self.service,
        ]
        .into_iter()
        .any(|k| k.is_some())
    }

    /// Key-value pairs carried in the ingest response (§7.1:
    /// `entity_keys_resolved`), for caller-side caching.
    pub fn resolved(&self) -> Vec<(&'static str, &str)> {
        [
            (EntityKey::Project.wire_name(), self.project.as_deref()),
            (EntityKey::Issue.wire_name(), self.issue.as_deref()),
            (EntityKey::Pr.wire_name(), self.pr.as_deref()),
            (EntityKey::Agent.wire_name(), self.agent.as_deref()),
            (EntityKey::Run.wire_name(), self.run.as_deref()),
            (EntityKey::Thread.wire_name(), self.thread.as_deref()),
            (EntityKey::Service.wire_name(), self.service.as_deref()),
        ]
        .into_iter()
        .filter_map(|(name, v)| v.map(|v| (name, v)))
        .collect()
    }
}

/// The processed, committed fact record — one immutable row (§5.1). Written
/// once at stage 4; never mutated (append-only is a feature).
#[derive(Debug, Clone)]
pub struct ProcessedFact {
    pub id: uuid::Uuid,
    pub tenant_id: String,
    pub fact_type: FactType,
    pub subject: Option<String>,
    pub body: String,
    pub body_hash: String,
    pub confidence: Option<f64>,
    pub supersedes: Option<uuid::Uuid>,
    /// Bi-temporal event axis (the "valid" axis).
    pub event_time: DateTime<Utc>,
    /// Bi-temporal transaction axis.
    pub ingested_at: DateTime<Utc>,
    pub entity: EntityKeys,
    pub keyed: bool,
    pub provenance_principal: String,
    pub provenance_source_kind: String,
    pub provenance_trace_id: Option<String>,
    /// Hint keys that failed validation — recorded, never silently dropped (§4).
    pub rejected_hints: Vec<String>,
    pub attachments: serde_json::Value,
}

/// Read shape returned by op 1 (fetch-by-keys) — the authoritative record
/// hydrated from the keyed store (§7.5 response-shape invariant).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FactRecord {
    pub id: uuid::Uuid,
    pub fact_type: String,
    pub subject: Option<String>,
    pub body: String,
    pub confidence: Option<f64>,
    pub supersedes: Option<uuid::Uuid>,
    pub event_time: DateTime<Utc>,
    pub ingested_at: DateTime<Utc>,
    pub entity_project: Option<String>,
    pub entity_issue: Option<String>,
    pub entity_pr: Option<String>,
    pub entity_agent: Option<String>,
    pub entity_run: Option<String>,
    pub entity_thread: Option<String>,
    pub entity_service: Option<String>,
    pub keyed: bool,
    pub provenance_principal: String,
}

impl FactRecord {
    /// Server-resolved entity keys of this record (same shape as the ingest
    /// response, §7.1/§7.5).
    pub fn entity_keys(&self) -> EntityKeys {
        EntityKeys {
            project: self.entity_project.clone(),
            issue: self.entity_issue.clone(),
            pr: self.entity_pr.clone(),
            agent: self.entity_agent.clone(),
            run: self.entity_run.clone(),
            thread: self.entity_thread.clone(),
            service: self.entity_service.clone(),
            counterpart_agent: None,
        }
    }
}

/// Supersession collapse mode for op 1 (§7.2 `valid` parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Validity {
    /// Default — drop records superseded as of the query's transaction axis.
    #[default]
    Current,
    /// Full history — no collapse.
    All,
}

/// Operation-1 query: attribute-keyed memory lookup (§7.2).
///
/// At least one entity key is required; multiple keys are ANDed. The tenant is
/// always the authenticated caller's (never a query parameter).
#[derive(Debug, Clone)]
pub struct KeyedQuery {
    pub keys: Vec<(EntityKey, String)>,
    pub fact_type: Option<FactType>,
    /// Point-in-time replay (§5.3): both bi-temporal axes ≤ as_of.
    pub as_of: Option<DateTime<Utc>>,
    pub valid: Validity,
    pub limit: i64,
}

impl KeyedQuery {
    pub const DEFAULT_LIMIT: i64 = 100;
    pub const MAX_LIMIT: i64 = 500;
}
