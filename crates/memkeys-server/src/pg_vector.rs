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

use chrono::{DateTime, Utc};
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

/// Reconcile a pgvector embedding column to `dim` — the active embedder's width
/// (ISI-6671). The query path casts generically to `::vector`, so the ONLY dim
/// coupling is the declared column width (migration 0003/0004 = `vector(256)`)
/// and its HNSW index. A benchmark/deploy that wires a wider external embedder
/// (e.g. nomic-embed-text, 768) needs the column to match or every `upsert`
/// hard-fails on the dim check.
///
/// - Column already `vector(dim)`: no-op (the common path).
/// - Width differs AND the table is EMPTY: re-dimension in place — drop the
///   HNSW index, `ALTER … TYPE vector(dim)`, rebuild the index. Safe because the
///   vector tier is rebuildable derived state (§5.4.2); the reconciler re-embeds
///   from the SoT off the hot path.
/// - Width differs AND the table is NON-EMPTY: fail fast with a remediation
///   message — we will not silently drop populated vectors.
///
/// `table`/`index` are internal constants (`fact_vectors` / `digests`), never
/// user input, so formatting them and `dim` (validated `1..=MAX_EMBED_DIM`) into
/// DDL is injection-safe.
pub async fn ensure_vector_column_dim(
    pool: &PgPool,
    table: &str,
    index: &str,
    dim: usize,
) -> Result<(), VectorError> {
    // pgvector stores the declared dimension directly as the column's
    // `atttypmod` (no VARHDRSZ offset); -1 means an unbounded `vector`.
    let current: Option<i32> = sqlx::query_scalar(
        "SELECT a.atttypmod FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         WHERE c.relname = $1 AND a.attname = 'embedding' AND a.attnum > 0 AND NOT a.attisdropped",
    )
    .bind(table)
    .fetch_optional(pool)
    .await
    .map_err(|e| VectorError::Backend(e.to_string()))?;

    let current = match current {
        // Column absent (migrations own creation) ⇒ nothing to reconcile.
        None => return Ok(()),
        Some(m) => m,
    };
    if current == dim as i32 {
        return Ok(());
    }

    let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .map_err(|e| VectorError::Backend(e.to_string()))?;
    if count > 0 {
        return Err(VectorError::Backend(format!(
            "{table}.embedding is vector({current}) but the active embedder emits {dim}-dim \
             vectors and the table holds {count} rows; re-dimensioning would drop them. Truncate \
             {table} (the reconciler re-embeds from the SoT) or set MEMKEYS_EMBED_DIM to {current}."
        )));
    }

    tracing::warn!(
        target: "memkeys",
        "memkeys.vector.redimension: {table}.embedding vector({current}) -> vector({dim}) \
         (table empty; rebuilding HNSW index {index})"
    );
    // Drop the index first — it is tied to the old width — then widen the column
    // and rebuild. Done inside the caller's single-connection boot path.
    sqlx::query(&format!("DROP INDEX IF EXISTS {index}"))
        .execute(pool)
        .await
        .map_err(|e| VectorError::Backend(e.to_string()))?;
    sqlx::query(&format!(
        "ALTER TABLE {table} ALTER COLUMN embedding TYPE vector({dim})"
    ))
    .execute(pool)
    .await
    .map_err(|e| VectorError::Backend(e.to_string()))?;
    sqlx::query(&format!(
        "CREATE INDEX {index} ON {table} USING hnsw (embedding vector_cosine_ops)"
    ))
    .execute(pool)
    .await
    .map_err(|e| VectorError::Backend(e.to_string()))?;
    Ok(())
}

/// Render a vector as pgvector's text input form `[f0,f1,…]`. `f32::to_string`
/// round-trips exactly (shortest decimal that parses back), so the stored
/// vector equals the embedder's output.
/// pgvector text form of a vector (`[1,0.5,-0.25]`) — shared with the M6
/// digest-tier writer (llm_worker).
pub(crate) fn encode_vector(v: &[f32]) -> String {
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
               (fact_id, tenant_id, embedding, fact_type, ingested_at, entity_project, \
                entity_issue, entity_pr, entity_agent, entity_run, entity_thread, entity_service, \
                model_id) \
             VALUES ($1, $2, $3::vector, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (fact_id) DO UPDATE SET \
               tenant_id = EXCLUDED.tenant_id, embedding = EXCLUDED.embedding, \
               fact_type = EXCLUDED.fact_type, ingested_at = EXCLUDED.ingested_at, \
               entity_project = EXCLUDED.entity_project, \
               entity_issue = EXCLUDED.entity_issue, entity_pr = EXCLUDED.entity_pr, \
               entity_agent = EXCLUDED.entity_agent, entity_run = EXCLUDED.entity_run, \
               entity_thread = EXCLUDED.entity_thread, entity_service = EXCLUDED.entity_service, \
               model_id = EXCLUDED.model_id",
        )
        .bind(v.fact_id)
        .bind(&v.tenant_id)
        .bind(&embedding)
        .bind(&v.fact_type)
        .bind(v.ingested_at)
        .bind(&v.entity.project)
        .bind(&v.entity.issue)
        .bind(&v.entity.pr)
        .bind(&v.entity.agent)
        .bind(&v.entity.run)
        .bind(&v.entity.thread)
        .bind(&v.entity.service)
        .bind(&v.model_id)
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
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<ScoredId>, VectorError> {
        let qvec = encode_vector(query);
        let limit = k as i64;
        // §7.4 point-in-time bound: `ingested_at <= as_of` ("facts known by
        // then"). A NULL `as_of` is the live-head read — the `IS NULL` short-
        // circuit makes one SQL serve both, and the predicate applies on the
        // CandidateSet::All (unscoped) path too so a whole-tenant `as_of`
        // search is temporally bounded, not a silent newer-than-as_of read
        // (ISI-6172 / CR ISI-6168 B1).
        let rows: Vec<(Uuid, f64)> = match candidates {
            CandidateSet::All => {
                sqlx::query_as(
                    "SELECT fact_id, (1.0 - (embedding <=> $2::vector))::float8 AS score \
                     FROM fact_vectors WHERE tenant_id = $1 \
                       AND ($3::timestamptz IS NULL OR ingested_at <= $3) \
                     ORDER BY embedding <=> $2::vector LIMIT $4",
                )
                .bind(tenant)
                .bind(&qvec)
                .bind(as_of)
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
                       AND ($4::timestamptz IS NULL OR ingested_at <= $4) \
                     ORDER BY embedding <=> $2::vector LIMIT $5",
                )
                .bind(tenant)
                .bind(&qvec)
                .bind(ids)
                .bind(as_of)
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
