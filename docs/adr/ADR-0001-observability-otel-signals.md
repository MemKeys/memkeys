# ADR-0001: Observability — OpenTelemetry-native signals

- **Status:** Proposed (pending Architect alignment — Winston)
- **Date:** 2026-10-14
- **Owner:** Observability Agent
- **Workstream:** WS-7 (cross-cutting) — Paperclip issue ISI-5872
- **Relates to:** [`docs/architecture.md`](../architecture.md) (WS-0 baseline — keyed-first & D3 invariants), [`docs/reference-architecture.md`](../reference-architecture.md) (WS-6), design study ISI-5800, roadmap ISI-5803
- **Deciders:** Observability Agent (author), Architect (alignment), WS-1/WS-2/WS-3/WS-4 owners (instrument against this contract)

---

## Context

MemKeys is an attribute-based memory server: facts are keyed by agent-supplied
entity attributes (project / issue / PR / agent / run / time-range), carry temporal
validity, and expose a two-layer retrieval model — **keyed/structured first, semantic
second** (ISI-5800). The Source of Truth (SoT) is the entity-keyed temporal store;
the graph and vector indexes are **derived views (projections)** maintained
asynchronously. This is exactly the architecture baseline's **D3 invariant** —
*source-of-truth + async rebuildable projections* ([`docs/architecture.md`](../architecture.md),
keyed-first / §5.4), with the retention rule *projection ≤ SoT* (§5.6). This CQRS-style
split means a projection necessarily **lags** the SoT; WS-7 makes that lag **bounded and
observable** — the operational, measured form of D3 that this ADR calls the **D3
observable-lag invariant**.

The board asked (ISI-5872) that MemKeys emit spans, metrics, and logs via
OpenTelemetry on the **latest semantic conventions**, vendor-neutrally, instrumented
inside every workstream. WS-7 owns the conventions, the signal catalog, and the
resource/export configuration — it does not own each call site (those live in
WS-1..WS-4), but every workstream instruments **against this contract**.

Constraints that shaped this decision:

1. **Three pillars must correlate** — a single write or retrieval must be traceable
   across traces, metrics, and logs via shared trace/span IDs.
2. **Multi-tenant, zero fact-body leakage** — telemetry must scope by tenant and
   entity *kind* without ever emitting fact bodies, entity-key *values*, raw query
   text, embedding vectors, or credentials. This is a hard safety boundary.
3. **Two distinct semantic-convention domains** — LLM pipeline work (WS-3) is
   `gen_ai.*`; store calls (WS-1/WS-2/SoT) are `db.*`. One of these (`gen_ai.*`) is
   still **experimental** and will churn.
4. **Cardinality is the enemy** — attributes must be bounded enums, not unbounded
   identifiers.
5. **Vendor neutrality** — instrument once (OTLP), export anywhere. Grail/Dynatrace
   is the reference backend, not a coupling.

---

## Decision

### D-1. OTLP export to a collector (never direct-to-backend)

Every MemKeys process exports **OTLP** (gRPC `:4317` primary, HTTP/protobuf `:4318`
fallback) to a **collector** it does not share lifecycle with. The application SDK
holds **no** backend credentials or vendor endpoints — only the collector address.
The collector owns backend routing, PII scrubbing enforcement, tail sampling, and
batching. Rationale: swapping or multi-homing backends (Grail, a local Jaeger/
Prometheus for dev) becomes a collector-config change, never an app redeploy; and the
PII guardrail is enforced a **second time** at an operator-controlled layer.

The reference collector pipeline is pinned in
[`../observability/collector-pipeline.yaml`](../observability/collector-pipeline.yaml).
It honors the ordering invariants: **`memory_limiter` is the first processor, `batch`
is the last**; `resourcedetection` is used for VM/bare-metal profiles and
`k8sattributes` **only** when deployed on Kubernetes.

### D-2. Resource attributes (identity, set once per process)

The SDK `Resource` is set once at startup and attached to every signal:

| Attribute | Value / source | Notes |
|---|---|---|
| `service.name` | `memkeys` | Fixed. |
| `service.version` | build version (semver + git sha) | From build metadata. |
| `service.namespace` | deployment grouping (e.g. `memkeys-prod`) | Optional. |
| `service.instance.id` | stable per-process UUID | For replica disambiguation. |
| `deployment.environment.name` | **deployment profile** (`dev`/`staging`/`prod`) | Current semconv name; `deployment.environment` is deprecated. Drives sampling ratio. |
| `telemetry.sdk.{name,language,version}` | SDK auto | — |
| `process.*`, `host.*`, `os.*` | `resourcedetection` (VM/bare-metal only) | — |
| `k8s.*`, `container.*` | `k8sattributes` (Kubernetes only) | Never both detectors. |

**Tenant is NOT a resource attribute** — a process serves many tenants, so tenant
scoping lives on spans/metrics (D-4), not on the shared Resource.

### D-3. Semantic-convention domains and version pinning

- **Store calls (WS-1/WS-2/SoT)** → OTel **database** semconv (`db.*`).
- **LLM pipeline (WS-3)** → OTel **GenAI** semconv (`gen_ai.*`).
- **General / resource / log** → OTel general + logs semconv.

We **pin to a single semantic-conventions release** and record it as a `schema_url`
on the Resource (and on instrumentation scopes). The pinned version and per-group
stability are tracked in
[`../observability/semconv-pins.yaml`](../observability/semconv-pins.yaml).
Policy:

- The `db.*`, general, and log groups are **Stable** — safe to depend on.
- The `gen_ai.*` group is **Experimental/Development** — it *will* change
  (attribute renames, metric shape). We pin it explicitly, isolate all `gen_ai.*`
  emission behind a single instrumentation helper, and **track upstream** so a semconv
  bump is a one-file, change-managed update, not a scattered refactor.
- Any change to a convention (new attribute, renamed metric, new span) goes through
  change management **first**: update this ADR + the catalog, then the WS owners
  update call sites.

### D-4. Tenant scoping without PII / fact-body leakage (hard boundary)

Telemetry carries an **opaque tenant identifier** and the **entity *kind***, never
the content:

**Allowed on spans/metrics/logs:**

| Attribute | Type | Example | Cardinality |
|---|---|---|---|
| `memkeys.tenant.id` | opaque id (uuid/hash — **not** a name) | `t_9f3a…` | bounded by tenant count |
| `memkeys.entity.kind` | enum | `project`\|`issue`\|`pr`\|`agent`\|`run` | ~5 |
| `memkeys.pipeline.stage` | enum | `normalize`\|`key`\|`temporalize`\|`commit` | 4 |
| `memkeys.retrieval.op` | enum | `keyed`\|`graph`\|`semantic` | 3 |
| `memkeys.projection.kind` | enum | `graph`\|`summary`\|`embedding`\|`category` | 4 |
| `memkeys.worker.kind` | enum | same set as projection.kind | 4 |

**NEVER emitted in any signal (deny-list, enforced in SDK helper *and* collector):**

- Fact bodies / content / summaries
- Entity-key **values** (the actual issue id, PR number, agent name, run id) — only
  `entity.kind` is allowed; the *value* is unbounded and often sensitive
- Raw query text (`db.query.text`), embedding input text, LLM prompt/completion content
- Embedding vectors
- Credentials, tokens, secrets
- Free-form user/agent identifiers

`db.query.text` and `gen_ai` prompt/response content capture are **disabled** by SDK
config. The collector runs a redaction/transform pass as defense-in-depth (see
pipeline config) and a cardinality-limit processor as a firewall.

### D-5. Sampling strategy (head + tail, profile-driven)

- **Head sampling (SDK):** `ParentBased(TraceIDRatioBased(r))` where `r` comes from
  `deployment.environment.name` — `dev` = 1.0, `staging` = 1.0, `prod` = a configured
  ratio (default 0.1). Parent-based so a sampled write keeps its worker/retrieval
  children coherent.
- **Tail sampling (collector):** `tail_sampling` processor keeps, regardless of head
  decision: (a) any trace with an **error** status, (b) traces exceeding a latency
  threshold, (c) **projection-lag anomaly** traces, and (d) always-on for low-volume
  worker trace types so rare workers stay visible.
- **Metrics and logs are never sampled** — they are aggregates/records; only *spans*
  are sampled. Write-path and projection errors are always recorded.

### D-6. Signal shape (summary — full detail in the catalog)

- **Traces:** write path as one trace — SERVER entry `memkeys.fact.ingest` →
  INTERNAL stage spans (`normalize`/`key`/`temporalize`) → CLIENT `commit` (`db.*`).
  Retrieval entry `memkeys.retrieve` (SERVER) → CLIENT store spans (`db.*`) + INTERNAL
  `rrf.fuse`. Async derived-view workers run as **CONSUMER** spans in their own trace,
  joined to the triggering write via a **span link** (not a parent edge — they are
  temporally decoupled). LLM spans use `gen_ai.*`.
- **Metrics:** write throughput/latency, retrieval latency + candidate-set size per
  op, RRF fusion inputs, **`memkeys.projection.lag`** (the D3 invariant, with an SLO),
  graph traverse latency + per-tenant edge cardinality (the ADR-GRAPH §10 promotion
  gate; `graph.traverse.warm` SLO — p95 ≤ 150 ms at depth ≤ 3),
  LLM token/cost (`gen_ai.client.token.usage` + a cost counter), worker queue depth.
- **Logs:** structured, trace-correlated (TraceId/SpanId on every record), bounded,
  PII-free. Async workers carry the originating trace context on the work item so
  their logs correlate back.

The authoritative, machine-readable definition is
[`../observability/signal-catalog.yaml`](../observability/signal-catalog.yaml);
the human-readable version is
[`../observability/signal-catalog.md`](../observability/signal-catalog.md).

---

## Consequences

**Positive**

- One instrumentation contract; WS-1..WS-4 implement against it with no bespoke
  conventions. Three pillars correlate by construction.
- The D3 observable-lag invariant becomes a **real, alertable SLO** via
  `memkeys.projection.lag`, not just a design aspiration.
- Backend-swappable (OTLP → collector). Dev can run Jaeger+Prometheus; prod routes to
  Grail/Dynatrace — same app binary.
- PII boundary enforced twice (SDK deny-list + collector redaction) and bounded by a
  cardinality firewall.

**Negative / costs**

- `gen_ai.*` churn is a standing maintenance task (mitigated by the single-helper
  isolation + semconv-pin tracking).
- A collector is now required infrastructure (dev gets a lightweight profile).
- Head sampling at 0.1 in prod means per-trace debugging relies on tail-sampling
  catching the interesting traces; the tail policies above are therefore
  load-bearing and must be reviewed whenever new error/lag modes appear.

**Follow-ups / handoffs**

- Architect (Winston): ADR alignment sign-off.
- WS-1/WS-2/SoT: implement `db.*` store spans + write-pipeline stage spans +
  `memkeys.projection.lag` emission at projection-apply.
- WS-3: implement `gen_ai.*` spans/metrics behind the shared helper.
- WS-4 (retrieval): implement `memkeys.retrieve` spans + candidate-set/RRF metrics.
- Testing Architect: validation requirements derive from the catalog (see handoff).
