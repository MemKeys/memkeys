# ADR-0002: Op-4 (semantic) supersession collapse — deterministic fact-update recall

- **Status:** Accepted (implemented, PR pending)
- **Date:** 2026-10-28
- **Owner:** Architect (Winston)
- **Workstream:** Retrieval contract (§7.4) — Paperclip issue ISI-6810
- **Relates to:** [`docs/retrieval-contract.md`](../retrieval-contract.md) (ISI-5869, `as_of` semantics), [`docs/architecture.md`](../architecture.md) (§5.1 append-only SoT + supersession, §5.4.2 fact-vector tier), ISI-6075 (M1 FactStore write), ISI-6131 (head composition `H`)
- **Deciders:** Architect (author); Code Reviewer (Amelia), Benchmark enablement track (verify)

---

## Context

The KubeCon Set B benchmark (ISI-5581, N=3) measured memkeys **factchange = 0.50
±0.43, range [0.25–1.00]**. When the crew updates a fact's value and the benchmark
asks for the new value, memkeys returns it only ~half the time, and *which* half is
non-deterministic — the spread (±0.43) is the tell.

memkeys' SoT is **append-only** (§5.1): a fact is never mutated. A value change is a
**new** fact that *supersedes* the old one. Supersession is established
deterministically by the **live-head composition** `H = (tenant, entity_identity,
fact_type, subject)` (ISI-6131): a plain write to an occupied head
**auto-supersedes** the current head and stamps `facts.supersedes` (`pg.rs`
`LIVE_HEAD_SQL` / `effective_supersedes`). Liveness is a *read-time* chain property —
"the record nothing supersedes" — with no `superseded` flag and no write-back.

Operation 1 (keyed fetch, §7.2) already honors this: `valid=current` (the default)
collapses superseded records via `NOT EXISTS (… s.supersedes = f.id …)`, as-of-aware
(`pg.rs` / `memstore.rs`).

**Operation 4 (semantic, §7.4) did not.** The fact-vector tier (§5.4.2) upserts
**idempotently on `fact_id`** — it is *not* head-aware — so after an update the index
holds **both** the old and the new vector. `semantic_search_inner` then hydrated
**every** ANN hit through `get_fact`, with no supersession collapse. A query phrased
close to the *old* fact ranks the stale vector first and returns the stale value; a
query phrased close to the *new* one returns the live value. That phrasing-dependence
is precisely the observed non-determinism.

## Decision

**Make op 4 honor supersession at read time — the deterministic, as-of-aware twin of
op 1's `valid=current` collapse — rather than mutating the vector index on write.**

Concretely: after the ANN pass, drop any hit whose `fact_id` is superseded as of the
query's `as_of`, using the SoT `supersedes` chain. A new `FactStore::superseded_ids
(tenant, ids, as_of)` is the batch form of op 1's collapse (`SELECT DISTINCT
s.supersedes … WHERE s.supersedes = ANY(ids) AND s.ingested_at <= as_of`); the
semantic path calls it between the zero-similarity filter and hydration.

### Rejected alternative — write-time cosine near-dup supersession (the issue's proposal)

The issue proposed detecting a near-duplicate on write (cosine ≥ 0.95 on
subject+predicate) and deleting/tombstoning the old vector. **Rejected:**

1. **Non-deterministic & threshold-fragile.** A cosine gate *reintroduces* the
   phrasing-dependence we are trying to remove — the issue itself flags that a 0.95
   threshold misses paraphrases. The SoT already *knows*, deterministically, which
   fact supersedes which (head composition `H`). Reusing that is exact; re-deriving it
   fuzzily from vectors is a strictly worse signal.
2. **Duplicated truth.** Supersession is an SoT fact. Computing it a second time, in a
   derived projection, risks the projection disagreeing with the SoT.
3. **Write-path latency.** A per-write ANN probe sits on (or races) the ack path; the
   issue flags this and floats an async job, which reopens the race it set out to close.
4. **History loss.** A hard delete breaks `as_of` semantic replay (ISI-5869/ISI-6172).
   The read-time collapse is as-of-aware: an `as_of` inside the old fact's live window
   still surfaces it, because the superseding fact is "not yet known."

Read-time collapse is the smaller, deterministic, history-preserving change and is
**symmetric** with the path that already works (op 1).

## Consequences

- **In scope (this issue, server, implemented):** `superseded_ids` on the `FactStore`
  port + both backends (`PgStore`, `MemStore`); the collapse in
  `semantic_search_inner` (so both `semantic_search` and `fused_search`'s semantic
  contribution are covered — single chokepoint). Regression test
  `semantic_search_drops_superseded_fact` (live search returns only the new version;
  an `as_of` between the two ingests still surfaces the old one). No write-path latency
  added.
- **Precondition for the benchmark score (Part B, benchmark-enablement track).** The
  collapse only fires when the update lands on the **same head** `H`. `H`'s entity
  component is `(project|issue|pr)` and the fact must carry a **stable `subject`**.
  Generic conversational memory facts (Set B via the kagent-memkeys adapter) may carry
  none of `project/issue/pr`, and/or derive `subject` from the full text (value
  included) so each update forms a *different* head. If so, **no `supersedes` edge is
  created and neither op 1 nor op 4 collapses** — the server fix is necessary but not
  sufficient. The adapter must map each fact to a stable `(entity_identity, subject)`
  across update turns (e.g. a benchmark-fixed `entity.project` + a subject that is the
  fact's *key*, not its value), or memkeys needs a subject-only head grain for keyless
  memory. This is the likely dominant factor in the Set B score and is tracked
  separately.
- **Success metric (owned by Part B):** Set B factchange ≥ 0.75 with spread ≤ 0.20 on
  a fresh N=3 run.
- **Graph path (op 2/3) not changed.** Out of scope for the factchange symptom; a
  follow-up can apply the same collapse to graph-sourced fact hits in `fused_search` if
  a case surfaces.
