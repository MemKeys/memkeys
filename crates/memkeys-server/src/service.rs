//! The MemKeys service: orchestrates pipeline stages 1-4 with OTel
//! instrumentation, and serves op 1 (fetch-by-keys, §7.2).

use std::sync::Arc;
use std::time::Instant;

use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::model::{KeyedQuery, ProcessedFact};
use crate::pipeline::{
    key_stage, normalize, temporalize, CallerContext, EntityResolver, NoopResolver,
};
use crate::store::{CommitOutcome, FactStore};
use crate::telemetry::{self, Instruments};
use opentelemetry::trace::{Span, SpanKind, Status};

/// Response of `POST /memory/facts` (§7.1 write-with-keys): the fact id, the
/// keyed flag, and the server-resolved entity keys so the caller can cache
/// them for subsequent keyed reads.
#[derive(Debug, Clone)]
pub struct IngestReceipt {
    pub fact_id: uuid::Uuid,
    pub keyed: bool,
    /// `(entity.* wire name, resolved value)` pairs (§7.1).
    pub entity_keys_resolved: Vec<(&'static str, String)>,
    /// True when this ingest was a retried flush that resolved to an existing
    /// row (no duplicate stored).
    pub duplicate: bool,
}

/// The M1 service. One instance serves many tenants — the tenant is a per-call
/// attribute (WS-4 Invariant T), never process state.
pub struct MemKeysService {
    store: Arc<dyn FactStore>,
    resolver: Arc<dyn EntityResolver>,
    instruments: Instruments,
}

impl MemKeysService {
    pub fn new(store: Arc<dyn FactStore>) -> Self {
        Self::with_resolver(store, Arc::new(NoopResolver))
    }

    pub fn with_resolver(store: Arc<dyn FactStore>, resolver: Arc<dyn EntityResolver>) -> Self {
        Self {
            store,
            resolver,
            instruments: Instruments::new(),
        }
    }

    /// Write path (§6): stages 1-4 as one trace, `memkeys.fact.ingest`
    /// (SERVER) → normalize/key/temporalize (INTERNAL) → `memkeys.commit`
    /// (CLIENT, db.*). The receipt — the "ack" — is gated on stage 4 only.
    pub async fn ingest(
        &self,
        envelope: &Envelope,
        caller: &CallerContext,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<IngestReceipt, IngestError> {
        let mut ingest_span = telemetry::start_span(
            "memkeys.fact.ingest",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller.tenant_id.clone()),
                telemetry::attr("memkeys.entity.kind", anchor_kind(envelope)),
            ],
        );

        let outcome = self
            .run_pipeline(envelope, caller, now, &mut ingest_span)
            .await;
        match &outcome {
            Ok(_) => {
                ingest_span.set_status(Status::Ok);
                self.instruments
                    .write_operation(anchor_kind(envelope), telemetry::OUTCOME_SUCCESS);
            }
            Err(IngestError::Rejected { .. }) => {
                self.instruments
                    .write_operation(anchor_kind(envelope), telemetry::OUTCOME_REJECTED);
            }
            Err(_) => {
                self.instruments
                    .write_operation(anchor_kind(envelope), telemetry::OUTCOME_ERROR);
            }
        }
        ingest_span.end();
        outcome
    }

    async fn run_pipeline(
        &self,
        envelope: &Envelope,
        caller: &CallerContext,
        now: chrono::DateTime<chrono::Utc>,
        parent: &mut opentelemetry::global::BoxedSpan,
    ) -> Result<IngestReceipt, IngestError> {
        // Stage 1 — normalize.
        let t = Instant::now();
        let mut fact = stage_span(telemetry::PIPELINE_STAGE_NORMALIZE, || {
            normalize(envelope, caller, now)
        })?;
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_NORMALIZE,
            telemetry::OUTCOME_SUCCESS,
            t.elapsed().as_secs_f64(),
        );

        // Stage 2 — key.
        let t = Instant::now();
        stage_span(telemetry::PIPELINE_STAGE_KEY, || Ok(()))?;
        let key_result = key_stage(envelope, caller, &mut fact, self.resolver.as_ref()).await;
        let key_outcome = match &key_result {
            Ok(_) => telemetry::OUTCOME_SUCCESS,
            Err(IngestError::Rejected { .. }) => telemetry::OUTCOME_REJECTED,
            Err(_) => telemetry::OUTCOME_ERROR,
        };
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_KEY,
            key_outcome,
            t.elapsed().as_secs_f64(),
        );
        key_result?;

        // Stage 3 — temporalize (+ the supersedes-target store check that
        // validates the target shares (entity, subject, type)).
        let t = Instant::now();
        stage_span(telemetry::PIPELINE_STAGE_TEMPORALIZE, || {
            temporalize(&mut fact)
        })?;
        if let Some(target_id) = fact.supersedes {
            let target = self.store.get_fact(&caller.tenant_id, target_id).await?;
            let target = target.ok_or_else(|| IngestError::Rejected {
                reason: "supersedes target fact not found".into(),
            })?;
            validate_supersedes_target(&fact, &target)?;
        }
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_TEMPORALIZE,
            telemetry::OUTCOME_SUCCESS,
            t.elapsed().as_secs_f64(),
        );

        // Stage 4 — commit (the ack point).
        let t = Instant::now();
        let mut commit_span = telemetry::start_span(
            "memkeys.commit",
            SpanKind::Client,
            telemetry::db_attrs("insert"),
        );
        let commit_result = self.store.commit_fact(&fact).await;
        let commit_outcome = match &commit_result {
            Ok(CommitOutcome::Inserted) => telemetry::OUTCOME_SUCCESS,
            Ok(CommitOutcome::Duplicate) => telemetry::OUTCOME_DUPLICATE,
            Err(IngestError::Rejected { .. }) => telemetry::OUTCOME_REJECTED,
            Err(_) => telemetry::OUTCOME_ERROR,
        };
        if let Err(e) = &commit_result {
            record_error(&mut commit_span, format!("{e}"));
        }
        commit_span.set_status(match &commit_result {
            Ok(_) => Status::Ok,
            Err(_) => Status::Error {
                description: "commit failed".into(),
            },
        });
        commit_span.end();
        self.instruments.write_stage_duration(
            telemetry::PIPELINE_STAGE_COMMIT,
            commit_outcome,
            t.elapsed().as_secs_f64(),
        );
        let commit_result = commit_result.map_err(|e| {
            record_error(parent, format!("{e}"));
            e
        })?;

        let keyed = fact.keyed;
        Ok(IngestReceipt {
            fact_id: fact.id,
            keyed,
            entity_keys_resolved: fact
                .entity
                .resolved()
                .into_iter()
                .map(|(k, v)| (k, v.to_string()))
                .collect(),
            duplicate: commit_result == CommitOutcome::Duplicate,
        })
    }

    /// Operation 1 — fetch-by-keys (§7.2). Requires ≥1 entity key (ANDed);
    /// `valid=current` (default) collapses supersession at read time. One
    /// trace: `memkeys.retrieve` (SERVER) → `memkeys.store.query` (CLIENT).
    pub async fn fetch_by_keys(
        &self,
        caller_tenant: &str,
        query: &KeyedQuery,
    ) -> Result<Vec<crate::model::FactRecord>, QueryError> {
        if query.keys.is_empty() {
            return Err(QueryError::Invalid {
                reason: "at least one entity.* key is required".into(),
            });
        }
        if query.limit <= 0 || query.limit > crate::model::KeyedQuery::MAX_LIMIT {
            return Err(QueryError::Invalid {
                reason: format!("limit must be in 1..={}", KeyedQuery::MAX_LIMIT),
            });
        }

        let mut retrieve_span = telemetry::start_span(
            "memkeys.retrieve",
            SpanKind::Server,
            vec![
                telemetry::attr("memkeys.tenant.id", caller_tenant.to_string()),
                telemetry::attr("memkeys.retrieval.op", telemetry::RETRIEVAL_OP_KEYED),
            ],
        );
        let t = Instant::now();
        let mut store_span = telemetry::start_span(
            "memkeys.store.query",
            SpanKind::Client,
            telemetry::db_attrs("select"),
        );
        let result = self.store.fetch_by_keys(caller_tenant, query).await;
        match &result {
            Ok(rows) => {
                store_span.set_status(Status::Ok);
                retrieve_span.set_status(Status::Ok);
                self.instruments
                    .candidate_set_size(telemetry::RETRIEVAL_OP_KEYED, rows.len() as u64);
            }
            Err(e) => {
                record_error(&mut store_span, format!("{e}"));
                record_error(&mut retrieve_span, format!("{e}"));
                store_span.set_status(Status::Error {
                    description: "query failed".into(),
                });
                retrieve_span.set_status(Status::Error {
                    description: "query failed".into(),
                });
            }
        }
        store_span.end();
        let secs = t.elapsed().as_secs_f64();
        let outcome = if result.is_ok() {
            telemetry::OUTCOME_SUCCESS
        } else {
            telemetry::OUTCOME_ERROR
        };
        self.instruments
            .retrieve_duration(telemetry::RETRIEVAL_OP_KEYED, outcome, secs);
        retrieve_span.end();
        result
    }
}

/// The entity-kind attribute of the envelope's anchor (the kind only — the
/// value is on the deny-list, signal-catalog "never fact content").
fn anchor_kind(envelope: &Envelope) -> &'static str {
    match (
        envelope.source.kind,
        envelope.entity.service.is_some() || envelope.entity.project.is_some(),
    ) {
        (crate::envelope::SourceKind::TelemetryLearning, true)
            if envelope.entity.service.is_some() =>
        {
            "service"
        }
        (crate::envelope::SourceKind::TelemetryLearning, _) => "project",
        (crate::envelope::SourceKind::A2aExchange, _) => "thread",
        _ => "run",
    }
}

fn validate_supersedes_target(
    fact: &ProcessedFact,
    target: &crate::model::FactRecord,
) -> Result<(), IngestError> {
    // §6 stage 3 (§7.6 T3-pin, Q2 unified on H-equality; Q1 corrected a4d3522):
    // "validate the target shares (entity, subject, type)". The supersession
    // grain is the *thing the fact is about* — `entity_identity`, the work-item
    // coordinate `(project, issue, pr)`. An explicit `supersedes` is valid iff
    // the target has the **same entity_identity**, subject, and type. The
    // per-source transport anchors `run`/`thread`/`service` are NOT compared:
    // they are excluded from identity, so sharing only a run/thread/service is
    // not a shared statement (that would re-introduce the cross-run / cross-
    // thread fork the gate removes). Cross-run versioning still works because
    // `run` is excluded on BOTH sides — a later-run decision on the same issue
    // has the same coordinate. Compared normalized so blank == absent.
    fn coord(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    let same_entity_identity = coord(&fact.entity.project) == coord(&target.entity_project)
        && coord(&fact.entity.issue) == coord(&target.entity_issue)
        && coord(&fact.entity.pr) == coord(&target.entity_pr);
    if !same_entity_identity {
        return Err(IngestError::Rejected {
            reason: "supersedes target does not share the entity_identity (project/issue/pr)"
                .into(),
        });
    }
    if target.subject != fact.subject {
        return Err(IngestError::Rejected {
            reason: "supersedes target does not share the subject".into(),
        });
    }
    if target.fact_type != fact.fact_type.as_str() {
        return Err(IngestError::Rejected {
            reason: "supersedes target does not share the fact type".into(),
        });
    }
    Ok(())
}

fn record_error(span: &mut opentelemetry::global::BoxedSpan, message: String) {
    span.set_status(Status::Error {
        description: message.into(),
    });
}

/// Start a pipeline-stage span around a synchronous stage body, recording the
/// stage attribute (`memkeys.pipeline.stage`).
fn stage_span<T>(
    stage: &'static str,
    body: impl FnOnce() -> Result<T, IngestError>,
) -> Result<T, IngestError> {
    let mut span = telemetry::start_span(
        stage_span_name(stage),
        SpanKind::Internal,
        vec![telemetry::attr("memkeys.pipeline.stage", stage)],
    );
    let result = body();
    match &result {
        Ok(_) => span.set_status(Status::Ok),
        Err(e) => record_error(&mut span, format!("{e}")),
    }
    span.end();
    result
}

fn stage_span_name(stage: &str) -> &'static str {
    match stage {
        telemetry::PIPELINE_STAGE_NORMALIZE => "memkeys.normalize",
        telemetry::PIPELINE_STAGE_KEY => "memkeys.key",
        telemetry::PIPELINE_STAGE_TEMPORALIZE => "memkeys.temporalize",
        _ => "memkeys.stage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityKey, Validity};

    #[test]
    fn keyed_query_defaults() {
        let q = KeyedQuery {
            keys: vec![(EntityKey::Issue, "i1".into())],
            fact_type: None,
            as_of: None,
            valid: Validity::default(),
            limit: KeyedQuery::DEFAULT_LIMIT,
        };
        assert_eq!(q.valid, Validity::Current);
        assert_eq!(q.limit, 100);
    }
}
