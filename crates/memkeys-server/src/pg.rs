//! Postgres FactStore (sqlx) — the M1 source-of-truth backend.
//!
//! Schema: migrations/0001_factstore.sql (append-only, bi-temporal, keyed
//! dimensions as first-class columns, supersession resolved at read time).
//! Queries are runtime-built (no compile-time DB requirement); `db.query.text`
//! is never emitted (deny-list).

use sha2::{Digest, Sha256};
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
    ///
    /// Only Postgres is supported. sqlx parses any URL as Postgres, so a
    /// non-Postgres scheme (e.g. `sqlite://`) would otherwise hang ~30s on pool
    /// acquisition and fail with a misleading `PoolTimedOut` (ISI-6318). We
    /// reject the scheme up front with a clear configuration error instead.
    pub async fn connect(url: &str) -> Result<Self, sqlx::Error> {
        validate_postgres_scheme(url)?;
        // Pool sizing is role-aware (see `pool_options_from_env`): MCP-only
        // per-session processes (MEMKEYS_ROLE=mcp) default to a tiny pool and
        // shed idle connections, so a fleet of them cannot saturate Postgres
        // (ISI-6549 RCA / ISI-6552). An explicit MEMKEYS_PG_MAX_CONNECTIONS
        // always wins, for every role.
        let pool = pool_options_from_env().connect(url).await?;
        // Preflight pgvector BEFORE any migration runs: migration 0003 does
        // `CREATE EXTENSION vector`, which on a stock Postgres image aborts
        // mid-migration with a raw sqlx error and can leave `_sqlx_migrations`
        // half-applied, so a naive retry hits a dirty state (ISI-6319). Failing
        // up front turns that into a clear remediation message and keeps the
        // migration ledger clean for a retry once pgvector is installed.
        let available: Option<(bool,)> =
            sqlx::query_as("SELECT true FROM pg_available_extensions WHERE name = 'vector'")
                .fetch_optional(&pool)
                .await?;
        ensure_pgvector_available(available.is_some())?;
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

/// Fail fast when `MEMKEYS_DATABASE_URL` is not a Postgres URL. Accepts the two
/// schemes sqlx honors for Postgres (`postgres://`, `postgresql://`), matched
/// case-insensitively per RFC 3986.
fn validate_postgres_scheme(url: &str) -> Result<(), sqlx::Error> {
    let scheme = url.split("://").next().unwrap_or("");
    if scheme.eq_ignore_ascii_case("postgres") || scheme.eq_ignore_ascii_case("postgresql") {
        return Ok(());
    }
    Err(sqlx::Error::Configuration(
        format!(
            "unsupported MEMKEYS_DATABASE_URL scheme {scheme:?}: only Postgres is supported \
             (expected postgres://… or postgresql://…)"
        )
        .into(),
    ))
}

/// Role-aware Postgres pool sizing (ISI-6552 defense-in-depth).
///
/// `MEMKEYS_ROLE=mcp` runs ONE sequential stdio session per process, so a fleet
/// of per-agent sessions each holding the historical default (8) connections
/// multiplies straight into Postgres `max_connections` — the ISI-6549
/// saturation that PoolTimedOut a rolling replacement. For that role we default
/// the pool to **2** and give it a short **idle timeout** so an abandoned-but-
/// still-open session reaps its connections instead of pinning them. Every
/// other role keeps the historical default 8 and no idle reaping. An explicit,
/// positive `MEMKEYS_PG_MAX_CONNECTIONS` always wins, for every role.
#[derive(Debug, PartialEq, Eq)]
struct PoolConfig {
    max_connections: u32,
    idle_timeout: Option<std::time::Duration>,
}

/// Pure resolver (unit-testable without env or a DB). `role` is the raw
/// `MEMKEYS_ROLE` value; `max_conn_override` is the raw
/// `MEMKEYS_PG_MAX_CONNECTIONS` value. Matches `ServerRole::parse`'s
/// case-insensitive `"mcp"` test (main.rs) so the two never drift.
fn resolve_pool_config(role: Option<&str>, max_conn_override: Option<&str>) -> PoolConfig {
    let is_mcp = role.is_some_and(|v| v.eq_ignore_ascii_case("mcp"));
    let explicit = max_conn_override
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n > 0);
    let max_connections = explicit.unwrap_or(if is_mcp { 2 } else { 8 });
    // 90s sits in the task's 60–120s window: long enough that a mid-task agent
    // pause does not thrash the pool, short enough that an abandoned session
    // frees its connections well before the next rolling replacement.
    let idle_timeout = is_mcp.then(|| std::time::Duration::from_secs(90));
    PoolConfig {
        max_connections,
        idle_timeout,
    }
}

/// Build [`PgPoolOptions`] from the process environment via
/// [`resolve_pool_config`]. Shared by every real connect path (FactStore and
/// the graph projection) so pool sizing lives in exactly one place.
pub(crate) fn pool_options_from_env() -> PgPoolOptions {
    let cfg = resolve_pool_config(
        std::env::var("MEMKEYS_ROLE").ok().as_deref(),
        std::env::var("MEMKEYS_PG_MAX_CONNECTIONS").ok().as_deref(),
    );
    let mut opts = PgPoolOptions::new().max_connections(cfg.max_connections);
    if let Some(idle) = cfg.idle_timeout {
        // min_connections defaults to 0, so idle_timeout reaps the whole pool
        // when a session goes quiet.
        opts = opts.idle_timeout(idle);
    }
    opts
}

/// Fail fast when the `vector` extension (pgvector) is not available on the
/// server. `available` is whether `pg_available_extensions` lists `vector`
/// (installable), not whether it is already created. The message names the
/// requirement and the remediation (ISI-6319) so a stock-Postgres deploy sees
/// an actionable error instead of a raw mid-migration sqlx failure.
fn ensure_pgvector_available(available: bool) -> Result<(), sqlx::Error> {
    if available {
        return Ok(());
    }
    Err(sqlx::Error::Configuration(
        "the Postgres 'vector' extension (pgvector) is not available on this server, but a \
         boot migration requires it (CREATE EXTENSION vector). Use a Postgres image with \
         pgvector preinstalled — e.g. pgvector/pgvector:pg16 (also pg17/pg18) — or install \
         the pgvector package into your Postgres. See charts/memkeys/README.md (\"Postgres \
         requirements\")."
            .into(),
    ))
}

#[cfg(test)]
mod pool_config_tests {
    use super::resolve_pool_config;
    use std::time::Duration;

    #[test]
    fn mcp_role_defaults_to_two_with_idle_timeout() {
        let cfg = resolve_pool_config(Some("mcp"), None);
        assert_eq!(cfg.max_connections, 2);
        assert_eq!(cfg.idle_timeout, Some(Duration::from_secs(90)));
    }

    #[test]
    fn mcp_role_is_case_insensitive() {
        assert_eq!(resolve_pool_config(Some("MCP"), None).max_connections, 2);
        assert_eq!(resolve_pool_config(Some("Mcp"), None).max_connections, 2);
    }

    #[test]
    fn other_roles_keep_default_eight_and_no_idle_reaping() {
        for role in [None, Some("worker"), Some(""), Some("all-in-one")] {
            let cfg = resolve_pool_config(role, None);
            assert_eq!(cfg.max_connections, 8, "role {role:?}");
            assert_eq!(cfg.idle_timeout, None, "role {role:?}");
        }
    }

    #[test]
    fn explicit_override_wins_for_every_role() {
        // Overriding up is allowed even for mcp; the idle reaper still applies.
        let mcp = resolve_pool_config(Some("mcp"), Some("5"));
        assert_eq!(mcp.max_connections, 5);
        assert_eq!(mcp.idle_timeout, Some(Duration::from_secs(90)));
        assert_eq!(
            resolve_pool_config(None, Some("20")).max_connections,
            20,
            "explicit override wins for the all-in-one role too"
        );
    }

    #[test]
    fn invalid_or_zero_override_falls_back_to_role_default() {
        for bad in ["0", "-1", "abc", ""] {
            assert_eq!(
                resolve_pool_config(Some("mcp"), Some(bad)).max_connections,
                2,
                "bad override {bad:?} should fall back to the mcp default"
            );
            assert_eq!(
                resolve_pool_config(None, Some(bad)).max_connections,
                8,
                "bad override {bad:?} should fall back to the default"
            );
        }
    }
}

#[cfg(test)]
mod scheme_tests {
    use super::validate_postgres_scheme;

    #[test]
    fn accepts_postgres_schemes() {
        assert!(validate_postgres_scheme("postgres://user:pass@host/db").is_ok());
        assert!(validate_postgres_scheme("postgresql://host/db").is_ok());
        assert!(validate_postgres_scheme("POSTGRES://host/db").is_ok());
    }

    #[test]
    fn rejects_non_postgres_schemes() {
        let err = validate_postgres_scheme("sqlite:///tmp/mk.db").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sqlite"),
            "message should name the bad scheme: {msg}"
        );
        assert!(
            msg.contains("Postgres"),
            "message should say Postgres is required: {msg}"
        );
        assert!(validate_postgres_scheme("mysql://host/db").is_err());
        assert!(validate_postgres_scheme("not-a-url").is_err());
    }
}

#[cfg(test)]
mod pgvector_tests {
    use super::ensure_pgvector_available;

    #[test]
    fn ok_when_available() {
        assert!(ensure_pgvector_available(true).is_ok());
    }

    #[test]
    fn errors_with_remediation_when_unavailable() {
        let msg = ensure_pgvector_available(false).unwrap_err().to_string();
        assert!(
            msg.contains("pgvector"),
            "message should name pgvector: {msg}"
        );
        assert!(
            msg.contains("pgvector/pgvector"),
            "message should name the remediation image: {msg}"
        );
    }
}

const INSERT_FACT_SQL: &str = "\
INSERT INTO facts (\
  id, tenant_id, fact_type, subject, body, body_hash, confidence, supersedes, \
  event_time, ingested_at, \
  entity_project, entity_issue, entity_pr, entity_agent, entity_run, \
  entity_thread, entity_service, counterpart_agent, \
  keyed, provenance_principal, provenance_source_kind, provenance_trace_id, \
  rejected_hints, attachments, entity_identity) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
  $16, $17, $18, $19, $20, $21, $22, $23, $24, $25) \
ON CONFLICT (id) DO NOTHING \
RETURNING id";

/// Current live head of the live-head key H = (tenant_id, entity_identity,
/// fact_type, subject): the record on H that nothing supersedes (≤1 under the
/// per-H advisory lock). Liveness is the read-time `supersedes`-chain property
/// (§5.1), so no `superseded` flag and no write-back to prior records.
const LIVE_HEAD_SQL: &str = "\
SELECT f.id FROM facts f \
WHERE f.tenant_id = $1 AND f.entity_identity = $2 AND f.fact_type = $3 AND f.subject = $4 \
  AND NOT EXISTS (SELECT 1 FROM facts s WHERE s.supersedes = f.id AND s.tenant_id = f.tenant_id) \
FOR UPDATE";

/// ASCII unit separator — joins the live-head entity components unambiguously
/// (never valid inside an opaque entity-key value).
const EID_SEP: &str = "\u{1f}";

/// The entity component of the live-head key `H` (§7.6 **T3-pin**, ratified by
/// ISI-6131) for a subject-bearing fact, else `None` (the fact skips the gate).
///
/// Composition: the **work-item coordinate tuple** `(project, issue, pr)` — the
/// §4 keys that identify *what the fact is about* — taken from the **enriched**
/// entity (this runs after §4 enrichment). Excluded: `entity.agent` (ISI-6053
/// R1: the head is per statement, not per producing agent; agent stays in the
/// `fact.id` salt only) and **all three per-source transport anchors `run`,
/// `thread`, `service`** (how the fact arrived, not what it is about). §4
/// enrichment lifts a resolvable anchor to its work-item container *before* `H`
/// is computed, so the same statement keeps chaining across runs/threads/
/// services; keeping `thread`/`service` would re-fork cross-thread / cross-
/// service versioning of one work-item statement (ISI-6131 Q1 correction,
/// a4d3522). An **empty composite ⇒ unkeyed for `H` ⇒ gate bypassed** (`None`)
/// — an anchor-only fact enrichment could not lift is an independent
/// observation, not a competing head. The value is written once at commit and
/// is immutable (append-only SoT preserved).
fn entity_identity(fact: &ProcessedFact) -> Option<String> {
    // Gate scope: subject-bearing (the supersession grain).
    let has_subject = fact
        .subject
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    if !has_subject {
        return None;
    }
    let e = &fact.entity;
    let coordinate: Vec<String> = [("project", &e.project), ("issue", &e.issue), ("pr", &e.pr)]
        .into_iter()
        .filter_map(|(k, v)| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|v| format!("{k}={v}"))
        })
        .collect();
    if coordinate.is_empty() {
        // Unkeyed for H (e.g. an unlifted anchor-only fact) — skip the gate.
        return None;
    }
    Some(coordinate.join(EID_SEP))
}

/// A stable 64-bit key for `pg_advisory_xact_lock` derived from the full
/// live-head key `H = (tenant_id, entity_identity, subject, fact_type)`.
/// Length-prefixed so distinct tuples cannot alias to one lock key.
fn live_head_lock_key(
    tenant_id: &str,
    entity_identity: &str,
    subject: &str,
    fact_type: &str,
) -> i64 {
    let mut h = Sha256::new();
    for part in [tenant_id, entity_identity, subject, fact_type] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    let digest = h.finalize();
    i64::from_be_bytes(digest[..8].try_into().expect("sha256 >= 8 bytes"))
}

#[async_trait::async_trait]
impl FactStore for PgStore {
    /// Stage 3–4 — the live-head critical section and the commit point
    /// (§7.6 T3). Idempotent on `fact.id`; a same-id replay with a different
    /// body is a conflict (409), per the FactStore contract.
    ///
    /// For a **subject-bearing keyed** fact (`entity_identity(fact)` is
    /// `Some`) the whole insert runs inside one transaction holding a
    /// transaction-scoped advisory lock on `hash(H)`, so all writers to the
    /// same live-head key `H = (tenant_id, entity_identity, subject, type)`
    /// serialize (realization (b), §7.6 T3):
    /// - a plain write to an occupied `H` **auto-supersedes** the current head;
    /// - an explicit `supersedes` MUST name the **current** head — a stale or
    ///   foreign id is a `conflict` (this also forecloses supersession cycles).
    ///
    /// Subject-null / unkeyed facts (`entity_identity` is `None`) skip the gate
    /// entirely — independent observations that may share `(entity, null, type)`.
    async fn commit_fact(&self, fact: &ProcessedFact) -> Result<CommitOutcome, IngestError> {
        let eid = entity_identity(fact);

        let mut tx = self.pool.begin().await?;

        // The server owns supersession (§3): for a gated write we may override
        // the envelope's `supersedes` with the auto-superseded current head.
        let mut effective_supersedes = fact.supersedes;

        if let Some(eid) = eid.as_deref() {
            let subject = fact.subject.as_deref().unwrap_or("");
            let ftype = fact.fact_type.as_str();

            // Serialize every writer on this statement's live-head key. The
            // lock auto-releases at COMMIT/ROLLBACK.
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(live_head_lock_key(&fact.tenant_id, eid, subject, ftype))
                .execute(&mut *tx)
                .await?;

            // Idempotency short-circuit under the lock (before any head CAS so
            // a retried flush of a gated fact never mis-reads as a conflict).
            let existing: Option<(String,)> =
                sqlx::query_as("SELECT body_hash FROM facts WHERE id = $1 AND tenant_id = $2")
                    .bind(fact.id)
                    .bind(&fact.tenant_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some((hash,)) = existing {
                tx.commit().await?;
                return if hash == fact.body_hash {
                    Ok(CommitOutcome::Duplicate)
                } else {
                    Err(IngestError::Conflict {
                        existing_id: fact.id,
                    })
                };
            }

            // Compare-and-set against the current head of H.
            let head: Option<(uuid::Uuid,)> = sqlx::query_as(LIVE_HEAD_SQL)
                .bind(&fact.tenant_id)
                .bind(eid)
                .bind(ftype)
                .bind(subject)
                .fetch_optional(&mut *tx)
                .await?;
            let head_id = head.map(|h| h.0);

            effective_supersedes = match (fact.supersedes, head_id) {
                // Explicit supersedes naming the current head → advance.
                (Some(explicit), Some(h)) if explicit == h => Some(h),
                // Explicit supersedes naming a stale/foreign id → conflict
                // (report the real head so the caller can retry against it).
                (Some(_), Some(h)) => {
                    tx.rollback().await.ok();
                    return Err(IngestError::Conflict { existing_id: h });
                }
                // Explicit supersedes but no live head on H → stale/foreign.
                (Some(_), None) => {
                    tx.rollback().await.ok();
                    return Err(IngestError::Conflict {
                        existing_id: fact.id,
                    });
                }
                // Plain write to an occupied head → auto-supersede it.
                (None, Some(h)) => Some(h),
                // First head on H.
                (None, None) => None,
            };
        }

        let inserted = sqlx::query(INSERT_FACT_SQL)
            .bind(fact.id)
            .bind(&fact.tenant_id)
            .bind(fact.fact_type.as_str())
            .bind(&fact.subject)
            .bind(&fact.body)
            .bind(&fact.body_hash)
            .bind(fact.confidence)
            .bind(effective_supersedes)
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
            .bind(eid.as_deref())
            .fetch_optional(&mut *tx)
            .await?;

        if inserted.is_some() {
            tx.commit().await?;
            return Ok(CommitOutcome::Inserted);
        }

        // Same fact id already exists (non-gated path; the gated path handled
        // idempotency under the lock above): retried flush (same body →
        // duplicate) or conflicting replay (different body → 409).
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT body_hash FROM facts WHERE id = $1 AND tenant_id = $2")
                .bind(fact.id)
                .bind(&fact.tenant_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
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
pub(crate) const FACT_COLUMNS: &str = "id, fact_type, subject, body, confidence, supersedes, \
    event_time, ingested_at, entity_project, entity_issue, entity_pr, entity_agent, \
    entity_run, entity_thread, entity_service, keyed, provenance_principal";

/// Column list for fetch_by_keys (aliased to f).
pub(crate) const FACT_COLUMNS_ALIAS: &str = "f.id, f.fact_type, f.subject, f.body, f.confidence, \
    f.supersedes, f.event_time, f.ingested_at, f.entity_project, f.entity_issue, f.entity_pr, \
    f.entity_agent, f.entity_run, f.entity_thread, f.entity_service, f.keyed, \
    f.provenance_principal";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityKey, EntityKeys, FactType};

    fn gated_fact(entity: EntityKeys, subject: Option<&str>) -> ProcessedFact {
        ProcessedFact {
            id: uuid::Uuid::nil(),
            tenant_id: "t".into(),
            fact_type: FactType::Decision,
            subject: subject.map(str::to_string),
            body: "b".into(),
            body_hash: "h".into(),
            confidence: None,
            supersedes: None,
            event_time: chrono::Utc::now(),
            ingested_at: chrono::Utc::now(),
            keyed: entity.any_anchor_key(),
            entity,
            provenance_principal: "p".into(),
            provenance_source_kind: "gateway_flush".into(),
            provenance_trace_id: None,
            rejected_hints: vec![],
            attachments: serde_json::json!([]),
        }
    }

    #[test]
    fn insert_sql_conflict_clause_is_idempotent() {
        assert!(INSERT_FACT_SQL.contains("ON CONFLICT (id) DO NOTHING"));
    }

    fn keys(
        project: Option<&str>,
        issue: Option<&str>,
        run: Option<&str>,
        agent: Option<&str>,
    ) -> EntityKeys {
        EntityKeys {
            project: project.map(str::to_string),
            issue: issue.map(str::to_string),
            run: run.map(str::to_string),
            agent: agent.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn entity_identity_is_run_insensitive_when_a_durable_key_resolves() {
        // The existing cross-run supersession test relies on this: same issue
        // + subject + type is ONE statement regardless of the incidental run.
        let a = keys(None, Some("I1"), Some("r1"), None);
        let b = keys(None, Some("I1"), Some("r2"), None);
        let ha = entity_identity(&gated_fact(a, Some("the chosen database")));
        let hb = entity_identity(&gated_fact(b, Some("the chosen database")));
        assert_eq!(ha, hb);
        assert_eq!(ha.as_deref(), Some("issue=I1"));
    }

    #[test]
    fn entity_identity_excludes_agent() {
        // Two agents' same-subject fact on one issue share a head (latest-wins).
        let a = keys(None, Some("I1"), None, Some("agent-a"));
        let b = keys(None, Some("I1"), None, Some("agent-b"));
        assert_eq!(
            entity_identity(&gated_fact(a, Some("s"))),
            entity_identity(&gated_fact(b, Some("s")))
        );
    }

    #[test]
    fn entity_identity_excludes_run_so_an_unlifted_run_only_fact_is_not_gated() {
        // §4 enrichment lifts a resolvable run into project/issue/pr before H;
        // a run that was not lifted leaves an empty work-item composite → the
        // fact is unkeyed for H and skips the gate (T3-pin / ISI-6131).
        let run_only = keys(None, None, Some("r1"), None);
        assert!(entity_identity(&gated_fact(run_only, Some("s"))).is_none());
    }

    #[test]
    fn entity_identity_excludes_thread_and_service_as_transport_anchors() {
        // ISI-6131 Q1 correction (a4d3522): thread/service are §4 per-source
        // transport anchors, excluded alongside run. An anchor-only fact that
        // enrichment could not lift to a work-item coordinate is unkeyed for H
        // and gate-skipped (an independent observation, not a competing head).
        let thread_only = EntityKeys {
            thread: Some("T1".into()),
            ..Default::default()
        };
        let service_only = EntityKeys {
            service: Some("svc-a".into()),
            ..Default::default()
        };
        assert!(entity_identity(&gated_fact(thread_only, Some("s"))).is_none());
        assert!(entity_identity(&gated_fact(service_only, Some("s"))).is_none());
    }

    #[test]
    fn entity_identity_collapses_cross_thread_and_cross_service_on_one_work_item() {
        // The same work-item statement reached over two threads (or two
        // services) resolves to ONE live head — excluding thread/service is
        // exactly what prevents the cross-thread / cross-service fork on the
        // A2A and telemetry paths (ISI-6131 Q1).
        let via_t1 = EntityKeys {
            issue: Some("I1".into()),
            thread: Some("T1".into()),
            ..Default::default()
        };
        let via_t2 = EntityKeys {
            issue: Some("I1".into()),
            thread: Some("T2".into()),
            ..Default::default()
        };
        let via_svc = EntityKeys {
            issue: Some("I1".into()),
            service: Some("svc-a".into()),
            ..Default::default()
        };
        let h1 = entity_identity(&gated_fact(via_t1, Some("s")));
        let h2 = entity_identity(&gated_fact(via_t2, Some("s")));
        let h3 = entity_identity(&gated_fact(via_svc, Some("s")));
        assert_eq!(h1.as_deref(), Some("issue=I1"));
        assert_eq!(h1, h2);
        assert_eq!(h1, h3);
    }

    #[test]
    fn entity_identity_is_none_for_subjectless_or_unkeyed() {
        assert!(entity_identity(&gated_fact(keys(None, Some("I1"), None, None), None)).is_none());
        // Unkeyed for H (no work-item coordinate) → None even with a subject.
        assert!(entity_identity(&gated_fact(EntityKeys::default(), Some("s"))).is_none());
    }

    #[test]
    fn live_head_lock_key_is_stable_and_tuple_sensitive() {
        let k = live_head_lock_key("t", "issue=I1", "s", "decision");
        assert_eq!(k, live_head_lock_key("t", "issue=I1", "s", "decision"));
        // Length-prefixing prevents boundary aliasing between distinct tuples.
        assert_ne!(k, live_head_lock_key("t", "issue=I1s", "", "decision"));
        assert_ne!(k, live_head_lock_key("t", "issue=I1", "s", "outcome"));
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
