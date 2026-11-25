# Golden Path C — augmented-read SLO (ISI-7814)

The augmented read (ISI-7813, [retrieval contract](../retrieval-contract.md))
returns a fetch/search response plus an `applicable_linters` block. This page
records the **service-level objective** for that fold-in and the committed
benchmark that proves it.

> **SLO — the linter attachment adds &lt; 10 ms p95 over the base
> fetch/search latency.**

Parent: [ISI-7787](../adr/ADR-0004-linter-data-model.md) · Linter data model:
[ADR-0004](../adr/ADR-0004-linter-data-model.md).

## What the attachment is

On a read that opts into linters, after the base fetch/search returns its
facts, the service calls `MemKeysService::resolve_applicable_linters` with the
verified tenant and the distinct in-scope projects of the returned facts. That
call is the only added work the SLO governs.

Two cost regimes:

- **warm** — the common **live-head** read (`as_of` unset). Served from a
  short-TTL cache keyed on `(tenant, in-scope project set)`
  (`MEMKEYS_LINTER_CACHE_TTL_MS`, default 5 s). Linters change rarely relative
  to the read rate, so steady state is a cache hit: an in-memory lookup and a
  clone of a small resolved set — **no store round-trip**.
- **cold** — a cache miss (first read of a key, after TTL expiry, or cache
  disabled): one `list_linters` read (served by the partial `facts_linter_idx`
  in Postgres, migration 0008) plus the de-dup and global-first-then-severity
  ordering. Historical reads (`as_of` set) always take this path — they bypass
  the cache.

Advisory-only v1 (ADR-0004): nothing blocks on the block, so a cache entry
stale by at most the TTL is acceptable and there is no write-time invalidation.

## The benchmark

`crates/memkeys-bench/benches/augmented_read.rs` (`cargo bench -p memkeys-bench
--bench augmented_read`). It builds a representative in-memory corpus —
8,000 regular facts across 16 projects, 40 global linters, 320 context linters
— and measures `resolve_applicable_linters` on both the warm and cold paths,
resolving against a 4-project in-scope set.

Like `hot_paths`, it runs against `MemStore` (no Postgres) so it isolates the
code path, not the database, and is cheap enough to schedule in CI. It emits a
p50/p95/p99 summary (the SLO's own percentile, 5,000 samples/path) and
registers Criterion `bench_function`s so CI tracks the mean for regressions.
`base_fetch` (one keyed read over the same corpus) is included as the
"current fetch/search latency" the overhead is measured over.

## Measured result — within budget

Reference run (dev profile, `cargo bench`, 5,000 samples/path):

| Path | p50 | p95 | p99 | vs. 10 ms budget |
|---|---|---|---|---|
| **warm (cached)** — steady state | 0.012 ms | **0.024 ms** | 0.031 ms | **~420× under** |
| **cold (uncached)** — cache-miss upper bound | 0.154 ms | **0.174 ms** | 0.199 ms | **~57× under** |
| base_fetch (context, not overhead) | 0.226 ms | 0.353 ms | 0.473 ms | — |

**The SLO is met with a wide margin on both paths.** The governing warm-path
overhead (p95 ≈ 0.024 ms) is three orders of magnitude under the 10 ms budget.
Even if *every* read missed the cache, the attachment compute (p95 ≈ 0.174 ms)
stays comfortably within budget; the cache's job in production is to remove the
`list_linters` round-trip's network/DB latency — which this in-memory bench
does not model — from the hot path, so the real-world warm win is larger than
the compute numbers alone show.

Absolute numbers scale with machine and corpus; the committed bench is the
reproducible regression guard, and the **p95 budget headroom** (hundreds of ×)
is the durable result.
