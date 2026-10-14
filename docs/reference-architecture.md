# MemKeys — Reference Architecture & Deployment (System View)

**Status:** Solutioning synthesis (WS-6, ISI-5871) · **Author:** Architect (Winston) · **Date:** 2026-10-14
**Phase:** BMAD Phase 3 (Solutioning) — synthesis of WS-1…WS-5 into one runnable system view
**Companion to:** `docs/architecture.md` (the conceptual/data-model design — `memory-organization.md` rev 3, promoted by WS-0)

> This document is the **system view**: *how MemKeys runs.* It takes the *what* and *why*
> settled in the conceptual design and the per-workstream decisions (vector backend, graph
> storage, LLM pipeline, retrieval contract, MCP surface) and composes them into process
> topology, storage deployment, the pluggable-backend boundary, and two deployment profiles.
> It does **not** restate the data model, the fact envelope (§3), or the four retrieval ops
> (§7) — those live in the conceptual design; this view references them.

## 0. What this synthesizes

| Input workstream | Decision this view consumes |
|---|---|
| **WS-1 Vector backend** | `VectorIndex` interface; **pgvector** reference impl (metadata pre-filter → ANN → RRF), two tiers (fact + digest) |
| **WS-2 Graph storage** | `GraphView` interface; **edge-rows + recursive CTE in the SoT store** reference impl (no separate graph DB) |
| **WS-3 LLM pipeline** | async worker set (key-extract, digest-summarize, categorize, embed); **generative** (Claude tier) + **embedder** (`BGE-M3`) roles; `(model_id, prompt_version)` versioning |
| **WS-4 Retrieval contract** | the four ops (keyed hot / graph warm / semantic scoped / write); scoped pre-filter → rank → hydrate; tenant-scope floor (R6) |
| **WS-5 MCP server + skills** | the five MCP tools (`memory.write/get/recall/search/traverse`); transport + per-call tenant scoping |
| **WS-7 Observability** | OTel traces/metrics/logs instrumented inside each component; projection-lag metric |

Governing invariant inherited from the design, unchanged here: **keyed-first.** Attribute keys
bound the candidate set before any vector/graph work; the keyed/temporal store is the single
source of truth; graph and vector are derived, rebuildable projections.

---

## 1. Two board leanings — resolved here

These were the two forks the roadmap left open for the board and tasked WS-6 to close. Both are
resolved below as architecture decisions, consistent with the approved roadmap leans, each with
an **evidence-gated promotion path** so neither is a one-way door.

### Decision A — OSS default stack: **single-Postgres** (pgvector + edge-rows)

**Resolved: Postgres-first.** The OSS default deployment runs **one Postgres instance** that
implements all three store roles behind their interfaces:

- **FactStore (SoT)** — bi-temporal fact rows (append-only; `event_time` + `ingested_at`;
  supersession computed read-time). This is the keyed/temporal source of truth.
- **VectorIndex** — `pgvector` (HNSW), two tiers (fact + digest), metadata columns carry the
  `entity.*` keys so the **pre-filter runs as a SQL `WHERE` before the ANN operator** — the
  keyed-first boundary is enforced by the query plan, not bolted on.
- **GraphView** — edge rows in the same Postgres; traversal via **recursive CTE**, always
  bounded by depth and tenant/project scope.

**Why.** Lowest operational weight (one datastore to run, back up, secure); the keyed-first
design never needs cross-store joins on the hot path; a single transactional boundary makes the
"ack gated on the keyed commit, projections async" contract trivial to honor. Developer
productivity *is* architecture — one `docker run postgres` is the whole minimal stack.

**Promotion path (not a one-way door).** Because vector and graph are *derived, rebuildable
projections* (D3), swapping a backend is a replay, not a migration:
- Vector → **Qdrant/Milvus** when pgvector scoped-filter latency or recall degrades at real
  scale (promote on evidence — candidate-set size and p95 from WS-7 metrics are the trigger).
- Graph → **Neo4j/Memgraph** only if traversal depth/perf demands outgrow recursive CTE.
- `ponytail:` defer both the dedicated vector store and the graph DB until a WS-7 metric
  (candidate-set size, RRF fusion latency, or CTE traversal p95) crosses a documented ceiling —
  upgrade path is "stand up the new store, point the interface impl at it, replay from SoT."

### Decision B — v1 consumer surface: **both** (MCP **and** direct gateway flush)

**Resolved: both, with distinct roles.**

- **MCP** is the **agent-facing** surface — the five tools (§WS-5) over stdio (local agents) or
  streamable HTTP (shared server). This is how an agent reads/writes memory interactively and
  reads the natural-language skill descriptions.
- **Direct HTTP envelope path** (`POST /memory/facts`, §3/§7.1) stays **first-class** as the
  **bulk write path** — the AgentMesh gateway flush, run-settle, and telemetry-learning
  producers push high-volume raw envelopes here without an MCP round-trip.

**Why.** The two surfaces serve different callers at different volumes. Forcing the gateway flush
through MCP would add a tool-call envelope to the highest-volume write path for no benefit; both
surfaces funnel into the **same ingest service and the same write pipeline** (§2), so there is one
modeling path and one source of truth regardless of entry point. MCP is not a second write
semantics — it is a thin adapter onto the same `/memory/*` contract.

---

## 2. Process topology

Two planes share the stores: a **synchronous request plane** (writes + keyed/graph reads, on the
hot path, latency-bound) and an **asynchronous projection plane** (derived-view workers, off the
hot path, throughput-bound). The `ack` is gated only on the synchronous keyed commit.

```
                 AGENT-FACING                         BULK PRODUCERS
            ┌───────────────────┐              ┌──────────────────────────┐
            │  agents via MCP   │              │ AgentMesh gateway flush   │
            │  (stdio / HTTP)   │              │ run-settle · telemetry    │
            └─────────┬─────────┘              └─────────────┬────────────┘
     memory.write/get/recall/search/traverse       POST /memory/facts (envelope §3)
                      │                                       │
                      ▼                                       ▼
            ┌───────────────────────────────────────────────────────────┐
            │                 API surface (one contract)                 │
            │   MCP server  ──adapts──▶  /memory/* HTTP  ◀──direct────    │
            │   • per-call tenant scoping (R6 floor) enforced here       │
            └───────────────┬───────────────────────────┬───────────────┘
            SYNC REQUEST PLANE                           │  writes enqueue change-log
                            │                            │
         ┌──────────────────┼────────────────┐          │
         ▼                  ▼                 ▼          ▼
   ┌──────────┐      ┌────────────┐    ┌──────────┐  ┌──────────────────────────┐
   │ WRITE    │      │ KEYED READ │    │ GRAPH    │  │  ingest change-log / queue│
   │ pipeline │      │ (op 1)     │    │ traverse │  │  (one row per commit)     │
   │ 1 norm   │      │ fetch-by-  │    │ (op 2)   │  └────────────┬─────────────┘
   │ 2 key    │      │ keys       │    │ rec. CTE │     ASYNC PROJECTION PLANE
   │ 3 temporal│     └─────┬──────┘    └────┬─────┘                │
   │ 4 commit │           │                │         ┌────────────┴─────────────┐
   └────┬─────┘           │                │         ▼        ▼        ▼        ▼
        │ ack (after 4)   │                │     ┌───────┐┌───────┐┌───────┐┌─────────┐
        ▼                 ▼                ▼     │5a graph││5b fact││5c dig. ││5d categ.│
   ┌───────────────────────────────────────┐   │project ││ embed ││rollup+ ││ label + │
   │              STORES                    │   │ edges  ││       ││summ.+  ││consol.  │
   │  FactStore (SoT, bi-temporal)  ◀───────┼───┤        ││       ││embed   ││         │
   │  VectorIndex (fact + digest)   ◀───────┼───┼────────┼┤◀──────┼┤◀───────┼┤         │
   │  GraphView (edge rows)         ◀───────┼───┤◀───────┤└───┬───┘└───┬────┘└────┬────┘
   │  Category facets               ◀───────┼───┼────────┼────┼────────┼──────────┘
   └───────────────────────────────────────┘   └────────┼────┴────────┴─── model endpoints:
        ▲ semantic search (op 3) hydrates here           │     embedder (BGE-M3) · summarizer ·
        └── scoped pre-filter → ANN(RRF) ─────────────────┘     categorizer (generative/Claude)

   reconciler/backfill job  ──scans SoT vs projections, rebuilds holes + stale (model_id,prompt_version)
   OTel ── every box emits traces (db.* / gen_ai.*), metrics (incl. projection-lag), trace-correlated logs
```

**Synchronous request plane (latency-bound, hot):**
- **Write pipeline** (stages 1–4): normalize → key (enrich from run/thread anchor) → temporalize →
  commit to FactStore. `ack` returns after stage 4 only. Entry is either MCP `memory.write` or the
  direct `POST /memory/facts` — both hit the same pipeline.
- **Keyed read (op 1 / `memory.get`)**: `WHERE entity.* AND time-range` over FactStore. No vector,
  no graph. Serves the majority of reads.
- **Graph traverse (op 2 / `memory.traverse`)**: recursive CTE over edge rows; returns ids + edge
  topology, hydrates bodies from FactStore.
- **Semantic search (op 3 / `memory.search` + `memory.recall`)**: scoped pre-filter (SQL `WHERE` on
  keys → candidate set) → ANN over the enabled tiers → RRF fusion → resolve digest hits to member
  facts → hydrate from FactStore. Present only in the **full** profile; returns `501
  feature_disabled` in **minimal** rather than a silent empty.

**Asynchronous projection plane (throughput-bound, off the hot path):**
- One **change-log row per commit** is the trigger (D3 change-log-driven); workers are idempotent on
  `fact.id` (+ `derivation_version` for model-stamped artifacts) and retryable.
- **5a graph-projection** (always on — both profiles): upsert entity nodes + edges.
- **5b fact-embed**, **5c digest-rollup+summarize+embed** (debounced per scope), **5d
  categorize+consolidate** — all **full profile only**, each gated by its feature flag.
- **Reconciler/backfill**: detects projection holes and stale `(model_id, prompt_version)` artifacts,
  rebuilds by replay from SoT. A projection failure never blocks stages 1–4.

**Scaling grain.** The request plane and each async worker class scale independently. The request
plane is stateless (scale horizontally behind the API surface); workers scale by queue depth; the
stores scale as Postgres does (read replicas for keyed reads; the SoT commit stays single-writer).

---

## 3. Pluggable-backend boundary

The portability seam the whole project is built around. Every backend choice sits behind an
interface; the **OSS default** is the Postgres implementation, the **reference plugin** is Grail +
DQL. No MemKeys core code knows which is wired in.

```
        MemKeys core (backend-agnostic)
   write pipeline · retrieval ops · workers · MCP · OTel
                         │
        ┌────────────────┼───────────────────────────────┐
   ┌────▼─────┐   ┌───────▼────────┐   ┌───▼────────┐  ┌──▼─────────┐  ┌──▼──────────┐
   │FactStore │   │ VectorIndex    │   │ GraphView  │  │ Embedder   │  │ Summarizer/ │
   │(SoT)     │   │ embed·upsert·  │   │ traverse·  │  │ embed(txt, │  │ Categorizer │
   │commit·   │   │ scoped_search· │   │ supersedes·│  │ task)      │  │ (generative)│
   │fetch·asof│   │ delete·rebuild │   │ links      │  │            │  │             │
   └────┬─────┘   └───────┬────────┘   └────┬───────┘  └────┬───────┘  └─────┬───────┘
        │                 │                 │               │                │
  ══════╪═════════════════╪═════════════════╪═══════════════╪════════════════╪══════ boundary
        │ OSS DEFAULT (bundled, permissive-licensed, self-hostable)          │
   ┌────▼───────────────────────────────────────────┐  ┌────▼───────┐  ┌─────▼───────┐
   │ Postgres: fact rows · pgvector · edge rows      │  │ BGE-M3     │  │ small open  │
   │ (one instance implements FactStore+Vector+Graph)│  │ (MIT)      │  │ instruct /  │
   └─────────────────────────────────────────────────┘  └────────────┘  │ Claude tier │
                                                                         └─────────────┘
        │ REFERENCE PLUGIN (enterprise path; never the lock-in)
   ┌────▼────────────────────────────────────────────┐  ┌────────────┐  ┌─────────────┐
   │ Grail buckets + DQL (FactStore) · Grail-native   │  │ Voyage-3 / │  │ Claude /    │
   │ vector · graph-over-DQL                          │  │ Jina-API / │  │ hosted LLM  │
   └──────────────────────────────────────────────────┘  │ Cohere     │  └─────────────┘
                                                          └────────────┘
```

**Boundary rules:**
- **Six interfaces** form the entire boundary: `FactStore`, `VectorIndex`, `GraphView`, `Embedder`,
  `Summarizer`, `Categorizer`. Everything above is portable core; everything below is swappable.
- **Grail is a reference plugin, never the default and never the lock-in.** It proves the boundary
  holds against a schema-on-read lakehouse (the hardest case) and is the Dynatrace/enterprise path.
- **Models are configuration, not code dependencies.** `BGE-M3` (MIT, self-hostable, multilingual
  EN+FR, 8k context, dense+sparse+ColBERT) is the bundled embedder default; a small open instruct
  model or a Claude tier is the generative default. Managed APIs (Voyage-3, Jina-v3-API, OpenAI,
  Cohere, hosted Claude) are **plugin** options, not bundled, because the OSS default must be
  permissively licensed and bundleable. `(model_id, prompt_version)` is pinned on every derived
  artifact so a model swap is a clean rebuild-by-replay.
- A plugin must satisfy the same **keyed-first contract**: scoped pre-filter before ANN, tenant floor,
  hydrate-from-SoT. A backend that cannot pre-filter the candidate set is not a conformant VectorIndex.

---

## 4. Two deployment profiles

The design's "minimal deployment = keyed+graph only" made concrete. Both profiles run the **same
core and the same contract**; the difference is which async workers and model endpoints exist, gated
by feature flags.

### 4.1 Minimal — keyed + graph only

> Target: early contributors, resource-constrained environments, "does my team need semantic at all?"

```
┌────────────────────────────────────────────────┐
│  one container/box                               │
│   API surface (MCP stdio/HTTP + /memory/*)       │
│   write pipeline (1–4) · keyed read · graph CTE  │
│   async: 5a graph-projection worker only         │
│   reconciler (graph holes only)                  │
│   ── one Postgres (fact rows + edge rows) ──      │
│   OTel → collector (optional)                    │
└──────────────────────────────────────────────────┘
   NO embedder · NO summarizer · NO vector index · NO LLM workers
```

- **Flags:** `memory.semantic = off` (⇒ `memory.semantic.digest` moot).
- **MCP tools live:** `memory.write`, `memory.get`, `memory.traverse`.
- **`memory.search` / `memory.recall`** return `501 feature_disabled` (callers degrade gracefully).
- **Footprint:** single Postgres + the MemKeys process. No GPU, no model endpoints, no queue beyond
  the in-process change-log. Runs on a laptop or a small VM.

### 4.2 Full — + two-tier vector + LLM pipeline

> Target: production; semantic recall, digest navigation, scoped categories.

```
┌──────────────────────┐   ┌─────────────────────────────────────────────┐
│ API surface          │   │ async worker pool (scales on queue depth)     │
│  MCP + /memory/*      │──▶│  5a graph  5b fact-embed  5c digest-rollup    │
│  all 5 tools          │   │            5d categorize+consolidate          │
│  request plane        │   │  reconciler (all tiers + stale model stamps)  │
└──────────┬───────────┘   └───────┬──────────────┬────────────────────────┘
           │                       │              │
   ┌───────▼───────────────────────▼───┐   ┌──────▼───────────────────────┐
   │ Postgres                           │   │ model endpoints              │
   │  fact rows (SoT)                   │   │  Embedder  : BGE-M3 (shared  │
   │  pgvector: fact tier + digest tier │   │             both tiers)      │
   │  edge rows (graph)                 │   │  Generative: summarizer +    │
   │  category facets                   │   │             categorizer      │
   └────────────────────────────────────┘   │             (Claude / local) │
                                             └──────────────────────────────┘
   change-log/queue (durable; debounce for 5c) · OTel collector (traces+metrics+logs)
```

- **Flags:** `memory.semantic = on`; `memory.semantic.digest = on|off` (fact-tier-only is a valid
  middle profile — precision search without paying for summarization).
- **MCP tools live:** all five. `memory.search` fuses fact + digest tiers via RRF (k=60, equal
  default weights); `memory.recall` targets the digest tier and always resolves to raw member facts.
- **Added infra over minimal:** one embedder endpoint (CPU-fine for BGE-M3), one generative endpoint
  (summarizer + categorizer), a durable queue with per-scope debounce for 5c, and worker replicas.
- **Middle profile (fact-tier only):** `memory.semantic = on`, `memory.semantic.digest = off` — adds
  5b + the embedder, skips 5c/5d and the summarizer. For deployments that want precision semantic
  search but not digest rollups or the summarizer cost.

| Capability | Minimal | Full (fact-tier) | Full (+digest) |
|---|---|---|---|
| Keyed fetch (op 1) · graph traverse (op 2) | ✅ | ✅ | ✅ |
| Semantic search — fact tier (op 3) | 501 | ✅ | ✅ |
| Digest recall (`memory.recall`) | 501 | 501 | ✅ |
| Scoped categories (D4 facets) | — | ✅ (5d) | ✅ (5d) |
| Model endpoints needed | none | embedder | embedder + generative |
| Async workers | 5a | 5a, 5b, 5d | 5a–5d |

---

## 5. Cross-cutting: observability & the read-only UI

- **WS-7 (OTel)** is instrumented *inside* every box above — it is not a separate tier. Write-path
  and retrieval spans use `db.*` semconv; LLM-pipeline spans use `gen_ai.*` semconv. The
  **projection-lag metric** (SoT → each projection) is what makes the D3 "observable lag" invariant
  real, and it is also the **evidence source** for the Decision-A promotion triggers (candidate-set
  size, RRF latency, CTE p95). Resource attrs carry `service.name=memkeys`, version, and the
  **deployment profile** (minimal/full) — never fact bodies or PII.
- **WS-8 (Inspector UI)** rides the WS-4 read contract as just another consumer — browse = keyed op,
  graph view = traverse op, search panel = scoped semantic op. It gets no privileged backdoor into the
  stores, so the keyed-first invariant and the tenant floor hold for it automatically. Off the core
  critical path; parallel to this synthesis.

---

## 6. Open items handed to downstream / dependencies

- **WS-4 (retrieval contract, ISI-5869) and WS-5 (MCP spec, ISI-5870)** are in flight. This system
  view is stable at its altitude (topology, profiles, boundary) and consumes their contracts by
  reference; it does not pin their exact request/response JSON. If either contract lands a change that
  moves a *topology* boundary (e.g. a new sync op, or a sixth MCP tool that is not a thin adapter onto
  `/memory/*`), that change should loop back here. Nothing currently indicates it will.
- **SoT final retention horizon** (365d placeholder) remains a product knob (design §5.6).
- **Decision-A promotion thresholds** (the exact pgvector→Qdrant / CTE→graph-DB metric ceilings) are
  deliberately left as `ponytail:` deferrals — set them from real WS-7 numbers, not a guess.

---

## 7. Decision log (this synthesis)

1. **OSS default stack = single-Postgres** (pgvector fact+digest tiers + edge-rows graph + fact SoT),
   keyed-first enforced by the SQL pre-filter; Grail a reference plugin; Qdrant/graph-DB evidence-gated
   promotions by replay (vector/graph are rebuildable projections). **[Decision A, resolved]**
2. **v1 consumer surface = both** — MCP (agent-facing, 5 tools) + direct `POST /memory/facts` (bulk
   gateway/run-settle/telemetry write path); both funnel into one ingest pipeline and one SoT. MCP is a
   thin adapter onto the same `/memory/*` contract, not a second write semantics. **[Decision B, resolved]**
3. **Pluggable boundary = six interfaces** (FactStore, VectorIndex, GraphView, Embedder, Summarizer,
   Categorizer); core is backend-agnostic; models are configuration; a conformant plugin must honor the
   keyed-first contract (pre-filter → ANN → hydrate-from-SoT).
4. **Two planes** — sync request plane (ack gated on keyed commit) and async projection plane
   (change-log-driven, idempotent, independently scalable); reconciler rebuilds holes + stale model
   stamps; a projection outage never blocks writes or keyed reads.
5. **Two (really three) deployment profiles** — minimal (keyed+graph, no models), full-fact-tier
   (+embedder), full-digest (+summarizer); gated by `memory.semantic` / `memory.semantic.digest`;
   disabled semantic ops return `501 feature_disabled`, not silent empties.
