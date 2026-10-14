# MemKeys — Reference Architecture & Deployment (System View)

**Status:** Solutioning synthesis (WS-6, ISI-5871) · **Author:** Architect (Winston) · **Date:** 2026-10-14
**Revision:** + Challenger boundary-mover resolutions C1/C3/C4/H1/H6/H7 (ISI-5880 → ISI-5887); + second-pass next-hole closures (ISI-5891); see §8 / §8.1
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

**Promotion path — the two *derived* backends are not one-way doors.** Because vector and graph
are *derived, rebuildable projections* (D3), swapping either is a replay, not a migration:
- Vector → **Qdrant/Milvus** when pgvector scoped-filter latency or recall degrades at real
  scale (promote on evidence — candidate-set size and p95 from WS-7 metrics are the trigger).
- Graph → **Neo4j/Memgraph** only if traversal depth/perf demands outgrow recursive CTE.
- `ponytail:` defer both the dedicated vector store and the graph DB until a WS-7 metric
  (candidate-set size, RRF fusion latency, or CTE traversal p95) crosses a documented ceiling —
  upgrade path is "stand up the new store, point the interface impl at it, replay from SoT."

**The SoT swap is a migration, not a replay — and the doc owns that (C3).** The replay-not-migration
claim above is scoped to the *derived* projections only. `FactStore` is the source of truth; nothing
upstream can regenerate it, so a `FactStore` backend swap (e.g. Postgres → Grail) is a genuine,
deliberate data migration. It is tractable *because* the SoT is an **append-only, backend-neutral
fact log** (the §3 envelope is the schema, and the interface is just `commit/fetch/asof`):
- **Strategy:** export the append-only fact log from the source and **replay-load** it into the
  target through the same `FactStore.commit` path (so target-side temporal/supersession indexing is
  rebuilt natively). The source stays **live** throughout, so the one-shot export is only a
  point-in-time baseline; every commit that lands *after* the snapshot is carried to the target by
  **continuously tailing the source's per-commit change-log** (§2 already emits one row per commit) and
  replaying each row through the same `commit` path. The **dual-read shadow** then compares
  `fetch`/`asof` on both stores, and the success criterion is **bounded catch-up lag** — the tail keeps
  pace with the live head, measured by the same projection-lag metric (§5) — *not* a one-shot "diff is
  zero" against a moving head (which is unreachable while the source keeps committing). Cutover is a
  short **writer quiesce** (admission-pause the single-writer commit pool, §2) held only until the tail
  drains to zero outstanding log rows and the shadow diff is clean on the now-frozen head, then reads
  flip to the target. Projections (vector/graph/digest) are rebuilt on the target by the normal
  reconciler replay — they are *not* part of the SoT migration.
- **It is one-way in the honest sense:** a deliberate, scheduled migration event, not a config flip.
  It is bounded (one append-only log, validated by row-count + content-hash + the shadow window) and
  reversible by keeping the source **authoritative** (the live rollback target) through the shadow and
  going read-only only at the cutover quiesce, until cutover is proven. We do **not** claim
  the SoT is hot-swappable; we claim the migration is *tractable and validated*, which is the honest
  property an append-only backend-neutral log actually buys.

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

**Tenant floor: identical invariant, surface-specific derivation (C4).** The R6 floor —
*every stored fact is pinned to exactly one tenant/project scope, and no read crosses it* — is the
**same invariant** on both surfaces. What differs is **how the authoritative scope is derived from
the caller's identity**, because the two surfaces have different principals:

| | **MCP surface** (agent-facing) | **Direct bulk path** (gateway/run-settle/telemetry) |
|---|---|---|
| **Principal** | the agent's run token (stdio, launch-bound) or per-request bearer (HTTP) | a **producer** service credential — *not* an end-tenant |
| **Scope source** | implicit in the principal: the token's bound tenant/project **is** the floor | explicit **per fact**: each envelope fact declares its `tenant`/`project` |
| **Authorization** | scope args may only *intersect* the principal's tenant; no arg widens it (WS-5) | `fact.tenant ∈ producer.allowed_tenants` (a producer→permitted-tenant allow-list) |
| **Stamped floor** | the principal's tenant | the **declared** `fact.tenant`, *after* the allow-list check |

- **A producer legitimately spans tenants** — flushing many tenants' facts is its whole job — but
  **each individual fact is pinned to exactly one tenant** taken from its envelope, and that tenant
  must be inside the producer's allow-list. A fact asserting a tenant the producer is **not**
  allow-listed for is rejected fail-closed, never silently stamped. This closes the confused-deputy
  write: a producer credential cannot inject a fact into a tenant it was never authorized to write.
- **Mixed-tenant envelope: admit-then-commit, whole-envelope atomicity, enumerated reject (C4 × H1).**
  Because one bulk envelope commits many facts in **one transaction** (§2 H1) and a producer envelope
  may span tenants, authorization is an **admission gate that runs on every fact *before* the
  transaction opens** — not inside it. If every fact passes admission, the whole set commits in one
  transaction (H1's atomicity preserved). If **any** fact fails admission, **nothing commits** and the
  envelope is refused as a whole, with a typed response that **enumerates the offending facts**
  (`{index, declared_tenant, reason}[]`) so the producer can resubmit the valid subset. This is the
  deliberate cut between the two bad options: it is **not** a silent per-fact drop (that would be the
  poison-pill data-loss the "first-class durability" claim forbids), and it is **not** a partial commit
  (that would break H1's one-transaction atomicity and force an ill-defined committed-vs-rejected
  success shape). Whole-envelope fail-closed + enumerated reasons keeps the security floor absolute
  while staying actionable; producers are expected to scope an envelope's facts to a single allow-list
  reach to avoid self-inflicted rejection (the enumerated list makes that self-correcting).
  - **Code disambiguation:** `403 cross_tenant` = the producer is authenticated but a fact declares a
    tenant **outside its allow-list** (an authorization failure). `422 envelope_invalid` = the envelope
    is **malformed** — a fact declares no tenant at all, or a tenant that does not exist (a validation
    failure). Both carry the enumerated per-fact list; a mixed envelope that trips both reports each
    fact under its own class. (The exact wire shape of the enumerated reject list is a WS-5 MCP/HTTP
    contract detail, [ISI-5870](/ISI/issues/ISI-5870); the *semantics* — pre-txn admission,
    whole-envelope atomicity, enumerated reasons, code split — are pinned here.)
- **On both surfaces the stamped scope is server-authoritative and never client-supplied as a
  widening.** MCP cannot widen past its token; the bulk path cannot assert outside its allow-list.
  The floor then governs reads identically (WS-4 Invariant T) regardless of which surface wrote the
  fact — which is the property the "enforced identically" claim was reaching for, now *shown* rather
  than asserted.

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
  categorize+consolidate**, **5e LLM key-extraction** — all **full profile only**, each gated by its
  feature flag (5e additionally needs the generative endpoint; see §2 single-writer notes and §4.2).
  *(The ASCII sketch above draws 5a–5d for legibility; 5e is the generative keying worker in the same
  async pool — full-profile-only, off the hot path.)*
- **Reconciler/backfill**: detects projection holes and stale `(model_id, prompt_version)` artifacts,
  rebuilds by replay from SoT. A projection failure never blocks stages 1–4.

**Scaling grain.** The request plane and each async worker class scale independently. The request
plane is stateless (scale horizontally behind the API surface); workers scale by queue depth; the
stores scale as Postgres does (read replicas for keyed reads; the SoT commit stays single-writer).

**Single-writer SoT — throughput assumption + admission control (H1).** The SoT commit is
deliberately **single-writer** (one logical writer serializes the append-only log and keeps
read-time supersession coherent); it is the one axis that does **not** scale horizontally, and both
entry surfaces — including the high-volume bulk path — converge on it. That convergence is made
**tractable under load** by *what a commit is* plus an explicit admission contract, not by pretending
the writer scales:
- **Throughput assumption.** A commit is stages 1–4 only — normalize → key → temporalize → append —
  with **no model call on the hot path** (WS-3 moved LLM key-extraction to async stage 5e, below). So a
  commit is an indexed append, not an LLM round-trip; a single Postgres primary is *expected* to sustain
  thousands of commits/sec at this shape. This figure is a **documented assumption to be validated by a
  WS-7 write-path benchmark, not yet a measured number** — the benchmark both sets the concrete ceiling
  value and arms the promotion trigger (`ponytail:` same as the Decision-A thresholds in §6: the exact
  commits/sec ceiling is set from real WS-7 numbers, not guessed). Once measured, it is the ceiling WS-7
  watches and the trigger for the usual Postgres write-scaling moves (batching, partitioned append,
  logical-writer sharding by tenant) **before** the single-writer property is ever itself the bottleneck.
- **Admission control / shedding.** The bulk path must never starve the interactive MCP path, and the
  interactive path must never starve bulk to zero. The API surface runs a **bounded commit-concurrency
  pool with two priority classes, each with a non-zero reserved floor**: interactive (MCP
  `memory.write`) holds the larger reserved share; bulk (`POST /memory/facts`) holds a smaller
  **guaranteed floor** and may burst into interactive's unused headroom. Under saturation **bulk sheds
  first — down to, but never below, its floor** — with a typed `429 Retry-After`; the gateway already
  batches and is built to backpressure/retry upstream. The bulk floor + the gateway's **bounded** send
  buffer are what make the "first-class durability" claim honest: a producer that cannot drain within
  the buffer bound receives typed backpressure (never a silent drop), so no fact is lost silently even
  under sustained interactive load. The bulk reserved floor, the gateway buffer bound, and a
  max-shed-duration are the explicit **durability SLA WS-7 watches** for the bulk path. Shedding is
  explicit and typed (never a silent drop or an unbounded queue), consistent with the
  degrade-don't-fail-silently taxonomy.
- **Stage 5e — async LLM key-extraction (full profile only).** The *generative* keying that sync
  stage 2 deliberately keeps off the hot path lives here, in the same async pool as 5b–5d, gated by
  `memory.semantic` and requiring the generative endpoint (so it runs **only** in the full-digest
  profile, never in minimal or fact-tier — see §4.2). It enriches a fact's `entity.*` keys from its
  content *after* commit; sync stage 2 only applies agent-supplied keys and the run/thread anchor. This
  is the premise C1's minimal fail-closed gate depends on: with no 5e in minimal, a minimal fact's only
  keys are agent-supplied `entity.*` + anchor, so a fact carrying neither is genuinely unretrievable and
  the write is refused (§4.1).
- **Batching amortizes the writer.** One bulk envelope = many facts committed in **one
  transaction**, so the high-volume path costs the single writer far fewer transactions than its
  fact count. The projection plane (change-log-driven) was already decoupled; this contract makes the
  *commit* side equally well-behaved under load.

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

> *Diagram note (post-review):* the sketch above shows six ports for legibility; the boundary is now
> **seven** — add **`DigestStore`** (digest membership/body projection; in the OSS default it is one
> more table in the same Postgres, co-located with `FactStore`/`VectorIndex`/`GraphView`). And
> `GraphView.supersedes` is **navigational-only** — authoritative currency lives in `FactStore`
> (see the H6 and H7 boundary rules below).

**Boundary rules:**
- **Seven interfaces** form the entire boundary: `FactStore`, `VectorIndex`, `GraphView`,
  `DigestStore`, `Embedder`, `Summarizer`, `Categorizer`. Everything above is portable core;
  everything below is swappable. (`DigestStore` is the seventh — added by H6; see below.)
- **`DigestStore` — the digest membership/body projection (H6).** A digest hit must resolve to its
  raw member facts (design R1: *a digest is a pointer, never the answer*), but the cut as originally
  drawn had no home for the `story_id → member_fact_ids[]` mapping or the digest summary body — the
  `VectorIndex` port is pure ANN (`embed·upsert·scoped_search·delete·rebuild`) over opaque
  vectors+metadata and should stay that way. So digest membership is its own interface:
  `DigestStore` = `resolve(story_id) → {summary_body, member_fact_ids[], derivation_version}` +
  `upsert_digest · delete`. The `memory.recall` flow is now fully expressible across the cut:
  **`VectorIndex.scoped_search`(digest tier) → `story_id`s → `DigestStore.resolve` → `member_fact_ids`
  → `FactStore.fetch`(members) → hydrate**. `DigestStore` is a *derived, rebuildable projection*
  (like `GraphView` and the vector tiers), **not** part of the SoT — the reconciler rebuilds it by
  replay, so it inherits the replay-not-migration property, not the C3 SoT-migration caveat.
  - **One digest now spans two projections — `derivation_version` is the coherence key.** On each
    roll-up, worker 5c writes the digest **body+members into `DigestStore`** *and* the digest **vector
    into `VectorIndex` (digest tier)** — two stores, two writes, both stamped the same
    `derivation_version`. On a re-roll (changed members/summary/embedding) there is otherwise a window
    where the vector is `v2` but the body/members are still `v1`, so recall could match the `v2` vector
    yet `resolve` a stale `v1` body over stale membership. Resolution: **(a)** where the backend lets
    both writes share a transaction — the single-Postgres default, both are tables in one instance — 5c
    writes them **atomically**, so no skew window exists; **(b)** where it cannot (an external vector
    backend), 5c writes **`DigestStore` first, then the vector**, both carrying the same
    `derivation_version`, and **recall treats a `derivation_version` mismatch as a stale-digest miss** —
    `VectorIndex.scoped_search` (digest tier) must **return the hit's `derivation_version`** to the
    orchestrator, which compares it against `DigestStore.resolve`'s version and, on mismatch, drops the
    hit (re-resolves / degrades to the fact tier) rather than serving a body that does not correspond to
    what matched. Exposing `derivation_version` on the vector hit is a small `VectorIndex`/WS-4 contract
    addition (tracked with [ISI-5869](/ISI/issues/ISI-5869); relates to the `scoped_search` return-shape
    and enumerate-read gaps raised in ISI-5883 H3/H4). The reconciler converges any residual skew on its
    normal replay pass.
  *(Rejected alternative: storing digest bodies as facts in `FactStore` — that would put rebuildable
  derived data in the append-only SoT and break D3; and overloading `VectorIndex` result rows to
  carry+serve bodies would muddy the one pure ANN seam. In the single-Postgres default `DigestStore`
  is just another table in the same instance — zero added operational weight, a modeling seam only.)*
- **Supersession has exactly one authority — `FactStore` (H7).** Currency ("which fact is the
  live one") is computed **read-time over the bi-temporal rows in `FactStore`**, the SoT, and that is
  the *only* authoritative answer. `GraphView.supersedes` stays in the cut but is declared
  **navigational-only / never authoritative**: it exposes the projected supersedes-chain as graph
  topology (for `memory.traverse`, the supersedes-chain walk, and the WS-8 UI), and under projection
  lag it may be stale. Any consumer that needs *correct current state* resolves it from `FactStore`;
  consumers that need the *shape* of the chain use `GraphView` and tolerate lag, which the WS-7
  projection-lag metric makes observable. This generalizes the WS-4 rule (graph/vector are never the
  *body* authority) to: **graph is never the *currency* authority either.**
  - **Navigational-only is enforced by the return type, not by a naming convention.**
    `GraphView.supersedes` returns **pure chain topology** — the node ids and the superseded-by edges
    of the chain — and carries **no `is_current` / `head` / liveness marker** of any kind. Currency is
    therefore *unobtainable* from `GraphView` **by construction**: a consumer (the WS-8 UI, a plugin,
    `memory.traverse`) cannot read a "which one is live" answer out of it to cache, because the type
    never exposes one. The named cache-staleness attack (cache a `supersedes` result, treat it as
    current) cannot land — there is nothing current-shaped to cache. Any currency answer must come from
    a `FactStore` read, and such reads **must not be cached past the read** (the SoT is the only place
    liveness is defined, and it is bi-temporal/read-time). So the two interfaces cannot be read as
    *disagreeing on currency* — not merely because "only one is consulted" by convention, but because
    only one of them can **express** a currency answer at all.
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
- **Writes fail closed on unretrievable facts (C1).** Minimal has no semantic tier, and design R6
  makes a fact with no keyed-queryable selector reachable *only* via semantic. Such a fact written to
  minimal would be a **write-only black hole** — stored in the SoT, served by no live op (op 1 needs a
  key; op 2 needs an edge). Minimal therefore **rejects the write fail-closed** rather than accept an
  unreadable fact: after the pipeline's key stage (§2 stage 2), if a fact carries **no keyed selector
  any live op could target** — no `entity.*` key, and no anchor key (`run_id`/`thread_id`) that op 1
  (`memory.get`) actually accepts as a selector, and it **declares no graph edge** — the write is
  refused with a typed `422 unkeyed_requires_semantic`, telling the producer to supply keys or use the
  **full** profile. (The ambient tenant/project scope is a *floor*, not a retrieval selector, so it does
  not by itself make a fact retrievable.) Two sharp edges in this gate, resolved explicitly:
  - **The anchor-key carve-out is contingent on a WS-4 op-1 contract point (tracked).** The carve-out
    "`run_id`/`thread_id` makes a fact retrievable" is only true if **op 1's `WHERE` accepts anchor keys
    as first-class `memory.get` selectors** — not merely as inputs stage 2 consumes to *derive*
    `entity.*`. That selector set is owned by WS-4 ([ISI-5869](/ISI/issues/ISI-5869), in flight, §6).
    **Fail-safe either way:** if WS-4 pins anchor keys as op-1 selectors, the carve-out stands as
    written; if it does **not**, the gate tightens to require a **resolved `entity.*`** (anchor-only no
    longer counts), which is strictly *more* fail-closed. The invariant — *no unretrievable fact is
    silently stored in minimal* — holds under both outcomes; only the breadth of the carve-out depends
    on WS-4. This is folded here as an explicit WS-4 dependency, not left implicit.
  - **The gate keys off *declared* edge intent, not projected `GraphView` state.** Graph edges are
    produced by async worker 5a **post-commit**, so at the sync stage-2 admission point the `GraphView`
    for this fact is necessarily empty — "has no graph edge" is **not** evaluable against the projection.
    The gate therefore tests the fact's **declared edge intent in the envelope** (synchronously known at
    admission), not the projected graph. A fact that declares an edge is admitted (op 2 will reach it
    once 5a lands); a fact that declares none, carries no key, and no accepted anchor is refused.
  We deliberately **do not** grow a GC/retention sink
  in minimal to collect facts nobody can read — a subsystem whose only job is to delete the
  unreadable is exactly the operational weight the "runs on a laptop" profile exists to avoid. Bulk
  producers that emit genuinely unkeyed blobs (e.g. `telemetry_learning`) therefore require the full
  profile; in minimal they get an explicit, early, typed rejection instead of silent accumulation.
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
| Async workers | 5a | 5a, 5b, 5d | 5a–5e (5e = LLM key-extraction, needs generative endpoint) |

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
3. **Pluggable boundary = seven interfaces** (FactStore, VectorIndex, GraphView, DigestStore, Embedder,
   Summarizer, Categorizer); core is backend-agnostic; models are configuration; a conformant plugin
   must honor the keyed-first contract (pre-filter → ANN → hydrate-from-SoT). *(DigestStore added by
   H6; see §8.)*
4. **Two planes** — sync request plane (ack gated on keyed commit) and async projection plane
   (change-log-driven, idempotent, independently scalable); reconciler rebuilds holes + stale model
   stamps; a projection outage never blocks writes or keyed reads.
5. **Two (really three) deployment profiles** — minimal (keyed+graph, no models), full-fact-tier
   (+embedder), full-digest (+summarizer); gated by `memory.semantic` / `memory.semantic.digest`;
   disabled semantic ops return `501 feature_disabled`, not silent empties.

---

## 8. Challenger boundary-mover resolutions (ISI-5880 → ISI-5887)

The Challenger red-team of this document (ISI-5880, 2026-10-14) raised 16 findings; 10 were doc-level
and fixed in place on ISI-5871. **Six moved a topology boundary and needed an Architect decision
before WS-6 finalizes.** All six are resolved below and folded into §§1–4 above. Each resolution is a
*smaller cut*, not a new subsystem — the pattern throughout is "name the honest property the design
already has, and fail closed where it didn't."

| # | Finding | Resolution | Folded into |
|---|---|---|---|
| **C1** | Minimal is a write-only black hole for `unkeyed` facts | **Fail closed.** Minimal rejects a write with no keyed selector any live op can target (`422 unkeyed_requires_semantic`); no GC/retention sink added. Unkeyed bulk blobs require the full profile. | §4.1 |
| **C3** | `replay-not-migration` wrongly covers the SoT | **Scope the claim to the derived stores.** FactStore swap is a *genuine migration* — but tractable: export the append-only backend-neutral fact log → replay-load via `commit` → dual-read shadow → cutover. Honest property = *validated/bounded*, not *hot-swappable*. | §1 Decision A |
| **C4** | Tenant floor "enforced identically" asserted, not shown | **Same invariant, surface-specific derivation.** MCP: scope implicit in the token. Bulk: scope declared per-fact in the envelope, checked against a producer→allowed-tenants allow-list; out-of-list → fail-closed. A producer spans tenants, each fact pins to one. Closes the confused-deputy write. | §1 Decision B |
| **H1** | Single-writer SoT, both surfaces converge, no backpressure | **Documented throughput assumption + admission control.** Commit is an indexed append (no hot-path model call) → thousands/sec on one primary. Bounded commit pool w/ two priority classes; interactive reserved, bulk sheds first via typed `429 Retry-After`; bulk batches many facts/txn. | §2 |
| **H6** | `VectorIndex` cut can't express digest hydrate | **Add a 7th interface, `DigestStore`** (`resolve(story_id)→{summary_body, member_fact_ids[], derivation_version}`, `upsert_digest`, `delete`). Keeps `VectorIndex` pure ANN. Derived/rebuildable projection (not SoT). Recall = search(digest)→resolve→fetch(members)→hydrate. | §3 |
| **H7** | Supersession behind two interfaces can disagree | **`FactStore` is the sole currency authority** (read-time over bi-temporal rows). `GraphView.supersedes` declared **navigational-only / never authoritative** — kept for chain traversal/UI, tolerates lag (WS-7 observable). Only one authority is ever consulted, so they cannot disagree. | §3 |

**Net boundary change from this review:** the pluggable boundary goes **six → seven interfaces**
(adds `DigestStore`); two interface contracts gain explicit authority annotations
(`GraphView.supersedes` navigational-only; `FactStore` sole currency + sole SoT-migration owner);
the minimal profile gains a fail-closed write rule; and §2 gains an admission-control contract. No
new runtime component is introduced in the OSS default — `DigestStore` is another table in the same
Postgres, and admission control lives in the existing API surface. The keyed-first invariant and the
two-plane topology are unchanged.

### 8.1 Second pass — next-hole closures (ISI-5891 re-verification)

The Challenger re-verified the six resolutions ([ISI-5891](/ISI/issues/ISI-5891)) and confirmed **all
six boundary decisions hold**, but landed one *next-hole* on each. None reverted a boundary; three were
in-doc defects/overstatements and three were open contract gaps. All six are now closed in §§1–4 above:

| # | Next-hole | Closure (folded above) |
|---|---|---|
| **C3** | "until the diff is zero" is unreachable under a live source | §1 Decision A: one-shot export is only a baseline; the shadow **continuously tails the source change-log**, success criterion is **bounded catch-up lag**, cutover is a short **writer quiesce** until the tail drains — not a diff-vs-moving-head. |
| **H1·a** | "safe" / "measured" overstated; bulk has no floor | §2: softened to "tractable under load"; throughput is a **documented assumption to be validated by a WS-7 benchmark** (not measured); bulk gets a **non-zero reserved floor** + **bounded gateway buffer** + **max-shed SLA** so first-class durability is honest. |
| **H1·b** | stage 5e referenced but placed nowhere | §2 worker inventory + §2 single-writer note + §4.2 capability matrix now carry **5e (LLM key-extraction, full-profile-only, needs generative endpoint)** with explicit profile gating. |
| **C1** | anchor carve-out depends on an ungranted op-1 selector; gate reads async graph state | §4.1: anchor carve-out folded as an **explicit WS-4 op-1 dependency** with a **fail-safe to require resolved `entity.*`** if WS-4 declines; gate keys off **declared edge intent**, not the post-commit `GraphView`. |
| **C4** | mixed-tenant single-envelope atomicity + partial-reject shape undefined | §1 Decision B: **pre-txn admission gate**, **whole-envelope atomicity** (all-or-nothing, no partial commit, no silent per-fact drop), **enumerated reject list** `{index, declared_tenant, reason}[]`, and **`403 cross_tenant` vs `422 envelope_invalid`** disambiguated. |
| **H6** | `derivation_version` skew between `DigestStore` body and `VectorIndex` digest vector | §3: 5c co-writes both **atomically where the backend allows** (single-Postgres default); otherwise body-first + same version + **recall treats a version mismatch as a stale-digest miss**; `scoped_search` must **return the hit's `derivation_version`** (WS-4 contract add, tracked). |
| **H7** | "cannot disagree" rested on an unenforced convention | §3: `GraphView.supersedes` **returns pure chain topology with no `is_current`/head marker** — currency is **unobtainable from `GraphView` by construction**, so the cache-as-current attack cannot land; currency reads come from `FactStore` and are not cached past the read. |

**Downstream-tracked contract items** (semantics pinned here, wire detail owned downstream): C1 anchor-selector
set → WS-4 [ISI-5869](/ISI/issues/ISI-5869); C4 enumerated-reject wire shape → WS-5 [ISI-5870](/ISI/issues/ISI-5870);
H6 `derivation_version` on the vector hit → WS-4 [ISI-5869](/ISI/issues/ISI-5869) (relates to ISI-5883 H3/H4).
