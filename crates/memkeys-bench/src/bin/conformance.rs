//! `conformance` — the M8 conformance suite for the M1+ surfaces.
//!
//! Numbered checks of the architecture invariants (docs/architecture.md) and
//! the FactStore contract, executed against a real Postgres through the real
//! `MemKeysService`. Every check must PASS for the run to be green (exit 0);
//! any FAIL exits 1. This is the conformance gate of
//! docs/promotion-thresholds.md: a milestone whose code drops any invariant
//! listed here does not promote to main.
//!
//! Sections: §3/§3.1 envelope contract, §4 key extraction, §5.1 bi-temporal
//! record + supersession, §5.3 point-in-time replay, §6 write pipeline
//! stages 1-4, §7.2 keyed read, WS-4 Invariant T tenant floor.

use std::sync::Arc;

use memkeys_server::envelope::{EntityHints, Envelope, FactBlock, SourceBlock, SourceKind};
use memkeys_server::error::{IngestError, QueryError};
use memkeys_server::model::{EntityKey, FactType, KeyedQuery, Validity};
use memkeys_server::pg::PgStore;
use memkeys_server::pipeline::{
    CallerContext, EntityResolver, NoopResolver, RunBinding, ThreadBinding,
};
use memkeys_server::service::MemKeysService;
use memkeys_server::store::FactStore;

/// Checks that currently DEVIATE from the architecture (check id, tracking
/// issue). A listed deviation keeps the suite green but prints loudly; an
/// unlisted failure is red. Remove the entry once the deviation is fixed —
/// the check then becomes a hard PASS again. Recorded in
/// docs/promotion-thresholds.md.
const KNOWN_DEVIATIONS: &[(&str, &str)] = &[];

struct Suite {
    total: usize,
    passed: usize,
    deviations: Vec<(&'static str, &'static str)>,
    failed: Vec<&'static str>,
}

impl Suite {
    fn record(&mut self, id: &'static str, title: &str, result: Result<(), String>) {
        self.total += 1;
        match result {
            Ok(()) => {
                self.passed += 1;
                println!("PASS {id} — {title}");
            }
            Err(detail) => {
                if let Some((_, issue)) = KNOWN_DEVIATIONS.iter().find(|(c, _)| *c == id) {
                    self.deviations.push((id, issue));
                    println!("DEVIATION {id} — {title}: {detail}");
                    println!("  -> known deviation, tracked in {issue}");
                } else {
                    self.failed.push(id);
                    println!("FAIL {id} — {title}: {detail}");
                }
            }
        }
    }
}

/// Corroborated-enrichment stub (§4 rule 2) for the resolver pivot checks:
/// run-R ⇒ (issue-X, proj-P), thread-T ⇒ proj-P + participants, svc-S ⇒ proj-P.
struct StubResolver;

#[async_trait::async_trait]
impl EntityResolver for StubResolver {
    async fn resolve_run(&self, _tenant: &str, run: &str) -> Option<RunBinding> {
        // The §8 roundtrip: run R ⇒ issue X, project P.
        if run == "run-R" {
            Some(RunBinding {
                project: Some("proj-P".into()),
                issue: Some("issue-X".into()),
                pr: None,
                agent: None,
            })
        } else {
            None
        }
    }

    async fn resolve_thread(&self, _tenant: &str, thread: &str) -> Option<ThreadBinding> {
        if thread == "thread-T" {
            Some(ThreadBinding {
                project: Some("proj-P".into()),
                issue: None,
                pr: None,
                participants: vec!["agent-a".into(), "agent-b".into()],
            })
        } else {
            None
        }
    }

    async fn resolve_service(&self, _tenant: &str, service: &str) -> Option<String> {
        if service == "svc-S" {
            Some("proj-P".into())
        } else {
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let url = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("MEMKEYS_DATABASE_URL").ok())
        .or_else(|| std::env::var("TEST_DATABASE_URL").ok())
        .unwrap_or_default();
    if url.is_empty() {
        eprintln!("usage: conformance [URL] (or set MEMKEYS_DATABASE_URL/TEST_DATABASE_URL)");
        std::process::exit(2);
    }
    let store = match PgStore::connect(&url).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("connect + migrate failed: {e}");
            std::process::exit(2);
        }
    };

    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let mut suite = Suite {
        total: 0,
        passed: 0,
        deviations: Vec::new(),
        failed: Vec::new(),
    };
    let mut tenant_seq: u64 = 0;
    macro_rules! caller {
        () => {{
            tenant_seq += 1;
            CallerContext {
                tenant_id: format!("conf-{nonce}-{tenant_seq}"),
                principal: "agent-a".into(),
                trace_id: Some("trace-conf".into()),
            }
        }};
    }

    // One service per resolver flavor, same store.
    let svc_noop = MemKeysService::with_resolver(store.clone(), Arc::new(NoopResolver));
    let svc_stub = MemKeysService::with_resolver(store.clone(), Arc::new(StubResolver));
    let now = chrono::Utc::now();

    // --- §3/§3.1 envelope contract --------------------------------------
    suite.record(
        "C01",
        "§3.1 anchor matrix: gateway_flush without entity.run is rejected",
        match svc_noop
            .ingest(
                &env_gateway(None, Some("issue-1".into()), "no anchor"),
                &caller!(),
                now,
            )
            .await
        {
            Err(IngestError::Rejected { .. }) => Ok(()),
            other => Err(format!("expected Rejected, got {other:?}")),
        },
    );

    suite.record(
        "C02",
        "§3 required fields: invalid source.emitted_at is rejected",
        {
            let mut e = env_gateway(Some("run-1".into()), Some("issue-1".into()), "body");
            e.source.emitted_at = "not-a-timestamp".into();
            match svc_noop.ingest(&e, &caller!(), now).await {
                Err(IngestError::Rejected { .. }) => Ok(()),
                other => Err(format!("expected Rejected, got {other:?}")),
            }
        },
    );

    suite.record(
        "C03",
        "§3 required fields: empty fact.body is rejected",
        match svc_noop
            .ingest(
                &env_gateway(Some("run-1".into()), Some("issue-1".into()), ""),
                &caller!(),
                now,
            )
            .await
        {
            Err(IngestError::Rejected { .. }) => Ok(()),
            other => Err(format!("expected Rejected, got {other:?}")),
        },
    );

    suite.record(
        "C04",
        "§3.1 confidence bounds: confidence outside [0.0, 1.0] is rejected",
        {
            let e = Envelope {
                source: source(SourceKind::GatewayFlush),
                entity: hints_run(),
                fact: FactBlock {
                    fact_type: None,
                    body: serde_json::Value::String("body".into()),
                    subject: None,
                    event_time: None,
                    confidence: Some(1.5),
                    supersedes: None,
                },
                attachments: vec![],
                provenance: None,
            };
            match svc_noop.ingest(&e, &caller!(), now).await {
                Err(IngestError::Rejected { .. }) => Ok(()),
                other => Err(format!("expected Rejected, got {other:?}")),
            }
        },
    );

    suite.record(
        "C05",
        "§3 provenance: a populated block is discarded and recorded as a rejected hint (never merged)",
        {
            let c = caller!();
            let e = Envelope {
                source: source(SourceKind::GatewayFlush),
                entity: hints_run(),
                fact: fact("provenance-populated fact"),
                attachments: vec![],
                provenance: Some(serde_json::json!({ "claimed_by": "attacker" })),
            };
            let receipt = svc_noop
                .ingest(&e, &c, now)
                .await
                .map_err(|e| format!("ingest should succeed: {e:?}"))?;
            let (rejected,): (serde_json::Value,) =
                sqlx::query_as("SELECT rejected_hints FROM facts WHERE id = $1 AND tenant_id = $2")
                    .bind(receipt.fact_id)
                    .bind(&c.tenant_id)
                    .fetch_one(store.pool())
                    .await
                    .map_err(|e| format!("row read failed: {e}"))?;
            let listed = rejected
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default();
            if listed.contains(&"provenance") {
                Ok(())
            } else {
                Err(format!("rejected_hints must record the discard, got {listed:?}"))
            }
        },
    );

    suite.record("C06", "§3.1 a2a_exchange: thread anchor required", {
        let e = Envelope {
            source: source(SourceKind::A2aExchange),
            entity: EntityHints::default(),
            fact: fact("a2a without thread"),
            attachments: vec![],
            provenance: None,
        };
        match svc_noop.ingest(&e, &caller!(), now).await {
            Err(IngestError::Rejected { .. }) => Ok(()),
            other => Err(format!("expected Rejected, got {other:?}")),
        }
    });

    suite.record("C07", "§3.1 a2a decisions require fact.subject", {
        let e = Envelope {
            source: source(SourceKind::A2aExchange),
            entity: hints_thread(),
            fact: FactBlock {
                fact_type: Some(FactType::Decision),
                body: serde_json::Value::String("decision without subject".into()),
                subject: None,
                event_time: None,
                confidence: None,
                supersedes: None,
            },
            attachments: vec![],
            provenance: None,
        };
        match svc_noop.ingest(&e, &caller!(), now).await {
            Err(IngestError::Rejected { .. }) => Ok(()),
            other => Err(format!("expected Rejected, got {other:?}")),
        }
    });

    // --- §4 key extraction ------------------------------------------------
    suite.record(
        "C08",
        "§4 rule 1: a disagreeing entity.agent claim is overwritten by the principal",
        {
            let c = caller!();
            let mut hints = hints_run();
            hints.agent = Some("someone-else".into());
            let e = Envelope {
                source: source(SourceKind::GatewayFlush),
                entity: hints,
                fact: fact("agent claim mismatch"),
                attachments: vec![],
                provenance: None,
            };
            let receipt = svc_noop
                .ingest(&e, &c, now)
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let stored = store
                .get_fact(&c.tenant_id, receipt.fact_id)
                .await
                .map_err(|e| format!("get_fact failed: {e:?}"))?
                .ok_or("fact not stored")?;
            if stored.entity_agent.as_deref() == Some("agent-a") {
                Ok(())
            } else {
                Err(format!(
                    "entity.agent must be the principal, got {:?}",
                    stored.entity_agent
                ))
            }
        },
    );

    suite.record(
        "C09",
        "§4 rule 2: run enrichment resolves entity.run into issue/project keys (§8 roundtrip)",
        {
            let c = caller!();
            let e = Envelope {
                source: source(SourceKind::GatewayFlush),
                entity: EntityHints {
                    run: Some("run-R".into()),
                    ..EntityHints::default()
                },
                fact: fact("run-only envelope enriches to issue keys"),
                attachments: vec![],
                provenance: None,
            };
            let receipt = svc_stub
                .ingest(&e, &c, now)
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let names: Vec<&str> = receipt
                .entity_keys_resolved
                .iter()
                .map(|(k, _)| *k)
                .collect();
            if names.contains(&"entity.issue") && names.contains(&"entity.project") {
                Ok(())
            } else {
                Err(format!(
                    "expected enriched issue+project keys, got {names:?}"
                ))
            }
        },
    );

    suite.record(
        "C10",
        "§4 rule 2: an unresolvable anchor yields no invented keys (corroborated, never guessed)",
        {
            let c = caller!();
            let e = Envelope {
                source: source(SourceKind::GatewayFlush),
                entity: EntityHints {
                    run: Some("run-UNKNOWN".into()),
                    ..EntityHints::default()
                },
                fact: fact("unresolvable run"),
                attachments: vec![],
                provenance: None,
            };
            let receipt = svc_stub
                .ingest(&e, &c, now)
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let names: Vec<&str> = receipt
                .entity_keys_resolved
                .iter()
                .map(|(k, _)| *k)
                .collect();
            if names.contains(&"entity.issue") || names.contains(&"entity.project") {
                Err(format!("invented keys for an unresolvable run: {names:?}"))
            } else {
                Ok(())
            }
        },
    );

    suite.record(
        "C11",
        "§4 unkeyed facts: telemetry_learning without anchor stores unkeyed and never surfaces in keyed reads",
        {
            let c = caller!();
            let e = Envelope {
                source: source(SourceKind::TelemetryLearning),
                entity: EntityHints::default(),
                fact: fact("telemetry learning without anchor"),
                attachments: vec![],
                provenance: None,
            };
            let receipt = svc_noop
                .ingest(&e, &c, now)
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            if receipt.keyed {
                Err("telemetry_learning without anchor must be unkeyed".into())
            } else {
                let rows = store
                    .fetch_by_keys(&c.tenant_id, &query(vec![(EntityKey::Agent, "agent-a".into())]))
                    .await
                    .map_err(|e| format!("fetch failed: {e:?}"))?;
                if rows.iter().any(|r| r.id == receipt.fact_id) {
                    Err("unkeyed fact surfaced in a keyed read".into())
                } else {
                    Ok(())
                }
            }
        },
    );

    // --- §6 stages / FactStore contract -----------------------------------
    suite.record(
        "C12",
        "§6 stage 1: a retried flush (same envelope) is idempotent — no duplicate row",
        {
            let c = caller!();
            let e = env_gateway(
                Some("run-1".into()),
                Some("issue-1".into()),
                "retried flush body",
            );
            let first = svc_noop
                .ingest(&e, &c, now)
                .await
                .map_err(|e| format!("first ingest failed: {e:?}"))?;
            let second = svc_noop
                .ingest(&e, &c, now + chrono::Duration::seconds(30))
                .await
                .map_err(|e| format!("retried ingest failed: {e:?}"))?;
            if first.fact_id != second.fact_id {
                Err("retried flush resolved to a different fact id".into())
            } else if !second.duplicate {
                Err("retried flush must report duplicate".into())
            } else {
                let rows = store
                    .fetch_by_keys(&c.tenant_id, &issue_query("issue-1"))
                    .await
                    .map_err(|e| format!("fetch failed: {e:?}"))?;
                let count = rows.iter().filter(|r| r.id == first.fact_id).count();
                if count == 1 {
                    Ok(())
                } else {
                    Err(format!("expected exactly 1 stored row, got {count}"))
                }
            }
        },
    );

    suite.record(
        "C13",
        "§5.1 store contract: same idempotency key with a different body is a conflict (409)",
        {
            let c = caller!();
            let mk = |body: &str| {
                let mut e = env_gateway(Some("run-1".into()), Some("issue-1".into()), body);
                e.source.idempotency_key = Some("idem-key-1".into());
                e
            };
            svc_noop
                .ingest(&mk("first body"), &c, now)
                .await
                .map_err(|e| format!("first ingest failed: {e:?}"))?;
            match svc_noop.ingest(&mk("different body"), &c, now).await {
                Err(IngestError::Conflict { .. }) => Ok(()),
                other => Err(format!("expected Conflict, got {other:?}")),
            }
        },
    );

    suite.record(
        "C14",
        "§6 stage 3: supersedes without fact.subject is rejected",
        {
            let c = caller!();
            let first = svc_noop
                .ingest(
                    &env_gateway(
                        Some("run-1".into()),
                        Some("issue-1".into()),
                        "original statement",
                    ),
                    &c,
                    now,
                )
                .await
                .map_err(|e| format!("first ingest failed: {e:?}"))?;
            let mut e = env_gateway(
                Some("run-1".into()),
                Some("issue-1".into()),
                "new version without subject",
            );
            e.fact.supersedes = Some(first.fact_id);
            match svc_noop.ingest(&e, &c, now).await {
                Err(IngestError::Rejected { .. }) => Ok(()),
                other => Err(format!("expected Rejected, got {other:?}")),
            }
        },
    );

    suite.record(
        "C15",
        "§6 stage 3: a supersedes target with a different subject is rejected",
        {
            let c = caller!();
            let mut first = env_gateway(Some("run-1".into()), Some("issue-1".into()), "original");
            first.fact.subject = Some("subject-A".into());
            let first_id = svc_noop
                .ingest(&first, &c, now)
                .await
                .map_err(|e| format!("first ingest failed: {e:?}"))?
                .fact_id;
            let mut e = env_gateway(
                Some("run-1".into()),
                Some("issue-1".into()),
                "superseding different grain",
            );
            e.fact.subject = Some("subject-B".into());
            e.fact.supersedes = Some(first_id);
            match svc_noop.ingest(&e, &c, now).await {
                Err(IngestError::Rejected { .. }) => Ok(()),
                other => Err(format!("expected Rejected, got {other:?}")),
            }
        },
    );

    suite.record(
        "C16",
        "§5.1 supersession: valid=current collapses the chain, valid=all keeps history",
        {
            let c = caller!();
            let mut first =
                env_gateway(Some("run-1".into()), Some("issue-9".into()), "v1 statement");
            first.fact.subject = Some("grain-1".into());
            let first_id = svc_noop
                .ingest(&first, &c, now)
                .await
                .map_err(|e| format!("first ingest failed: {e:?}"))?
                .fact_id;
            let mut second =
                env_gateway(Some("run-1".into()), Some("issue-9".into()), "v2 statement");
            second.fact.subject = Some("grain-1".into());
            second.fact.supersedes = Some(first_id);
            let second_id = svc_noop
                .ingest(&second, &c, now + chrono::Duration::seconds(10))
                .await
                .map_err(|e| format!("second ingest failed: {e:?}"))?
                .fact_id;

            let current = store
                .fetch_by_keys(&c.tenant_id, &issue_query("issue-9"))
                .await
                .map_err(|e| format!("fetch current failed: {e:?}"))?;
            let all = store
                .fetch_by_keys(&c.tenant_id, &issue_query_valid("issue-9", Validity::All))
                .await
                .map_err(|e| format!("fetch all failed: {e:?}"))?;
            let has =
                |rows: &[memkeys_server::model::FactRecord], id| rows.iter().any(|r| r.id == id);
            if has(&current, second_id)
                && !has(&current, first_id)
                && has(&all, first_id)
                && has(&all, second_id)
            {
                Ok(())
            } else {
                Err(format!(
                    "collapse wrong: current={:?} all={:?}",
                    current.iter().map(|r| r.id).collect::<Vec<_>>(),
                    all.iter().map(|r| r.id).collect::<Vec<_>>()
                ))
            }
        },
    );

    suite.record(
        "C17",
        "§5.3 point-in-time replay: as_of hides the fact before it was known, shows it after",
        {
            let c = caller!();
            let svc_time = now;
            let mut e = env_gateway(
                Some("run-1".into()),
                Some("issue-17".into()),
                "temporal visibility fact",
            );
            e.source.emitted_at = "2026-10-17T09:00:00Z".into();
            let receipt = svc_noop
                .ingest(&e, &c, svc_time)
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let mut before_q = issue_query("issue-17");
            before_q.as_of = Some(svc_time - chrono::Duration::hours(1));
            let mut after_q = issue_query("issue-17");
            after_q.as_of = Some(svc_time + chrono::Duration::seconds(1));
            let visible_before = store
                .fetch_by_keys(&c.tenant_id, &before_q)
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?;
            let visible_after = store
                .fetch_by_keys(&c.tenant_id, &after_q)
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?;
            let seen = |rows: &Vec<memkeys_server::model::FactRecord>| {
                rows.iter().any(|r| r.id == receipt.fact_id)
            };
            if !seen(&visible_before) && seen(&visible_after) {
                Ok(())
            } else {
                Err("temporal visibility wrong".into())
            }
        },
    );

    // --- §7.2 keyed read ---------------------------------------------------
    suite.record(
        "C18",
        "§7.2: keyed read requires at least one entity key",
        {
            let c = caller!();
            match svc_noop.fetch_by_keys(&c.tenant_id, &query(vec![])).await {
                Err(QueryError::Invalid { .. }) => Ok(()),
                other => Err(format!("expected Invalid, got {other:?}")),
            }
        },
    );

    suite.record(
        "C19",
        "§7.2: limit bounds are enforced (0 and MAX+1 are Invalid)",
        {
            let c = caller!();
            let mut zero = issue_query("issue-1");
            zero.limit = 0;
            let mut over = issue_query("issue-1");
            over.limit = KeyedQuery::MAX_LIMIT + 1;
            match (
                svc_noop.fetch_by_keys(&c.tenant_id, &zero).await,
                svc_noop.fetch_by_keys(&c.tenant_id, &over).await,
            ) {
                (Err(QueryError::Invalid { .. }), Err(QueryError::Invalid { .. })) => Ok(()),
                (a, b) => Err(format!("expected Invalid for both, got {a:?} / {b:?}")),
            }
        },
    );

    suite.record(
        "C20",
        "§7.2: fact_type filter and ANDed keys narrow the result set",
        {
            let c = caller!();
            svc_noop
                .ingest(
                    &env_gateway(
                        Some("run-1".into()),
                        Some("issue-20".into()),
                        "context fact for filtering",
                    ),
                    &c,
                    now,
                )
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let outcome = Envelope {
                source: source(SourceKind::RunSettle),
                entity: hints_run_issue("issue-20"),
                fact: FactBlock {
                    fact_type: Some(FactType::Outcome),
                    body: serde_json::Value::String("outcome fact for filtering".into()),
                    subject: None,
                    event_time: None,
                    confidence: None,
                    supersedes: None,
                },
                attachments: vec![],
                provenance: None,
            };
            svc_noop
                .ingest(&outcome, &c, now)
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let both = store
                .fetch_by_keys(&c.tenant_id, &issue_query("issue-20"))
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?;
            let mut filtered = issue_query_valid("issue-20", Validity::All);
            filtered.keys.push((EntityKey::Project, "proj-1".into()));
            filtered.fact_type = Some(FactType::Outcome);
            let only_outcome = store
                .fetch_by_keys(&c.tenant_id, &filtered)
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?;
            // Negative case: the ANDed project key must actually narrow —
            // a non-matching project excludes every row it is ANDed with.
            let mut negative = issue_query_valid("issue-20", Validity::All);
            negative
                .keys
                .push((EntityKey::Project, "proj-other".into()));
            let excluded = store
                .fetch_by_keys(&c.tenant_id, &negative)
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?;
            if both.len() >= 2
                && only_outcome.len() == 1
                && only_outcome[0].fact_type == "outcome"
                && excluded.is_empty()
            {
                Ok(())
            } else {
                Err(format!(
                    "filter wrong: both={} filtered={} excluded_negative={}",
                    both.len(),
                    only_outcome.len(),
                    excluded.len()
                ))
            }
        },
    );

    // --- WS-4 Invariant T ---------------------------------------------------
    suite.record(
        "C21",
        "Invariant T: facts never cross the tenant floor (fetch_by_keys and get_fact)",
        {
            let a = caller!();
            let b = caller!();
            let receipt = svc_noop
                .ingest(
                    &env_gateway(
                        Some("run-1".into()),
                        Some("issue-21".into()),
                        "tenant-isolated fact",
                    ),
                    &a,
                    now,
                )
                .await
                .map_err(|e| format!("ingest failed: {e:?}"))?;
            let leaked_read = store
                .fetch_by_keys(&b.tenant_id, &issue_query("issue-21"))
                .await
                .map_err(|e| format!("fetch failed: {e:?}"))?;
            let leaked_get = store
                .get_fact(&b.tenant_id, receipt.fact_id)
                .await
                .map_err(|e| format!("get_fact failed: {e:?}"))?;
            if leaked_read.iter().any(|r| r.id == receipt.fact_id) {
                Err("fetch_by_keys leaked across tenants".into())
            } else if leaked_get.is_some() {
                Err("get_fact leaked across tenants".into())
            } else {
                Ok(())
            }
        },
    );

    // --- summary -------------------------------------------------------------
    println!(
        "=== conformance: {}/{} checks passed, {} known deviation(s), {} failed ===",
        suite.passed,
        suite.total,
        suite.deviations.len(),
        suite.failed.len()
    );
    if suite.failed.is_empty() {
        println!("CONFORMANCE GREEN");
        if !suite.deviations.is_empty() {
            println!(
                "known deviations (see docs/promotion-thresholds.md): {:?}",
                suite.deviations
            );
        }
    } else {
        eprintln!("CONFORMANCE RED — failing: {:?}", suite.failed);
        std::process::exit(1);
    }
    Ok(())
}

// --- envelope/query helpers ----------------------------------------------------

fn source(kind: SourceKind) -> SourceBlock {
    SourceBlock {
        kind,
        producer: None,
        emitted_at: "2026-10-17T10:00:00Z".into(),
        idempotency_key: None,
    }
}

fn fact(body: &str) -> FactBlock {
    FactBlock {
        fact_type: None,
        body: serde_json::Value::String(body.into()),
        subject: None,
        event_time: None,
        confidence: None,
        supersedes: None,
    }
}

fn hints_run() -> EntityHints {
    EntityHints {
        run: Some("run-1".into()),
        ..EntityHints::default()
    }
}

fn hints_thread() -> EntityHints {
    EntityHints {
        thread: Some("thread-T".into()),
        ..EntityHints::default()
    }
}

/// run-1 anchor + issue/project hints (the §8 keyed roundtrip shape).
fn hints_run_issue(issue: &str) -> EntityHints {
    EntityHints {
        run: Some("run-1".into()),
        issue: Some(issue.into()),
        project: Some("proj-1".into()),
        ..EntityHints::default()
    }
}

fn env_gateway(run: Option<String>, issue: Option<String>, body: &str) -> Envelope {
    Envelope {
        source: source(SourceKind::GatewayFlush),
        entity: EntityHints {
            run,
            issue: issue.clone(),
            project: issue.as_ref().map(|_| "proj-1".into()),
            ..EntityHints::default()
        },
        fact: fact(body),
        attachments: vec![],
        provenance: None,
    }
}

fn query(keys: Vec<(EntityKey, String)>) -> KeyedQuery {
    KeyedQuery {
        keys,
        fact_type: None,
        as_of: None,
        valid: Validity::Current,
        limit: 100,
    }
}

fn issue_query(issue: &str) -> KeyedQuery {
    query(vec![(EntityKey::Issue, issue.into())])
}

fn issue_query_valid(issue: &str, valid: Validity) -> KeyedQuery {
    let mut q = issue_query(issue);
    q.valid = valid;
    q
}
