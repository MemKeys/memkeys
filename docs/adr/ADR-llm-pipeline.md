---
adr: llm-pipeline
title: "ADR — LLM processing pipeline & embedding model (WS-3)"
status: accepted
author: Architect (Winston)
date: 2026-10-14
last_updated: 2026-10-14
workstream: WS-3
issue: ISI-5868
remediation_issue: ISI-5888
review: ISI-5881 (Challenger, 16 findings — all remediated)
feasibility: ISI-5882 (Coder, feasible; 4 improvements folded)
parent: ISI-5803
depends_on: [WS-0]
feeds: [WS-4, WS-7, WS-1]
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
| **Generative** | `Generator.generate(task, input, schema) → structured output` | extract keys, summarize scopes, categorize | **pinned Claude builds, per task** (below) |
| **Embedding** | `Embedder.embed(texts[], task) → vectors[]` | fact-tier + digest-tier vectors | **BGE-M3 (BAAI)** |

- `task ∈ {extract, summarize, categorize}` selects the prompt contract for the generative role
  and `{query, passage}` asymmetry for the embedding role.
- **Default `model_id` is a pinned build string, never a floating alias (F17).** The shipped
  default is a *specific* Claude build id (e.g. `claude-<tier>@<yyyy-mm-dd-build>`), not
  "latest". A floating "latest" alias would let a vendor release silently change the active
  `model_id`, and under Decision 5 **any** `model_id` change is a fleet-wide change-log backfill
  event — so "latest" would trigger an unplanned re-derivation storm across every tenant (and
  re-burn every tenant's budget, see Decision 8 / F9). Upgrading the default tier is therefore a
  **deliberate config change** (new pinned build → change-log row → metered backfill), never an
  automatic follow of the vendor's alias.
- **Per-task tier defaults (Coder-2, ISI-5882).** Per-task `model_id` costs **zero added ops
  surface** — all three tasks hit the same Claude provider/API (one integration, one auth, one
  cost meter), so Decision 1's "one endpoint = minimal ops" rationale is fully preserved *while*
  down-tiering the cheap tasks:
  - `summarize` → **mid tier** (pinned Sonnet-class build) — faithfulness-sensitive multi-fact
    synthesis that feeds the digest vector; recall quality depends on it.
  - `extract`, `categorize` → **small tier** (pinned Haiku-class build) — schema-bounded,
    single-fact, temperature-0 tasks where the small tier holds parity (structured-output/tool
    mode is supported on the small tier, so schema forcing still works).
  - A single `generative.quality_floor` config bumps all three to one tier for max-quality
    deployments; a per-task **eval gate** (small labeled set) confirms small-tier parity before the
    down-tier ships. Default down-tiered; floor is one line up. The per-task `model_id` still obeys
    the pinned-build rule above (no floating alias).
- **This refines §5.5**, which named an "embedder" + a "summarizer" endpoint. WS-3 broadens the
  summarizer into a single **generative** role serving three tasks (one provider integration,
  three prompt contracts) — fewer moving parts, one place to version, one cost meter. The embedder
  role is unchanged in shape.

Rationale: one generative provider integration keeps ops surface minimal (lazy-architecture: don't
run three model services when one API serves three prompt contracts at three pinned tiers).
Keeping the roles *distinct* is
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
  **validated** against the known entity registry/scope and written back as a **derived-key
  projection** (versioned like any projection). The fact then becomes reachable by the enriched
  keys through the **keyed/graph candidate-id pre-filter** — the derived keys are indexed into the
  keyed lookup and graph edge set so a keyed fetch / traverse by the new key hits the fact.
  **Re-triggering W-embed is *not* the mechanism (F15):** W-embed keys on
  `(fact.id, embed_model_id, dim, normalization)` and embeds `fact.body`, which is immutable — new
  keys change nothing the embedder sees, so a re-embed would upsert an identical vector (a no-op).
  The trigger that must fire on key enrichment is the **keyed/graph index update** for the fact's
  candidate ids; it is idempotent on `(fact.id, derivation_version["extract"])` (upsert the
  derived-key index rows). W-embed is re-triggered only when `fact.body` itself changes, which
  W-extract never does.

**Security boundary (R8 injection containment) — hard rule.** `fact.body` is untrusted agent
content. The generative model's proposed keys are **never authoritative and never overwrite**
client- or server-stamped keys, and the model **never emits provenance**. Proposals are written
only as `derived_key[]` (low-trust, source=`llm-extract`, stamped `derivation_version`) and are
subject to the same validation (§4 verify step) before they widen any candidate set. A fact that
the model mis-reads gains, at worst, an unvalidated-and-rejected proposed key — it can never gain
a forged tenant/provenance attribute.

**Custody-key restriction (F4) — hard rule.** `allowed_key_space` for W-extract is limited to the
**low-custody** dimensions only: `entity.project`, `entity.issue`, `entity.pr`, `entity.thread`,
`entity.entity`. The **high-custody pivot keys `entity.agent` and `entity.run` are never
proposable** by the generative model. §4 binds `agent` to the authenticated principal
(server-overwritten) and enriches `run` only by corroboration — and the "§4 verify" W-extract
relies on is **existence validation against the registry, not ownership/corroboration
validation**. A registry-existence check alone would let a poisoned body naming a real-but-
unrelated `entity.run=R99` cross-link the fact into another context's run. So `agent`/`run` are
(a) excluded from `allowed_key_space` and (b) passed into the contract's `existing_keys` block, so
even a malformed proposal naming them has no overwrite path and is dropped. Custody dims are
set exclusively by the synchronous §4 path, never by the async LLM path.

**`evidence_span` is anti-confabulation, not anti-injection (F5).** The verbatim-substring check
on `evidence_span` only stops the model from *inventing* a value absent from the text. It gives
**~zero protection against injection**, because in the R8 threat the attacker *controls the text*:
any value they want to inject (a victim's `ISI-9999`) is trivially a verbatim substring and passes
the check. Injection containment rests **solely** on (i) the custody-key restriction above,
(ii) registry/scope validation (§4 verify), and (iii) low-trust `derived_key[]` tagging — never on
`evidence_span`. The contract states this explicitly so no reader mistakes the span check for an
injection guard.

**Reachability during the async gap.** Between `ack` and W-extract landing, a fact is reachable
by (i) its client/stage-2 keys and (ii) in a **semantic-enabled** deployment the **tenant floor**
(R6, §7.4) — semantic search over "everything the caller's tenant owns" already covers
sparsely-keyed and `unkeyed` facts. In that deployment the async gap costs *finer-key precision
latency*, never *findability*.

**Keyed-only deployments need a synchronous fallback (F1) — hard rule.** The tenant floor is a
**semantic-tier** mechanism (gated behind `memory.semantic`). In the **keyed+graph-only** minimal
deployment Decision 3 promotes, there is *no* tenant floor, so the reachability argument above does
not hold: during the gap, a fact whose only path to `entity.issue=X` would be LLM extraction is
invisible to a keyed fetch by issue — the §8 primary recall journey — with zero fallback. This is
the fact class answering open-question 1: *facts whose target key is not derivable by the §4
deterministic enricher and that live in a keyed-only deployment*. Resolution:

- A **synchronous deterministic key-extractor** runs in stage 2 (no LLM, on the hot path, cheap),
  covering the **well-formed-reference key class** — identifiers matchable by pattern against the
  known registry (e.g. `ISI-1234` → `entity.issue`, `#123`/PR urls → `entity.pr`, `scope:`/`run:`
  tokens resolved only by *corroboration*, never pattern alone for custody dims). It writes
  server-stamped keys (not low-trust) because a validated registry match is deterministic, not a
  model guess.
- This fallback is **always on** (it is cheap and strictly additive); it is the *primary* path for
  the reference key class and makes those facts finer-keyed **before `ack`**, closing the gap for
  keyed-only deployments. W-extract remains the async path for the *harder* class — natural-language
  mentions with no well-formed reference — which only a generative read can propose, and which in a
  keyed-only deployment is accepted as a known precision-latency gap (documented, not silent).

**Bounded-staleness / read-your-writes signal (F2) — hard rule.** The §6 reconciler lag metric is
observable to *operators*, not to the *caller*. For sub-second agent handoffs a recalling agent
must be able to distinguish "fact exists but is not finer-keyed yet" from "no such fact". The read
contract (WS-4 owns the wire field) MUST therefore carry a **per-request freshness signal** —
`pending_derivations` / `projection_lag` for the scope touched — so a caller can tell *degraded*
recall from *empty* recall, with an optional bounded-wait for read-your-writes on a handoff. This
ADR requires the signal to exist and be emitted by the pipeline (per-scope pending-derivation
count); WS-4 defines its response shape.

### Decision 2b — W-digest, W-categorize, W-embed are as specified in §5.4.2 / §6

No change to §6 stages 5b/5c/5d. W-digest is **debounced and coalesced per the tenant-qualified
key `(tenant_id, scope, scope_id)`** (a burst of facts → one rollup). The digest tier stays gated
behind the `memory.semantic.digest` sub-flag (a deployment can buy fact-tier precision without
paying for a summarizer). Digest hits always resolve down to `member_fact_ids` (R1).

**Tenant-partitioned rollup is a hard summarizer precondition (WS-5 Challenger F6, ISI-5878 →
ISI-5889).** The rollup key is tenant-qualified so the `summarize` task **never** sweeps two
tenants' member facts into one digest, story, or embedding; every `member_fact_id` in a digest
MUST share the digest's `tenant_id`. Because `memory.recall` returns the digest `summary` text
directly (MCP contract §3.4), a cross-tenant rollup would leak *before* any member-fact id check
— so this is enforced at rollup time, not as a read-time filter (§5.4.2, §6 stage 5c).

**Untrusted-content influence paths beyond key-forging (F6, F7) — the R8 analysis extends past
keys.** Decision 2a's R8 treatment covers key *authority*, but two derived surfaces carry a
weaker, real **ranking/vocabulary influence** from untrusted `fact.body`:

- **Categorize → project vocabulary (F6).** Labels derived from untrusted bodies feed the
  consolidation job into a **project-wide controlled vocabulary** (§5.4.3), and `category` is a
  **scope pre-filter** in keyed fetch and search (§7.4). One poisoned fact could (a) seed/reinforce
  a vocabulary term every project agent sees and (b) make itself surface under an attacker-chosen
  category scope. Containment: a **trust + frequency floor on label → vocabulary promotion** — a
  new term is promoted only after it appears across **≥N distinct facts from ≥M distinct writers**
  (deployment knobs), and single-fact / single-writer labels stay fact-local and never enter the
  shared vocabulary. This keeps one untrusted body from minting shared terms. (Contract detail in
  `prompt-contracts.md` → categorize.)
- **Digest ranking (F7).** R1 protects the digest *content* (never the answer), but not the
  *ranking*: the digest summary is embedded and ANN-ranked, so a single poisoned member body can
  steer a scope's digest to rank for attacker-chosen queries (recall poisoning). This is a
  bounded, accepted residual for v1 — mitigations are (i) digest hits always resolve to members
  (a poisoned rank still yields real member facts the caller evaluates), and (ii) the same
  trust/frequency signalling is available to down-weight single-writer-dominated scopes in WS-4
  ranking. Flagged as a known ranking-influence surface, not a silent gap.

## Decision 3 — Embedding model: BGE-M3 default, one embedder for both tiers

**Board-approved default: `BGE-M3` (BAAI)** — multilingual (EN + FR first-class, 100+ langs),
8192-token context, **MIT-licensed & self-hostable**, 1024-dim dense, and uniquely emits
**dense + sparse (lexical) + ColBERT multi-vector in one pass**, which feeds the WS-4 hybrid RRF
fusion (§7.4) directly.

- **One embedder, both tiers.** The same model embeds fact-tier (`fact.body`) and digest-tier
  (scope summary) so the two vector spaces are comparable and RRF-fusable (§7.4). `Embedder.embed`
  takes a `task` arg (`query`/`passage`) for asymmetric retrieval tuning.
- **Return shape — dense + sparse in v1, ColBERT deferred (Coder-1, ISI-5882).**
  `Embedder.embed()` returns `{ dense: float[1024], sparse: {idx: weight}, multi_vector?: float[][] }`.
  v1 populates **dense + sparse** — both come from the *same forward pass* (near-zero marginal
  cost) and both land on storage WS-1 can realistically carry (dense HNSW + sparse
  `sparsevec`/inverted index); sparse feeds the hybrid RRF (k=60, §7.4) directly, which is the
  whole reason BGE-M3 was picked. **ColBERT multi-vector is optional/capability-gated and deferred
  to a WS-1 increment** — cheap to produce but expensive to *serve* (one vector per token → storage
  blow-up + a late-interaction PLAID-class index, not a drop-in for HNSW/pgvector; best via
  FlagEmbedding, not the standard TEI endpoint). Adding it later is an additive field, no interface
  change.
- **Not a one-way door (D3).** The vector index is a rebuildable projection, so changing the
  embedder is a **re-embed by replay**, not a migration. Pick a sane default; swap on evidence.
- **Self-host footprint (Coder-1 feasibility).** BGE-M3 is XLM-RoBERTa-large class (~560M params;
  ~1.1 GB FP16): fits one 24 GB GPU comfortably at the 8192-token window, runs on 16 GB (T4/L4)
  with the input cap below; CPU via ONNX/int8 is viable but low-throughput. No hot-path concern —
  W-embed is post-commit and batched. Config knobs: **batch N≈32**; **per-input ~512-tok cap on
  fact bodies**, reserving the 8192 window for digest-tier inputs (keeps activation memory bounded
  and the 16 GB tier honest).
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
  are meaningless.

**Stale-stamp read policy (F16) — one rule, resolving the Decision 4 / Decision 5 contradiction.**
A read that finds a vector whose `embed_stamp` ≠ active config treats it as **stale-but-servable**:

- A stale-stamp vector whose `(dim, normalization)` are **compatible with the active query
  embedding** is **served with a `stale_projection` warning** on the result (degrade, don't break).
  This is the graceful-degrade state Decision 5 relies on during a backfill — the tenant's semantic
  recall stays lit while re-embed runs, just flagged as stale.
- A stale-stamp vector whose `dim` or `normalization` is **incompatible** with the active config
  (scores would be meaningless / un-comparable) is **excluded** from ranking until re-embedded —
  serving it would return nonsense similarities.

This replaces the earlier absolute "never a valid hit": a mismatched *build* is served-with-warning,
only a mismatched *geometry* (`dim`/`normalization`) is excluded. Pinning per-record (not just
per-index) is what makes the mid-flight swap safe: the index holds mixed stamps during a rebuild,
compatible stale rows keep serving under warning, and the reconciler drains them to current.

**Digest `embed_stamp` omits `scope_version` → a read-time currency rule is required (F12).** The
digest-tier `embed_stamp` carries `(embed_model_id, dim, normalization)` but **not** `scope_version`,
so a stale-`scope_version` digest vector with a *current* embed_stamp is not caught by the
stale-stamp guard above. Because W-digest's idempotency key includes `scope_version` (Decision 6),
a late-landing old-`scope_version` rollup writes to a **different row**, not a no-op — leaving two
live digest rows for one `(scope, scope_id)`. Therefore: **digest reads MUST select the row with
`max(scope_version)` per `(scope, scope_id)`**; lower-`scope_version` digest rows (and their vector
rows) are superseded and **reaped by the reconciler**. Recency/ingest-time MUST NOT be the digest
currency selector, or a late old-sv write could shadow the current one.

**Latest-`derivation_version`-wins for generative artifacts (F13) — hard rule.** There is a
stale-stamp read guard for *vectors*, but generative artifacts (derived keys, category labels) had
no equivalent, so a prompt bump v1→v2 that *intentionally drops* a key/label v1 emitted would leave
the v1 artifact live (it keys on a different `prompt_version`, so v2 accretes alongside rather than
replacing). Rule: **derived keys and category labels are read-filtered to the active
`derivation_version` for their task** — an artifact whose `derivation_version` ≠ active is ignored
at read time and retracted by the reconciler on the next backfill pass. "Latest derivation wins,
prior is retracted," symmetric with the vector stale-stamp policy. (See Decision 6.)

**Projections are `as_of`-opaque in v1 (F3, F14) — stated explicitly.** Derived keys and digests
carry `derivation_version`, not a bitemporal binding to the fact's event-time axis, so they cannot
faithfully answer a point-in-time `as_of=T` query (a derived key or a digest built from post-T
facts would surface). v1 ruling: **`as_of` fidelity is a property of the keyed/temporal SoT and the
fact-tier read, not of the derived projections.** `as_of`-scoped reads resolve against the SoT;
the digest tier is **excluded from `as_of`-pinned queries** (it answers "current scope recall", not
"scope as it was at T"), and derived-key matches on an `as_of` read are confirmed against the SoT's
bitemporal state before they count. Making projections fully `as_of`-resolvable is a deferred
increment, not a v1 claim — the ADR no longer implies projection-level `as_of` fidelity.

## Decision 5 — Change-log trigger (D3): a version bump is a backfill event

The active config declares the current `derivation_version` / `embed_stamp` per role. A change to
any of them (`model_id`, `prompt_version`, `embed_model_id`, `dim`, `normalization`) is a
**change-log event** the reconciler (§6) consumes:

1. Config bump writes a change-log row `(role, task, old_version, new_version, ts)`.
2. The reconciler enqueues **both** classes of backfill work (F11): (a) **stale projections** —
   existing rows whose stamp ≠ new active version (re-embed / re-summarize / re-categorize /
   re-extract); **and** (b) **projection holes** — facts/scopes in the active key/scope set with
   *no* current projection, including **poison-queue items** (Decision 8). Scanning stale stamps
   alone would miss holes, and a poisoned fact *is* a hole (it has no projection to find stale) —
   so the backfill enqueue is defined over "live members lacking an active-version projection",
   which covers both stale rows and holes.
3. Backfill runs at low priority behind live writes; **projection lag is the observable SLO**
   (§6 — per-tier lag metric, consumed by WS-7).

Re-derivation is a **first-class backfill, not a migration** (R3): the SoT never changes, only its
projections. During a backfill the index serves **compatible stale-stamp hits with a
`stale_projection` warning** and excludes only geometry-incompatible rows (Decision 4 / F16
stale-stamp policy) — graceful degrade, never a blanket dark window.

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
is what lets a debounced digest coalesce correctly. Note the idempotency key makes a late
old-`scope_version` rollup a **distinct row**, *not* a literal no-op (F12): it is neutralised at
**read time** by the digest currency rule (Decision 4) — reads select `max(scope_version)` per
`(scope, scope_id)` and the reconciler reaps the superseded row — so a late old-sv write can never
shadow the current digest.

Two read-time rules make these keys safe against stale/accreted writes, both specified in
Decision 4: **digest currency** (`max(scope_version)` wins, F12) and **latest-`derivation_version`
-wins** for derived keys and labels (an artifact whose `derivation_version` ≠ active is ignored and
retracted, F13). The idempotency keys guarantee *no duplicate/corrupt writes*; these read rules
guarantee *the right version is what a reader sees*.

## Decision 7 — Cost controls

The semantic/digest tiers are where spend concentrates. Controls, in order of leverage:

1. **Tier gating (biggest lever).** `memory.semantic` off → zero model spend. `memory.semantic`
   on, digest off → embedding only, **no generative summarizer cost**. Both on → full pipeline.
   (§5.4.2 ruling — unchanged.)
2. **Batch embedding with pre-batch admission (Coder-4, ISI-5882).** W-embed coalesces queued
   `embed()` calls into **batched requests** (N≈32, a knob) — dominant cost saver. Per-tenant
   budget is a **pre-batch admission filter, not a post-hoc debit**: the batcher checks each item's
   tenant budget **at batch-assembly time** and excludes over-budget tenants' items (shed to
   backlog, item 5) *before* forming the request — never debiting after a mixed batch dispatches.
   The tenant tag rides each input and cost is debited **per item** (token counts are per-text), so
   attribution across a multi-tenant batch stays correct.
3. **Debounce generative rollups.** W-digest is debounced/coalesced per scope (§6 5c) — a burst of
   N facts on a thread triggers **one** summarization, not N.
4. **Categorize batching.** W-categorize batches `fact.body` inputs per generative call where the
   contract allows (independent per fact → batchable).
5. **Per-tenant budget + rate caps, with bounded backlog (Coder-4).** Each tenant has a model-spend
   budget and worker rate cap; on breach, workers shed to the backlog (keyed writes/reads never
   affected). The shed backlog is **bounded** (per-tenant depth/age cap) with a
   `derivation_dropped` metric (WS-7), so a persistently over-budget tenant's starved tier is
   **observable, not silently infinite**. Fail-soft, never fail-closed on the SoT.
6. **Separate spend meters for embedding vs generative (Coder-4).** Self-hosted BGE-M3 embedding is
   ~compute-only while generative is $-API-metered; a single blended budget muddies attribution, so
   meter them separately (or in a normalized cost unit). The counter is a cheap per-tenant/per-window
   atomic (Redis/DB) off the keyed path — no SoT impact.
7. **Short-input skip.** Facts below a token floor skip the summarizer (nothing to roll up) and
   embed directly.

## Decision 8 — Retry / backoff + poison handling

Workers are post-commit and **a projection failure never blocks stages 1–4** (R2). Failure policy:

- **Split the two failure classes (Coder-3, ISI-5882)** — they have opposite retry economics:
  - **Infra failure** (timeout, 429, 5xx): **exponential backoff with jitter**, capped at
    `max_attempts` (default 5). Respect provider `Retry-After` on 429.
  - **Deterministic failure** (schema-invalid / semantic-reject generative output — at
    temperature 0 these **re-fail identically**, so the backoff loop would burn 5 identical
    failures + 5× cost/latency): allow **at most one repair re-prompt** (echo the validation error
    back), then go **straight to poison**. A deterministic failure is **not** fed into the
    `max_attempts` counter.
- **Poison item** moved to a **dead-letter (poison) queue** with a **tagged reason** —
  `truncation` (hit `max_tokens`; fixable by sizing, not a model defect), `schema_invalid`,
  `semantic_reject`, or `infra_exhausted` — plus the last attempt's output, a `first_poisoned_at`
  timestamp and a `bump_attempts` counter. Tagging makes the queue triageable and version-bump
  replay targeted (WS-7's poison metric is tagged by reason). The fact **stays committed and keyed**
  — only its derived view is missing, and it is still reachable by keyed (+ tenant-floor, where
  semantic is enabled) search.
- **Poison replay, quarantined and budget-subordinate (F9).** Poison items are re-attempted on the
  next `derivation_version` bump (a new model/prompt may succeed) and can be drained manually —
  **but** two guards stop a systemic failure class from re-burning budget each bump: (i)
  poison-replay work is **strictly subordinate to live-write budget** (it draws only from leftover
  per-tenant budget behind live derivations, never shedding live writes to backlog — Decision 7),
  and (ii) after `K` consecutive failing bumps an item is marked **`terminally_poisoned` and
  excluded from auto-replay** (manual/forced drain only), so a reliably-failing input class stops
  consuming replay budget on every bump. Convergence is thus bounded, not merely asserted.
- **Supersession reaping (F10).** When a poisoned fact F is superseded by F′, F's dead-letter entry
  is **reaped** (F is read-invisible, so re-deriving it is zombie work). The poison queue is
  reconciled against the supersession chain so superseded entries neither inflate depth nor
  re-attempt — only live (non-superseded) facts stay in the queue.
- **Poison observability — age, not just depth (F8).** Queue *depth* alone hides a single
  permanently-stuck item (a healthy, draining queue sits at depth≈1 forever, below any threshold).
  The pipeline therefore emits **`poison_oldest_age`** (age of the oldest live poison item, from
  `first_poisoned_at`) and a **per-item `terminally_poisoned` alert** (fired when `bump_attempts`
  crosses `K`), alongside projection-lag + poison-depth. An item that re-fails on every replay now
  trips an age SLO and a terminal alert instead of sitting unprojected and silent. (WS-7 owns the
  concrete metric/alert names; this ADR requires the age + per-item signals exist.) This is the
  direct remediation of open-question 3 (yes, depth-only was silent).
- **Schema guard on generative output.** Generative workers validate output against the contract
  schema before writing (Decision in `prompt-contracts.md`). A parse/schema failure is a poison
  condition, not a silent bad write — we never persist a hallucinated structure.
- **Output-length is a first-class poison guard, not a trap (F18).** A fixed `max_tokens` that
  truncates a legitimately large structured output → invalid JSON → deterministic poison that can
  *never* succeed until the cap is raised (a contract change). To avoid self-poisoning large
  scopes: generative contracts specify **output-length handling** — the worker detects a
  length-capped / unterminated structured output and, rather than dead-lettering it, applies the
  contract's **segment/continue strategy** (e.g. summarize in bounded chunks for W-digest) or
  raises a *distinct* `output_truncated` condition that is operator-actionable (bump the cap),
  never conflated with a hallucination poison. (Detail in `prompt-contracts.md` rule 6.)

---

## Consequences

- **Positive.** Hot path stays model-free (D3 preserved). One generative endpoint + one embedder
  = minimal ops surface. Every derivation is versioned, idempotent, rebuildable, and cost-gated.
  Model/prompt swaps are backfills, not migrations. Failures degrade a projection, never the SoT.
- **Negative / accepted.** BGE-M3 default raises the semantic-module compute floor vs. bge-small
  (mitigated: opt-in module + lighter swaps). W-extract's async gap means *natural-language-only*
  finer-key precision lags `ack` by the worker latency; the **well-formed-reference key class is
  closed synchronously** by the stage-2 deterministic extractor (F1), so keyed-only deployments
  keep issue/project/pr reachability at `ack`. Residual accepted gap: NL-only mentions in a
  keyed-only deployment lag until W-extract lands (documented + surfaced via the per-request
  `pending_derivations` freshness signal, F2). Digest-tier ranking carries a bounded untrusted
  influence surface (F7, resolve-to-members + trust-weighting mitigations).
- **Follow-ups for siblings.** WS-4 consumes the two-tier RRF output, the `via: fact|digest`
  surfacing, **and must expose the per-request `pending_derivations` / `projection_lag` freshness
  signal** (F2) so callers distinguish degraded from empty recall. WS-7 owns the concrete
  `gen_ai.*` / embedding span + metric names; this ADR enumerates *what* must be measured:
  token/cost counters, projection lag, **poison depth _and_ `poison_oldest_age`**, **per-item
  `terminally_poisoned` alert** (F8), worker queue depth, batch size, and the
  `stale_projection` served-under-warning rate (F16). WS-1 consumes `embed_stamp` as a vector-row
  column and must support the `max(scope_version)` digest-currency reap (F12).

## Open questions — all resolved by Challenger review (ISI-5881) + Coder feasibility (ISI-5882) + remediation (ISI-5888)

1. **W-extract async-gap — resolved (F1).** Yes, a hard class exists: facts whose target key is
   not derivable by the §4 deterministic enricher in a keyed-only deployment. Resolution: a
   **synchronous deterministic extractor** in stage 2 closes the **well-formed-reference key
   class** at `ack` (no LLM); W-extract stays async for the NL-only residual, surfaced via the F2
   freshness signal. See Decision 2a.
2. **BGE-M3 footprint + sparse/ColBERT in v1 — resolved (Coder-1, ISI-5882).** Feasible: ~560M
   params, fits a 24 GB GPU (16 GB with the input cap), post-commit + batched so no hot-path
   concern. **Ship dense + sparse in v1** (same forward pass, WS-1-carriable, feeds RRF); defer
   ColBERT multi-vector to a WS-1 increment behind an optional `multi_vector?` field. See Decision 3.
3. **One generative endpoint vs. per-task model — resolved: per-task pinned tiers (Coder-2,
   ISI-5882).** Per-task `model_id` costs zero added ops (same Claude provider), so the default is
   **down-tiered**: `summarize` → mid (Sonnet-class), `extract`/`categorize` → small (Haiku-class),
   with a `generative.quality_floor` escape hatch and a per-task eval gate. See Decision 1.

## Decision log entry (for §9 / architecture.md)

- **D-WS3.1** Two model roles (generative = **pinned Claude builds per task**, embedding = BGE-M3),
  two interfaces. Default `model_id` is a pinned build string, never "latest" (F17). Per-task tier
  defaults: summarize = mid (Sonnet-class), extract/categorize = small (Haiku-class), with a
  `generative.quality_floor` escape hatch + eval gate (Coder-2).
- **D-WS3.2** LLM-assisted key extraction is **async** (W-extract, stage 5e); §4 deterministic
  resolution stays synchronous. R8 containment: proposed keys are low-trust derived-only, and
  `allowed_key_space` excludes custody dims `agent`/`run` (F4); `evidence_span` is anti-
  confabulation, not anti-injection (F5). A **synchronous deterministic extractor** closes the
  well-formed-reference key class at `ack` so keyed-only deployments keep reachability (F1).
- **D-WS3.3** BGE-M3 bundled OSS default (updates §5.5); one embedder both tiers; Jina = API plugin.
  Return dense + sparse in v1 (`multi_vector?` optional); ColBERT deferred to a WS-1 increment
  (Coder-1). Knobs: batch N≈32, ~512-tok fact-body cap.
- **D-WS3.4** `embed_stamp = (embed_model_id, dim, normalization)` pinned per vector row;
  generative artifacts stamped `(model_id, prompt_version)`. Stale-stamp reads are
  **stale-but-serve-with-warning** (compatible geometry) / excluded (incompatible), resolving the
  old D4/D5 contradiction (F16). Latest-`derivation_version`-wins retraction for derived keys +
  labels (F13); digest reads select `max(scope_version)` (F12); projections are `as_of`-opaque in
  v1 (F3/F14).
- **D-WS3.5** Config version bump = change-log backfill event; backfill enqueues **stale stamps
  _and_ holes (incl. poison)** (F11); re-derive is a backfill, not a migration.
- **D-WS3.6** Per-worker idempotency keys (table above); read-time currency + retraction rules
  layered on top (F12/F13).
- **D-WS3.7** Cost controls: tier-gating → batch (with **pre-batch budget admission**, Coder-4) →
  debounce → **bounded-backlog** per-tenant caps (`derivation_dropped` metric) → **separate
  embed/generative meters** (Coder-4) → short-skip; poison-replay is **subordinate to live-write
  budget** (F9).
- **D-WS3.8** Retry **splits infra (exp backoff+jitter ≤5) from deterministic (≤1 repair re-prompt
  → poison, not counted toward max_attempts)** (Coder-3); poison → dead-letter with **tagged reason**
  (`truncation`/`schema_invalid`/`semantic_reject`/`infra_exhausted`, Coder-3), fact stays committed;
  **`poison_oldest_age` + per-item terminal alert** (F8), quarantine after K bumps (F9),
  supersession reaping (F10), output-truncation handled distinctly from hallucination poison (F18).

## Remediation log — Challenger review ISI-5881 → ISI-5888

ADR moved `proposed → accepted` after remediating all 16 findings (4 CRIT, 7 HIGH, 8 MEDIUM).

| # | Sev | Finding | Resolution | Where |
|---|---|---|---|---|
| F1 | CRIT | async gap collapses in keyed-only (no tenant floor) | synchronous deterministic stage-2 extractor for well-formed-reference key class; W-extract async for NL-only residual | Decision 2a |
| F4 | CRIT | `allowed_key_space` includes custody `agent`/`run`; verify is existence-only | restrict to low-custody dims; pass `agent`/`run` as `existing_keys` block | Decision 2a + contract |
| F8 | CRIT | poison alerting depth-only; stuck item silent | `poison_oldest_age` + per-item `terminally_poisoned` alert | Decision 8 |
| F16 | CRIT | D4 "never a valid hit" vs D5 "serve old-stamp" contradiction | stale-but-serve-with-warning (compatible) / exclude (incompatible geometry) | Decision 4 + 5 |
| F2 | HIGH | no read-your-writes / bounded-staleness signal | per-request `pending_derivations` / `projection_lag` (WS-4 wire) | Decision 2a + Consequences |
| F5 | HIGH | `evidence_span` ⟂ injection, doc conflates | stated plainly: anti-confabulation only | Decision 2a + contract |
| F6 | HIGH | categorize→vocabulary cross-agent influence | trust + frequency floor on label→vocabulary promotion | Decision 2b + contract |
| F9 | HIGH | poison replay re-burns budget, can starve live | subordinate to live budget + K-bump quarantine | Decision 8 |
| F12 | HIGH | digest read-currency unspecified; embed_stamp omits scope_version | reads select `max(scope_version)`; reap superseded rows | Decision 4 + 6 |
| F13 | HIGH | generative version bumps accrete, never retract | latest-`derivation_version`-wins retraction (keys + labels) | Decision 4 + 6 |
| F17 | HIGH | "latest Claude tier" triggers fleet backfill storm | pin a specific build id; upgrade deliberately | Decision 1 |
| F3 | MED | `as_of` not projection-aware | projections `as_of`-opaque in v1; SoT answers point-in-time | Decision 4 |
| F7 | MED | digest ranking influence from one poisoned member | resolve-to-members + trust-weighting; flagged residual | Decision 2b |
| F10 | MED | poison queue ignores supersession (zombie work) | reap superseded poison entries against supersession chain | Decision 8 |
| F11 | MED | D5 backfill scans stale stamps, misses holes | enqueue over "live members lacking active-version projection" | Decision 5 |
| F14 | MED | digest tier breaks `as_of` | digest excluded from `as_of`-pinned queries | Decision 4 |
| F15 | MED | "re-trigger W-embed" is a no-op | correct trigger = keyed/graph candidate-id pre-filter update | Decision 2a |
| F18 | MED | output `max_tokens` truncation = deterministic poison trap | distinct `output_truncated` condition + segment/continue | Decision 8 + contract |
| F19 | MED | temp=0 ≠ reproducible; stamp implies content equality | stamp certifies provenance not content; re-run equivalence best-effort | contract rule 4 |

## Feasibility fold — Coder review ISI-5882

Coder verdict: **implementable as written, no hard blockers.** Four improvements folded (none
changes the spine; all land before WS-1/WS-4 build on the contract):

| # | Finding | Resolution | Where |
|---|---|---|---|
| Coder-1 | sparse/ColBERT in v1? + BGE-M3 footprint | dense+sparse v1, `multi_vector?` optional, ColBERT → WS-1 increment; batch N≈32, ~512-tok body cap | Decision 3 |
| Coder-2 | one model vs per-task tier | per-task pinned tiers (summarize=mid, extract/categorize=small) + `quality_floor` + eval gate | Decision 1 |
| Coder-3 | schema failure in backoff loop | split infra-retry vs deterministic (≤1 repair → poison, off `max_attempts`); tagged poison reasons | Decision 8 |
| Coder-4 | batch budget + backlog | pre-batch admission filter; bounded backlog + `derivation_dropped`; separate embed/generative meters | Decision 7 |
