//! OTel instrumentation for the M1 write path and keyed read.
//!
//! Contract sources (WS-7, owner Observability Agent):
//! - docs/observability/signal-catalog.yaml — spans, metrics, attribute registry
//! - docs/observability/semconv-pins.yaml   — pinned release 1.30.0; db.* stable
//!
//! Spans (one trace per write): `memkeys.fact.ingest` (SERVER) →
//! `memkeys.normalize` / `memkeys.key` / `memkeys.temporalize` (INTERNAL) →
//! `memkeys.commit` (CLIENT, db.* attributes). Retrieval: `memkeys.retrieve`
//! (SERVER) → `memkeys.store.query` (CLIENT, db.*).
//!
//! Metrics: `memkeys.write.operations` (counter, {operation}, attrs
//! entity.kind + outcome), `memkeys.write.duration` (histogram, s, attrs
//! pipeline.stage + outcome), `memkeys.retrieve.duration` (histogram, s,
//! attrs retrieval.op + outcome), `memkeys.retrieve.candidate_set.size`
//! (histogram, {candidate}, attrs retrieval.op).
//!
//! Deny-list (hard boundary): fact bodies, entity-key VALUES (only the kind),
//! raw query text (`db.query.text` is disabled), credentials. All emission
//! goes through [`attr`], which validates keys against the registry below.

use opentelemetry::global;
use opentelemetry::trace::{SpanKind, Tracer};
use opentelemetry::{KeyValue, Value};

/// Pinned semconv release (semconv-pins.yaml).
pub const SCHEMA_URL: &str = "https://opentelemetry.io/schemas/1.30.0";

/// `memkeys.pipeline.stage` values (signal-catalog custom attribute registry).
pub const PIPELINE_STAGE_NORMALIZE: &str = "normalize";
pub const PIPELINE_STAGE_KEY: &str = "key";
pub const PIPELINE_STAGE_TEMPORALIZE: &str = "temporalize";
pub const PIPELINE_STAGE_COMMIT: &str = "commit";
pub const RETRIEVAL_OP_KEYED: &str = "keyed";
pub const OUTCOME_SUCCESS: &str = "success";
pub const OUTCOME_ERROR: &str = "error";
pub const OUTCOME_REJECTED: &str = "rejected";
pub const OUTCOME_DUPLICATE: &str = "duplicate";

/// The complete attribute-key registry every signal may carry (signal-catalog
/// "Custom attribute registry" + the pinned stable db.* group). Anything not
/// listed is a catalog violation and is refused in debug builds.
const ALLOWED_ATTR_KEYS: &[&str] = &[
    "memkeys.tenant.id",
    "memkeys.entity.kind",
    "memkeys.pipeline.stage",
    "memkeys.retrieval.op",
    "outcome",
    // Pinned stable db.* group (semconv-pins.yaml; db.query.text is DISABLED).
    "db.system.name",
    "db.operation.name",
    "db.collection.name",
];

/// The single emission helper (signal-catalog: "enforced by the single SDK
/// emission helper (validates keys against this registry)"). Values are
/// opaque ids/kinds only — never fact bodies or entity-key values.
pub fn attr(key: &'static str, value: impl Into<Value>) -> KeyValue {
    debug_assert!(
        ALLOWED_ATTR_KEYS.contains(&key),
        "attribute key {key} is not in the MemKeys signal registry"
    );
    KeyValue::new(key, value)
}

fn tracer() -> global::BoxedTracer {
    global::tracer("memkeys")
}

/// Start a named span with registry-validated attributes.
pub fn start_span(name: &'static str, kind: SpanKind, attrs: Vec<KeyValue>) -> global::BoxedSpan {
    tracer()
        .span_builder(name)
        .with_kind(kind)
        .with_attributes(attrs)
        .start(&tracer())
}

/// Start a child span under an explicit parent context. The write path uses
/// this (not thread-local attach) so spans parent correctly across `.await`
/// boundaries without making the pipeline future `!Send`.
pub fn start_child_span(
    name: &'static str,
    kind: SpanKind,
    attrs: Vec<KeyValue>,
    parent: &opentelemetry::Context,
) -> global::BoxedSpan {
    tracer()
        .span_builder(name)
        .with_kind(kind)
        .with_attributes(attrs)
        .start_with_context(&tracer(), parent)
}

/// The M1 instrument set, built once per process from the global meter.
#[derive(Debug, Clone)]
pub struct Instruments {
    write_operations: opentelemetry::metrics::Counter<u64>,
    write_duration: opentelemetry::metrics::Histogram<f64>,
    retrieve_duration: opentelemetry::metrics::Histogram<f64>,
    retrieve_candidate_set_size: opentelemetry::metrics::Histogram<u64>,
}

impl Instruments {
    pub fn new() -> Self {
        let meter = global::meter("memkeys");
        Instruments {
            write_operations: meter
                .u64_counter("memkeys.write.operations")
                .with_unit("{operation}")
                .with_description("Write throughput (count of ingest operations).")
                .build(),
            write_duration: meter
                .f64_histogram("memkeys.write.duration")
                .with_unit("s")
                .with_description("Write latency, broken down per pipeline stage.")
                .build(),
            retrieve_duration: meter
                .f64_histogram("memkeys.retrieve.duration")
                .with_unit("s")
                .with_description("Retrieval latency per op (keyed/graph/semantic).")
                .build(),
            retrieve_candidate_set_size: meter
                .u64_histogram("memkeys.retrieve.candidate_set.size")
                .with_unit("{candidate}")
                .with_description("Candidate-set size per retrieval op.")
                .build(),
        }
    }

    pub fn write_operation(&self, entity_kind: &'static str, outcome: &'static str) {
        self.write_operations.add(
            1,
            &[
                attr("memkeys.entity.kind", entity_kind),
                attr("outcome", outcome),
            ],
        );
    }

    pub fn write_stage_duration(&self, stage: &'static str, outcome: &'static str, seconds: f64) {
        self.write_duration.record(
            seconds,
            &[
                attr("memkeys.pipeline.stage", stage),
                attr("outcome", outcome),
            ],
        );
    }

    pub fn retrieve_duration(&self, op: &'static str, outcome: &'static str, seconds: f64) {
        self.retrieve_duration.record(
            seconds,
            &[attr("memkeys.retrieval.op", op), attr("outcome", outcome)],
        );
    }

    pub fn candidate_set_size(&self, op: &'static str, size: u64) {
        self.retrieve_candidate_set_size
            .record(size, &[attr("memkeys.retrieval.op", op)]);
    }
}

impl Default for Instruments {
    fn default() -> Self {
        Self::new()
    }
}

/// db.* attributes for the commit / store-query spans (pinned stable group).
/// `db.query.text` is deliberately never set — it carries fact content.
pub fn db_attrs(operation: &'static str) -> Vec<KeyValue> {
    vec![
        attr("db.system.name", "postgresql"),
        attr("db.operation.name", operation),
        attr("db.collection.name", "facts"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_no_content_bearing_keys() {
        // db.query.text is disabled by the semconv pins (carries fact content).
        assert!(!ALLOWED_ATTR_KEYS.contains(&"db.query.text"));
        assert!(ALLOWED_ATTR_KEYS.contains(&"memkeys.tenant.id"));
        assert!(ALLOWED_ATTR_KEYS.contains(&"db.system.name"));
    }
}
