//! M4 conformance tests — the core emits spans/metrics/logs per the WS-7
//! signal catalog (docs/observability/signal-catalog.yaml).
//!
//! Verified against in-memory SDK exporters, so the full contract — trace
//! shape, attributes, the db.* pinned group, the deny-list, metric
//! instruments, and three-pillar log correlation — is asserted without a
//! collector or Postgres. The local collector/Jaeger walkthrough is
//! docs/observability/local-check.md; these tests are its CI-grade proof.

// The tests serialize on one mutex because they swap GLOBAL OTel providers;
// holding that lock across the awaited service calls is exactly the point.
#![allow(clippy::await_holding_lock)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{TimeZone, Utc};
use memkeys_server::envelope::Envelope;
use memkeys_server::memstore::MemStore;
use memkeys_server::model::{EntityKey, KeyedQuery};
use memkeys_server::pipeline::{CallerContext, NoopResolver};
use memkeys_server::service::MemKeysService;

use opentelemetry::global;
use opentelemetry::trace::{SpanKind, Status};
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::InMemorySpanExporterBuilder;
use opentelemetry_sdk::trace::SpanData;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

/// The tests swap global providers, so they serialize on one lock (the rest
/// of the suite does not touch OTel globals).
fn otel_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// The service under test: the real pipeline over the in-memory FactStore
/// (`memstore.rs`) — full write path + keyed read, no Postgres needed.
fn service() -> MemKeysService {
    MemKeysService::with_resolver(Arc::new(MemStore::new()), Arc::new(NoopResolver))
}

fn caller(tenant: &str) -> CallerContext {
    CallerContext {
        tenant_id: tenant.into(),
        principal: "agent-a".into(),
        trace_id: Some("trace-1".into()),
    }
}

fn gw_flush(body: &str) -> Envelope {
    serde_json::from_value(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "project": "proj-1" },
        "fact": { "body": body }
    }))
    .expect("valid envelope")
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 17, 10, 0, 2).unwrap()
}

fn tracer_provider_with(exporter: InMemorySpanExporter) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(exporter)
        .build()
}

fn span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("span {name} not found in {:?}", spans_names(spans)))
}

fn spans_names(spans: &[SpanData]) -> Vec<&str> {
    spans.iter().map(|s| s.name.as_ref()).collect()
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn value_str(v: &opentelemetry::Value) -> String {
    v.as_str().into_owned()
}

#[tokio::test]
async fn write_path_emits_the_catalog_trace() {
    let _lock = otel_lock().lock().unwrap();

    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = tracer_provider_with(exporter.clone());
    global::set_tracer_provider(provider.clone());

    let service = service();
    let receipt = service
        .ingest(
            &gw_flush("chose the append-only store"),
            &caller("t-1"),
            now(),
        )
        .await
        .expect("ingest");
    assert!(receipt.keyed);

    let spans = exporter.get_finished_spans().expect("spans");
    assert_eq!(
        spans_names(&spans),
        vec![
            "memkeys.normalize",
            "memkeys.key",
            "memkeys.temporalize",
            "memkeys.commit",
            "memkeys.fact.ingest",
        ],
        "write-path span set + order must match the signal catalog"
    );

    let ingest = span(&spans, "memkeys.fact.ingest");
    assert_eq!(ingest.span_kind, SpanKind::Server);
    assert_eq!(value_str(attr(ingest, "memkeys.tenant.id").unwrap()), "t-1");
    assert_eq!(
        value_str(attr(ingest, "memkeys.entity.kind").unwrap()),
        "run"
    );
    assert_eq!(ingest.parent_span_id, opentelemetry::trace::SpanId::INVALID);

    // One trace: every stage is a child of the ingest span.
    let trace_id = ingest.span_context.trace_id();
    for (name, stage) in [
        ("memkeys.normalize", "normalize"),
        ("memkeys.key", "key"),
        ("memkeys.temporalize", "temporalize"),
    ] {
        let s = span(&spans, name);
        assert_eq!(s.span_kind, SpanKind::Internal);
        assert_eq!(s.span_context.trace_id(), trace_id);
        assert_eq!(s.parent_span_id, ingest.span_context.span_id());
        assert_eq!(value_str(attr(s, "memkeys.pipeline.stage").unwrap()), stage);
    }

    // signal-catalog `memkeys.key`: entity kind, never entity-key values.
    let key = span(&spans, "memkeys.key");
    assert_eq!(
        value_str(attr(key, "memkeys.entity.kind").unwrap()),
        "run",
        "memkeys.key must carry memkeys.entity.kind per the signal catalog"
    );

    // memkeys.commit: CLIENT, pinned stable db.* group, db.query.text denied.
    let commit = span(&spans, "memkeys.commit");
    assert_eq!(commit.span_kind, SpanKind::Client);
    assert_eq!(commit.span_context.trace_id(), trace_id);
    assert_eq!(commit.parent_span_id, ingest.span_context.span_id());
    assert_eq!(
        value_str(attr(commit, "db.system.name").unwrap()),
        "postgresql"
    );
    assert_eq!(
        value_str(attr(commit, "db.operation.name").unwrap()),
        "insert"
    );
    assert_eq!(
        value_str(attr(commit, "db.collection.name").unwrap()),
        "facts"
    );

    // The deny-list, enforced: no span carries content-bearing keys.
    for s in &spans {
        assert!(
            attr(s, "db.query.text").is_none(),
            "{} leaked db.query.text",
            s.name
        );
        assert_eq!(s.status, Status::Ok, "{} should be Ok", s.name);
    }

    provider.shutdown().unwrap();
}

#[tokio::test]
async fn keyed_read_emits_the_catalog_trace() {
    let _lock = otel_lock().lock().unwrap();

    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = tracer_provider_with(exporter.clone());
    global::set_tracer_provider(provider.clone());

    let service = service();
    // A committed fact to read back — the keyed read is a real roundtrip
    // through the same service (ingest first, then fetch).
    service
        .ingest(&gw_flush("a decision to read back"), &caller("t-1"), now())
        .await
        .expect("ingest");

    let rows = service
        .fetch_by_keys(
            "t-1",
            &KeyedQuery {
                keys: vec![(EntityKey::Project, "proj-1".into())],
                fact_type: None,
                as_of: None,
                valid: Default::default(),
                limit: KeyedQuery::DEFAULT_LIMIT,
            },
        )
        .await
        .expect("keyed read");
    assert_eq!(rows.len(), 1);

    let spans = exporter.get_finished_spans().expect("spans");
    let names = spans_names(&spans);
    assert!(
        names.ends_with(&["memkeys.store.query", "memkeys.retrieve"]),
        "keyed-read span set must end with the catalog pair, got {names:?}"
    );
    let spans = &spans[spans.len() - 2..];

    let retrieve = span(spans, "memkeys.retrieve");
    assert_eq!(retrieve.span_kind, SpanKind::Server);
    assert_eq!(
        value_str(attr(retrieve, "memkeys.retrieval.op").unwrap()),
        "keyed"
    );
    assert_eq!(
        value_str(attr(retrieve, "memkeys.tenant.id").unwrap()),
        "t-1"
    );

    let store = span(spans, "memkeys.store.query");
    assert_eq!(store.span_kind, SpanKind::Client);
    assert_eq!(
        store.span_context.trace_id(),
        retrieve.span_context.trace_id()
    );
    assert_eq!(store.parent_span_id, retrieve.span_context.span_id());
    assert_eq!(
        value_str(attr(store, "db.operation.name").unwrap()),
        "select"
    );
    assert!(attr(store, "db.query.text").is_none());

    provider.shutdown().unwrap();
}

#[tokio::test]
async fn catalog_metrics_are_emitted_with_registry_attributes() {
    let _lock = otel_lock().lock().unwrap();

    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter.clone()).build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    global::set_meter_provider(provider.clone());

    let service = service();
    service
        .ingest(&gw_flush("a decision"), &caller("t-1"), now())
        .await
        .expect("ingest");
    service
        .fetch_by_keys(
            "t-1",
            &KeyedQuery {
                keys: vec![(EntityKey::Project, "proj-1".into())],
                fact_type: None,
                as_of: None,
                valid: Default::default(),
                limit: KeyedQuery::DEFAULT_LIMIT,
            },
        )
        .await
        .expect("keyed read");

    provider.force_flush().unwrap();
    let metrics = exporter.get_finished_metrics().expect("metrics");
    assert!(!metrics.is_empty(), "no metrics collected");
    let rm = &metrics[0];

    let mut names = HashSet::new();
    for sm in &rm.scope_metrics {
        for m in &sm.metrics {
            names.insert(m.name.to_string());
        }
    }
    for expected in [
        "memkeys.write.operations",
        "memkeys.write.duration",
        "memkeys.retrieve.duration",
        "memkeys.retrieve.candidate_set.size",
    ] {
        assert!(
            names.contains(expected),
            "metric {expected} missing; got {names:?}"
        );
    }

    // Units follow the catalog ({operation}, s, {candidate}).
    let units: Vec<(String, String)> = rm
        .scope_metrics
        .iter()
        .flat_map(|sm| sm.metrics.iter())
        .map(|m| (m.name.to_string(), m.unit.to_string()))
        .collect();
    assert!(units.contains(&("memkeys.write.operations".into(), "{operation}".into())));
    assert!(units.contains(&("memkeys.write.duration".into(), "s".into())));
    assert!(units.contains(&(
        "memkeys.retrieve.candidate_set.size".into(),
        "{candidate}".into()
    )));

    provider.shutdown().unwrap();
}

#[tokio::test]
async fn rejected_write_emits_correlated_log_event() {
    let _lock = otel_lock().lock().unwrap();

    // Traces: in-memory span exporter, so the log record can be correlated to
    // a real ingest trace id.
    let span_exporter = InMemorySpanExporterBuilder::new().build();
    let tracer_provider = tracer_provider_with(span_exporter.clone());
    global::set_tracer_provider(tracer_provider.clone());

    // Logs: in-memory log exporter behind the tracing bridge (the M4 log
    // channel — the same transport prod uses).
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();

    let bridge =
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logger_provider);
    use tracing_subscriber::layer::SubscriberExt;
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(bridge));

    let service = service();

    // An envelope with an empty fact body is rejected in stage 1.
    let rejected = serde_json::from_value::<Envelope>(serde_json::json!({
        "source": { "kind": "gateway_flush", "emitted_at": "2026-10-17T10:00:00Z" },
        "entity": { "project": "proj-1" },
        "fact": { "body": "" }
    }))
    .expect("parsable envelope");
    let result = service.ingest(&rejected, &caller("t-1"), now()).await;
    assert!(matches!(
        result,
        Err(memkeys_server::error::IngestError::Rejected { .. })
    ));

    let spans = span_exporter.get_finished_spans().expect("spans");
    let ingest = span(&spans, "memkeys.fact.ingest");

    logger_provider.force_flush().unwrap();
    let logs = log_exporter.get_emitted_logs().expect("logs");
    let event = logs
        .iter()
        .find(|l| {
            matches!(
                l.record.body(),
                Some(opentelemetry::logs::AnyValue::String(s))
                    if s.as_ref().contains("memkeys.write.rejected")
            )
        })
        .expect("memkeys.write.rejected log event emitted");
    // Three-pillar correlation: the log carries the ingest trace id.
    let tc = event.record.trace_context().expect("log trace context");
    assert_eq!(tc.trace_id, ingest.span_context.trace_id());

    logger_provider.shutdown().unwrap();
    tracer_provider.shutdown().unwrap();
}
