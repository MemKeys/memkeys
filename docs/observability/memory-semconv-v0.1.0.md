# memkeys ↔ memory-semconv v0.1.0 alignment (ISI-6216 / 5581-§2b)

**Owner:** Observability Agent (WS-7). **Status:** implemented on branch
`isi-6216/memory-semconv-v0.1.0`. **Parent:** ISI-5581 (KubeCon memory-benchmark
re-run), plan rev 3 §2b. Spun out of ISI-6182 / 5581-D.

## Why

memkeys already emits OTel natively (`crates/memkeys-server/src/telemetry.rs` +
`telemetry_setup.rs`) under its own `memkeys.*` / `gen_ai.*` vocabulary. The other
benchmark substrates (mem0 / memU / MemOS forks + the mcp-memory-server shim +
MemPalace PR #2142) emit the shared **memory-semconv v0.1.0** contract. Without a
shared vocabulary memkeys' numbers are not apples-to-apples in the §2c comparison.

This change **keeps the native stack** and **dual-emits** the memory-semconv v0.1.0
contract alongside it. Nothing is removed; the native `memkeys.*` spans/metrics are
unchanged. The exporter gate (`OTEL_EXPORTER_OTLP_ENDPOINT`) is unchanged — no-op
when unset.

## Span mapping (`memory.<op>`, SpanKind.CLIENT)

The `memory.<op>` span is the **outer CLIENT span** the agent→memkeys call nests
beneath. When memkeys is reached through the mcp-memory-server shim, the shim's own
`memory.<op>` span is the parent and memkeys' `memory.<op>` nests under it (the
**two-source rule**); the native `memkeys.*` SERVER span nests under memkeys'
`memory.<op>`.

| memkeys path (native span)                     | memory-semconv span | `memory.store.kind` |
|------------------------------------------------|---------------------|---------------------|
| `ingest()` → `memkeys.fact.ingest`             | `memory.add`        | `postgres`          |
| `fetch_by_keys()` → `memkeys.retrieve` (keyed) | `memory.recall`     | `keyed`             |
| `semantic_search()` → `memkeys.retrieve` (sem) | `memory.search`     | `vector`            |
| `traverse/supersedes/links()` → graph read     | `memory.search`     | `graph`             |

Temporal invalidate/update (supersession on the write path) maps to
`memory.invalidate` / `memory.update` — constants reserved in `telemetry.rs`
(`MEMORY_OP_INVALIDATE` / `MEMORY_OP_UPDATE`); wiring deferred until the benchmark
exercises an explicit invalidate op.

Resource (set in `build_resource`, every signal): `memory.sut.name=memkeys`,
`memory.sut.architecture=keyed+temporal+graph+2-tier-vector`. Tempo:
`{name="memory.<op>" && resource.memory.sut.name="memkeys"}`.

## Metric mapping (shared contract names)

Instrument names/units are chosen so the OTLP→collector→Prometheus render produces
the exact contract metric names the other substrates emit. Curly-brace annotation
units (`{item}`, `{byte}`, `{result}`, `{score}`) are **not** suffixed by the
Prometheus exporter; only the duration histogram keeps a real unit (`s` →
`_seconds`), which is the intended name.

| memkeys instrument (OTel name)  | unit       | Prometheus render                      | from native signal                     |
|---------------------------------|------------|----------------------------------------|----------------------------------------|
| `memory.operation.duration`     | `s`        | `memory_operation_duration_seconds`    | `memkeys.write.duration` / `.retrieve.duration` |
| `memory.recall.results.count`   | `{result}` | `memory_recall_results_count`          | `memkeys.retrieve.candidate_set.size`  |
| `memory.recall.top.similarity`  | `{score}`  | `memory_recall_top_similarity`         | top fused/similarity score (semantic)  |
| `memory.items` (counter)        | `{item}`   | `memory_items_total` **(gap-fill)**    | none — no native equivalent            |
| `memory.bytes` (counter)        | `{byte}`   | `memory_bytes_total` **(gap-fill)**    | none — byte COUNT of committed body    |

> Exact Prometheus-name match is **gated in the benchmark LGTM conformance step**
> (same gate mem0 / memU / MemOS passed). If the render diverges from the Python
> substrates, it is a one-line instrument-name fix here.

### Bounded label budget (4) + status

`memory.operation` · `memory.store.kind` · `memory.sut.name` · `memory.embedder.model`
(+ `status` on the duration histogram only). All bounded enums / a fixed per-process
model id. Extends the native approved-key registry (`ALLOWED_ATTR_KEYS`) — the single
`attr()` emission helper refuses any key not listed (debug builds panic).

`memory.embedder.model` is read from `MEMKEYS_EMBEDDER_MODEL` (one value per
deployment → not a cardinality risk); unset → `none`.

## PII gate (unchanged)

The deny-list still excludes fact bodies, entity-key VALUES, raw query text, and
creds. `db.query.text` stays disabled. The read spans carry `memory.query.length`
(a **length**, never the text) as the safe proxy. `memory_bytes_total` is a byte
**COUNT**, never body content. Guarded by the registry test
(`memory.query.text` is asserted *not* registered).

## Emerging standard: v0.1.0 → OTel GenAI semconv mapping

The OTel **GenAI** semantic conventions add a memory/agent surface. We keep
v0.1.0 for apples-to-apples continuity with the existing forks, and record the
mapping so 5581-G and the talk can show "the standard caught up to what we proposed"
(our PRs #6712 / #617 / #2207 / #2142 pre-date / parallel the convention).

**Pinned revision:** `open-telemetry/semantic-conventions` GenAI group —
**v1.30.0** semconv release (matches our existing `schema_url` pin,
`semconv-pins.yaml`); the GenAI memory operations are **Development** stability
(experimental) → isolate behind one helper and treat any bump as a change-managed,
one-file edit (same policy as the existing `gen_ai.*` group, ADR-0001 D-3).

| memory-semconv v0.1.0 (`memory.operation`) | OTel GenAI `gen_ai.operation.name` |
|--------------------------------------------|------------------------------------|
| `add`                                      | `create_memory` (a.k.a. `upsert_memory`) |
| `search`                                   | `search_memory`                    |
| `recall`                                   | `get_memory` / `search_memory`     |
| `update`                                   | `update_memory` (`upsert_memory`)  |
| `invalidate`                               | `delete_memory`                    |

memkeys already emits the **stable** GenAI client metrics
`gen_ai.client.token.usage` and `gen_ai.client.operation.duration` for its M6 LLM
pipeline (WS-3), so it is already aligned on the GenAI client surface; the memory
operations above are the **experimental** `gen_ai.*_memory` surface and are recorded
here as a mapping only (not emitted) until the GenAI memory group stabilises.

## Where

- `crates/memkeys-server/src/telemetry.rs` — constants, registry, instruments,
  `start_memory_op_span`, recorders.
- `crates/memkeys-server/src/telemetry_setup.rs` — `memory.sut.*` resource attrs.
- `crates/memkeys-server/src/service.rs` — dual-emit at the 6 operation boundaries.
- `crates/memkeys-server/tests/otel_signals.rs` — topology + metric conformance.

## Rev 2 — extended recall/search observability (ISI-6251)

Board ask (ISI-6216 plan rev-2, gate `0ab1b4e9`): *facts found with a search, the
search key applied, quantity recalled.* All additive; no native-stack change; the
PII deny-list is a HARD boundary (entity-key VALUES never leave the process).

### Found-vs-returned funnel
- **Metric** `memory.recall.candidates.count` → Prometheus `memory_recall_candidates_count`
  (`{result}`, labels `memory.operation` + `memory.store.kind` + `memory.sut.name` —
  the SAME label set as `memory_recall_results_count`, so the **candidates/results
  ratio** is a direct pruning/precision signal). Recorded at all three recall sites
  (keyed `rows.len()`, graph `nodes.len()`, semantic `hits.len()`) alongside the
  native `memkeys.retrieve.candidate_set.size`. **No new metric labels** — the
  bounded label budget is unchanged.

### Per-span recall detail (Tempo visibility)
On the outer `memory.recall` / `memory.search` CLIENT span:
- `memory.recall.candidates_count` (i64), `memory.recall.results_count` (i64)
- `memory.recall.top_similarity` (f64) — semantic/vector read only.

### Search-key observability — STRUCTURAL ONLY
- `memory.recall.key_kind` — the `EntityKey` KIND(s) from the scope (`wire_name()`,
  bounded enum; multi-kind scopes sorted + deduped + joined). **Span attr only, never
  a metric label** (keeps the bounded metric budget).
- `memory.recall.key_count` — `scope.len()`.
- **`memory.recall.key_id` is intentionally NOT emitted and NOT registered** — raw
  entity-key values breach the deny-list (ISI-6216 board comment `c2a0e066`). Asserted
  absent in `telemetry.rs` (registry) and `otel_signals.rs` (live keyed-read span).

### Upstream gap
`candidates_count` / `key_kind` / `key_count` have no equivalent in memory-semconv
v0.1.0 or the GenAI memory conventions yet → tracked as an upstream extension proposal
(talk narrative: "the standard caught up to what we proposed").
