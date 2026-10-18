//! Error types for the write path (stages 1-4) and the keyed read.

use thiserror::Error;

/// Failure of an ingest attempt before or at commit.
#[derive(Debug, Error)]
pub enum IngestError {
    /// The envelope was rejected by validation (client error; nothing was stored).
    #[error("rejected: {reason}")]
    Rejected {
        /// Human-readable rejection reason (safe to return to the caller).
        reason: String,
    },
    /// A retried flush resolved to an existing fact id but the body differs (409).
    #[error("conflict: fact {existing_id} already exists with a different body")]
    Conflict {
        /// The id the replay collided with.
        existing_id: uuid::Uuid,
    },
    /// Storage failure (server error; nothing can be assumed about durability).
    #[error("store error: {0}")]
    Store(#[from] sqlx::Error),
}

/// Failure of a keyed read.
#[derive(Debug, Error)]
pub enum QueryError {
    /// The query was invalid (no entity key supplied, bad limit, ...).
    #[error("invalid query: {reason}")]
    Invalid { reason: String },
    /// The requested optional module is not enabled (§7.4: op-4 returns 501
    /// `feature_disabled` when `memory.semantic` is off, not a silent empty).
    #[error("feature disabled: {feature}")]
    FeatureDisabled { feature: &'static str },
    /// Embedding- or vector-index-backend failure (M5 semantic path).
    #[error("semantic backend error: {0}")]
    Semantic(String),
    /// Storage failure.
    #[error("store error: {0}")]
    Store(#[from] sqlx::Error),
}

impl From<QueryError> for IngestError {
    fn from(e: QueryError) -> Self {
        match e {
            QueryError::Store(s) => IngestError::Store(s),
            QueryError::Invalid { reason } => IngestError::Rejected { reason },
            // Read-only semantic errors never arise on the ingest path; map to
            // a rejection so the conversion stays total.
            QueryError::FeatureDisabled { feature } => IngestError::Rejected {
                reason: format!("feature disabled: {feature}"),
            },
            QueryError::Semantic(reason) => IngestError::Rejected { reason },
        }
    }
}
