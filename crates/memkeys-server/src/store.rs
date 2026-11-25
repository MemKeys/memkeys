//! Storage port (§5.1/§7.2): the FactStore — the source-of-truth keyed store
//! behind stages 4 and the keyed read.

use crate::error::QueryError;
use crate::model::{FactRecord, KeyedQuery, ProcessedFact};

/// Outcome of a stage-4 commit.
#[derive(Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// New durable row written (the ack point, §6 stage 4).
    Inserted,
    /// The idempotency key resolved to an existing row with the SAME body —
    /// a retried flush; no duplicate was stored.
    Duplicate,
}

/// The FactStore port. Implementations must:
/// - be idempotent on `fact.id` (retried flush → same row, no duplicate),
/// - reject a same-id replay with a DIFFERENT body (409 conflict),
/// - enforce the tenant floor on every read (rows never cross tenants).
#[async_trait::async_trait]
pub trait FactStore: Send + Sync {
    /// Stage 4 — commit one processed fact (append-only; never mutates).
    async fn commit_fact(
        &self,
        fact: &ProcessedFact,
    ) -> Result<CommitOutcome, crate::error::IngestError>;

    /// Fetch one record by id within the tenant floor (used by the
    /// supersession-target validation of stage 3's store check).
    async fn get_fact(
        &self,
        tenant_id: &str,
        fact_id: uuid::Uuid,
    ) -> Result<Option<FactRecord>, QueryError>;

    /// Operation 1 — attribute-keyed fetch (§7.2).
    async fn fetch_by_keys(
        &self,
        tenant_id: &str,
        query: &KeyedQuery,
    ) -> Result<Vec<FactRecord>, QueryError>;

    /// Of `ids` (within the tenant floor), return the subset that are
    /// **superseded** as of `as_of` — i.e. some other fact `s` in the tenant
    /// has `s.supersedes = id` and (when `as_of` is `Some`) `s.ingested_at <=
    /// as_of`. `as_of = None` is the live-head read.
    ///
    /// This is the batch form of op 1's `Validity::Current` read-time collapse
    /// (the `NOT EXISTS (… s.supersedes = f.id …)` chain of §5.1), exposed so
    /// the semantic path (op 4) can honor supersession too: the vector tier
    /// keeps a superseded head's vector, so an ANN hit can resolve to a stale
    /// version unless the hydration step drops these ids (ISI-6810). Using the
    /// SoT `supersedes` chain keeps the collapse deterministic and as-of-aware
    /// (history preserved), rather than a fuzzy cosine near-dup threshold.
    async fn superseded_ids(
        &self,
        tenant_id: &str,
        ids: &[uuid::Uuid],
        as_of: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<std::collections::HashSet<uuid::Uuid>, QueryError>;

    /// Golden Path B (ISI-7813, parent ISI-7787) — the applicable-linter read.
    /// Returns the **live-head** linter facts (`fact_type = 'linter'`,
    /// ADR-0004) that apply to a tenant and a set of in-scope `projects`, in
    /// ONE round-trip, served by the partial `facts_linter_idx` (migration
    /// 0008). Two scopes come back in the same result:
    ///   * **global** linters — unkeyed (`entity_project IS NULL`, `keyed =
    ///     false`): the tenant floor, always applicable;
    ///   * **context** linters — keyed on `entity_project ∈ projects`.
    ///
    /// Supersession is collapsed on op-1's `Validity::Current` grain (newest
    /// per `subject = rule_id` wins) and honored as of `as_of` (`None` = live
    /// head). This read is deliberately OUTSIDE the op-4 semantic ranking path
    /// (`semantic_search`): linters are policy, not recall — the resolver that
    /// folds them into fetch/search responses as `applicable_linters` reads
    /// them deterministically here, so they never pollute recall hits.
    ///
    /// An empty `projects` slice yields global linters only. Order is
    /// unspecified here (newest-first per the index); the resolver
    /// (`MemKeysService::resolve_applicable_linters`) imposes the
    /// global-first-then-severity wire order.
    async fn list_linters(
        &self,
        tenant_id: &str,
        projects: &[String],
        as_of: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<FactRecord>, QueryError>;
}
