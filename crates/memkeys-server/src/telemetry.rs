//! OTel instrumentation for the M1 write path + keyed read, and the M2 graph
//! projection (worker + reads).
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
/// `memkeys.retrieval.op` value for graph reads (signal catalog enum).
pub const RETRIEVAL_OP_GRAPH: &str = "graph";
/// `memkeys.retrieval.op` value for op-3 semantic search (M5, §7.4).
pub const RETRIEVAL_OP_SEMANTIC: &str = "semantic";
/// `memkeys.worker.kind` value for the stage-5a graph-projection worker.
pub const WORKER_KIND_GRAPH_PROJECTION: &str = "graph_projection";
/// `memkeys.worker.kind` value for the stage-5b vector-projection worker (M5).
pub const WORKER_KIND_VECTOR_PROJECTION: &str = "vector_projection";
/// `memkeys.projection.kind` value for the graph projection.
pub const PROJECTION_KIND_GRAPH: &str = "graph";
/// `memkeys.projection.kind` value for the fact-vector projection (M5 stage 5b).
pub const PROJECTION_KIND_VECTOR: &str = "vector";
/// `memkeys.worker.kind` values for the M6 LLM pipeline workers (ADR-llm-pipeline
/// Decision 2: W-extract = stage 5e, W-digest = 5c, W-categorize = 5d).
pub const WORKER_KIND_LLM_EXTRACT: &str = "llm_extract";
pub const WORKER_KIND_DIGEST_ROLLUP: &str = "digest_rollup";
pub const WORKER_KIND_CATEGORIZE: &str = "categorize";
/// `memkeys.projection.kind` values for the M6 derived views.
pub const PROJECTION_KIND_DERIVED_KEYS: &str = "derived_keys";
pub const PROJECTION_KIND_DIGEST: &str = "digest";
pub const PROJECTION_KIND_CATEGORY_FACETS: &str = "category_facets";
pub const OUTCOME_SUCCESS: &str = "success";
pub const OUTCOME_ERROR: &str = "error";
pub const OUTCOME_REJECTED: &str = "rejected";
pub const OUTCOME_DUPLICATE: &str = "duplicate";

/// M3 protocol-surface attribute values (`memkeys.transport`). stdio = a
/// launch-bound single-principal subprocess; http = the per-request-auth
/// Streamable-HTTP shared server (WS-5 §2).
pub const TRANSPORT_STDIO: &str = "stdio";
pub const TRANSPORT_HTTP: &str = "http";

/// The protocol surface a service call arrived on — the transport and, for
/// MCP, the tool name. Stamped onto the `memkeys.fact.ingest` / `memkeys.retrieve`
/// SERVER spans as `memkeys.transport` + `memkeys.mcp.tool` (WS-5 §7 / ISI-6137
/// B1+B2). A direct library/test call has no protocol surface ([`INTERNAL`]) and
/// emits neither attribute — the transport registry keys carry only real
/// protocol traffic. Tool *names* only; never argument values (the deny-list).
///
/// [`INTERNAL`]: CallScope::INTERNAL
#[derive(Debug, Clone, Copy)]
pub struct CallScope {
    pub transport: Option<&'static str>,
    pub tool: Option<&'static str>,
}

impl CallScope {
    /// A direct library/test call: no protocol surface, no transport attrs.
    pub const INTERNAL: CallScope = CallScope {
        transport: None,
        tool: None,
    };

    /// An HTTP `/memory/*` request (no MCP tool name on this transport).
    pub const fn http() -> CallScope {
        CallScope {
            transport: Some(TRANSPORT_HTTP),
            tool: None,
        }
    }

    /// An MCP `tools/call` over stdio, carrying the ratified tool name.
    pub const fn stdio(tool: &'static str) -> CallScope {
        CallScope {
            transport: Some(TRANSPORT_STDIO),
            tool: Some(tool),
        }
    }

    /// The span attributes this scope contributes (empty for [`INTERNAL`]).
    ///
    /// [`INTERNAL`]: CallScope::INTERNAL
    pub fn span_attrs(&self) -> Vec<KeyValue> {
        let mut v = Vec::new();
        if let Some(t) = self.transport {
            v.push(attr("memkeys.transport", t));
        }
        if let Some(tool) = self.tool {
            v.push(attr("memkeys.mcp.tool", tool));
        }
        v
    }
}

/// The complete attribute-key registry every signal may carry (signal-catalog
/// "Custom attribute registry" + the pinned stable db.* group). Anything not
/// listed is a catalog violation and is refused in debug builds.
const ALLOWED_ATTR_KEYS: &[&str] = &[
    "memkeys.tenant.id",
    "memkeys.entity.kind",
    "memkeys.pipeline.stage",
    "memkeys.retrieval.op",
    "memkeys.worker.kind",
    "memkeys.projection.kind",
    "memkeys.graph.depth",
    "memkeys.graph.truncate.reason",
    // M3 protocol surface (WS-5 §2/§7): the transport the request arrived on
    // and the MCP tool name. Kinds/names only — never argument VALUES (the
    // deny-list still holds: no bodies, no entity-key values, no query text).
    "memkeys.transport",
    "memkeys.mcp.tool",
    "outcome",
    // Pinned stable db.* group (semconv-pins.yaml; db.query.text is DISABLED).
    "db.system.name",
    "db.operation.name",
    "db.collection.name",
    // M6 (WS-3, ADR-llm-pipeline): the gen_ai.* EXPERIMENTAL semconv group,
    // pinned + isolated in the single emission helper `gen_attr` (signal
    // catalog: "behind one helper"). Values are model/operation/token KINDS
    // and COUNTS only — the forbidden list still holds: no prompt, no
    // completion, never fact bodies or key values.
    "gen_ai.operation.name",
    "gen_ai.system",
    "gen_ai.request.model",
    "gen_ai.response.model",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.token.type",
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

/// The gen_ai.* emission helper (M6, WS-3). The experimental semconv group is
/// deliberately isolated in its own helper (same registry validation; the
/// catalog forbids prompt/completion CONTENT — kinds, model ids, and token
/// COUNTS only).
pub fn gen_attr(key: &'static str, value: impl Into<Value>) -> KeyValue {
    attr(key, value)
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

/// Start a span with a RUNTIME-composed name (M6: the gen_ai semconv span-name
/// convention is "{gen_ai.operation.name} {gen_ai.request.model}", where the
/// model id is configuration). Same registry validation as [`start_span`].
pub fn start_dynamic_span(name: String, kind: SpanKind, attrs: Vec<KeyValue>) -> global::BoxedSpan {
    tracer()
        .span_builder(name)
        .with_kind(kind)
        .with_attributes(attrs)
        .start(&tracer())
}

/// Start a span LINKED to another span context (WS-7: the derived-view worker
/// runs as its own trace root with a SPAN_LINK to the originating commit —
/// temporally decoupled, not a parent edge).
pub fn start_span_with_links(
    name: &'static str,
    kind: SpanKind,
    attrs: Vec<KeyValue>,
    link_to: Option<&opentelemetry::trace::SpanContext>,
) -> global::BoxedSpan {
    let mut builder = tracer()
        .span_builder(name)
        .with_kind(kind)
        .with_attributes(attrs);
    if let Some(ctx) = link_to {
        builder = builder.with_links(vec![opentelemetry::trace::Link::new(
            ctx.clone(),
            Vec::new(),
            0,
        )]);
    }
    builder.start(&tracer())
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

/// Transport-boundary SERVER span (M3, WS-5 §7.6). This is the ingress span for
/// a single protocol call; it carries the protocol surface (`memkeys.transport`)
/// and, for an MCP `tools/call`, the tool name (`memkeys.mcp.tool`). Emitting
/// here is what makes those two registry keys *live* — before M3 they were
/// registered in [`ALLOWED_ATTR_KEYS`] but never emitted (ISI-6134 B1/B2).
///
/// The service's own `memkeys.fact.ingest` / `memkeys.retrieve` spans nest
/// beneath this one when the returned span's context is attached around the
/// service call (OTel parents a new span to the current context on its first,
/// synchronous poll). Attribute VALUES are kinds/names only — never argument
/// bodies (the deny-list still holds).
pub fn transport_span(
    span_name: &'static str,
    transport: &'static str,
    mcp_tool: Option<&str>,
) -> global::BoxedSpan {
    let mut attrs = vec![attr("memkeys.transport", transport)];
    if let Some(tool) = mcp_tool {
        attrs.push(attr("memkeys.mcp.tool", tool.to_string()));
    }
    start_span(span_name, SpanKind::Server, attrs)
}

/// The M1 instrument set, built once per process from the global meter.
#[derive(Debug, Clone)]
pub struct Instruments {
    write_operations: opentelemetry::metrics::Counter<u64>,
    write_duration: opentelemetry::metrics::Histogram<f64>,
    retrieve_duration: opentelemetry::metrics::Histogram<f64>,
    retrieve_candidate_set_size: opentelemetry::metrics::Histogram<u64>,
    projection_lag: opentelemetry::metrics::Histogram<f64>,
    projection_lag_current: opentelemetry::metrics::Gauge<f64>,
    graph_traverse_latency: opentelemetry::metrics::Histogram<f64>,
    graph_edges_cardinality: opentelemetry::metrics::Gauge<u64>,
    worker_queue_depth_gauge: opentelemetry::metrics::Gauge<i64>,
    poison_queue_depth_gauge: opentelemetry::metrics::Gauge<i64>,
    llm_token_usage: opentelemetry::metrics::Histogram<f64>,
    llm_operation_duration: opentelemetry::metrics::Histogram<f64>,
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
            projection_lag: meter
                .f64_histogram("memkeys.projection.lag")
                .with_unit("s")
                .with_description(
                    "Measured lag (apply_time - commit_time) when a projection \
                     applies a committed fact (the D3 bounded-lag invariant).",
                )
                .build(),
            projection_lag_current: meter
                .f64_gauge("memkeys.projection.lag.current")
                .with_unit("s")
                .with_description(
                    "Current max outstanding lag per projection — age of the oldest \
                     un-applied/stale committed fact for a tier. Observed by the M7 \
                     reconciler at maintenance time (same async-observed pattern as \
                     graph.edges.cardinality); 0 when the tier is converged.",
                )
                .build(),
            graph_traverse_latency: meter
                .f64_histogram("memkeys.graph.traverse.latency")
                .with_unit("s")
                .with_description(
                    "Latency of one graph-projection BFS traversal, bucketed by \
                     the post-clamp depth (warm-path SLO: p95 <= 150ms at depth <= 3; \
                     promotion triggers #1/#2, ADR-GRAPH rev-3 F11 / ISI-5911).",
                )
                .build(),
            graph_edges_cardinality: meter
                .u64_gauge("memkeys.graph.edges.cardinality")
                .with_unit("{edge}")
                .with_description(
                    "Current edge count of the graph projection per tenant \
                     (async-observed at maintenance time; promotion trigger #3).",
                )
                .build(),
            worker_queue_depth_gauge: meter
                .i64_gauge("memkeys.worker.queue.depth")
                .with_unit("{item}")
                .with_description("Pending work items per derived-view worker.")
                .build(),
            poison_queue_depth_gauge: meter
                .i64_gauge("memkeys.poison.queue.depth")
                .with_unit("{item}")
                .with_description(
                    "Live poison-queue (dead-letter) items per tier — derivations \
                     that exhausted retries (WS-3/ADR-llm Decision 8; companion to \
                     poison.oldest_age).",
                )
                .build(),
            llm_token_usage: meter
                .f64_histogram("gen_ai.client.token.usage")
                .with_unit("{token}")
                .with_description(
                    "Token usage per LLM call (standard GenAI metric, WS-3 signal \
                     catalog; input and output recorded as separate points).",
                )
                .build(),
            llm_operation_duration: meter
                .f64_histogram("gen_ai.client.operation.duration")
                .with_unit("s")
                .with_description("LLM call duration (standard GenAI metric, WS-3 signal catalog).")
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

    /// `memkeys.projection.lag` — recorded when a projection applies a
    /// committed fact (worker call site).
    pub fn projection_lag(&self, projection_kind: &'static str, seconds: f64) {
        self.projection_lag
            .record(seconds, &[attr("memkeys.projection.kind", projection_kind)]);
    }

    /// `memkeys.projection.lag.current` — the age (seconds) of the oldest
    /// outstanding (un-applied or stale) committed fact for a projection tier,
    /// recorded by the M7 reconciler at maintenance time. Realizes the reserved
    /// signal-catalog gauge; 0.0 means the tier is converged.
    pub fn projection_lag_current(&self, projection_kind: &'static str, seconds: f64) {
        self.projection_lag_current
            .record(seconds, &[attr("memkeys.projection.kind", projection_kind)]);
    }

    /// `memkeys.graph.traverse.latency` — recorded at the GraphView traverse
    /// call site, bucketed by the post-clamp depth (ISI-5911).
    pub fn graph_traverse_latency(&self, depth: u8, seconds: f64) {
        self.graph_traverse_latency
            .record(seconds, &[attr("memkeys.graph.depth", depth as i64)]);
    }

    /// `memkeys.graph.edges.cardinality` — per-tenant standing edge count,
    /// observed at maintenance time, never per-request.
    pub fn graph_edges_cardinality(&self, tenant: &str, edges: u64) {
        self.graph_edges_cardinality
            .record(edges, &[attr("memkeys.tenant.id", tenant.to_string())]);
    }

    /// `memkeys.worker.queue.depth` — live pending items (absolute depth).
    pub fn worker_queue_depth(&self, worker_kind: &'static str, depth: usize) {
        self.worker_queue_depth_gauge
            .record(depth as i64, &[attr("memkeys.worker.kind", worker_kind)]);
    }

    /// `memkeys.poison.queue.depth` — live dead-letter count (absolute), per
    /// derived-view worker kind (graph_projection / vector_projection).
    pub fn poison_queue_depth(&self, worker_kind: &'static str, count: i64) {
        self.poison_queue_depth_gauge
            .record(count, &[attr("memkeys.worker.kind", worker_kind)]);
    }

    /// `gen_ai.client.token.usage` (WS-3) — input and output recorded as two
    /// points (the standard GenAI metric carries `gen_ai.token.type`).
    /// Counts are byte/4 estimates until adapters surface real usage.
    pub fn llm_token_usage(
        &self,
        model: &str,
        system: &str,
        input_tokens: i64,
        output_tokens: i64,
    ) {
        self.llm_token_usage.record(
            input_tokens as f64,
            &[
                gen_attr("gen_ai.token.type", "input"),
                gen_attr("gen_ai.operation.name", "chat"),
                gen_attr("gen_ai.request.model", model.to_string()),
                gen_attr("gen_ai.system", system.to_string()),
            ],
        );
        self.llm_token_usage.record(
            output_tokens as f64,
            &[
                gen_attr("gen_ai.token.type", "output"),
                gen_attr("gen_ai.operation.name", "chat"),
                gen_attr("gen_ai.request.model", model.to_string()),
                gen_attr("gen_ai.system", system.to_string()),
            ],
        );
    }

    /// `gen_ai.client.operation.duration` (WS-3).
    pub fn llm_operation_duration(&self, model: &str, system: &str, seconds: f64) {
        self.llm_operation_duration.record(
            seconds,
            &[
                gen_attr("gen_ai.operation.name", "chat"),
                gen_attr("gen_ai.request.model", model.to_string()),
                gen_attr("gen_ai.system", system.to_string()),
            ],
        );
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

/// db.* attributes for graph-projection store spans (`edges` collection).
pub fn db_attrs_edges(operation: &'static str) -> Vec<KeyValue> {
    vec![
        attr("db.system.name", "postgresql"),
        attr("db.operation.name", operation),
        attr("db.collection.name", "edges"),
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
        // M2 graph additions (WS-7 catalog enums; ISI-5911 promotion signals).
        assert!(ALLOWED_ATTR_KEYS.contains(&"memkeys.graph.depth"));
        assert!(ALLOWED_ATTR_KEYS.contains(&"memkeys.worker.kind"));
        // M3 protocol surface — now emitted via transport_span (ISI-6134 B1/B2).
        assert!(ALLOWED_ATTR_KEYS.contains(&"memkeys.transport"));
        assert!(ALLOWED_ATTR_KEYS.contains(&"memkeys.mcp.tool"));
    }

    #[test]
    fn transport_span_attrs_are_registry_valid() {
        // Would panic in debug (attr()'s debug_assert) if either key were not
        // registered — the regression guard for "registered but never emitted".
        let _http = transport_span("memkeys.http.request", TRANSPORT_HTTP, None);
        let _mcp = transport_span(
            "memkeys.mcp.tool_call",
            TRANSPORT_STDIO,
            Some("memory_store"),
        );
    }
}
