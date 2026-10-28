# ADR-0002: Hybrid recency + semantic re-rank for the fact-vector tier

- **Status:** Accepted — **implemented** (ISI-6814: `hybrid_rerank` + `RecencyConfig` in `semantic.rs`, threaded through `semantic_search_inner`, env knobs in `main.rs`; Architect decision — Winston; ISI-6809 delegated the implementation-option choice to the Architect)
- **Date:** 2026-10-28
- **Owner:** Architect (Winston)
- **Workstream:** Retrieval (§7.4) — Paperclip issue ISI-6809
- **Relates to:** [`docs/retrieval-contract.md`](../retrieval-contract.md), [`crates/memkeys-server/src/semantic.rs`](../../crates/memkeys-server/src/semantic.rs), [`crates/memkeys-server/src/pg_vector.rs`](../../crates/memkeys-server/src/pg_vector.rs), ISI-6671 (external embedder / PR #74), ISI-6657 (keyed-path interim result), ISI-6742 (Set A re-run), ISI-5581 (KubeCon benchmark)
- **Deciders:** Architect (author/decision), implementing engineer (Claude Code), benchmark owner (Research Engineer, validation)

---

## Context

The KubeCon Set A benchmark (ISI-5581, apples-to-apples re-run ISI-6742) measured
memkeys on a single-agent SRE investigation task:

- warm score **0.343**, cold **0.405**, Δ **−0.062** (negative payoff)
- matched subset excluding 2 A2A failures: cold 0.433, warm 0.429, Δ **−0.004** (flat)

Memory is not helping the agent. Root cause: the current semantic path
([`PgVectorIndex::search`](../../crates/memkeys-server/src/pg_vector.rs)) returns the
top-k by **pure cosine similarity** (`ORDER BY embedding <=> q`). For SRE recall the
agent needs the most *recent* relevant fact; a stale-but-semantically-similar memory
crowds out the correct recent one. The 256-dim keyed path — which injects the full
**recency-ranked** fact set verbatim — showed a strong interim result (warm 0.867,
Δ+0.300 on ISI-6657). The signal that was working was **recency ordering**, not pure
cosine. This mirrors zep/graphiti, where timestamped graph edges naturally weight
recent nodes.

## Decision

Introduce a **hybrid score** that combines semantic similarity with recency decay:

```
score   = α · cosine_similarity + (1 − α) · recency_decay
recency_decay = exp(−λ · age_seconds)
age_seconds   = max(0, reference_time − fact.ingested_at)   // seconds
```

Implemented as **Option A2 — post-process re-rank in the backend-portable core**,
*not* Option B (index-level scoring). Chosen for four reasons:

1. **Keeps scoring portable and pure.** `semantic.rs` is explicitly the
   backend-portable core — `cosine()` and `rrf_fuse()` already live there and
   unit-test green in CI with no Postgres and no model server. The recency math
   belongs in exactly that place, as a single pure function, tested the same way.
   Option B would push the formula into SQL, duplicate it in `MemVectorIndex`, and
   make α/λ un-testable without a database.
2. **Preserves the HNSW index.** The ANN pass stays `ORDER BY embedding <=> q`, which
   is the only ORDER BY the HNSW index can serve. Option B's composite
   `α·sim + (1−α)·decay` ORDER BY is not index-servable and forces a full scan +
   sort over the tenant's partition on every query.
3. **One formula, one place.** Both backends (`MemVectorIndex`, `PgVectorIndex`) feed
   the same re-ranker; the two adapters change only by returning one extra column
   they already hold (`ingested_at`).
4. **Tuning stays at the edge.** α and λ are deployment knobs read at the
   service/config layer (§2: "the embedding/scoring edge varies most across
   deployments"), not baked into migrations.

### Pipeline (replaces the single ANN truncation in `semantic_search_inner`)

1. Scope → `CandidateSet` (unchanged).
2. **Oversample**: ANN-retrieve the top **N** by cosine (N = `MEMKEYS_RECENCY_CANDIDATES`,
   default 200), not k. HNSW-served, one indexed query.
3. **Positive-cosine filter** (unchanged, but now applied *before* re-rank): drop
   `cosine ≤ 0`. A fact with no shared direction is noise regardless of how recent it
   is — re-ranking first would let a high-recency / zero-cosine fact earn undeserved
   credit, the exact failure the filter exists to prevent (ISI-6079 / CR ISI-6168).
4. **Hybrid re-rank** the survivors by the formula above; sort desc; **truncate to k**.
5. The k-list becomes the `SemanticFact` `RankedList` into RRF — unchanged. Recency
   improves the *within-semantic* ordering; RRF fusion across keyed/graph/semantic is
   untouched.

### `as_of` correctness (hard requirement)

`reference_time = req.as_of.unwrap_or(now)`. For a point-in-time read, age is measured
from `as_of`, so "recent" means recent *as of that historical viewpoint* — not
"recent relative to wall-clock now," which would collapse every historical fact's
decay to ≈0 and silently revert a point-in-time query to pure cosine. `now` is
computed by the service (keeps the core pure/deterministic) and passed into the
re-ranker. This composes with the existing `ingested_at <= as_of` eligibility filter
(ISI-6172): that filter decides *which* facts are visible; `reference_time` decides
*how their age is scored*.

### Interface changes (minimal)

- `ScoredId` gains `ingested_at: DateTime<Utc>`. Both backends already have it
  (`MemVectorIndex` holds `FactVector`; `PgVectorIndex` already stores the
  `ingested_at` column — add it to the two `SELECT`s and the `query_as` tuple).
- New pure fn in `semantic.rs`:
  `hybrid_rerank(scored: Vec<ScoredId>, cfg: &RecencyConfig, reference_time: DateTime<Utc>, k: usize) -> Vec<ScoredId>`.
- `RecencyConfig { alpha: f64, lambda: f64, candidates: usize }`, read once in
  `main.rs` and threaded to `semantic_search_inner`.
- The `VectorIndex::search` call in `semantic_search_inner` requests `cfg.candidates`
  (not `k`); the re-rank truncates to `k`.

### Config knobs (env, global to start)

| Env var | Default | Meaning |
|---|---|---|
| `MEMKEYS_RECENCY_ALPHA` | `0.6` | semantic weight α ∈ [0,1]. **α = 1.0 ⇒ pure cosine (old behavior)** — graceful kill-switch, no separate feature flag needed. |
| `MEMKEYS_RECENCY_LAMBDA` | `1e-6` | decay rate λ per second (e-folding ≈ 11.6 days). |
| `MEMKEYS_RECENCY_CANDIDATES` | `200` | oversample pool size N before re-rank. |

Per-tenant α/λ is a deliberate follow-up, not v1 (YAGNI until a tenant needs a
different half-life). Env is sufficient to ship and validate.

## Consequences

**Positive.** Recovers the recency signal that drove the keyed-path interim win;
degrades gracefully (α=1.0); no migration, no index change, no new dependency; the
one piece of real logic is a pure, CI-green unit test.

**Cost.** Per query, the re-rank sorts ≤ N candidates in Rust (N=200 ⇒ negligible vs
the embed + ANN round-trips). The oversample fetches N rows instead of k; for
agent-memory tenant sizes this is one cheap indexed query. At the SLO ceiling
(p95 ≤ 150 ms @ depth ≤ 3) this is well within budget — the ANN query dominates.

**Risk / ceiling.** If the genuinely-recent-relevant fact falls outside the cosine
top-N, the re-rank can't recover it (N is a recall ceiling). N=200 over per-tenant
fact counts in the thousands is generous; if a future large tenant needs more, N is a
knob. `ponytail:` oversample-capped recall — raise `MEMKEYS_RECENCY_CANDIDATES` or
move to Option B index-level scoring only if a tenant's fact count makes the full-N
fetch expensive.

## Validation (owned by benchmark re-run)

Re-run Set A SRE scenarios. **Success: warm Δ > 0; target Δ ≥ 0.10** to enter the
top-3 tier. Sweep α ∈ {0.4, 0.6, 0.8} and λ for e-folding {3 d, 7 d, 11.6 d}; record
the winning (α, λ) back into the defaults if it beats the suggested starting point.
Unit tests assert: (a) with α<1, a recent-lower-cosine fact outranks a stale-higher-
cosine fact past the decay crossover; (b) α=1.0 reproduces pure-cosine order exactly;
(c) `as_of` uses as_of (not now) as the age reference.

## Out of scope (separate sub-issue)

The 2 A2A empty-answer warm failures (imagepull, oomkill; `tokens_in=0`) are **not** a
memory/retrieval defect — they are a kagent A2A session TTL/reconnect issue in the
`crew-memory-bench` Set A runner harness (`seta_runner.py`). Tracked as a sibling
issue; it does not block this retrieval change and this change does not fix it.
