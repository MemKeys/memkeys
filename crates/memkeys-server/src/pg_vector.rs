//! Postgres `VectorIndex` adapter (pgvector) — the fact-vector tier backend
//! (§5.4.2, §7.4; migration 0003). The deployment-specific seam behind the
//! backend-portable [`VectorIndex`] port: the service and the stage-5b worker
//! depend only on the trait, so this file is the ONLY place that knows about
//! pgvector.
//!
//! SQL is runtime-built like `pg.rs`/`pg_graph.rs` (no compile-time DB
//! requirement) and `db.query.text` is never emitted (telemetry deny-list).
//! The embedding crosses the wire as pgvector's text form `[f0,f1,…]` cast to
//! `vector` — sqlx needs no pgvector-specific type for that.
//!
//! **Multi-tenancy (§7.4 R6).** Every [`search`](VectorIndex::search) is a hard
//! `tenant_id = $1` equality before the ANN distance is computed — a tenant's
//! vectors are a physical partition, never a forgettable query predicate. The
//! [`CandidateSet`] narrows WITHIN that floor (a `fact_id = ANY(...)` filter),
//! never across it.

use sqlx::PgPool;
use uuid::Uuid;

use crate::semantic::{CandidateSet, FactVector, ScoredId, VectorError, VectorIndex};

/// A connected pgvector index over the same pool as the FactStore/graph.
#[derive(Debug, Clone)]
pub struct PgVectorIndex {
    pool: PgPool,
}

impl PgVectorIndex {
    /// Wrap an existing (already migrated) pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Render a vector as pgvector's text input form `[f0,f1,…]`. `f32::to_string`
/// round-trips exactly (shortest decimal that parses back), so the stored
/// vector equals the embedder's output.
fn encode_vector(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

#[async_trait::async_trait]
impl VectorIndex for PgVectorIndex {
    /// Stage 5b upsert — idempotent on `fact_id` (`ON CONFLICT DO UPDATE`): a
    /// replay re-embeds to the same vector and overwrites, never duplicates
    /// (§5.4.2). Keys + vector only — the body is never stored here (R1).
    async fn upsert(&self, v: &FactVector) -> Result<(), VectorError> {
        let embedding = encode_vector(&v.embedding);
        sqlx::query(
            "INSERT INTO fact_vectors \
               (fact_id, tenant_id, embedding, fact_type, entity_project, entity_issue, \
                entity_pr, entity_agent, entity_run, entity_thread, entity_service) \
             VALUES ($1, $2, $3::vector, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (fact_id) DO UPDATE SET \
               tenant_id = EXCLUDED.tenant_id, embedding = EXCLUDED.embedding, \
               fact_type = EXCLUDED.fact_type, entity_project = EXCLUDED.entity_project, \
               entity_issue = EXCLUDED.entity_issue, entity_pr = EXCLUDED.entity_pr, \
               entity_agent = EXCLUDED.entity_agent, entity_run = EXCLUDED.entity_run, \
               entity_thread = EXCLUDED.entity_thread, entity_service = EXCLUDED.entity_service",
        )
        .bind(v.fact_id)
        .bind(&v.tenant_id)
        .bind(&embedding)
        .bind(&v.fact_type)
        .bind(&v.entity.project)
        .bind(&v.entity.issue)
        .bind(&v.entity.pr)
        .bind(&v.entity.agent)
        .bind(&v.entity.run)
        .bind(&v.entity.thread)
        .bind(&v.entity.service)
        .execute(&self.pool)
        .await
        .map_err(|e| VectorError::Backend(e.to_string()))?;
        Ok(())
    }

    /// ANN top-`k` within the tenant floor, restricted to `candidates`. Ranked
    /// by cosine similarity `1 - (embedding <=> q)` (pgvector `<=>` is cosine
    /// DISTANCE; `1 - distance` is the similarity, matching [`crate::semantic::cosine`]).
    /// Score-descending == distance-ascending, so `ORDER BY <=>` gives the right
    /// order and lets the HNSW index serve the unfiltered path.
    async fn search(
        &self,
        tenant: &str,
        candidates: &CandidateSet,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<ScoredId>, VectorError> {
        let qvec = encode_vector(query);
        let limit = k as i64;
        let rows: Vec<(Uuid, f64)> = match candidates {
            CandidateSet::All => {
                sqlx::query_as(
                    "SELECT fact_id, (1.0 - (embedding <=> $2::vector))::float8 AS score \
                     FROM fact_vectors WHERE tenant_id = $1 \
                     ORDER BY embedding <=> $2::vector LIMIT $3",
                )
                .bind(tenant)
                .bind(&qvec)
                .bind(limit)
                .fetch_all(&self.pool)
                .await
            }
            CandidateSet::Only(ids) => {
                // Empty candidate set ⇒ `= ANY('{}')` ⇒ no rows (the scope
                // excluded everything) — same outcome as MemVectorIndex.
                sqlx::query_as(
                    "SELECT fact_id, (1.0 - (embedding <=> $2::vector))::float8 AS score \
                     FROM fact_vectors WHERE tenant_id = $1 AND fact_id = ANY($3) \
                     ORDER BY embedding <=> $2::vector LIMIT $4",
                )
                .bind(tenant)
                .bind(&qvec)
                .bind(ids)
                .bind(limit)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| VectorError::Backend(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(fact_id, score)| ScoredId {
                fact_id,
                score: score as f32,
            })
            .collect())
    }

    /// Retention sweep / disable (§5.4.2) — drop one fact's vector within the
    /// tenant floor.
    async fn delete(&self, tenant: &str, fact_id: Uuid) -> Result<(), VectorError> {
        sqlx::query("DELETE FROM fact_vectors WHERE tenant_id = $1 AND fact_id = $2")
            .bind(tenant)
            .bind(fact_id)
            .execute(&self.pool)
            .await
            .map_err(|e| VectorError::Backend(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_vector_is_pgvector_text_form() {
        assert_eq!(encode_vector(&[1.0, -0.5, 0.0]), "[1,-0.5,0]");
        assert_eq!(encode_vector(&[]), "[]");
    }
}
