---
adr: llm-pipeline
title: "ADR — LLM processing pipeline & embedding model (WS-3)"
status: proposed
author: Architect (Winston)
date: 2026-10-14
workstream: WS-3
issue: ISI-5868
parent: ISI-5803
depends_on: [WS-0]
feeds: [WS-4, WS-7]
reviewers: [Challenger, Coder]
supersedes_sections: ["architecture.md §5.5 reference defaults"]
---

# ADR — LLM processing pipeline & embedding model (WS-3)

> Deliverable for [ISI-5868](/ISI/issues/ISI-5868), from the approved MemKeys roadmap
> ([ISI-5803](/ISI/issues/ISI-5803)). Companion: [`prompt-contracts.md`](../contracts/prompt-contracts.md).
> Section references (§) point at [`architecture.md`](../architecture.md) unless noted.

## Context

MemKeys is **keyed-first**: the keyed/temporal store is the single source of truth (SoT);
graph and both vector tiers are **derived, rebuildable projections** (D3). The write path is
cheap and synchronous — `normalize → key → temporalize → commit → ack` (§6, stages 1–4) — and
**no model call sits on it**. Everything an LLM or embedder touches is async, post-commit, and
rebuildable.

This ADR nails down the part §6 left as "stage 5": **which models do the derivation work, how
they are versioned, triggered, made idempotent and cost-bounded, and how failure is contained.**
It refines §5.5 (derivation models & versioning) and does not change any SoT behaviour.

Non-goals: the vector store backend (WS-1, ADR-vector), the graph store (WS-2), the retrieval
request/response shapes (WS-4), and the OTel signal catalog (WS-7). This ADR only produces the
spans/metrics those WSes consume — it does not define them.

---

## Decision 1 — Two model roles behind two interfaces

The pipeline uses exactly **two model roles**, each a configured endpoint (not a code
dependency), each swappable per deployment:

| Role | Interface | Does | Default |
|---|---|---|---|
| **Generative** | `Generator.generate(task, input, schema) → structured output` | extract keys, summarize scopes, categorize | **latest Claude tier** (configurable) |
| **Embedding** | `Embedder.embed(texts[], task) → vectors[]` | fact-tier + digest-tier vectors | **BGE-M3 (BAAI)** |

- `task ∈ {extract, summarize, categorize}` selects the prompt contract for the generative role
  and `{query, passage}` asymmetry for the embedding role.
- **This refines §5.5**, which named an "embedder" + a "summarizer" endpoint. WS-3 broadens the
  summarizer into a single **generative** role serving three tasks (one endpoint, three prompt
  contracts) — fewer moving parts, one place to version, one cost meter. The embedder role is
  unchanged in shape.

Rationale: one generative endpoint keeps ops surface minimal (lazy-architecture: don't run three
model services when one serves three prompt contracts). Keeping the roles *distinct* is
non-negotiable — a generative model cannot produce a vector surface, and co-locating them would
couple two independent swap decisions.

## Decision 2 — The async worker set (off the hot path)

Four workers, all triggered **post-commit** off the committed record, all idempotent, all
rebuildable. They map onto §6 stage 5 and the two roles:

| Worker | Role | §6 stage | Trigger | Gating |
|---|---|---|---|---|
| **W-extract** (a) | generative | **5e (new)** | commit event | always (key enrichment) |
| **W-embed** (d) | embedding | 5b + embed-of-5c | commit / digest-ready | `memory.semantic` |
| **W-digest** (b) | generative | 5c | scope dirty (debounced) | `memory.semantic.digest` |
| **W-categorize** (c) | generative | 5d | commit event | `memory.semantic` |

### Decision 2a — LLM-assisted key extraction (W-extract) runs ASYNC, never at ack

The task brief lists "(a) entity-key extraction + validation **at commit**." We read "at commit"
as **triggered by the commit event, executed asynchronously** — *not* synchronously inside stage
4. Putting an LLM call in front of `ack` would reintroduce model latency onto the hot path and
break D3 and the whole keyed-first-is-cheap premise.

Reconciliation with §4 (synchronous key resolution): the two do different jobs and both are kept.

- **§4 / stage 2 (synchronous, no LLM):** deterministic resolution of client-supplied key
  *hints* — verify → enrich → accept-low-trust. Cheap, always runs, gates `ack`.
- **W-extract (async, generative):** when a committed fact is sparsely-keyed, the generative
  model reads `fact.body` and **proposes additional candidate entity keys**. Proposals are
  **validated** against the known entity registry/scope, written back as a **derived-key
  projection** (versioned like any projection), and then re-trigger W-embed/graph so the fact
  becomes reachable by the enriched keys.

**Security boundary (R8 injection containment) — hard rule.** `fact.body` is untrusted agent
content. The generative model's proposed keys are **never authoritative and never overwrite**
client- or server-stamped keys, and the model **never emits provenance**. Proposals are written
only as `derived_key[]` (low-trust, source=`llm-extract`, stamped `derivation_version`) and are
subject to the same validation (§4 verify step) before they widen any candidate set. A fact that
the model mis-reads gains, at worst, an unvalidated-and-rejected proposed key — it can never gain
a forged tenant/provenance attribute.

**Reachability during the async gap.** Between `ack` and W-extract landing, a fact is reachable
by (i) its client/stage-2 keys and (ii) the **tenant floor** (R6, §7.4) — semantic search over
"everything the caller's tenant owns" already covers sparsely-keyed and `unkeyed` facts. So the
async gap costs *finer-key precision latency*, never *findability*. Projection lag is observable
(§6 reconciler) so the gap is measurable.

### Decision 2b — W-digest, W-categorize, W-embed are as specified in §5.4.2 / §6

No change to §6 stages 5b/5c/5d. W-digest is **debounced and coalesced per `(scope, scope_id)`**
(a burst of facts → one rollup). The digest tier stays gated behind the `memory.semantic.digest`
sub-flag (a deployment can buy fact-tier precision without paying for a summarizer). Digest hits
always resolve down to `member_fact_ids` (R1).

## Decision 3 — Embedding model: BGE-M3 default, one embedder for both tiers

**Board-approved default: `BGE-M3` (BAAI)** — multilingual (EN + FR first-class, 100+ langs),
8192-token context, **MIT-licensed & self-hostable**, 1024-dim dense, and uniquely emits
**dense + sparse (lexical) + ColBERT multi-vector in one pass**, which feeds the WS-4 hybrid RRF
fusion (§7.4) directly.

- **One embedder, both tiers.** The same model embeds fact-tier (`fact.body`) and digest-tier
  (scope summary) so the two vector spaces are comparable and RRF-fusable (§7.4). `Embedder.embed`
  takes a `task` arg (`query`/`passage`) for asymmetric retrieval tuning.
- **Not a one-way door (D3).** The vector index is a rebuildable projection, so changing the
  embedder is a **re-embed by replay**, not a migration. Pick a sane default; swap on evidence.
- **This updates §5.5's reference default** from the `bge-small`/`all-MiniLM` (~384-dim,
  CPU-friendly) class to **BGE-M3** as the bundled OSS default. Trade-off acknowledged: BGE-M3 is
  heavier (1024-dim, GPU-preferred) than bge-small. Mitigations: (i) the semantic module is
  already opt-in — a deployment running it has accepted embedding compute; (ii) the **minimal
  deployment runs keyed+graph only** (no embedder at all); (iii) `bge-small`/`e5-small` remain
  first-class swaps via the `Embedder` interface for CPU-only semantic deployments.
  `ponytail:` default tuned for quality on a module you opted into; lighter model is one config
  line away for the CPU-constrained.
- **Swappable alternatives (via the interface).** Self-host: `multilingual-e5-large`,
  `Nomic-embed-v1.5`, `GTE-multilingual`, `Qwen3-Embedding`. Managed API (vendor *plugin*, not
  bundled default): Voyage-3 / voyage-3-large, OpenAI text-embedding-3-large, Cohere
  embed-multilingual-v3, Gemini embedding.
- **Jina Embeddings v3 — API plugin only.** Legitimate (multilingual, 8192 ctx, Matryoshka
  32–1024, task LoRA) but its open weights are **CC-BY-NC (non-commercial)** → cannot be bundled
  as the default in a permissively-licensed OSS server. Slotted as a Jina-**API** plugin, never
  bundled.

## Decision 4 — Versioning: `derivation_version` + the embedding stamp

Every derived artifact already carries `derivation_version = (model_id, prompt_version)` (§5.5).
WS-3 makes it precise per role:

- **Generative artifacts** (digest, story, category facet, derived keys):
  `derivation_version = (model_id, prompt_version)`, where `prompt_version` is the monotonic
  version of the task's prompt contract (`prompt-contracts.md`). Changing a prompt bumps it.
- **Embedding artifacts** (both vector tiers): pin the full embedding identity into **each fact
  record's vector row** —

  ```
  embed_stamp = (embed_model_id, dim, normalization)
  # e.g. ("bge-m3@<build>", 1024, "l2")
  ```

  `embed_model_id` ties to the build; `dim` guards against silent dimension mismatch on a model
  swap; `normalization` (l2 / none) must match what the index was built with or similarity scores
  are meaningless. A read that finds a vector whose `embed_stamp` ≠ active config treats it as
  stale (rebuildable), never as a valid hit.

Pinning per-record (not just per-index) is what makes a mid-flight model swap safe: the index can
hold mixed stamps during a rebuild, and the reconciler drains the stale ones.

## Decision 5 — Change-log trigger (D3): a version bump is a backfill event

The active config declares the current `derivation_version` / `embed_stamp` per role. A change to
any of them (`model_id`, `prompt_version`, `embed_model_id`, `dim`, `normalization`) is a
**change-log event** the reconciler (§6) consumes:

1. Config bump writes a change-log row `(role, task, old_version, new_version, ts)`.
2. The reconciler scans projections whose stamp is stale vs. the new active version and enqueues
   them as **backfill work** (re-embed / re-summarize / re-categorize / re-extract).
3. Backfill runs at low priority behind live writes; **projection lag is the observable SLO**
   (§6 — per-tier lag metric, consumed by WS-7).

Re-derivation is a **first-class backfill, not a migration** (R3): the SoT never changes, only its
projections, and the index can serve old-stamp hits until the new ones land (degrade, don't break).

## Decision 6 — Idempotency keys (per worker)

Every worker is idempotent so retries, replays, and reconciler backfills never duplicate or
corrupt. The natural key per worker:

| Worker | Idempotency key | On re-run |
|---|---|---|
| W-extract | `(fact.id, model_id, prompt_version["extract"])` | upsert derived_key set |
| W-embed (fact) | `(fact.id, embed_model_id, dim, normalization)` | upsert vector row |
| W-embed (digest) | `(digest.id, scope_version, embed_model_id, dim, normalization)` | upsert vector row |
| W-digest | `(scope, scope_id, scope_version, model_id, prompt_version["summarize"])` | upsert digest+story |
| W-categorize | `(fact.id, model_id, prompt_version["categorize"])` | upsert category facets |

`scope_version` (monotonic per `(scope, scope_id)`, bumped when a member fact is added/superseded)
is what lets a debounced digest coalesce correctly: a rollup for an older `scope_version` that
lands late is a no-op against the newer one.

## Decision 7 — Cost controls

The semantic/digest tiers are where spend concentrates. Controls, in order of leverage:

1. **Tier gating (biggest lever).** `memory.semantic` off → zero model spend. `memory.semantic`
   on, digest off → embedding only, **no generative summarizer cost**. Both on → full pipeline.
   (§5.4.2 ruling — unchanged.)
2. **Batch embedding.** W-embed coalesces queued `embed()` calls into **batched requests**
   (N texts per call, N a deployment knob) — dominant cost saver for the embedder.
3. **Debounce generative rollups.** W-digest is debounced/coalesced per scope (§6 5c) — a burst of
   N facts on a thread triggers **one** summarization, not N.
4. **Categorize batching.** W-categorize batches `fact.body` inputs per generative call where the
   contract allows (independent per fact → batchable).
5. **Per-tenant budget + rate caps.** Each tenant has a model-spend budget and a worker rate cap;
   on breach, workers shed to the backlog (keyed writes/reads are never affected) and emit a
   budget metric (WS-7). Fail-soft, never fail-closed on the SoT.
6. **Short-input skip.** Facts below a token floor skip the summarizer (nothing to roll up) and
   embed directly.

## Decision 8 — Retry / backoff + poison handling

Workers are post-commit and **a projection failure never blocks stages 1–4** (R2). Failure policy:

- **Transient failure** (timeout, 429, 5xx): **exponential backoff with jitter**, capped at
  `max_attempts` (default 5). Respect provider `Retry-After` on 429.
- **Poison item** (exhausts retries, or a deterministic failure — e.g. schema-invalid generative
  output that re-fails): moved to a **dead-letter (poison) queue** with the failure reason and the
  last attempt's output. The fact **stays committed and keyed** — only its derived view is
  missing, and it is still reachable by keyed + tenant-floor search.
- **Poison replay.** Poison items are re-attempted automatically on the next `derivation_version`
  bump (a new model/prompt may succeed) and can be drained manually. The reconciler counts poison
  items as projection holes → **projection-lag + poison-depth metrics** (WS-7) make a stuck tier
  alertable.
- **Schema guard on generative output.** Generative workers validate output against the contract
  schema before writing (Decision in `prompt-contracts.md`). A parse/schema failure is a poison
  condition, not a silent bad write — we never persist a hallucinated structure.

---

## Consequences

- **Positive.** Hot path stays model-free (D3 preserved). One generative endpoint + one embedder
  = minimal ops surface. Every derivation is versioned, idempotent, rebuildable, and cost-gated.
  Model/prompt swaps are backfills, not migrations. Failures degrade a projection, never the SoT.
- **Negative / accepted.** BGE-M3 default raises the semantic-module compute floor vs. bge-small
  (mitigated: opt-in module + lighter swaps). W-extract's async gap means finer-key precision
  lags `ack` by the worker latency (mitigated: tenant-floor reachability + observable lag).
- **Follow-ups for siblings.** WS-4 consumes the two-tier RRF output and the `via: fact|digest`
  surfacing. WS-7 owns the concrete `gen_ai.*` / embedding span + metric names (this ADR only
  enumerates *what* must be measured: token/cost counters, projection lag, poison depth, worker
  queue depth, batch size). WS-1 consumes `embed_stamp` as a vector-row column.

## Open questions for review

1. **Challenger:** is the W-extract async-gap acceptable for the primary agent-recall journeys,
   or is there a class of fact that must be finer-keyed *before* it is searchable? (If yes, the
   fallback is a synchronous *deterministic* extractor for that class — still no LLM on the hot
   path.)
2. **Coder (feasibility):** BGE-M3 self-host footprint (GPU/CPU, latency at our batch sizes) and
   whether `Embedder` should expose the sparse + ColBERT outputs in v1 or defer multi-vector to a
   later increment behind the same interface.
3. **Both:** is one generative endpoint for all three tasks right, or do extract/categorize want a
   cheaper/smaller model than summarize? (The interface already allows per-task `model_id`; the
   question is the default.)

## Decision log entry (for §9 / architecture.md)

- **D-WS3.1** Two model roles (generative=Claude-latest, embedding=BGE-M3), two interfaces.
- **D-WS3.2** LLM-assisted key extraction is **async** (W-extract, stage 5e); §4 deterministic
  resolution stays synchronous. R8 containment: proposed keys are low-trust derived-only.
- **D-WS3.3** BGE-M3 bundled OSS default (updates §5.5); one embedder both tiers; Jina = API plugin.
- **D-WS3.4** `embed_stamp = (embed_model_id, dim, normalization)` pinned per vector row;
  generative artifacts stamped `(model_id, prompt_version)`.
- **D-WS3.5** Config version bump = change-log backfill event; re-derive is a backfill, not a migration.
- **D-WS3.6** Per-worker idempotency keys (table above).
- **D-WS3.7** Cost controls: tier-gating → batch → debounce → per-tenant budget caps → short-skip.
- **D-WS3.8** Retry = exp backoff+jitter (≤5); poison → dead-letter, fact stays committed,
  poison-depth observable, replay on version bump.
