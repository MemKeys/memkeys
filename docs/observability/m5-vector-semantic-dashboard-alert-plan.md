# M5 Vector/Semantic — Observability Plan (catalog + dashboards + alerts)

> Issue: **ISI-6169** · Parent: ISI-6079 (M5 vector/semantic) · Branch: `isi-6079/m5-vector-semantic` (PR #19)
> Owner: Observability Agent (WS-7) · Companion: [`signal-catalog.yaml`](./signal-catalog.yaml) v0.3.0
> Status: **Plan** — catalog reconciled to code; dashboards/alerts + emission gaps delegated below.

## 1. Scope

M5 adds the semantic retrieval path (op-4 `semantic_search`, RRF `fused_search`) and the
stage-5b vector-projection worker (ISI-6167). This plan confirms the telemetry constants
match the signal catalog, records the catalog reconciliation already applied, and specifies
the dashboard + alert + SLO coverage and the emission gaps that must be wired.

## 2. Constant ↔ catalog reconciliation (DONE in this change)

telemetry.rs constants vs the catalog enums before this change:

| Constant (telemetry.rs) | Value | Catalog enum before | Resolution |
|---|---|---|---|
| `RETRIEVAL_OP_SEMANTIC` | `semantic` | `memkeys.retrieval.op` already had `semantic` | ✅ no change — matched |
| `PROJECTION_KIND_VECTOR` | `vector` | `projection.kind` = [graph, summary, **embedding**, category] | ⚠️ `vector` was missing → **`embedding` → `vector`** (one name per tier) |
| `WORKER_KIND_VECTOR_PROJECTION` | `vector_projection` | `worker.kind` = [graph_projection, summarization, **embedding**, categorization] | ⚠️ `vector_projection` missing → **`embedding` → `vector_projection`** |

`embedding` was the pre-M5 placeholder name for the ANN/vector-index tier; M5's concrete
names are `vector` / `vector_projection`. Renamed rather than added so there is exactly one
series per tier (no dual-emit cardinality split). `summary`/`category` + their workers stay
as future WS-3 LLM-derived views. Catalog bumped **0.2.0 → 0.3.0**.

## 3. Signals covering M5 (what's already emitted vs what's a gap)

### Already emitted (service.semantic_search — verified in `service.rs`)
- `memkeys.retrieve` SERVER span, `memkeys.retrieval.op=semantic`, `memkeys.tenant.id` (server-set floor).
- `memkeys.retrieve.duration{op=semantic, outcome}` histogram.
- `memkeys.retrieve.candidate_set.size{op=semantic}` histogram.

### Gaps (new instrumentation — delegated in §6)
- **G1 — feature_disabled invisible.** `semantic_search` returns `QueryError::FeatureDisabled`
  *before* starting the span or recording any metric. A request to a semantic-off deployment
  is telemetry-silent. Fix: record `memkeys.retrieve.duration{op=semantic, outcome=feature_disabled}`
  (+ span with outcome) before returning 501. Catalog: `feature_disabled` added to the retrieval
  outcome enum.
- **G2 — fused reads emit no fusion telemetry.** `fused_search` starts no span of its own and
  the `via`/path-mix (`FusionSource` on each `SemanticHit`) never reaches telemetry. The spec'd
  `memkeys.rrf.fuse` span + `memkeys.fusion.rrf.inputs` histogram are not emitted. Fix: emit the
  `memkeys.rrf.fuse` INTERNAL span with `memkeys.fusion.algorithm=rrf` + `memkeys.fusion.sources`
  (bounded path-mix enum, added to catalog) and mirror `fusion.sources` onto the `retrieve` entry
  span; record `memkeys.fusion.rrf.inputs`. **Requires** adding `memkeys.fusion.algorithm` and
  `memkeys.fusion.sources` to `ALLOWED_ATTR_KEYS` in telemetry.rs (today absent → a `debug_assert`
  would fire).
- **G3 — poison depth not attributable to the vector worker.** `Instruments::poison_queue_depth()`
  hardcodes `WORKER_KIND_GRAPH_PROJECTION` and takes no `worker_kind` arg. The stage-5b vector
  worker cannot report its own poison/dead-letter depth. Fix: `poison_queue_depth(worker_kind, count)`.
  Owned by ISI-6167 (worker wiring).

## 4. Dashboard plan — "M5 Semantic Retrieval & Vector Projection"

24-column grid; all tiles filter the deny-list-safe registry attributes only. Build via the
`dt-app-dashboards` skill (validate every DQL against sample output before embedding).

| # | Tile | Signal / query shape | Split by |
|---|---|---|---|
| 1 | Semantic request rate | count `memkeys.retrieve.duration{op=semantic}` | outcome |
| 2 | Semantic latency p50/p95/p99 | `memkeys.retrieve.duration{op=semantic, outcome=success}` | — |
| 3 | Candidate-set size distribution | `memkeys.retrieve.candidate_set.size{op=semantic}` | — |
| 4 | feature_disabled rate (shadow demand / misconfig) | `memkeys.retrieve.duration{op=semantic, outcome=feature_disabled}` count | tenant.id (top-N) |
| 5 | Fused read path-mix | `memkeys.rrf.fuse` span count or `fusion.rrf.inputs` | memkeys.fusion.sources |
| 6 | Multi-path agreement share | fused reads where `fusion.sources` contains `+` ÷ all fused | — |
| 7 | Vector projection lag p95 | `memkeys.projection.lag{projection.kind=vector}` | — |
| 8 | Vector projection lag current (max outstanding) | `memkeys.projection.lag.current{projection.kind=vector}` | — |
| 9 | Vector worker queue depth | `memkeys.worker.queue.depth{worker.kind=vector_projection}` | — |
| 10 | Vector worker poison / dead-letter depth | poison depth `{worker.kind=vector_projection}` (needs G3) | — |
| 11 | Embedding LLM spend (if real embedder) | `memkeys.llm.cost{worker.kind=vector_projection}` | gen_ai.request.model |
| 12 | Semantic errors | `memkeys.retrieve.duration{op=semantic, outcome=error}` count | — |

## 5. SLO + alert plan

SLO targets land in `define-slos` output (same pattern as `projection.lag` / `graph.traverse`).
Proposed targets (to ratify with Testing Architect):

| Signal | SLO / threshold | Alert | Severity |
|---|---|---|---|
| `retrieve.duration{op=semantic, outcome=success}` | p95 ≤ **250 ms** warm (query-embed + ANN + hydrate); ratify with first prod data | p95 breach sustained 10 min | warning |
| `retrieve.duration{op=semantic, outcome=error}` | error ratio < 1% | error ratio > 5% / 5 min | critical |
| `retrieve.duration{op=semantic, outcome=feature_disabled}` | expected 0 when feature ON | any sustained rate while deployment declares semantic ON = **misconfiguration**; also surfaces shadow demand when OFF | warning |
| `projection.lag{projection.kind=vector}` | p95 ≤ **D3 bound** (shared with graph; ratify per profile) | p95 breach → tail-sample + alert | critical (D3 invariant) |
| `projection.lag.current{projection.kind=vector}` | max outstanding ≤ lag bound | oldest un-applied grows unbounded | critical |
| `worker.queue.depth{worker.kind=vector_projection}` | bounded / draining | sustained growth (not draining) 15 min | warning |
| poison depth `{worker.kind=vector_projection}` | **0** steady-state | any non-zero sustained = dead-lettered embeddings | critical |

Tail-sampling: a `projection.lag{vector}` breach keeps the worker trace (already covered by the
lag-anomaly tail policy in `collector-pipeline.yaml`; the new `projection.kind=vector` value flows
through the existing policy — no collector change needed).

## 6. Delegation / next actions

- **G1 + G2 (service.rs emission)** → implementation story under ISI-6079 (code owner). Catalog
  contract is ready; the two fusion attr keys must be added to `ALLOWED_ATTR_KEYS` in the same change.
- **G3 (poison_queue_depth parameterization + vector-worker emission)** → requirement on **ISI-6167**
  (stage-5b worker): emit `worker.process`/`projection.lag`/`queue.depth`/poison with
  `worker.kind=vector_projection`, `projection.kind=vector`.
- **SLO target ratification** → Testing Architect (`define-slos`): semantic p95, vector lag bound,
  feature_disabled misconfig alert as a test-consumable KPI.
- **Dashboard build** → `dt-app-dashboards` skill once G1–G3 emit (tiles 4,5,6,10 depend on them).

Dashboards/alerts that depend only on already-emitted signals (tiles 1,2,3,7,8,9,12) can ship now.
