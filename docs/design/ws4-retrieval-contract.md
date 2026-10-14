# MemKeys — WS-4 Retrieval Contract: the four ops (spec)

> **Status:** Finalized (rev 2) — Challenger review (ISI-5875) + PM consumer-shape rulings
> (ISI-5876) folded. Ratifiable.
> **Owner:** Architect (Winston) · **Issue:** ISI-5869 · **Roadmap:** ISI-5803 (WS-4)
> **Grounding:** `docs/architecture.md` §3 (envelope), §5 (data model / DQL), §7 (retrieval
> contract sketch), §9 (decision log). This document **promotes** architecture §7 from sketch
> into the implementation-ready HTTP contract and is authoritative over it where they differ.
> **Consumers:** WS-5 MCP surface (`docs/design/mcp-contract.md`, ISI-5870) — every MCP field
> is a projection of a field defined here. WS-7 observability signals key off these shapes.
> **Depends on:** WS-1 (`VectorIndex`, ISI-5866/5892), WS-2 (`GraphView`, ISI-5867).
> **Carries one downstream dependency:** `tenant_id` persistence + backfill → **ISI-5889**
> (Architect-owned, in_progress — see §7).

---

## 0. What changed in rev 2 (review fold-in)

Both commissioned reviews are complete and closed; their rulings are folded here and the
per-finding disposition is in §8.

- **Challenger (ISI-5875):** 11 findings, 2 critical blocking (F1 tenant-grain undefined,
  F2 403-is-an-existence-oracle). Both resolved below. The spec's spine (hydrate-from-SoT
  always, 501-not-empty degrade, pinned read flow) was CONFIRMED sound.
- **PM / John (ISI-5876):** 3 consumer-shape rulings — `recall` vs `search` as two preset
  verbs over op 3; keyless `browse` is a *coarse-key op 1*, not op 3; token-budget defaults.
  Folded into §5.

The **F2 404 ruling and principal-salted identity (F1)** also match what WS-5
(`mcp-contract.md` §3) already shipped — the two specs now agree on the tenant boundary
instead of contradicting it.

---

## 1. Invariant T — the tenant-scope floor (R6)

**Every read is bounded by the caller's tenant. The floor is never omittable by the client
and is enforced *before* any vector or graph work.** This is the hard multi-tenancy boundary
(architecture §5.4.2); it is the `R6` ruling (§9 item 6) made into a contract predicate.

### 1.1 The grain of "tenant" — pinned (resolves Challenger F1, CRITICAL)

The review was correct that "tenant" was used at four conflicting grains (principal / agent /
project / org). Pinned, authoritatively:

- **`tenant_id` is the isolation boundary — the workspace/org the caller belongs to.** It is
  an **explicit named claim in the run token**, *distinct from* `agent_id` (the principal) and
  *distinct from* any `entity.project`. The `principal → tenant_id` mapping is fixed in the
  token; the client never supplies, widens, or omits it.
- The floor predicate is therefore a single column: **`tenant_id == caller.tenant_id`.**
- **Cross-agent handoff still works** (the §8 prototype criterion the review flagged):
  Agent A (`agent_id=A`) and Agent B (`agent_id=B`) in the same workspace share one
  `tenant_id`. B's `GET /memory/facts?entity.issue=X` reads A's handoff because they are inside
  the *same* floor. The floor is the workspace, **not** the principal — so principal-grain
  isolation (which would break handoff) is explicitly rejected.
- **Org-level shared reads across projects** are *inside* one `tenant_id` already: two projects
  in the same workspace share the floor, so an org-level `learning` is visible to both via a
  keyless (tenant-floor-only) op-3 search or a coarse-key op-1 read. No second floor level is
  introduced in v1; cross-*tenant* sharing is out of scope (see §7 assumption A1).

Identity note (aligns with WS-5 `mcp-contract.md` §3 / ISI-5889 F1): `fact.id` is
**principal-salted** — `tenant_id` and `agent_id` are mandatory derivation inputs — so an id
minted in one tenant cannot collide with or be guessed into another tenant's id-space.

### 1.2 Fail-closed as `not_found`, never `forbidden` (resolves Challenger F2, CRITICAL)

When any op names an `entity.*` key, a `fact.id`/node id, or a supersede target the caller's
`tenant_id` does **not** own, the server returns **`404 not_found`**, **never** `403`.

- A `403 cross_tenant` *confirms the id exists* — an existence oracle that leaks exactly the
  cross-tenant information Invariant T exists to protect. `404` is fail-closed: "no such thing
  in your world."
- This **reconciles WS-4 with WS-5**, which already ruled `not_found` (not `forbidden`) on the
  same boundary (`mcp-contract.md` §3 point 3). The MCP layer no longer has to translate a
  WS-4 `403` into its own `404`.
- Because a silent `404` on every boundary crossing is itself an id-probing oracle, **every
  boundary-crossing `not_found` is audited, and repeated boundary-crossing `not_found`s from
  one principal are `rate_limited`** (mirrors `mcp-contract.md` §3 point 3 / F15).
- `403` is reserved for a genuinely authenticated-but-unauthorized caller (e.g. a revoked
  token) — not for the tenant boundary.

### 1.3 The floor bounds the *candidate set*, not the result set

Scope resolution translates `tenant_id` (+ any finer scope) into a **candidate id set before**
the ANN/graph pass (architecture §5.4.2, F5). A backend that runs ANN globally and trims
foreign hits afterward is **non-conformant** — the boundary must bound the candidate set, so
the index MUST be tenant-partitioned or the scope step MUST resolve to a hard id-set
pre-filter. (`VectorIndex` conformance: the adapter must accept a candidate-id restriction;
see `vector-index-conformance-suite.md`.)

---

## 2. The pinned read flow — `scoped pre-filter → rank → hydrate from SoT`

All three read ops are instances of one flow; this is the WS-4 invariant the issue asks to pin.

```
  ┌─ 1. SCOPE ──────────────────────────────────────────────────────────┐
  │  tenant_id floor (§1, mandatory) ∧ caller scope attrs (entity.*,     │
  │  category, type, time) → candidate id-set.  HARD multi-tenancy       │
  │  boundary. Runs as a SQL WHERE / keyed DQL BEFORE any ANN or graph.  │
  └─────────────────────────────────────────────────────────────────────┘
                              │ candidate id-set
                              ▼
  ┌─ 2. RANK (op-specific) ─────────────────────────────────────────────┐
  │  op1 keyed : order by event_time desc (no rank model)               │
  │  op2 graph : BFS/CTE hop over edges within the candidate set         │
  │  op3 sem.  : ANN over candidate set per enabled tier → RRF fusion    │
  └─────────────────────────────────────────────────────────────────────┘
                              │ ranked fact.id-set
                              ▼
  ┌─ 3. HYDRATE from SoT ───────────────────────────────────────────────┐
  │  fetch authoritative records from the keyed/temporal store           │
  │  (agentmesh_facts) by id. The SoT fetch is ALWAYS the final          │
  │  authority — graph/vector carry ids, never bodies (§6). Re-apply     │
  │  valid=/as_of at hydration (bi-temporal, §3.4 F6).                   │
  └─────────────────────────────────────────────────────────────────────┘
```

Steps 1 and 3 are identical across all three reads; only step 2 differs. This is what lets a
caller mix ops and still get coherent, authoritative records.

---

## 3. The four operations

Priority order (hot → warm → optional → write). All reads return the **same fact shape** (§6).

### 3.1 Op 1 — fetch-by-keys (hot, O(1) per key) · `GET /memory/facts`

The default path. No graph, no vector. This is also the home of **browse** (PM ruling 2).

```
GET /memory/facts
    ?entity.issue=…  &entity.pr=…  &entity.agent=…  &entity.run=…  &entity.project=…
    &type=decision|outcome|observation|learning|context   (optional)
    &as_of=<RFC3339>       (optional; point-in-time replay, §3.4 bi-temporal rule)
    &valid=current|all     (default: current — collapses supersession to latest head)
    &limit=20              (default 20; max 500)      ← PM ruling 3 (token budget)
    &cursor=<opaque>       (optional; as_of-pinned, §3.3)
  → 200 { "facts": [ <FactRecord §6> ],
          "next_cursor": "<opaque|absent>",
          "token_estimate": <int> }
```

- **≥1 `entity.*` key is required.** A truly keyless call is `400 missing_scope` — a
  "dump my whole tenant" read is unbounded and almost never the real intent (PM ruling 2).
- Multiple keys are **ANDed** (intersection). Maps to the DQL `fetch | filter` patterns
  (architecture §5.2–5.3).
- **Browse = a single *coarse* `entity.*` key** (`entity.project`, `entity.agent`, or
  `entity.issue`), ordered `event_time desc`. A waking agent always has one of these. This is
  the lowest-floor, always-available discovery path and **works on every deployment tier with
  no semantic module** — it is **not** routed to op 3 (corrects the prior "browse → op 3" bug
  the PM flagged; op 3 is ANN and needs a query string + the optional module).

### 3.2 Op 2 — graph traversal (warm) · `POST /memory/graph/traverse`

```
POST /memory/graph/traverse
  { "start":     { "kind": "fact|entity", "id": "…" },   // REQUIRED; tenant-owned else 404
    "edges":     ["supersedes","entity_fact","produced_by","temporal","a2a_participant"],
    "direction": "out|in|both",
    "depth":     2,
    "hydrate":   true,          // false ⇒ ids + edge topology only (introspection)
    "limit":     20,            // node-visit budget (default 20; max 500)
    "cursor":    "<opaque>"
  }
  → 200 { "nodes":       [ { "kind","id", …FactRecord §6 if hydrate=true… } ],
          "edges":       [ { "from","to","type" } ],
          "truncated":   false,          // first-class signal when the visit budget is hit
          "next_cursor": "<opaque|absent>" }
```

- `edges` is a filter — omit to walk all edge types. `hydrate:false` returns topology only.
- **Tenant floor is applied on *every hop*, not just at `start`** (aligns with
  `mcp-contract.md` §3 point 5 / F4): a hop that would leave the floor is pruned, not followed.
- **`truncated` is a first-class signal, not a silent cap** — if the visit budget is reached,
  `truncated: true` and a `next_cursor` is returned. Bounded coverage must be legible.
- **`as_of` is rejected in v1 (`400 unsupported_param`)** — resolves Challenger F3. The graph
  is *not* bi-temporal in v1: edge topology is current-state. Accepting `as_of` while
  hydrating only the as-of subset would return a topology **that never existed at `as_of`**
  (edges pointing at dropped nodes; today's supersession chain, not the one current then) — an
  incoherent, silently-wrong result. Time-travelled topology is deferred as a clean YAGNI cut;
  a supersession-chain-at-past-time walk, when needed, is a future bi-temporal-graph feature,
  not a half-temporal trap shipped today.

### 3.3 Op 3 — semantic search (optional module, scoped) · `POST /memory/search`

```
POST /memory/search
  { "query":  "<text>",                 // REQUIRED (ANN needs a query vector)
    "scope":  {                         // ANDed within the tenant floor (§1) — never widens it
      "entity.project": "…", "entity.issue": "…", "entity.agent": "…",
      "fact.type": "…", "category": "…" // D4 facets are attributes → usable as scope
    },
    "k":      10,                        // default per preset (§5); search=10, recall=15
    "tiers":  "auto|fact|digest",        // optional override; preset sets the default (§5)
    "as_of":  "<RFC3339>",               // optional; see digest rule below
    "cursor": "<opaque>"                 // as_of-pinned (see pagination)
  }
  → 200 { "hits": [ { …FactRecord §6…,
                      "score":        <float>,     // fused RRF score
                      "via":          "fact|digest",
                      "digest_scope": {…}? } ],     // present only when via=digest
          "returned":     <int>,         // ← F4: coverage legibility
          "requested":    <int>,         // = k
          "dropped_count": <int>,        // hits dropped at hydrate (see below)
          "next_cursor":  "<opaque|absent>",
          "token_estimate": <int> }
  → 501 { "error": "feature_disabled" }  // memory.semantic module off — NOT a silent empty
```

**Server flow (the §2 flow, specialized):**

1. **Scope** → tenant floor (§1) + scope attrs/categories → candidate id-set (keyed DQL → ids,
   or graph hop for entity-centric scopes).
2. **ANN** over the candidate set per enabled tier: *fact* tier (precision) and, when
   `memory.semantic.digest` is on and `tiers` permits, the *digest* tier (span recall).
3. **RRF fusion** — `score = Σ 1/(k_rrf + rank_i)`.
   - **Tier weights default equal (fact 1.0 / digest 1.0)** and are a deployment knob
     (CONFIRMED by Challenger F5: RRF is rank-based and robust to the digest tier's different
     score scale; the tiers are complementary, not ranked — let RRF decide, don't pre-bias).
   - **`k_rrf` scales with `k`, it is not a fixed 60** (resolves Challenger F5). `k_rrf=60` is
     tuned for TREC-scale lists; at `k≈10` it flattens the entire result list to a <15% score
     spread and degenerates to "present in both lists." **Default `k_rrf = max(10, k)`** so
     small-k agent reads keep meaningful rank separation. The constant is documented, not
     silently transplanted.
   - **Cross-tier agreement SUMS, then the record is de-duped for display** (resolves
     Challenger F5 dedup-gap). A fact surfacing in *both* tiers gets both rank contributions
     added (the strongest relevance signal RRF offers); only the displayed record is collapsed
     to one. "De-dup" is a display operation, not a score operation.
4. **Resolve + hydrate** — digest hits → `member_fact_ids` → fetch authoritative records from
   `agentmesh_facts` (R1 resolve-to-ground-truth).

**`dropped_count` (resolves Challenger F4).** A ranked hit can rarely fail to hydrate — but
**not** from retention eviction: §5.6's hard invariant *projection horizon ≤ SoT horizon*
guarantees the SoT record outlives any projection pointer (that cause is removed from the
spec). The only genuine residuals are (a) a seconds-wide retention-boundary race between the
ANN pass and the hydrate pass, and (b) `valid=current` collapsing a hit that is a superseded
head. The response reports `returned`/`requested`/`dropped_count` — a **count only**, no ids,
no reasons, leaking nothing about projection internals. This removes the indefensible
asymmetry with op 2's first-class `truncated` signal (same spec, same legibility rule).

**`as_of` disables the digest tier (resolves Challenger F9).** A digest embedding encodes a
summary rolled up over facts **through today** (its own `derivation_version`); you cannot
retroactively un-know a summary, so ANN over a digest vector "knows the future" relative to
`as_of`. When `as_of` is set, op 3 runs **fact-tier only** (the fact vector is written after
its keyed commit, so it is sound under `as_of`) and sets a `Warning` header noting the digest
tier was skipped. Never silently rank on post-`as_of` content.

**Pagination is `as_of`-pinned (resolves Challenger F10).** Op 3 has no natural stable cursor —
between calls the async projections shift and RRF scores move. The server therefore **pins the
`as_of` into the opaque cursor on the first call** (if the caller gave none, the server stamps
the request-time instant). "Page 2" re-queries against the *pinned* candidate set, so results
don't duplicate/vanish across calls. This matches WS-5's cursor contract (`mcp-contract.md`
§6). Digest-tier paging under a pinned `as_of` is skipped per F9 (fact-tier only).

**Degrade, never silently empty.** `501 feature_disabled` when `memory.semantic` is off (so
callers degrade gracefully). A digest-only request when the digest sub-flag is off degrades to
the fact tier with a `Warning` header, not a 501.

### 3.4 Op 4 — write-with-keys (hot) · `POST /memory/facts`

```
POST /memory/facts
  { …envelope per architecture §3… }      // one mandatory anchor; fact.body required
  → 201 { "fact.id": "…",
          "keyed": true,                   // see F11 note below
          "entity_keys_resolved": { "entity.issue":"…", "entity.project":"…", … } }
```

- The response carries the **server-resolved** entity keys so the caller caches them for later
  keyed reads without a lookup round-trip (the dumb-client / server-modeling contract, §8
  prototype).
- `fact.id` is **principal-salted** (`tenant_id`+`agent_id` mandatory inputs) and body-complete
  (aligns with WS-5 F1 / ISI-5889).
- `supersedes` (optional) targets a prior `fact.id`; the **target MUST be owned by the caller's
  `tenant_id`, else the write fails closed as `404 not_found`** (§1.2). Cross-agent-within-
  tenant supersession is allowed and attributed; cross-tenant supersession is impossible.
- Idempotent on the caller's idempotency key — a retried flush produces no duplicate.

**Bi-temporal write/read ordering (resolves Challenger F6, HIGH).** `valid=current` is a
supersession **collapse (GROUP/dedup)**, not a `WHERE` filter, and it must be evaluated **as of
the transaction axis** *before* the collapse — never WHERE-then-GROUP. Concretely, an `as_of`
read evaluates supersession state **as it stood at `as_of`**: a fact that was the current head
*at* `as_of` but has since been superseded MUST be returned (it was current then); a fact
superseded *before* `as_of` MUST NOT. The naïve "apply today's non-superseded flag, then
time-filter" reading is wrong and is explicitly forbidden. This ordering is pinned here and
inherited by ops 1 and 3 at their hydrate step.

---

## 4. Error model (reconciled with WS-5)

| Code | Name | When |
|------|------|------|
| `400` | `missing_scope` | op 1 with zero `entity.*` keys (keyless read refused) |
| `400` | `unsupported_param` | op 2 with `as_of` in v1 (§3.2, F3) |
| `400` | `bad_request` | malformed envelope / missing `fact.body` on op 4 |
| `404` | `not_found` | any op naming a key/id/supersede-target outside the caller's `tenant_id` (§1.2) — **fail-closed, never 403**; audited + rate-limited on repeat |
| `409` | `conflict` | idempotency-key replay with a *different* body |
| `429` | `rate_limited` | repeated boundary-crossing `not_found`s from one principal (§1.2) |
| `501` | `feature_disabled` | op 3 when `memory.semantic` is off (§3.3) — not a silent empty |

`403` is **reserved** for authenticated-but-unauthorized (e.g. revoked token). The tenant
boundary never uses it.

---

## 5. Consumer (MCP) surface — PM rulings (ISI-5876), folded

The MCP verbs are **thin JTBD-named presets over these ops** — no MCP field is independent of
this contract. Finalized surface for WS-5 (`mcp-contract.md` §4) to reconcile verbatim:

| MCP verb | Op | Default | Note |
|----------|----|---------|------|
| `memory.get` | 1 | `limit=20` | ≥1 entity key; **coarse-key = browse** |
| `memory.traverse` | 2 | `limit=20` | node-visit budget; `truncated` surfaced |
| `memory.recall` | 3 | `tiers=auto, k=15` | **resumption** preset — "what happened in thread T" |
| `memory.search` | 3 | `tiers=fact, k=10` | **precision** preset — "find a fact like X" |
| `memory.write` | 4 | — | envelope path |

**Why two verbs over op 3, not one verb + a `tiers` arg (PM ruling 1).** For an LLM caller the
**verb name is the strongest tool-selection signal; an optional enum is the weakest.** A
resumption job ("what happened") run on precision defaults silently returns a sparse,
incoherent picture and nobody sees it fail. Two near-synonym verbs are cheap (same op, same
candidate set, same SoT hydration); a mis-set default is expensive. `tiers` remains an
**optional override** on both for power callers — the verb sets the right default, the arg is
the escape hatch.

**Defaults are a context-token budget, not a page size (PM ruling 3).** Every op returns
fully-hydrated FactRecords with bodies — there is no cheap id-only listing mode. So: op-1/op-2
agent-facing `limit=20` (50 hydrated facts ≈ 15k tokens, mostly unread when orienting); `max=500`
+ cursor for the rare deep read. `search` `k=10` (ANN precision falls off past ~10);
`recall` `k=15` (narrative reconstruction needs a few more; the digest tier is doing recall).

---

## 6. Response-shape invariant

All reads return the same **FactRecord** — the authoritative record hydrated from the keyed
store:

```
FactRecord = { "fact.id", "type", "subject", "body", "confidence",
               "entity": {…}, "event_time", "provenance" }
```

- Ops 2 and 3 **never carry body text from the graph or vector index** — they carry ids; the
  §2 hydrate step is always the final authority. This is what lets callers mix ops and still
  get coherent, authoritative results.
- **`keyed:false` is reachable only via op 3** (resolves Challenger F11, editorial). Op 1
  requires ≥1 key and fetches only the keyed store, so it can never return an `unkeyed`
  (`keyed:false`) record — the `keyed` flag is meaningful on the op-4 write response and on
  op-3 hits (where the `unkeyed` tier surfaces within the tenant floor), not on op-1 reads.

---

## 7. Open items & downstream dependencies

- **DEP-1 (`tenant_id` persistence + backfill) — tracked by ISI-5889 (Architect-owned,
  in_progress).** Challenger F7 (HIGH) correctly notes the storage schema (architecture §5.1)
  keys on `entity.*` and `provenance.principal` but shows **no `tenant_id` column**, so
  Invariant T's "cheap, already-indexed predicate" is not yet true. Pinning the grain (§1.1)
  does not persist it. ISI-5889 carries the storage-layer reconciliation: add `tenant_id` as a
  first-class indexed dimension on `agentmesh_facts` and on both vector tiers, and backfill
  existing facts (`provenance.principal → tenant_id` via the token mapping). **This spec is
  ratifiable now** (the contract/grain is fixed); the index is a storage build task, not a
  spec blocker.
- **A1 (shared entities are single-tenant in v1) — explicit assumption** (resolves Challenger
  F8). Entity keys are **namespaced within `tenant_id`**: issue `X` in tenant A and issue `X`
  in tenant B are distinct keyed dimensions. Two tenants genuinely collaborating on one
  external entity (e.g. the same GitHub issue) is **out of scope for v1** — if that need
  materializes it is a deliberate cross-tenant-sharing feature, not an implicit recall path.
  The `entity_fact` edge therefore never crosses a tenant boundary by construction.
- **Open-Q (op-3 single-call sizing).** With `as_of`-pinned cursors (§3.3) op 3 paginates
  stably; callers should still size `k` to one call where possible (agent context economics).

---

## 8. Review disposition log

### Challenger (ISI-5875) — 11 findings

| # | Sev | Finding | Disposition |
|---|-----|---------|-------------|
| F1 | CRIT | Tenant grain undefined → Invariant T un-implementable | **RESOLVED** §1.1 — `tenant_id` = workspace/org claim in run token, distinct from principal/project; floor = `tenant_id==caller`; cross-agent handoff preserved |
| F2 | CRIT | `403 cross_tenant` is existence oracle + contradicts WS-5 | **RESOLVED** §1.2/§4 — `404 not_found`, audited + rate-limited; now agrees with WS-5 |
| F3 | CONCERN | graph `as_of` incoherently half-temporal | **RESOLVED** §3.2 — reject `as_of` on op 2 in v1 (`400`) |
| F4 | CONCERN | silent drop of un-hydratable ids; retention-eviction cause spurious | **RESOLVED** §3.3 — add `returned`/`requested`/`dropped_count`; removed retention-eviction cause (precluded by §5.6) |
| F5 | MIXED | equal-weight ✓; `k_rrf=60` wrong at k=10; dedup-vs-sum unspec | **RESOLVED** §3.3 — keep equal weights; `k_rrf=max(10,k)`; cross-tier agreement SUMS then dedup-for-display |
| F6 | HIGH | op1 `as_of`+`valid=current` bi-temporal trap | **RESOLVED** §3.4 — supersession evaluated as-of txn axis *before* collapse; WHERE-then-GROUP forbidden |
| F7 | HIGH | `tenant` index unsubstantiated (no stored column) | **ROUTED** §7 DEP-1 → ISI-5889 (storage schema + backfill); spec ratifiable now |
| F8 | HIGH | `entity_fact` assumes issues single-tenant | **RESOLVED** §7 A1 — entity keys tenant-namespaced; cross-tenant shared entities out of v1 scope |
| F9 | MED | `as_of` on digest tier unsound, silently served | **RESOLVED** §3.3 — `as_of` disables digest tier (fact-tier only + Warning) |
| F10 | MED | op 3 pagination unstable (no cursor) | **RESOLVED** §3.3 — `as_of`-pinned cursor; matches WS-5 |
| F11 | LOW | `keyed:false` dead/misleading on op 1 | **RESOLVED** §6 — documented reachable only via op 3 |

### PM / John (ISI-5876) — 3 rulings

| # | Ruling | Disposition |
|---|--------|-------------|
| 1 | `recall` vs `search` = two preset verbs over op 3, not a `tiers` arg | **FOLDED** §5 |
| 2 | keyless `browse` = coarse-key op 1 (kill "browse → op 3"); keep `400` on truly keyless | **FOLDED** §3.1, §5 |
| 3 | defaults: `search` k=10, `recall` k=15, op1/op2 `limit=20` (max 500, cursor) | **FOLDED** §3.1–3.3, §5 |
