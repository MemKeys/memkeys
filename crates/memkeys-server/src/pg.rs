//! Postgres FactStore (sqlx) — the M1 source-of-truth backend.
//!
//! Schema: migrations/0001_factstore.sql (append-only, bi-temporal, keyed
//! dimensions as first-class columns, supersession resolved at read time).
//! Queries are runtime-built (no compile-time DB requirement); `db.query.text`
//! is never emitted (deny-list).

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::error::{IngestError, QueryError};
use crate::model::{FactRecord, KeyedQuery, ProcessedFact, Validity};
use crate::store::{CommitOutcome, FactStore};

/// A connected Postgres FactStore. Create with [`PgStore::connect`] (applies
/// pending migrations) or [`PgStore::from_pool`] when the caller owns setup.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect and apply pending migrations. `url` is a Postgres connection
    /// string (e.g. `postgres://user:pass@host/db`).
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool (already migrated by the caller).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

const INSERT_FACT_SQL: &str = "\
INSERT INTO facts (\
  id, tenant_id, fact_type, subject, body, body_hash, confidence, supersedes, \
  event_time, ingested_at, \
  entity_project, entity_issue, entity_pr, entity_agent, entity_run, \
  entity_thread, entity_service, counterpart_agent, \
  keyed, provenance_principal, provenance_source_kind, provenance_trace_id, \
  rejected_hints, attachments) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
  $16, $17, $18, $19, $20, $21, $22, $23, $24) \
ON CONFLICT (id) DO NOTHING \
RETURNING id";

#[async_trait::async_trait]
impl FactStore for PgStore {
    /// Stage 4 — the commit point. Idempotent on `fact.id`; a same-id replay
    /// with a different body is a conflict (409), per the FactStore contract.
    async fn commit_fact(&self, fact: &ProcessedFact) -> Result<CommitOutcome, IngestError> {
        let inserted = sqlx::query(INSERT_FACT_SQL)
            .bind(fact.id)
            .bind(&fact.tenant_id)
            .bind(fact.fact_type.as_str())
            .bind(&fact.subject)
            .bind(&fact.body)
            .bind(&fact.body_hash)
            .bind(fact.confidence)
            .bind(fact.supersedes)
            .bind(fact.event_time)
            .bind(fact.ingested_at)
            .bind(&fact.entity.project)
            .bind(&fact.entity.issue)
            .bind(&fact.entity.pr)
            .bind(&fact.entity.agent)
            .bind(&fact.entity.run)
            .bind(&fact.entity.thread)
            .bind(&fact.entity.service)
            .bind(&fact.entity.counterpart_agent)
            .bind(fact.keyed)
            .bind(&fact.provenance_principal)
            .bind(&fact.provenance_source_kind)
            .bind(&fact.provenance_trace_id)
            .bind(serde_json::json!(&fact.rejected_hints))
            .bind(&fact.attachments)
            .fetch_optional(&self.pool)
            .await?;

        if inserted.is_some() {
            return Ok(CommitOutcome::Inserted);
        }

        // Same fact id already exists: retried flush (same body → duplicate,
        // no error) or conflicting replay (different body → 409).
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT body_hash FROM facts WHERE id = $1 AND tenant_id = $2")
                .bind(fact.id)
                .bind(&fact.tenant_id)
                .fetch_optional(&self.pool)
                .await?;
        match existing {
            Some((hash,)) if hash == fact.body_hash => Ok(CommitOutcome::Duplicate),
            Some(_) => Err(IngestError::Conflict {
                existing_id: fact.id,
            }),
            // ON CONFLICT DO NOTHING cannot produce this (id is PK), but keep
            // the store total instead of unwrapping.
            None => Err(IngestError::Store(sqlx::Error::RowNotFound)),
        }
    }

    async fn get_fact(
        &self,
        tenant_id: &str,
        fact_id: uuid::Uuid,
    ) -> Result<Option<FactRecord>, QueryError> {
        let row = sqlx::query_as::<_, FactRecord>(&format!(
            "SELECT {} FROM facts WHERE tenant_id = $1 AND id = $2",
            FACT_COLUMNS
        ))
        .bind(tenant_id)
        .bind(fact_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Operation 1 (§7.2): ANDed entity keys, optional type filter, as-of
    /// replay on BOTH bi-temporal axes, read-time supersession collapse
    /// (`valid=current`, the default), newest first, bounded limit. Keyed rows
    /// only — unkeyed facts never pollute a keyed result set (§4).
    async fn fetch_by_keys(
        &self,
        tenant_id: &str,
        query: &KeyedQuery,
    ) -> Result<Vec<FactRecord>, QueryError> {
        let mut sql = format!(
            "SELECT {} FROM facts f WHERE f.tenant_id = $1 AND f.keyed",
            FACT_COLUMNS_ALIAS
        );
        // Typed bind values (Postgres checks types at the wire level).
        let mut binds: Vec<Bind> = vec![Bind::Text(tenant_id.to_string())];
        let mut next = 2usize;

        for (dim, value) in &query.keys {
            sql.push_str(&format!(" AND f.{} = ${}", dim.column(), next));
            binds.push(Bind::Text(value.clone()));
            next += 1;
        }
        if let Some(t) = &query.fact_type {
            sql.push_str(&format!(" AND f.fact_type = ${}", next));
            binds.push(Bind::Text(t.as_str().to_string()));
            next += 1;
        }
        if let Some(as_of) = query.as_of {
            // §5.3: event-time ≤ as_of AND we actually knew it by then.
            sql.push_str(&format!(
                " AND f.event_time <= ${n} AND f.ingested_at <= ${n}",
                n = next
            ));
            binds.push(Bind::Timestamptz(as_of));
            next += 1;
        }
        if query.valid == Validity::Current {
            // Read-time collapse (§5.1): drop records superseded on the
            // transaction axis (as-of aware).
            let mut sub = String::from(
                " AND NOT EXISTS (SELECT 1 FROM facts s \
                 WHERE s.supersedes = f.id AND s.tenant_id = f.tenant_id",
            );
            if let Some(as_of) = query.as_of {
                sub.push_str(&format!(" AND s.ingested_at <= ${}", next));
                binds.push(Bind::Timestamptz(as_of));
                next += 1;
            }
            sub.push(')');
            sql.push_str(&sub);
        }
        sql.push_str(&format!(
            " ORDER BY f.event_time DESC, f.id DESC LIMIT ${}",
            next
        ));
        binds.push(Bind::I64(query.limit));

        let mut q = sqlx::query_as::<_, FactRecord>(&sql);
        for b in &binds {
            q = match b {
                Bind::Text(s) => q.bind(s),
                Bind::Timestamptz(t) => q.bind(*t),
                Bind::I64(i) => q.bind(*i),
            };
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows)
    }
}

/// Typed dynamic-bind values for the query builder.
#[derive(Debug)]
enum Bind {
    Text(String),
    Timestamptz(chrono::DateTime<chrono::Utc>),
    I64(i64),
}

/// Column list for get_fact (unaliased).
const FACT_COLUMNS: &str = "id, fact_type, subject, body, confidence, supersedes, \
    event_time, ingested_at, entity_project, entity_issue, entity_pr, entity_agent, \
    entity_run, entity_thread, entity_service, keyed, provenance_principal";

/// Column list for fetch_by_keys (aliased to f).
const FACT_COLUMNS_ALIAS: &str = "f.id, f.fact_type, f.subject, f.body, f.confidence, \
    f.supersedes, f.event_time, f.ingested_at, f.entity_project, f.entity_issue, f.entity_pr, \
    f.entity_agent, f.entity_run, f.entity_thread, f.entity_service, f.keyed, \
    f.provenance_principal";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityKey, FactType};

    #[test]
    fn insert_sql_conflict_clause_is_idempotent() {
        assert!(INSERT_FACT_SQL.contains("ON CONFLICT (id) DO NOTHING"));
    }

    #[test]
    fn entity_key_columns_match_migration() {
        assert_eq!(EntityKey::Project.column(), "entity_project");
        assert_eq!(EntityKey::Issue.column(), "entity_issue");
        assert_eq!(EntityKey::Pr.column(), "entity_pr");
        assert_eq!(EntityKey::Agent.column(), "entity_agent");
        assert_eq!(EntityKey::Run.column(), "entity_run");
        assert_eq!(EntityKey::Thread.column(), "entity_thread");
        assert_eq!(EntityKey::Service.column(), "entity_service");
    }

    #[test]
    fn fact_type_round_trips_wire_names() {
        for (t, s) in [
            (FactType::Decision, "decision"),
            (FactType::Outcome, "outcome"),
            (FactType::Observation, "observation"),
            (FactType::Learning, "learning"),
            (FactType::Context, "context"),
        ] {
            assert_eq!(t.as_str(), s);
        }
    }
}
