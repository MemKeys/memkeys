# MemKeys — `VectorIndex` Conformance Suite (WS-1)

**Status:** Test-architecture spec (WS-1 follow-up, ISI-5892) · **Author:** Testing Architect (Amelia) · **Date:** 2026-10-14
**Implements:** `docs/architecture.md` §10 (ADR-vector, **Accepted** rev 2, ISI-5866) — esp. §10.3 port, §10.2.5/§10.2.5a scope property, §10.8 split-suite mandate (M6), §10.9 server-side refinements.
**Companion to:** `docs/reference-architecture.md` (system view), `docs/design/mcp-contract.md` (retrieval surface).
**Coordination:** Coder/Claude Code owns the exact-scan threshold + partition-strategy benchmarks (§10.10); this suite consumes those as the **tunable baseline** under which the correctness gates run.

> This document is the **executable specification** for the conformance suite every `VectorIndex`
> adapter (pgvector default, Grail-native plugin, future Qdrant) must satisfy. Each gate is a
> citable test ID with a fixture, procedure, assertion, and pass criterion, structured so it
> renders directly to Go tests (reference language per §10.3) when the adapters land. It does
> **not** re-decide any ADR-vector question; where it refines wording it defers to §10.

---

## 0. The split (M6) and *where each gate runs*

ADR-vector §10.8 mandates a **split** suite. This spec adds a second, equally important split that §10.9/§10.2.6 force but the issue text folds together — **which layer owns the behavior**:

| Split axis | Buckets |
|---|---|
| **By enforcement (M6, §10.8)** | **Correctness gates** — *every adapter must pass, pass/fail, CI-blocking*. **Performance characterization** — *per-adapter, measured + documented, NOT gated*. |
| **By layer (§10.9, §10.2.6)** | **Adapter gates** — run against each live `VectorIndex` impl via a factory. **Server-layer gates** — run **once**, backend-agnostic, over a **recording stub adapter**; exercise fusion / resolve-down / temporal re-filter / RRF, which §10.9 places **server-side** (the adapter returns `ids+scores+Tier+DerivationVersion` only). |

Getting the layer split wrong is the classic failure here: H1/H2/M4 (digest resolve-down, RRF accumulation, over-fetch/truncation) are **not** adapter behavior and must **not** be asserted against a live backend — doing so would both under-test the server and falsely fail a correct adapter. This suite assigns each gate a layer explicitly.

```
suite/
  harness/                 # adapter-agnostic driver
    factory.go             # VectorIndex factory + capability flags (mechanisms, min version)
    embedder_stub.go       # deterministic Embedder (M1) — text→vector, no network
    fixtures.go            # tenants, scopes, derivation versions, as_of timeline, corpora
    recall.go              # exact-KNN ground-truth oracle for recall@k
  adapter/                 # CG-A* — run per backend via factory
  server/                  # CG-S* — run once over recording stub
  perf/                    # PC-* — per adapter, documented not gated
```

---

## 1. Harness contract (how a backend plugs in)

```go
// Every adapter registers a factory + declares which §10.2.5a mechanisms it can exercise,
// so mechanism-specific gates (CG-A1a/b/c) skip-with-reason on backends that lack one
// rather than silently passing.
type Conformance struct {
    Name            string            // "pgvector", "grail", "qdrant"
    New             func(t *testing.T, cfg Config) VectorIndex
    Mechanisms      []ScopeMechanism  // {PartitionPrune, IterativeScan, ExactFlatScan}
    MinVersionNote  string            // e.g. "pgvector >= 0.8.0" (§10.8)
    Gated           bool              // true for OSS default (pgvector); perf is still ungated
}
```

- **Embedder stub (M1, §10.3).** `embedder_stub.go` maps text → a fixed-width unit vector deterministically (hash → seeded basis, L2-normalized), so cosine ranks are reproducible with **no external model**. The stub is the sole embedding source for correctness gates; `Embed` delegation (`a.embedder.Embed`) is tested against it independently (CG-A0).
- **Determinism.** No wall-clock, no RNG in assertions. The `as_of` timeline is a fixed set of logical instants `T0<T1<T2<T3`. Vectors come from the stub. Ground-truth recall uses an in-harness exact KNN over the same stub vectors.
- **Isolation.** Each test gets a fresh namespace (fresh partition/collection/schema) and tears down via `Delete` with a full tenant floor — exercising CG-A4 teardown as a side effect.

---

## 2. Shared fixtures

- **Tenants:** `tA`, `tB` (cross-tenant leakage checks always use both).
- **Scopes (metadata):** `entity.project ∈ {p1,p2}`, `entity.agent ∈ {a1,a2}`, `fact.type`, `category`. Digest adds `scope`, `scope_id`, `member_fact_ids`, `story_id`.
- **Derivation versions:** `gen1=(bge-small, prompt@v1)`, `gen2=(bge-large, prompt@v2)` — incomparable generations for H4.
- **as_of timeline:** facts `f@T0…T3`; supersession `f1 --supersedes--> f1'` at `T2`; a deletion at `T3`; a digest rolled at `T2` whose frozen `member_fact_ids` includes a fact that did not exist at `T1` and one superseded by `T2`.
- **Corpora:** `C_small` (below exact-scan threshold, drives CG-A1c), `C_mixed` (spans partitions, drives A1a/A1b), `C_recall` (≥10k vectors/tenant for PC recall@k).

---

## 3. Correctness gates — ADAPTER (CG-A*) — **every adapter must pass**

### CG-A0 — `Embed` delegation + Embedder stub (M1)
- **When:** call `idx.Embed(ctx, text)` and `embedder.Embed(ctx, text)` with equal text.
- **Assert:** byte-identical vectors; `Embed` performs no write; width = adapter config. **Pass:** equal, pure delegation.

### CG-A1 — Scope isolation (§10.2.5, B1) — the headline gate
Property under test (verbatim intent of #5): (i) **no fact outside the caller's scope** appears in or influences results; (ii) a scope holding **≥ k** in-scope facts returns **exactly k** (no silent under-return).

- **CG-A1-iso (cross-tenant):** load identical vectors under `tA` and `tB`; `ScopedSearch(filter.Tenant=tA, k)`. **Assert:** zero `tB` ids in results, at any k, including k > |tA in-scope|. **Pass:** no cross-tenant id ever returned.
- **CG-A1-fill (no silent under-return):** scope with exactly `N ≥ k` in-scope facts. **Assert:** `len(results) == k`; all k in-scope; raise k to N → `len == N`; k = N+1 → `len == N` (not an error, scope exhausted). This is the gate that fails a "post-filter over fixed top-N ANN" backend (§10.2.5 last sentence).
- **Mechanism coverage (pgvector, §10.2.5a) — each asserts A1-iso + A1-fill:**
  - **CG-A1a partition/partial-index prune** — scope maps to a per-tenant (opt. per-project) partition/partial HNSW; traversal is a full HNSW over in-scope rows only.
  - **CG-A1b iterative index scan** — finer sub-scope within a partition; `hnsw.iterative_scan` keeps traversing past the first N until k in-scope collected **or** scan ceiling (`hnsw.max_scan_tuples`). **Assert:** at ceiling the adapter does **not** fabricate out-of-scope fill; documents a truncated-at-ceiling result honestly.
  - **CG-A1c exact flat scan** — `C_small` below the adapter's count threshold; bypass ANN, exact KNN. **Assert:** recall = 1.0 vs ground-truth oracle; A1-fill holds exactly.
  - Backends declaring only a native-filtered-HNSW mechanism (Qdrant/Grail) run A1-iso + A1-fill once; A1a/b/c skip-with-reason.
- **CG-A1-empty:** zero-candidate scope → **empty slice, not error**; unpopulated tier → empty slice; `k <= 0` → **error** (§10.3 `ScopedSearch` doc, M5).

### CG-A2 — Idempotent `Upsert` + atomic batch (M3)
- **CG-A2-idem:** `Upsert` the same `Item.ID` twice (changed vector/metadata 2nd time). **Assert:** one row; last write wins; `Stat.Count` unchanged by the 2nd. Digest tier: re-roll with **stable `digest.id` per (scope,scope_id)** (Coder-2) is a pure `ON CONFLICT` replace — no id churn, `member_fact_ids` de-dup unambiguous.
- **CG-A2-atomic:** batch where item *j* is invalid (e.g. wrong vector width, or un-normalized vector under cosine — §10.3 validates normalization at Upsert). **Assert:** **all-or-nothing** — zero items from the batch persist; `Stat.Count` == pre-batch. **Pass:** no partial commit.
- **CG-A2-batchmax:** batch > adapter `maxBatch` → error, nothing persisted.

### CG-A3 — `Rebuild`-from-SoT equivalence + retention horizon (B5-i, §5.6)
- **CG-A3-equiv:** seed SoT scope → `Rebuild` into empty index → compare to a direct `Upsert` of the same facts. **Assert:** identical id set, identical `DerivationVersion` stamps, `ScopedSearch` rankings equal for a fixed query (vectors are stub-deterministic).
- **CG-A3-horizon (B5-i):** SoT scope contains facts **clamped by the §5.6 projection horizon** (older than retention / beyond horizon). `Rebuild`. **Assert:** horizon-clamped vectors are **NOT** resurrected — the replay source query applies the §5.6 horizon; post-rebuild index contains only within-horizon facts. **Pass:** zero resurrected retention-clamped ids. *(This is the gate a naive "replay everything in SoT" implementation fails.)*

### CG-A4 — Tenant-floor non-omittable on destructive ops (B4)
- **CG-A4-del-empty:** `Delete(tier, filter{Tenant:""}, ids)` → **hard error**, nothing deleted (even with explicit ids). 
- **CG-A4-rebuild-empty:** `Rebuild(tier, filter{Tenant:""})` → **hard error**, no rebuild.
- **CG-A4-no-tenantwide-by-omission:** `Delete(tier, filter{Tenant:tA}, nil)` deletes **only tA's tier scope** (teardown) and **never** touches `tB`. **Assert:** `tB` `Stat.Count` unchanged. **Pass:** empty Tenant is never "match all"; cross-tenant blast radius = 0.

### CG-A5 — `Delete` ↔ `Rebuild` scope-lock isolation + shadow-swap (B5-ii, H5)
- **CG-A5-lock:** concurrent `Rebuild(scope S)` and `Delete(scope S)` on overlapping scope. **Assert:** serialized via per-`(tier,scope)` lock; end state is well-defined (no half-deleted/half-rebuilt scope, no lost delete, no orphan rows). **Pass:** deterministic, order-equivalent-to-some-serial-schedule outcome.
- **CG-A5-serving (H5):** issue `ScopedSearch(S)` continuously while `Rebuild(S)` runs. **Assert:** every search is served from the **prior** generation until the **atomic swap**; no empty/partial window; after swap, results reflect the new generation. **Pass:** zero search returns a partial build.

### CG-A6 — `derivation_version` pinning (H4)
- **CG-A6-filter:** index holds `gen1` + `gen2` for the same scope; `ScopedSearch(filter.DerivationVersion=gen1)`. **Assert:** only `gen1` ids returned; each `ScoredID.DerivationVersion == gen1`.
- **CG-A6-return:** without a `DerivationVersion` filter, every `ScoredID` still **carries** its `DerivationVersion` (so the server never fuses incomparable generations — enforced in CG-S4). **Pass:** adapter never blends generations when pinned, and always labels the generation it returns.

### CG-A7 — `Stat` read-side (H3 / Coder-4)
- **When:** after known Upserts/Deletes, `Stat(tier, filter)`.
- **Assert:** `Count` == in-scope projection count (powers §6 projection-lag metric); `MaxDerivationVersion` == newest generation present; respects tenant floor (empty Tenant → error, consistent with B4). **Pass:** cheap, no vector query, matches the oracle count.

---

## 4. Correctness gates — SERVER-LAYER (CG-S*) — run once, backend-agnostic

Run over a **recording stub adapter** that returns scripted `ScoredID` lists (with `Tier` + `DerivationVersion`). These assert server behavior §10.9/§10.2.6 places **above** the port. They are **not** adapter gates.

### CG-S1 — Digest resolve-down temporal re-filter (H1)
- **Setup:** a digest hit at `T2` whose frozen `member_fact_ids` includes (x) a fact not existing at `T1` and (y) a fact superseded by `T2`; query `as_of = T1`.
- **Assert:** server re-filters the frozen member list by `as_of` **and** current supersession/deletion state **before hydration** — (x) excluded (did not exist at T1), (y) resolved to its state-at-T1. **Pass:** frozen `member_fact_ids` treated as a *candidate pointer set*, never a trusted result; no `as_of` leak.

### CG-S2 — RRF accumulation + digest explosion (H2)
- **Setup:** a `fact.id` that is both a direct fact-tier hit (rank r1) and a member of a ranked digest hit (rank r2).
- **Assert:** final score = `Σ_i w_i / (k_rrf + rank_i)` with `k_rrf = 60`, equal default weights — the fact accumulates **both** contributions; a high-ranked digest boosts all its members. **Pass:** cross-tier ordering matches the formula exactly.

### CG-S3 — Over-fetch + final-k truncation (M4)
- **Assert:** each tier over-fetched to `max(k, ceil(k · overfetch))` (default `overfetch ≈ 4`) before fusion; digests exploded to members; scores accumulated by `fact.id`; cross-tier de-dup; **then** truncate to final `k`. **Pass:** final list length == k, contains the true top-k by accumulated score (not top-k of either single tier).

### CG-S4 — Never fuse across incomparable generations (H4, server half)
- **Setup:** stub returns `gen1` and `gen2` `ScoredID`s for one query during a generation transition.
- **Assert:** server pins to the active `DerivationVersion` until swap; distances/ranks from different generations are **never** co-mingled in one RRF accumulation. **Pass:** no cross-generation fusion.

---

## 5. Performance characterization (PC-*) — per adapter, **documented NOT gated** (M6)

Measured against the **tuned** pgvector baseline (partitioned/partial indexes + iterative scan + exact-scan fallback already tuned — Coder §10.10), never a naive single shared index. Output is a per-adapter report, the measured input to the §10.6 Qdrant-promotion decision — **no pass/fail threshold here.**

| ID | Metric | Procedure | Reported |
|---|---|---|---|
| **PC-1** | **recall@k** | `ScopedSearch` vs in-harness exact-KNN oracle over `C_recall`, across representative scope filters (broad tenant, per-project, highly selective sub-scope) | recall@{1,10,50} per scope selectivity bucket |
| **PC-2** | **p99 `ScopedSearch` latency** | representative concurrent scoped-search load, warm index | p50/p95/p99 per scope selectivity + concurrency level |
| **PC-3** | mechanism cross-over | sweep candidate cardinality across the exact-scan threshold | observed A1c→A1b/A1a switch point vs Coder's configured threshold |
| **PC-4** | churn/maintenance (H6) | digest re-roll rate → dead-tuple growth → `REINDEX`/rebuild reclaim | bloat curve, maintenance cadence |

**Grail-native (§10.7, M6):** held to **all** CG-A correctness gates, but **characterizes its own** PC-* rather than being held to pgvector's bar. Reports PC-1/PC-2 for its DQL-filtered similarity path.

---

## 6. Traceability matrix

| Gate | Layer | §ref / invariant | Enforced |
|---|---|---|---|
| CG-A0 | adapter | §10.3 `Embed`, M1 | gate |
| CG-A1 (iso/fill/a/b/c/empty) | adapter | §10.2.5 + §10.2.5a, **B1**, M5 | gate |
| CG-A2 (idem/atomic/batchmax) | adapter | §10.3 `Upsert`, **M3**, Coder-2 | gate |
| CG-A3 (equiv/horizon) | adapter | §10.3 `Rebuild`, **B5-i**, §5.6 | gate |
| CG-A4 (del/rebuild/omission) | adapter | §10.3 destructive-op floor, **B4** | gate |
| CG-A5 (lock/serving) | adapter | **B5-ii**, **H5** | gate |
| CG-A6 (filter/return) | adapter | **H4** (adapter half) | gate |
| CG-A7 | adapter | `Stat`, H3 / Coder-4 | gate |
| CG-S1 | server | **H1** | gate |
| CG-S2 | server | **H2** | gate |
| CG-S3 | server | **M4** | gate |
| CG-S4 | server | **H4** (server half) | gate |
| PC-1…PC-4 | adapter | §10.6 triggers, **M6** | **documented, not gated** |

Every ADR-vector blocker/high/medium that names a testable behavior (B1, B4, B5-i, B5-ii, H1, H2, H3, H4, H5, M1, M3, M4, M6, Coder-2/4) maps to at least one gate above. Behaviors §10.9 marks server-side (H1/H2/M4 + H4-server) are in §4, not §3 — the intended coverage boundary for an adapter author.

---

## 7. Status & handoffs

- **Ready to encode now:** the ADR is Accepted (rev 2); this spec is stable against §10.3's six-op port. No adapter code exists yet (spec-only repo), so this is the *specification of record*; it renders to `suite/adapter`, `suite/server`, `suite/perf` Go tests when the pgvector adapter lands.
- **Depends on (soft, §10.10):** Coder/Claude Code's exact-scan **threshold** + **partition strategy** config — consumed as the tuned baseline for CG-A1c mechanism coverage and all PC-* runs. Not a hard blocker to this spec; a hard input to *executing* PC-* and CG-A1c against real pgvector.
- **Review path:** Architect (Winston, ADR-vector owner) for gate↔invariant fidelity; Coder for CG-A1a/b/c mechanism realism + threshold/partition inputs.
