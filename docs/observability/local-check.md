# MemKeys OTel local check (M4)

A five-minute, docker-only walkthrough that shows **traces from the write path
and the keyed read** flowing out of the Rust core over OTLP — the M4 acceptance
check for [ISI-6078](https://github.com/MemKeys/memkeys).

The CI-grade proof of the same contract (span names/kinds/attributes, the pinned
`db.*` group, the deny-list, metric instruments, and log correlation) lives in
`crates/memkeys-server/tests/otel_signals.rs`, asserted against in-memory SDK
exporters. This page is the collector/Jaeger view of it.

## What the core emits (per the WS-7 signal catalog)

| Signal | Where |
|---|---|
| One trace per write: `memkeys.fact.ingest` (SERVER) → `memkeys.normalize` / `memkeys.key` / `memkeys.temporalize` (INTERNAL) → `memkeys.commit` (CLIENT, `db.*`) | `service.rs` write path |
| One trace per keyed read: `memkeys.retrieve` (SERVER) → `memkeys.store.query` (CLIENT, `db.*`) | `service.rs` op 1 |
| Metrics: `memkeys.write.operations`, `memkeys.write.duration`, `memkeys.retrieve.duration`, `memkeys.retrieve.candidate_set.size` | `telemetry.rs` |
| Logs via the tracing→OTel bridge, e.g. the `memkeys.write.rejected` key event (auto-correlated to its trace) | `telemetry_setup.rs` |

Attribute keys are validated against the signal-catalog registry
(`signal-catalog.yaml`); `db.query.text` and every content-bearing key are on
the deny-list and never emitted.

## Run it

Prereqs: docker, a recent Rust toolchain. No Postgres needed — the example
runs the real pipeline over the in-memory store (`memstore.rs`).

```sh
# 1. Jaeger all-in-one (OTLP gRPC receiver on 4317, UI on 16686)
docker run --rm -p 16686:16686 -p 4317:4317 jaegertracing/all-in-one:latest

# 2. The local-check example against the M4 OTel wiring
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
  cargo run -p memkeys-server --example otel_local_check
```

Expected console output:

```text
otel mode: Otlp (profile: Dev)
ingested fact <uuid> (keyed: true)
rejected as expected: fact.body must not be empty
keyed read returned 1 fact(s)
flushed; open Jaeger and search for service "memkeys"
```

## What to verify in Jaeger (http://localhost:16686)

Search **Service = memkeys**. You should see:

1. **The write trace** `memkeys.fact.ingest` — SERVER, with
   `memkeys.tenant.id` and `memkeys.entity.kind` attributes, four children:
   - `memkeys.normalize` / `memkeys.key` / `memkeys.temporalize` (INTERNAL,
     each carrying `memkeys.pipeline.stage`)
   - `memkeys.commit` (CLIENT) with the pinned stable group:
     `db.system.name=postgresql`, `db.operation.name=insert`,
     `db.collection.name=facts` — and **no** `db.query.text`.
2. **The rejected write trace** `memkeys.fact.ingest` with an ERROR status,
   whose correlated **log record** `memkeys.write.rejected` appears in the
   trace's logs (three-pillar correlation via trace id).
3. **The keyed-read trace** `memkeys.retrieve` (SERVER,
   `memkeys.retrieval.op=keyed`) → `memkeys.store.query` (CLIENT,
   `db.operation.name=select`).

With a collector instead of Jaeger-direct, the reference pipeline
(`collector-pipeline.yaml`) applies the second deny-list/redaction pass; the
example ships straight to Jaeger for simplicity.

## Configuration (env/profile)

| Variable | Meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | **Gate**: set → OTLP gRPC for traces/metrics/logs; unset → no-op providers, nothing leaves the process |
| `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT/TIMEOUT/HEADERS` | Per-signal overrides (honored by the opentelemetry-otlp builders) |
| `MEMKEYS_DEPLOYMENT_ENV` | `dev` \| `staging` \| `prod` → `deployment.environment.name` resource attribute; anything else fails startup |
| `MEMKEYS_TRACES_SAMPLER_RATIO` | Overrides the prod head-sampling ratio (ADR-0001 D-5: dev/staging sample 1.0, prod defaults to 0.1) |
| `MEMKEYS_SERVICE_NAMESPACE` | Optional `service.namespace` resource attribute |

Resource identity per ADR-0001 D-2: `service.name=memkeys`,
`service.version=<semver>+<git sha>`, `service.instance.id=<process uuid>`,
`deployment.environment.name`, under the pinned schema URL
`https://opentelemetry.io/schemas/1.30.0` (semconv-pins.yaml).
