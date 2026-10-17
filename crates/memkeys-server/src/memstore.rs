//! In-memory [`FactStore`] — a non-durable reference backend.
//!
//! NOT for production: MemKeys' source of truth is the Postgres FactStore
//! (§5.1, `pg.rs`). This backend exists so the OTel local check
//! (`examples/otel_local_check.rs`, `docs/observability/local-check.md`) and
//! library-level tests can exercise the full write path + keyed read —
//! spans, metrics, logs — without a database deployment.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::{IngestError, QueryError};
use crate::model::{EntityKey, FactRecord, KeyedQuery, ProcessedFact, Validity};
use crate::store::{CommitOutcome, FactStore};

/// Append-only in-memory store. One instance is single-tenant by convention
/// of its callers (the tenant floor is still enforced on every read).
#[derive(Debug, Default)]
pub struct MemStore {
    facts: RwLock<HashMap<uuid::Uuid, FactRecord>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn to_record(fact: &ProcessedFact) -> FactRecord {
        FactRecord {
            id: fact.id,
            fact_type: fact.fact_type.as_str().to_string(),
            subject: fact.subject.clone(),
            body: fact.body.clone(),
            confidence: fact.confidence,
            supersedes: fact.supersedes,
            event_time: fact.event_time,
            ingested_at: fact.ingested_at,
            entity_project: fact.entity.project.clone(),
            entity_issue: fact.entity.issue.clone(),
            entity_pr: fact.entity.pr.clone(),
            entity_agent: fact.entity.agent.clone(),
            entity_run: fact.entity.run.clone(),
            entity_thread: fact.entity.thread.clone(),
            entity_service: fact.entity.service.clone(),
            keyed: fact.keyed,
            provenance_principal: fact.provenance_principal.clone(),
        }
    }
}

#[async_trait::async_trait]
impl FactStore for MemStore {
    async fn commit_fact(&self, fact: &ProcessedFact) -> Result<CommitOutcome, IngestError> {
        let mut facts = self.facts.write().expect("memstore lock");
        if let Some(existing) = facts.get(&fact.id) {
            // Same id + same body hash = retried flush (idempotent ack).
            if existing.body == fact.body {
                return Ok(CommitOutcome::Duplicate);
            }
            return Err(IngestError::Conflict {
                existing_id: fact.id,
            });
        }
        facts.insert(fact.id, Self::to_record(fact));
        Ok(CommitOutcome::Inserted)
    }

    async fn get_fact(
        &self,
        _tenant_id: &str,
        fact_id: uuid::Uuid,
    ) -> Result<Option<FactRecord>, QueryError> {
        // Single-tenant by construction (see module docs): the tenant floor is
        // the caller's own deployment, so no per-record tenant column exists.
        Ok(self
            .facts
            .read()
            .expect("memstore lock")
            .get(&fact_id)
            .cloned())
    }

    async fn fetch_by_keys(
        &self,
        tenant_id: &str,
        query: &KeyedQuery,
    ) -> Result<Vec<FactRecord>, QueryError> {
        let _ = tenant_id; // single-tenant by construction (see module docs)
        let facts = self.facts.read().expect("memstore lock");

        let mut rows: Vec<FactRecord> = facts
            .values()
            .filter(|r| r.keyed)
            .filter(|r| {
                query.keys.iter().all(|(kind, value)| match kind {
                    EntityKey::Project => r.entity_project.as_deref() == Some(value.as_str()),
                    EntityKey::Issue => r.entity_issue.as_deref() == Some(value.as_str()),
                    EntityKey::Pr => r.entity_pr.as_deref() == Some(value.as_str()),
                    EntityKey::Agent => r.entity_agent.as_deref() == Some(value.as_str()),
                    EntityKey::Run => r.entity_run.as_deref() == Some(value.as_str()),
                    EntityKey::Thread => r.entity_thread.as_deref() == Some(value.as_str()),
                    EntityKey::Service => r.entity_service.as_deref() == Some(value.as_str()),
                })
            })
            .filter(|r| query.fact_type.is_none_or(|t| r.fact_type == t.as_str()))
            .filter(|r| {
                query
                    .as_of
                    .is_none_or(|as_of| r.event_time <= as_of && r.ingested_at <= as_of)
            })
            .cloned()
            .collect();

        // Newest event first, bounded (same contract as the Postgres read).
        rows.sort_by(|a, b| b.event_time.cmp(&a.event_time));
        rows.truncate(query.limit as usize);

        // Read-time supersession collapse (§5.2, `valid=current`): drop records
        // superseded by another record on (entity, subject, type) that is
        // visible under the same as_of/limit window. The collapse grain is the
        // same as validate_supersedes_target: entity+subject+type.
        if query.valid == Validity::Current {
            let superseded: std::collections::HashSet<uuid::Uuid> =
                rows.iter().filter_map(|r| r.supersedes).collect();
            rows.retain(|r| !superseded.contains(&r.id));
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityKeys, FactType};
    use chrono::Utc;

    fn fact(id: uuid::Uuid, issue: &str) -> ProcessedFact {
        ProcessedFact {
            id,
            tenant_id: "t".into(),
            fact_type: FactType::Observation,
            subject: Some("subject".into()),
            body: "body".into(),
            body_hash: String::new(),
            confidence: None,
            supersedes: None,
            event_time: Utc::now(),
            ingested_at: Utc::now(),
            entity: EntityKeys {
                issue: Some(issue.into()),
                ..Default::default()
            },
            keyed: true,
            provenance_principal: "agent-a".into(),
            provenance_source_kind: "gateway_flush".into(),
            provenance_trace_id: None,
            rejected_hints: Vec::new(),
            attachments: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn idempotent_commit_and_keyed_read() {
        let store = MemStore::new();
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            store.commit_fact(&fact(id, "I-1")).await.unwrap(),
            CommitOutcome::Inserted
        );
        assert_eq!(
            store.commit_fact(&fact(id, "I-1")).await.unwrap(),
            CommitOutcome::Duplicate
        );
        let rows = store
            .fetch_by_keys(
                "t",
                &KeyedQuery {
                    keys: vec![(EntityKey::Issue, "I-1".into())],
                    fact_type: None,
                    as_of: None,
                    valid: Validity::Current,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
    }
}
