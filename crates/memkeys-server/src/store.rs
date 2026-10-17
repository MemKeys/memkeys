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
}
