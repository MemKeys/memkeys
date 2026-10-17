# MemKeys Signal Catalog (human-readable)

> Authoritative machine-readable source: [`signal-catalog.yaml`](./signal-catalog.yaml).
> Design rationale: [`../adr/ADR-0001-observability-otel-signals.md`](../adr/ADR-0001-observability-otel-signals.md).
> Owner: Observability Agent · WS-7 · ISI-5872. **Instrument against this contract.**

MemKeys emits three correlated pillars over OTLP to a collector. Every signal carries
the shared Resource; traces/metrics/logs join on `trace_id`/`span_id`. Tenant scoping is
an **opaque id + entity *kind*** only — never fact content (see the deny-list).

## Spans (traces)

| Span | Kind | Domain | Owner | Semconv |
|---|---|---|---|---|
| `memkeys.fact.ingest` | SERVER | write | WS-1 | — |
| `memkeys.normalize` | INTERNAL | write | WS-1 | — |
| `memkeys.key` | INTERNAL | write | WS-1 | — |
| `memkeys.temporalize` | INTERNAL | write | WS-1 | — |
| `memkeys.commit` | CLIENT | write | WS-2/SoT | `db.*` |
| `memkeys.retrieve` | SERVER | retrieval | WS-4 | — |
| `memkeys.store.query` | CLIENT | retrieval | WS-4 | `db.*` |
| `memkeys.rrf.fuse` | INTERNAL | retrieval | WS-4 | — |
| `memkeys.worker.process` | CONSUMER | derived-view | workers | — (span-link to commit) |
| `memkeys.llm.call` → `{gen_ai.operation.name} {model}` | CLIENT | llm | WS-3 | `gen_ai.*` (experimental) |

**Write path** is one trace: `fact.ingest` → `normalize`/`key`/`temporalize` → `commit`.
**Retrieval** is one trace: `retrieve` → one `store.query` per store + optional `rrf.fuse`.
**Workers** run async in their own trace with a **span link** back to the originating
`commit` (temporally decoupled, not a parent). Errors are always recorded (not sampled).

## Metrics

| Metric | Instrument | Unit | Key attributes |
|---|---|---|---|
| `memkeys.write.operations` | counter | `{operation}` | entity.kind, outcome |
| `memkeys.write.duration` | histogram | s | pipeline.stage, outcome |
| `memkeys.retrieve.duration` | histogram | s | retrieval.op, outcome |
| `memkeys.retrieve.candidate_set.size` | histogram | `{candidate}` | retrieval.op |
| `memkeys.fusion.rrf.inputs` | histogram | `{list}` | — |
| **`memkeys.projection.lag`** | histogram | s | projection.kind |
| `memkeys.projection.lag.current` | observable gauge | s | projection.kind |
| `memkeys.worker.queue.depth` | observable up/down counter | `{item}` | worker.kind |
| `memkeys.graph.traverse.duration` | histogram | s | tenant.id, graph.depth, truncate.reason, outcome |
| `memkeys.graph.edges.cardinality` | observable gauge | `{edge}` | tenant.id |
| `gen_ai.client.token.usage` | histogram | `{token}` | token.type, model, system |
| `gen_ai.client.operation.duration` | histogram | s | operation.name, model |
| `memkeys.llm.cost` | counter | `{usd}` | model, worker.kind |

> **`memkeys.projection.lag` is the D3 observable-lag invariant made real** — the
> measured SoT→projection delay, with an SLO (see `define-slos`). Breach → tail-sampling
> keeps the trace + a `memkeys.projection.lag.breach` log event fires.

> **The two graph metrics are the ADR-GRAPH §10 promotion gate** —
> `memkeys.graph.traverse.duration` carries the `graph.traverse.warm` SLO (p95 ≤ 150 ms at
> `graph.depth ≤ 3`, ratified ISI-5907; depth 4–5 is best-effort, outside the SLO), and
> `memkeys.graph.edges.cardinality` is the polled per-tenant standing edge count. Promote to
> a dedicated graph DB only on a measured, sustained, per-tenant breach — never a guess.
> (F11 ask `graph.traverse.latency` realized as `traverse.duration` — semconv histograms
> measure duration; ISI-5911 fold.)

## Logs

Structured, trace-correlated (TraceId + SpanId on every record), bounded, PII-free.
Async workers carry the originating trace context on the work item so their records
correlate to the triggering write. Key structured events:
`memkeys.write.rejected`, `memkeys.projection.lag.breach`, `memkeys.worker.dead_letter`.

## Custom attribute registry (all bounded)

| Attribute | Values |
|---|---|
| `memkeys.tenant.id` | opaque id/hash (never a name) |
| `memkeys.entity.kind` | project · issue · pr · agent · run |
| `memkeys.pipeline.stage` | normalize · key · temporalize · commit |
| `memkeys.retrieval.op` | keyed · graph · semantic |
| `memkeys.projection.kind` | graph · summary · embedding · category |
| `memkeys.worker.kind` | graph_projection · summarization · embedding · categorization |
| `memkeys.graph.depth` | 1 · 2 · 3 · 4 · 5 (post-clamp; MaxDepth=5 ceiling) |
| `memkeys.graph.truncate.reason` | none · depth_clamp · fanout · node_budget · deadline · orphan |

## Deny-list (hard boundary — SDK + collector)

Never emitted in any signal: fact bodies/content, **entity-key values** (only `kind`),
raw query text (`db.query.text`), LLM prompt/completion, embedding input text + vectors,
credentials, free-form user/agent identifiers. Enforced by the single SDK emission helper
(validates keys against this registry) and re-enforced by the collector
(`transform/scrub` + cardinality firewall). See [`collector-pipeline.yaml`](./collector-pipeline.yaml).
