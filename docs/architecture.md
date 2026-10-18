# MemKeys — Architecture

**Project:** MemKeys — the attribute-based memory server (facts keyed by agent-supplied attributes, temporal validity, graph + vector as derived views, open contract, pluggable backends).
**Status:** Architecture baseline (promoted from design study rev 3 — two-tier vector + scoped categories; ISI-5803 pipeline decisions folded in)
**Layer:** Temporal memory contract
**Reference backend:** Grail + DQL (portable; not a lock-in)
**Repo home:** `github.com/MemKeys/memkeys` → `docs/architecture.md` (this file). The design originated in the AgentMesh repo (`docs/design/memory-organization.md`); that copy is now **archival input**, superseded by this one.

> **Promotion note (Architect, ISI-5865, 2026-10-14):** this document was promoted verbatim
> from the AgentMesh design study (`memory-organization.md`, rev 3) into the MemKeys repo as the
> canonical architecture baseline. It is the **landing pad for the WS-1…WS-6 ADRs** approved in
> the MemKeys Solutioning Roadmap (ISI-5803 plan doc, rev 4, board-accepted 2026-10-14):
> subsequent ADRs extend and refine the sections below rather than restating them. Nothing in the
> technical body changed on promotion — the **keyed-first invariant** (§2, Decision D3: the
> keyed/temporal store is the single source of truth; graph + vector are derived, rebuildable
> projections) is preserved exactly. One naming reconciliation is deferred to the WS ADRs: the
> reference bucket is still named `agentmesh_facts` (§5) from the design-phase repo; renaming it to
> a MemKeys-native identifier is a substantive change that belongs in an ADR, not this doc-move.

> **Rev 3 (Architect, ISI-5808, 2026-10-14):** folds in the four decisions Henrik locked in
> the ISI-5803 pipeline brainstorm — **D1** dual-vector surface (→ two-tier vector model, §5.4.2),
> **D2** async scope-level rollup summarization (§5.4.2 digest tier, §6 stage 5c), **D3**
> source-of-truth + async rebuildable projections (already the design's stance; made explicit
> in §5.4 and §6), **D4** hybrid scoped categories as attributes/facets (§5.4.3, §6 stage 5d).
> The four open items the brainstorm couldn't close are ruled on in §9 (items 5, 6, 11, 12): R6
> sparse-key retrievability (tenant-scope floor, §7.4), retention horizon (projection ≤ SoT
> invariant, §5.6), summarizer/embedder model + `(model_id, prompt_version)` versioning (§5.5),
> and digest-tier gating + RRF weighting (§5.4.2, §7.4).

## 0. Ecosystem positioning (Henrik, 2026-10-13)

The memory server is **one of three distinct projects** in this ecosystem:

| Project | What it is | This doc's relationship |
|---|---|---|
| **AgentMesh** | The A2A mesh for agents on k8s — gateway, standing agents, A2A listening, lifecycles. *Not* the memory server. | Consumer of the memory contract; the gateway flush is the primary write path (§3) |
| **MemKeys** (this project) | Standalone OSS deliverable: the attribute-based memory server — keyed by agent-supplied attributes, temporal, graph+vector derived views, open contract, Grail plugin. | **This is what this architecture specifies** (`github.com/MemKeys/memkeys`) |
| **k8squad** | Improves its own agent memory by *consuming* this contract, and evolves its architecture in the AgentMesh style (gateway/supervisor/standing agents, ISI-5799) | Consumer; k8squad-scoped attributes (`k8squad.project`, etc.) are one namespace convention the server supports |

**Repo home (resolved).** MemKeys now has its own repo (`github.com/MemKeys/memkeys`) and Paperclip project. The design originated in the AgentMesh workstream; this promotion (ISI-5865) moves the canonical architecture here. The AgentMesh copy (`docs/design/memory-organization.md`) is retained as archival design input.

**Horizon item:** a k8s-native packaging of this server (CRD-driven, running on-cluster, consumed by k8squad and other k8s agent platforms) is a natural v2 once the open contract stabilizes. Not a separate workstream yet.

## 1. Summary

Agents produce *facts*: a decision taken on a pull request, the outcome of a run, a
learning derived from telemetry, the context an agent flushes at the end of a session.
Every fact is *about something* — a project, an issue, a PR, an agent, a run, a thread.
Those "somethings" are the **entities** (agent-supplied attributes), and they are the
primary index into memory. Any agent platform — AgentMesh, k8squad, or another — is a
*consumer* of this contract; the server is attribute-agnostic.

The memory layer is organized in three storage layers and four retrieval operations:

**Storage layers (single source of truth + derived views):**
- **Keyed / temporal store (source of truth).** Grail `agentmesh_facts` — every fact lives
  here once, append-only, bi-temporal. This is what the server writes at commit time.
- **Knowledge graph (derived, async).** Graph edges projected from the keyed store:
  entity → entity relationships, supersedes-chains, cross-fact links. Maintained
  server-side after the keyed write; rebuildable by replay.
- **Vector index — two tiers (derived, async, optional).** The semantic surface is **two
  complementary tiers**, not one embedding (D1 + D2, §5.4.2):
  - **Fact tier** — embeds the raw `fact.body` (precision; faithful to ground truth; always
    present when the semantic module is on).
  - **Digest tier** — embeds scope-level **rollup summaries** (thread / run / entity) produced
    by an async summarizer (context/recall; navigational). A digest hit *always* resolves down
    to its underlying raw facts — the summary is never the answer. Gated behind a sub-flag so a
    deployment can run fact-tier precision without paying the summarization cost.
  All derived views activate on demand — a minimal deployment runs keyed+graph only.
- **Categories (derived attributes, async).** Emergent LLM labels consolidated into a
  per-project scoped controlled vocabulary, persisted as facets on the keyed record (D4,
  §5.4.3). Categories *are* attributes — they join the keyed/scoped filter surface, not a
  separate store.

**Four retrieval operations (in priority order):**
1. **Keyed / structured access (default path, hot).** Fact → (project/issue/pr/agent/run/thread
   + time-range) → direct DQL fetch. Deterministic, no embeddings, no graph traversal.
   Serves the overwhelming majority of agent reads.
2. **Graph traversal (derived, warm).** Navigate relations, supersedes-chains, entity links —
   "what did this agent decide in the last sprint?", "what superseded this fact?"
3. **Semantic search (derived, scoped, optional).** Similarity over a key-bounded candidate
   set — "similar past incidents within this project". Caller supplies attribute filters;
   server pre-filters then ranks; results hydrated back from the keyed store.
4. **Write-with-keys (hot, caller-attributed).** The envelope path in §3; server does all
   modeling.

The **server** does the modeling. Clients push raw facts; the server normalizes them,
extracts and validates the entity keys, applies temporal semantics (validity windows and
supersedes-links), writes the authoritative keyed record, and maintains the semantic
index as a derived, rebuildable view. Clients stay dumb — identical to how a gateway
flush already works.

```
   client (dumb)                      memory server (does the modeling)                storage
 ┌──────────────┐   raw fact        ┌──────────────────────────────────────┐    ┌─────────────────────────────┐
 │ gateway flush │ ───envelope────▶ │ normalize → key → temporalize        │──▶ │ keyed store (Grail)          │ ◀─ source of truth
 │ run settle    │                  │    └─ commit ──────────────────────── │    │  agentmesh_facts bucket      │
 │ A2A exchange  │                  │                                       │    └────────────┬────────────────┘
 └──────────────┘                  │ async derived-view maintenance:        │                 │ replay
                                    │  project graph edges + supersedes     │──▶ graph store  │ (rebuildable)
                                    │  embed fact.body (if enabled)         │──▶ vector index │ (rebuildable, optional)
                                    └──────────────────────────────────────┘
```

## 2. Why keyed-first

Most memory reads an agent issues are not fuzzy. "Pick up issue X" wants *everything
known about X* — prior decisions, the last run outcome, the handoff note the previous
agent left. That is a deterministic lookup by a key, not a similarity search. Making the
default path a keyed fetch means:

- **Deterministic & reproducible.** The same key returns the same set; no ranking drift,
  no embedding-model dependence, no false neighbors.
- **Cheap.** No vector math on the hot path; a filtered scan over an indexed dimension.
- **Portable.** Any store with "filter records by field + time range" can serve it. The
  semantic tier, which is the part that varies most across backends, is pushed to the edge
  as a replaceable index.
- **Honest failure mode.** A keyed miss means "nothing recorded about X", not "the
  embedding didn't surface it". Discovery ambiguity is confined to the semantic tier where
  it belongs.

Semantic search earns its place for the genuinely open question ("has anyone hit a
NetworkPolicy blackhole like this before?") — but it is answered *within* a keyed scope
wherever one is known, and never as the primary retrieval path.

## 3. The fact envelope (on the wire)

> The envelope contract is co-owned with the Product Manager, who governs what gateways,
> run-settle, and A2A exchanges actually push. **PM ruling — ISI-5801 (2026-10-13):** the
> strawman direction (raw in, server does the rest) is *confirmed*. The envelope below
> folds in the ruling's adjustments, and §3.1 is replaced by the ratified
> required / allowed / server-owned matrix. Clients stay dumb: a producer sends only what
> it *natively knows* without modeling — one anchor key plus the fact body — and the
> server derives the rest.

A producer pushes a **raw** fact. The envelope carries *hints*, not authority — the
server is the authority on keys, time, and supersession.

```jsonc
{
  "source": {
    "kind": "gateway_flush | run_settle | a2a_exchange | telemetry_learning",  // REQUIRED
    "producer": "<agent or gateway id>",     // advisory only — server-verified against caller principal, overwritten on conflict
    "emitted_at": "<RFC3339 timestamp>"      // REQUIRED — client clock; default for fact.event_time
  },
  "entity": {                                  // CLIENT-ASSERTED KEY HINTS — validated/enriched server-side; send AT LEAST this source's anchor
    "project": "<id>",                         // hint; normally server-enriched, not sent
    "issue":   "<id>",                         // hint; normally server-enriched, not sent
    "pr":      "<id>",                         // hint; normally server-enriched, not sent
    "agent":   "<id>",                         // = the AUTHENTICATED producer agent (single, server-verified)
    "run":     "<id>",                         // anchor for gateway_flush / run_settle
    "thread":  "<id>",                         // anchor for a2a_exchange
    "service": "<id>",                         // anchor-hint for telemetry_learning; server maps service→project
    "counterpart_agent": "<id>"                // a2a only; HINT-ONLY, corroborated against thread membership, never written as a verified key
  },
  "fact": {
    "type": "decision | outcome | observation | learning | context",  // optional — server defaults per source.kind if omitted
    "body": "<natural-language text OR structured payload>",           // REQUIRED — the one thing only the producer can supply
    "subject": "<the thing within the entity this fact is about>",     // optional; REQUIRED when `supersedes` is set
    "event_time": "<optional RFC3339; when the thing happened; defaults to emitted_at>",
    "confidence": 0.0,                         // optional (server-defaulted); SHOULD be set for type=learning
    "supersedes": "<optional prior fact id this fact replaces>"        // server validates target shares (entity,subject,type)
  },
  "provenance": { },                           // MUST be absent/empty — server strips & stamps (principal, custody, trace); a populated block is ignored, never merged
  "attachments": [ ]                           // optional refs (doc ids, blob uris) — never inlined
}
```

Design rules:

- **Raw in, modeled server-side.** The producer never computes keys, validity windows,
  or embeddings. It reports what it saw.
- **`entity` is a hint block, not trust.** The server validates each asserted key and may
  *enrich* missing ones from context (§4), but never writes a custody-sensitive key on a
  client's unverified say-so. A key that fails validation is recorded as a rejected-hint
  (§4), never silently dropped.
- **One mandatory anchor per source.** Each source must send at least the one key it
  natively knows (the enrichment pivot — see §3.1). Everything else in `entity` is
  optional; the server enriches it. A fact with no resolvable anchor is still stored but
  tagged `unkeyed` (§4) — tolerated only for `telemetry_learning`.
- **`fact.body` is the only always-required payload.** `source.kind`, `source.emitted_at`,
  and the per-source anchor are the other required fields. `fact.type` is optional and
  defaults per `source.kind` (gateway_flush→context, run_settle→outcome,
  a2a_exchange→observation, telemetry_learning→learning).
- **`provenance` is server-owned and rejected-if-populated.** Principal, custody, and trace
  context are stamped from the verified caller, mirroring how run provenance is already
  stamped — copied from verified headers, never from the body. A client-supplied
  `provenance` block is discarded whole, not merged.
- **`entity.agent` is always the authenticated producer agent** (single, server-verified).
  A2A's second participant is a **hint** — either enriched from the `thread`'s participant
  set or asserted in `counterpart_agent` and corroborated against it; it is never written
  as a verified custody key on one agent's say-so about another.
- **`subject` is the supersession grain.** It groups facts that are successive versions of
  the same statement (e.g. "the chosen database"), so the server can link supersedes and
  the reader can ask for the *current* value. It is **required whenever `supersedes` is
  set** — you cannot version a statement without naming it.

### 3.1 Per-source required / allowed / server-owned matrix (PM ruling, ISI-5801)

The governing principle: **required = what the producer natively knows without modeling**
(one anchor key + the fact body); **allowed = hints it may also report**, which the server
validates and enriches; **server-owned = never accepted from the client body**. The anchor
is the single key the server pivots on to enrich the rest (§4).

| Source | Default `fact.type` | **Required** (client must send) | **Allowed** (hints — validated/enriched) | **Server-enriched from anchor** | **Server-owned / rejected from body** |
|---|---|---|---|---|---|
| `gateway_flush` | `context` | `source.kind`, `emitted_at`, **`entity.run`**, `fact.body` | `agent`, `thread`, `subject`, `event_time`, `confidence`, `supersedes`, `attachments` | `project`, `issue`, `pr` (from run binding); `agent`, `thread` corroborated | `producer` identity, authoritative `agent` binding, `provenance.*` |
| `run_settle` | `outcome` | `source.kind`, `emitted_at`, **`entity.run`**, `fact.body` | `agent`, `subject`, `event_time` (= run end), `confidence`, `supersedes`, `attachments` | `project`, `issue`, `pr`, `agent` (from run record — authoritative) | `producer` identity, `provenance.*` |
| `a2a_exchange` | `observation` | `source.kind`, `emitted_at`, **`entity.thread`**, `fact.body` (+ `subject` when `type=decision`) | `counterpart_agent`, `issue`, `pr`, `subject`, `event_time`, `confidence`, `supersedes`, `attachments` | `issue`, `pr` (from thread's work item); participant set (from thread) | verified `agent` (producer, from principal), `provenance.*`; **counterpart never written as a verified key** |
| `telemetry_learning` | `learning` | `source.kind`, `emitted_at`, **`entity.service` OR `entity.project`**, `fact.body` | `issue`, `pr` (rare), `subject`, `event_time`, `confidence` (SHOULD set), `supersedes`, `attachments` | `project` (from service→project map) | `provenance.*` |

**Notes on the ruling:**

- **The anchor is the only mandatory `entity` key.** `gateway_flush` and `run_settle` pivot
  on **`run`** (resolves to project/issue/pr/agent via the run registry). `a2a_exchange`
  pivots on **`thread`** (resolves to the work item and participant set). This is what lets
  a dumb client push only its run/thread id and still produce a fully-keyed fact.
- **`agent` is never required in the body** — it is verified from the authenticated
  principal (gateway_flush) or read from the run record (run_settle). A client assertion is
  advisory and overwritten on conflict.
- **A2A's "agent ×2" is resolved without expanding custody.** `entity.agent` = the verified
  producer agent; the counterpart is enriched from the thread's participant set, or supplied
  as a corroborated `counterpart_agent` **hint**. One agent cannot mint a verified key *about
  another* — this is the custody line.
- **`telemetry_learning` sends the service it observed, not a computed project.** The
  service→project mapping is server enrichment (§4.2), preserving the dumb-client invariant.
  `issue`/`pr` are usually absent and that is fine; if nothing resolves, the learning is
  stored `unkeyed` (semantic-only) rather than rejected. This is the one source for which
  `unkeyed` is an expected, acceptable outcome.
- **`confidence` SHOULD be set for `learning`** (learnings are inherently probabilistic);
  it is optional and server-defaulted for every other type.

## 4. Key extraction (the write-time modeling)

Keys come from three places, in descending authority:

1. **Verified from the caller.** `source.producer` and the `agent` key are bound to the
   authenticated principal — the server overwrites any client claim that disagrees.
2. **Enriched from context.** A `run` id resolves, via the run registry, to its
   `project` / `issue` / `pr`. A `thread` id resolves to its work item. This is where a
   dumb client gets away with pushing only what it naturally knows (its run id) and still
   produces a fully-keyed fact. Enrichment is **corroborated**, never guessed.
3. **Accepted as asserted (low-trust fields only).** Free-form keys that carry no custody
   weight are taken as given and flagged with their (hint) provenance so a reader can
   weigh them.

A fact that cannot be resolved to **at least one** durable entity key is still stored, but
tagged `unkeyed` and is reachable only through the semantic tier — it never pollutes a
keyed result set. Silent key-dropping is forbidden: an un-resolvable asserted key is
recorded as a rejected-hint on the fact, not discarded.

## 5. Store layout on Grail

Grail is a schema-on-read, append-only record lakehouse; DQL is its pipe-oriented query
language. The mapping:

### 5.1 Bucket & record shape

One custom bucket, `agentmesh_facts`, with retention set to the memory horizon (e.g.
365d; tune per deployment). Each processed fact is **one immutable record**:

```
timestamp          // = fact.event_time (the bi-temporal "valid/event" axis)
ingested_at        // Grail's own ingest time (the bi-temporal "transaction" axis)
fact.id            // server-assigned, stable, idempotency key
fact.type          // decision | outcome | observation | learning | context
fact.subject       // supersession grain (nullable)
fact.body          // text or serialized structured payload
fact.confidence
fact.supersedes    // prior fact.id (nullable)
entity.project     // ─┐
entity.issue       //  │  first-class keyed dimensions
entity.pr          //  │  (indexed filter targets)
entity.agent       //  │
entity.run         //  │
entity.thread      // ─┘
provenance.principal, provenance.custody, provenance.trace_id, provenance.source_kind
keyed              // bool — false ⇒ semantic-only
```

**Append-only is a feature, not a limitation.** Grail records are immutable, so we do
**not** mutate a prior fact to close its validity window. Instead:

- A new version is a new record carrying `fact.supersedes = <old id>`.
- "Currently valid" is **computed at read time**: the latest non-superseded record per
  `(entity, subject, type)`. The full history stays queryable for audit and as-of replay.

This is standard bi-temporal modeling (event time vs. transaction time) and it falls out
naturally from an append-only log.

### 5.2 DQL — keyed fetch (the default path)

Everything known about an issue, currently valid, newest first:

```dql
fetch logs, from: now() - 365d
| filter bucket == "agentmesh_facts"
| filter entity.issue == "<ISSUE_ID>"
| filter keyed == true
// keep only the current version per (subject,type): drop any record whose id was superseded
| join [ fetch logs, from: now()-365d
         | filter bucket == "agentmesh_facts" and entity.issue == "<ISSUE_ID>"
         | filter isNotNull(fact.supersedes)
         | fields superseded = fact.supersedes ],
       kind: leftouter, on: { left: fact.id, right: superseded }, prefix: "s."
| filter isNull(s.superseded)                 // this record was not replaced by a newer one
| sort timestamp desc
| fields fact.id, fact.type, fact.subject, fact.body, fact.confidence,
         entity.agent, entity.run, timestamp, provenance.principal
```

For the prototype, a simpler **latest-wins** collapse is acceptable and is the query the
roundtrip in §8 uses:

```dql
fetch logs, from: now() - 365d
| filter bucket == "agentmesh_facts" and entity.issue == "<ISSUE_ID>"
| sort timestamp desc
| dedup { entity.issue, fact.subject, fact.type }   // keep newest per statement
| fields fact.id, fact.type, fact.subject, fact.body, timestamp, entity.agent, entity.run
```

Other keyed shapes are the same filter with a different dimension:

```dql
// everything an agent learned, last 30d
| filter entity.agent == "<AGENT_ID>" and fact.type == "learning"

// everything about a PR
| filter entity.pr == "<PR_ID>"

// a run's settle outcome
| filter entity.run == "<RUN_ID>" and fact.type == "outcome"
```

### 5.3 DQL — temporal "as-of" query

What was known about an issue at a past instant (point-in-time replay):

```dql
fetch logs, from: now() - 365d
| filter bucket == "agentmesh_facts" and entity.issue == "<ISSUE_ID>"
| filter timestamp <= toTimestamp("2026-09-01T00:00:00Z")   // event-time ≤ as-of
| filter ingested_at <= toTimestamp("2026-09-01T00:00:00Z") // and we actually knew it then
| sort timestamp desc
| dedup { fact.subject, fact.type }
```

Filtering on **both** axes answers two different questions precisely: "what was true then"
(event time) and "what had we recorded by then" (transaction time).

### 5.4 Derived views: knowledge graph + vector index

Both derived views are projections of the keyed store. They share three invariants:

1. **Rebuildable.** Losing either costs a replay, not data. The keyed store is the authority.
2. **Written async after the keyed commit.** The client's `ack` is gated on the Grail write,
   not on the derived views — a reader racing the ingest queue sees a momentarily stale graph
   or vector index, never a stale keyed record.
3. **Maintained by the server.** Clients never write graph edges or embeddings directly. This
   is the same dumb-client invariant as the envelope.

#### 5.4.1 Knowledge graph (default derived tier)

The graph is present in all deployments (no opt-in required). Each processed fact projects
**edges** into the graph:

| Edge type | Source | Target | Condition |
|---|---|---|---|
| `entity_fact` | entity node (project/issue/pr/agent/run) | fact node | always |
| `supersedes` | new fact node | prior fact node | when `fact.supersedes` is set |
| `produced_by` | fact node | agent node | always (from `provenance.principal`) |
| `temporal` | fact node | time-bucket node | for navigating by epoch |
| `a2a_participant` | fact node | counterpart agent node | `a2a_exchange` facts with resolved counterpart |

**Graph traversal enables queries** that are awkward or expensive in pure DQL:
- "What is the supersession chain for statement X on issue Y?" → walk `supersedes` edges
  forward from the anchor fact.
- "What did agent A decide last sprint across all its issues?" → expand `produced_by` inverse
  + `temporal` filter; hydrate from keyed store.
- "What entities is this fact connected to?" → expand `entity_fact` edges.

The graph is **never hydrated body from its own store**. It carries node ids and edge
metadata only. Body retrieval always goes back to the Grail keyed store (§5.2), keeping the
graph lean and the source of truth singular.

#### 5.4.2 Vector index — two tiers (optional, activates on demand)

The vector index is an opt-in module. A minimal AgentMesh deployment (intended for early
contributors and resource-constrained environments) runs keyed+graph only. Enabling it
requires: an embedding model endpoint, a vector store backend, and the `memory.semantic`
feature flag.

**Why two tiers (D1 + D2).** A single embedding of `fact.body` is precise but cannot answer
"what happened in this thread / run / entity" — that question is about a *span* of facts, not
one. Embedding the raw fact (precision) and a derived scope summary (recall) are not two
versions of the same vector; they serve different recall needs and compose into two tiers:

| Tier | Embeds | Serves | Gating |
|---|---|---|---|
| **Fact** | raw `fact.body` | precision — "the fact that says X" | on when `memory.semantic` is on |
| **Digest** | scope rollup summary (+ server "story"/lineage) | context/recall — "what happened in thread/run/entity T" | on when `memory.semantic.digest` sub-flag is also on |

**Invariant (R1, telephone-game).** The fact tier is *always* present whenever the semantic
module is on, so a ground-truth-faithful vector always exists. A **digest-tier hit must always
resolve down to its underlying raw facts + provenance** (via the scope→member-fact mapping
recorded with the digest). The summary is never returned as the answer — it is a pointer into
ground truth. This is why no summarization sits on the hot write path: digests are a
navigational projection, not a record.

**Gating — why digest is a sub-flag, not always-on (open item 4 ruling, §9).** The digest tier
needs a rollup *summarizer* (an extra model endpoint + async summarization cost) that the fact
tier does not. A deployment should be able to buy fact-tier precision without paying for
summarization. So the gating is three-step: `memory.semantic` off → keyed+graph only;
`memory.semantic` on, `memory.semantic.digest` off → **fact tier only**; both on → both tiers +
RRF fusion (§7.4). Always-on was rejected: it forces a summarizer on resource-constrained
deployments that only want precision search, violating the minimal-deployment principle.

When the module is enabled:
- **Fact tier.** On each successful keyed write, the server enqueues `embed(fact.id, fact.body)`
  (idempotent on `fact.id`). The fact-vector store holds `{fact.id, embedding, entity.*,
  fact.type, derivation_version}` — **keys and vector only, never body text**.
- **Digest tier** *(if `memory.semantic.digest` on).* The async scope-rollup summarizer (§5.5,
  pipeline stage 5c) produces one digest per `(scope, scope_id)` — e.g. per thread, per run,
  per entity — re-rolled as member facts arrive (debounced; not per-write). The digest-vector
  store holds `{digest.id, scope, scope_id, embedding, member_fact_ids[], story_id,
  derivation_version}`. `member_fact_ids` is the resolve-down path that enforces the R1
  invariant; `story` is the server-generated processing-lineage artifact (§5.5), kept distinct
  from envelope provenance per the ISI-5801 ruling.
- On disable (either flag), the affected index is dropped; re-enable triggers a replay/rebuild
  from `agentmesh_facts` (fact tier) and a re-rollup (digest tier).

**Scoped semantic search** (the caller's attribute filters — dual-tier):

```
1. scope   : caller supplies attribute filters — entity.project, entity.issue, fact.type,
             category facets, etc. Server translates to a candidate id set (DQL keyed fetch →
             ids, or graph hop). THIS IS THE HARD MULTI-TENANCY BOUNDARY (§7.4).
2. rank    : ANN search over the scoped candidate set by similarity to the query text,
             run against BOTH tiers (fact = precision, digest = context) when digest is on;
             fuse the two ranked lists with Reciprocal Rank Fusion (RRF, §7.4).
3. resolve : digest hits expand to member_fact_ids; de-dup against fact-tier hits.
4. hydrate : DQL fetch of the resulting top-k fact ids from agentmesh_facts
             (authoritative body + provenance) — the R1 resolve-to-ground-truth step.
```

The scope step is what makes the index multi-tenant safe: team A's facts never surface in
team B's similarity results because the candidate set is pre-filtered by project/team key
before the ANN pass. Caller supplies the scope; server enforces it. The digest tier does **not**
widen this boundary — a digest is itself scoped to the entity/thread/run it rolls up, so it is
subject to the same pre-filter.

```dql
-- step 4 (same pattern regardless of how the id list was produced — fact or resolved-digest)
fetch logs, from: now() - 365d
| filter bucket == "agentmesh_facts"
| filter in(fact.id, array("<id1>","<id2>","<id3>"))
| fields fact.id, fact.type, fact.body, entity.issue, entity.pr, timestamp, provenance.principal
```

**Benchmark rationale (Henrik, 2026-10-13):** hybrid graph+vector systems (e.g. Zep
KG+vector) dominated temporal-recall tests over pure-vector stores. The graph captures
relation and supersession structure that embeddings miss; the vector captures lexical and
semantic proximity the graph doesn't model. The two-tier split extends the same logic to the
vector layer: the fact tier captures the proximity of a single statement, the digest tier
captures the proximity of a *span* of related statements — complementary, not redundant.

#### 5.4.3 Categories as scoped facets (D4, derived)

Categories are a **hybrid emergent→controlled** classification, and they are **attributes, not a
separate store**: once consolidated they live as facets on the keyed record and join the same
filter surface as `entity.*` keys.

- **Emergent.** Pipeline stage 5d (§6) runs an LLM categorizer over `fact.body` and proposes
  free-form labels. The categorizer is **constrained to emit from / extend a fixed schema** (a
  label string + optional confidence) — it never emits structured keys or provenance (R8
  injection containment).
- **Controlled, per-project scope.** A periodic consolidation job maps the emergent labels into
  a **scoped controlled vocabulary** owned by the project (the anchor scope). Near-duplicate and
  low-frequency labels are merged/pruned so the vocabulary cannot explode or leak across projects
  (R4). The consolidation is itself a rebuildable projection.
- **As facets.** The consolidated category is written back as a `category[]` facet on the keyed
  record (a derived attribute, versioned like any projection). It is then available as an
  additional pre-filter in both keyed fetch and the semantic scope step — "decisions tagged
  `rollout-risk` on project P".

Because categories are scoped to the anchor project and governed by consolidation, they do not
become an uncontrolled free-tag swamp, and they never cross the multi-tenancy boundary.

### 5.5 Derivation models & versioning (open item 3 ruling, §9)

The design does **not** hard-pick a vendor — portability is the project's whole point, and the
semantic tier is exactly the part that varies most across deployments (§2). It specifies the
*contract* and ships safe, swappable reference defaults.

- **Two configured endpoints, both replaceable.** An **embedder** endpoint (fact + digest
  vectors) and a **summarizer** endpoint (digest rollups + "story"). Both are configuration, like
  the existing embedding endpoint — not code dependencies.
- **Reference defaults (minimal deployment).** Embedder: a small open model in the
  `bge-small` / `all-MiniLM` class (CPU-friendly, ~384-dim). Summarizer: a small open
  instruct model. These are *defaults to get running*, explicitly expected to be swapped for a
  deployment's preferred models. No proprietary model is required to run the server.
- **The "story" artifact** is a short, server-generated processing-lineage note the summarizer
  emits alongside the rollup ("rolled up N facts on thread T spanning <range>; sources: …"). It
  is a **server artifact, distinct from envelope provenance/subject** (ISI-5801, R7). Provenance
  = who/what/trust of a fact; story = how the server derived this digest.
- **Versioning — `(model_id, prompt_version)` stamped on every derived artifact.** Every vector,
  digest, story, and category facet carries a `derivation_version = (model_id, prompt_version)`.
  `model_id` identifies the embedder/summarizer build; `prompt_version` is a monotonic string
  bumped whenever a summarizer/categorizer prompt changes. The active config declares the current
  `derivation_version`; the reconciliation/backfill job (§6) treats any artifact whose stamp is
  stale as rebuildable work and re-derives it. **Re-embedding and re-summarizing are first-class
  backfills, not migrations** (R3, model drift) — the keyed source of truth never changes, only
  its projections.

### 5.6 Retention horizon (open item 2 ruling, §9)

Retention is **tiered**, with one hard invariant.

- **Source of truth (keyed/temporal store).** Keeps the product memory horizon — 365d
  placeholder, tunable per deployment (§5.1). This is the only horizon that governs what memory
  *exists*.
- **Derived projections (graph, both vector tiers, digests, category facets).** Inherit the SoT
  horizon by default, and **may be configured shorter** to cap index cost — they are rebuildable
  from SoT within the SoT window, so a shorter projection horizon loses nothing permanent.
- **Hard invariant: projection horizon ≤ SoT horizon.** A projection must never retain a
  derived view of a fact whose authoritative record has expired — otherwise search could surface a
  hit that cannot be hydrated (violating the §7.5 response-shape invariant). The server enforces
  this at config load: a projection horizon set longer than SoT is clamped down with a warning.
- **Digests specifically** default to the full SoT horizon (they are the recall surface and are
  cheap relative to raw fact storage); a deployment may shorten them like any projection.

## 6. Write pipeline (server-side stages)

```
ingest ─▶ [1 normalize] ─▶ [2 key] ─▶ [3 temporalize] ─▶ [4 commit] ─▶ ack
                                                                │
                                              async derived-view maintenance (off the hot path)
                                              ├─ [5a] project graph edges
                                              ├─ [5b] embed fact.body            → fact-vector tier   (if memory.semantic)
                                              ├─ [5c] scope rollup → summary+story→ embed → digest tier (if …semantic.digest)
                                              └─ [5d] categorize → consolidate   → category facets     (if memory.semantic)
```

**Stages 1–4 are synchronous, cheap, and authoritative** (D3). The `ack` is gated on stage 4
only — an agent's write latency never depends on embedding, summarization, or categorization.
**Stage 5 is a set of async, rebuildable projections off the committed record** (D2, D3); every
5-stage artifact is stamped with `derivation_version` (§5.5).

1. **Normalize.** Validate envelope schema; canonicalize `fact.type`; stamp `ingested_at`;
   assign `fact.id` (stable, idempotency key so retried flushes don't duplicate).
2. **Key.** Resolve entity keys per §4 (verify → enrich → accept-low-trust). Mark `keyed`.
   Attach rejected hints.
3. **Temporalize.** Set the event-time axis (`fact.event_time` or `emitted_at`). If
   `supersedes` is set, validate the target shares `(entity, subject, type)`. Compute no
   write-back — supersession is read-time (§5.1).
4. **Commit (keyed write).** Write the authoritative record into `agentmesh_facts` (Grail
   ingest). This is the **commit point**: `ack` is sent to the client as soon as this
   succeeds. The client is never blocked on derived-view maintenance.
5. **Async derived-view maintenance** (post-commit, idempotent, retryable; each artifact
   stamped with `derivation_version`, §5.5):
   - **5a — Graph projection.** For each entity key in the committed record, upsert the
     corresponding entity node and write the `entity_fact` edge; write `supersedes` edge if
     `fact.supersedes` is set; write `produced_by` edge from `provenance.principal`; write
     `temporal` bucket edge; write `a2a_participant` edge for resolved A2A counterparts. All
     idempotent on `fact.id` so retries are safe.
   - **5b — Fact-vector embedding** *(if `memory.semantic` enabled).* Enqueue
     `embed(fact.id, fact.body)` to the configured embedder; write `{fact.id, embedding,
     entity.*, fact.type, derivation_version}` to the fact-vector tier. Idempotent on `fact.id`.
     Skipped entirely when the module is disabled — no stub written.
   - **5c — Digest rollup** *(if `memory.semantic.digest` enabled).* Mark the fact's owning
     scopes (thread / run / entity) dirty and enqueue a **debounced** per-scope rollup — the
     summarizer regenerates the scope's digest + "story" lineage from its current member facts,
     the embedder embeds the summary, and the result is written to the digest tier as `{digest.id,
     scope, scope_id, embedding, member_fact_ids[], story_id, derivation_version}`. Debounced and
     coalesced per `(scope, scope_id)` so a burst of facts triggers one rollup, not N. Never on
     the hot path (D2). Idempotent per scope version.
   - **5d — Categorize** *(if `memory.semantic` enabled).* Run the schema-constrained categorizer
     over `fact.body` → emergent labels; the periodic consolidation job folds them into the
     project-scoped controlled vocabulary and writes `category[]` facets back onto the keyed
     record's derived-attribute projection (§5.4.3). Idempotent on `(fact.id, derivation_version)`.

Stages 1–3 are pure and backend-agnostic. Stage 4 is the only synchronous backend write.
Stages 5a–5d run in the ingest queue: order-independent per fact, serialized per supersedes
chain (so the `supersedes` edge is written only after both endpoints are committed) and
debounced per scope (5c). A derived-view outage never blocks writes or keyed reads; it only
delays graph/semantic/category freshness.

**Reconciliation / backfill job.** A background reconciler detects (a) projection lag or holes
(a committed fact with no graph edge / fact vector / category facet, or a dirty scope with no
current digest) and rebuilds them, and (b) artifacts whose `derivation_version` is stale versus
the active config (§5.5) and re-derives them. **A projection failure never blocks stages 1–4**
(R2). **Projection lag is an observable metric** — the reconciler exports per-tier lag so a
deployment can alert on a derived view falling behind its source of truth.

## 7. Retrieval contract (backend-portable)

Four operations, all returning the same **fact shape** (authoritative record hydrated from
the keyed store). Every operation is expressible against any backend that implements:
"filter by field + time range" (keyed), "graph hop" (graph), and "nearest-neighbor"
(semantic, optional).

### 7.1 Operation 1 — write-with-keys (hot)

```
POST /memory/facts
     { ...envelope per §3... }
  → { fact.id, keyed: bool, entity_keys_resolved: {…} }
```

The response carries the server-resolved entity keys so the caller can cache them for
subsequent keyed reads without a lookup round-trip.

### 7.2 Operation 2 — fetch-by-keys (hot, O(1) per key)

```
GET  /memory/facts
     ?entity.issue=…   &entity.pr=…   &entity.agent=…   &entity.run=…   &entity.project=…
     &type=decision|outcome|observation|learning|context   (optional)
     &as_of=<RFC3339>    (optional; point-in-time replay, §5.3)
     &valid=current|all  (default: current — collapses supersession)
     &limit=…
  → { facts: [ { fact.id, type, subject, body, confidence, entity, event_time, provenance } ] }
```

At least one `entity.*` key is required. Multiple keys are ANDed (intersection). This maps
to the DQL `fetch | filter` patterns in §5.2–5.3 — no graph or vector touch.

### 7.3 Operation 3 — graph traversal (warm)

```
POST /memory/graph/traverse
     { "start": { "kind": "fact|entity", "id": "…" },
       "edges": ["supersedes", "entity_fact", "produced_by", "temporal", "a2a_participant"],
       "direction": "out|in|both",
       "depth":  2,
       "hydrate": true   // if true, fetch bodies from keyed store for leaf nodes
     }
  → { nodes: [ { kind, id, …keyed record if hydrate=true… } ],
      edges: [ { from, to, type } ] }
```

`edges` is a filter — omit to walk all edge types. `hydrate: false` returns graph structure
only (node ids + edge topology), useful for introspection without the Grail fetch.

### 7.4 Operation 4 — semantic search (optional module, scoped)

```
POST /memory/search
     { "query": "<text>",
       "scope": {                          // bounded by the caller's tenant floor (see R6 below)
         "entity.project": "…",           // caller-supplied scope attributes are ANDed
         "entity.issue":   "…",
         "entity.agent":   "…",
         "fact.type":      "…",
         "category":       "…"            // D4 facet — categories are attributes, usable as scope
       },
       "k":      10,
       "tiers":  "auto|fact|digest",       // optional; default auto = both tiers fused when digest on
       "as_of":  "<RFC3339>"               // optional; scopes candidate set to facts known by then
     }
  → { hits: [ { fact.id, score, fact.type, fact.subject, fact.body,
                entity, event_time, provenance,
                via: "fact|digest", digest_scope: {…}? } ] }  // via/digest_scope show how the hit surfaced
```

**Scope is bounded by the caller's tenant, never unbounded (open item 1 / R6 ruling, §9).**
The attribute pre-filter is the hard multi-tenancy boundary (§5.4.2) and stays mandatory — but
"the caller must always name a finer key" is what made a **mis-keyed or sparsely-keyed fact
invisible** to search. Resolution:

- The **minimum scope is the caller's tenant/principal**, which the server *always* knows from
  the authenticated caller — it is never supplied by, nor omittable by, the client. A search
  with no finer `entity.*` filter runs against *everything the caller's tenant owns*, including
  `unkeyed` and sparsely-keyed facts. This is **not global search**: the tenant boundary is still
  enforced; team A never sees team B.
- Finer `entity.*` / `category` filters narrow *within* that tenant floor and are ANDed as before.
- This makes the `unkeyed` tier (§4) reachable: a telemetry learning that resolved to no
  project is still found by its owning tenant's semantic search, closing the R6 recall hole
  without widening the isolation boundary.

The server:
1. Translates the tenant floor + any scope attributes/categories to a candidate id set (keyed
   DQL → ids, or graph hop for entity-centric scopes).
2. Runs ANN over the candidate set against the enabled tiers: fact tier (precision) and, when
   `memory.semantic.digest` is on and `tiers` permits, the digest tier (context).
3. **Fuses the two ranked lists with Reciprocal Rank Fusion** — `score = Σ 1/(k_rrf + rank_i)`,
   `k_rrf = 60` (standard). Default tier weights are **equal (fact 1.0 / digest 1.0)** and are a
   deployment knob; a deployment that wants precision-first can raise `fact_weight`. Equal-weight
   was chosen as the default because the tiers are complementary (precision vs. recall), not
   ranked — let RRF rather than a hand-tuned prior decide (open item 4 ruling, §9).
4. Resolves digest hits to their `member_fact_ids`, de-dups against fact hits, and hydrates full
   records from `agentmesh_facts` (R1 resolve-to-ground-truth; same as §5.4.2 step 4).

Returns a 501 `feature_disabled` when the `memory.semantic` module is not enabled, rather
than a silent empty result — callers can degrade gracefully. (Digest-only requests when the
digest sub-flag is off degrade to the fact tier with a warning header, not a 501.)

### 7.5 Response shape invariant

All four operations return facts in the same shape: the authoritative record from the keyed
store. Operations 3 and 4 never carry body text from the graph or vector index — they carry
ids; the hydration step in §5.2 is always the final authority. This is what allows callers
to mix retrieval operations and still get coherent, authoritative results.

### 7.6 Principal↔tenant binding — known M3 limitation (ISI-6137 S1)

§7.4/R6 states the governing invariant: *the tenant floor is the caller's authenticated
principal — "never supplied by, nor omittable by, the client."* **The M3 transport does not
yet realize that invariant, and ships with a documented deviation from it.**

- **What M3 ships.** Both transports accept the tenant as a *self-asserted* value — the
  `X-MemKeys-Tenant` HTTP header and the MCP `tenant_id` tool argument — gated (on HTTP) only
  by a single optional global bearer token (`MEMKEYS_AUTH_TOKEN`). There is no per-principal
  token whose claims *derive* the tenant floor. stdio has no principal check at all; it trusts
  the launch-bound pipe (unlike the WS-5 §2.1 launch-bound run token, which M3 does not verify).
- **Consequence.** Any caller who clears the single token (or reaches an untokened server) can
  assert *any* tenant id → **cross-tenant read/write impersonation**. This is a confidentiality
  and integrity boundary violation, not a degraded-mode inconvenience.
- **Decision (Architect, Winston).** Full per-tenant authz is **deferred out of the M3 transport
  milestone**. M3's charter is transport wiring (HTTP + MCP over the existing service); replacing
  the tenant-trust model with token-claim-derived floors is a security-critical change to the
  contract on *both* transports and must land as its own reviewed, integration-tested PR — not
  bundled into transport plumbing where it cannot be reviewed on its merits. Bundling it here
  would make an un-reviewable PR and risk shipping an under-tested authz path, which is worse
  than a clearly-fenced known limitation.
- **Mitigation shipped in M3 (ISI-6137 B3).** The server binds **loopback by default**
  (`127.0.0.1`). A non-loopback bind with no `MEMKEYS_AUTH_TOKEN` is **refused at startup**
  unless the operator sets `MEMKEYS_ALLOW_INSECURE=1`, which also emits a loud warning. The
  fail-open admission surface is therefore not network-reachable in the default deployment, and
  single-tenant / trusted-loopback deployments are safe to run today.
- **Closure path.** A dedicated follow-up issue wires token-verified principals into the tenant
  floor on both transports: HTTP verifies a per-request bearer token and derives `tenant_id`
  from its claims (never the header); stdio binds the principal once at launch from the run
  token (§2.1). Reference groundwork already exists in the M3 branch as an uncommitted module
  (`crates/memkeys-server/src/auth.rs` — an HS256 `JwtVerifier` deriving the R6 floor from
  `company_id`, plus a `StaticVerifier` for the stdio launch binding) and the `wire.rs` §7 error
  taxonomy (`unauthenticated`/`forbidden`/`not_found` collapse). That follow-up, not M3, is where
  §7.4/R6 becomes enforced rather than asserted.

Until that follow-up lands, **MemKeys M3 must not be exposed to mutually-distrusting tenants on
a shared network bind.** This limitation is restated in the ISI-6077 PR body.

## 8. What the first prototype proves

**Roundtrip: flush → key → hydrate-by-issue.**

```
Agent A (end of session, working issue X)
  └─ POST /memory/facts  envelope { source.kind: gateway_flush,
                                    entity.run: R,                 // A knows only its run id
                                    fact: { type: context, body: "<handoff note>" } }

Server
  ├─ normalize  → fact.id = F
  ├─ key        → enrich entity.run=R ⇒ entity.issue=X, entity.project=P   (§4 enrichment)
  ├─ temporalize→ event_time = emitted_at
  └─ index      → write record to agentmesh_facts (keyed=true); enqueue embed(F)

Agent B (later, picks up issue X)
  └─ GET /memory/facts?entity.issue=X&valid=current
       └─ server runs the §5.2 latest-wins DQL
       └─ returns A's handoff context, keyed directly by issue — no embeddings touched
```

Success criteria:

- B retrieves A's flushed context by **issue key alone**, with zero semantic involvement.
- The enrichment step turns A's `run`-only envelope into a fully `issue`-keyed record
  (proves server-side modeling, dumb client).
- A re-sent (retried) flush with the same idempotency key produces **no duplicate**.
- The record is visible to an as-of query at a timestamp after the flush and invisible
  before it (proves the temporal axis).

What the prototype deliberately does **not** yet prove: supersession chains at scale, the
semantic rebuild, cross-backend portability of `/memory/search`, and all of the rev-3 derived
surfaces — the **two-tier vector fusion** (§5.4.2), the **async digest rollup** (§6 stage 5c),
and **scoped categories** (§5.4.3). Those build on the keyed roundtrip and are follow-ups once
it is solid: the keyed/temporal source of truth is the foundation every rev-3 projection is
rebuilt from, so proving it first is the correct order.

## 9. Decision log

1. **Server-side processing responsibility** — **CONFIRMED (Henrik, 2026-10-13):** server
   does normalize/key/temporalize/index; dumb clients push raw envelopes.
2. **Semantic index as a derived, rebuildable view** — **CONFIRMED (Henrik, 2026-10-13):**
   semantic tier is never source of truth; the portability seam is the keyed/temporal store.
3. **Envelope contract** — **RESOLVED (PM, ISI-5801, 2026-10-13):** strawman direction
   confirmed; §3 envelope and §3.1 matrix are the ratified contract. One mandatory anchor
   per source; `fact.body` the only always-required payload; `provenance` rejected-if-
   populated; A2A counterpart is a corroborated hint, never a verified key.
4. **Hybrid graph+vector** — **ADOPTED (Henrik, 2026-10-13):** knowledge graph (always on)
   + vector index (opt-in module) as derived views of the keyed store. Graph captures
   relation and supersession structure; vector captures semantic proximity; both are
   rebuildable. Benchmark evidence: hybrid systems dominated temporal-recall tests.
   Scoped semantic search (caller supplies attribute filters, server pre-filters before ANN)
   is the multi-tenancy enforcement mechanism. Minimal deployment = keyed+graph only.
5. **Retention & horizon** — **RESOLVED (Architect, ISI-5808, 2026-10-14):** tiered, with a hard
   invariant *projection horizon ≤ SoT horizon* (§5.6). SoT keeps the product horizon (365d
   placeholder, tunable — still a product knob for the final number); projections inherit it and
   may be shortened to cap index cost since they are rebuildable. Digests default to the full SoT
   horizon.
6. **`unkeyed` facts policy / sparse-key retrievability (R6)** — **RESOLVED (Architect,
   ISI-5808, 2026-10-14):** `unkeyed` facts remain stored-but-semantic-only, tolerated as an
   expected outcome only for `telemetry_learning` (unchanged). The retrievability hole is closed
   by the **tenant-scope floor** (§7.4): semantic search is always bounded by the caller's tenant
   but needs no finer key, so `unkeyed`/sparsely-keyed facts are findable within their owning
   tenant without ever widening the multi-tenancy boundary. *(Board confirmation of the
   `telemetry_learning`-only tolerance still welcome, but no longer blocks search recall.)*

   The following fold in the four decisions Henrik locked in the ISI-5803 pipeline brainstorm:

7. **D1 — Dual-vector / two-tier model** — **ADOPTED (Henrik, ISI-5803, 2026-10-14):** embed both
   the raw fact (**fact tier**, precision) and the scope rollup summary (**digest tier**, recall),
   §5.4.2. Invariant R1: a digest hit always resolves to its raw member facts + provenance; the
   summary is never the answer.
8. **D2 — Async scope-level rollup summarization** — **ADOPTED (Henrik, ISI-5803, 2026-10-14):**
   summarization is a debounced per-scope async projection (pipeline stage 5c), never on the hot
   write path. Pairs with D1 to produce the digest tier.
9. **D3 — Source-of-truth + async rebuildable projections** — **CONFIRMED (Henrik, ISI-5803,
   2026-10-14):** restates and reinforces the design's existing stance — the keyed/temporal store
   is authoritative; graph, both vector tiers, digests, and category facets are change-log-driven
   rebuildable projections (§5.4, §6). Reconciler makes projection lag observable and rebuilds
   holes/stale artifacts.
10. **D4 — Hybrid scoped categories** — **ADOPTED (Henrik, ISI-5803, 2026-10-14):** emergent LLM
    labels consolidated into a per-project scoped controlled vocabulary, persisted as `category`
    facets (§5.4.3). Categories are attributes → they join the keyed/scope filter surface; scoping
    + consolidation contain explosion/leak (R4).
11. **Derivation models & versioning** — **RULED (Architect, ISI-5808, 2026-10-14):** no vendor
    lock-in; embedder + summarizer are configured, swappable endpoints with small-open-model
    reference defaults; every derived artifact stamped `(model_id, prompt_version)`; re-embed /
    re-summarize are first-class backfills (§5.5, R3).
12. **Digest-tier gating & RRF weighting** — **RULED (Architect, ISI-5808, 2026-10-14):** digest
    tier is **opt-in behind a `memory.semantic.digest` sub-flag** (not always-on — it carries a
    summarizer cost a precision-only deployment shouldn't pay); dual-tier results fused with
    **standard RRF (k=60), equal default weights (fact 1.0 / digest 1.0)**, tier weights a
    deployment knob (§5.4.2, §7.4).
