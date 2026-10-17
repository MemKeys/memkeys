# MemKeys — promotion thresholds and conformance gates (M8)

This document records the M8 harness — the write-path benchmark and the
conformance suite — and the thresholds a change must meet before it is
promoted to `main`. Milestones M2+ extend both the suite and the baseline;
the thresholds here are re-baselined explicitly (never silently).

**How to run (scripted, reproducible):**

```bash
scripts/run-write-bench.sh postgres://user:pass@host:5432/db
```

The script builds the release binaries, runs the conformance suite (must be
`CONFORMANCE GREEN`), then the benchmark with the pinned baseline
configuration. A JSON report lands in `bench-results/` (gitignored). CI runs
both on every PR (`.github/workflows/ci.yml`) against the postgres service
container, with a short smoke configuration.

## 1. Conformance gate (hard)

The conformance suite (`memkeys-bench --bin conformance`) executes numbered
checks of the architecture invariants against a real Postgres through the
real `MemKeysService`. Current coverage (M1 surfaces):

| Group | Checks | Invariants |
|---|---|---|
| §3/§3.1 envelope contract | C01–C07 | per-source anchor matrix, required fields, confidence bounds, provenance discard, a2a decision subject |
| §4 key extraction | C08–C11 | principal-overwrite, run/thread/service enrichment, no invented keys, unkeyed policy |
| §6 stages / FactStore contract | C12–C15 | idempotent retried flush, 409 on conflicting replay, supersession grain |
| §5.1/§5.3 temporal | C16–C17 | read-time supersession collapse, as-of replay visibility |
| §7.2 keyed read | C18–C20 | key required, limit bounds, type filter + ANDed keys |
| WS-4 Invariant T | C21 | tenant floor on every read |

**Promotion rule:** a change is promotable only when the suite reports
`CONFORMANCE GREEN`: zero unlisted failures. Checks may be marked as *known
deviations* in the suite's `KNOWN_DEVIATIONS` list (each entry carries a
tracking issue and is mirrored in the table below) — a deviation is loud, not
silent, and must be removed once fixed. Any NEW invariant dropped by a
milestone is RED and blocks promotion until either fixed or explicitly
registered as a deviation with a tracking issue.

**Known deviations: none.**

## 2. Write-path single-writer ceiling (baseline + thresholds)

The benchmark (`memkeys-bench --bin write-bench`) measures the synchronous
stages 1-4 path — one writer, sequential ops, ack on stage 4 — plus a
supplementary op-1 keyed-read phase. Configuration is pinned for
comparability:

| Parameter | Value |
|---|---|
| ops / warmup / read-ops | 1000 / 100 / 1000 |
| seed | 106112001380375 (deterministic workload) |
| supersede ratio | 0.05 |
| entity.issue key cardinality | 64 |
| fact body size | 256 bytes |

### Baseline (recorded 2026-10-17)

Environment: local dev box, Postgres 18 (scratch instance), synchronous
commit (default durability), release build. This storage has a slow
single-writer commit path — raw single-row autocommit INSERT p50 measured at
~39 ms on the same box, so the pipeline's own overhead over the store is
small (~2 ms p50). The numbers below are the honest ceiling *of that
hardware*, not a hardware-independent constant; thresholds are therefore
expressed relative to the baseline.

| Metric | Run 1 | Run 2 |
|---|---|---|
| Sustained write ceiling (ops/sec) | 10 | 12 |
| Ingest p50 (ms) | 41.3 | 27.1 |
| Ingest p95 (ms) | 178.5 | 128.1 |
| Ingest p99 (ms) | 1733.7 | 1769.3 |
| Ingest max (ms) | 3983.0 | 9204.1 |
| Keyed read ops/sec | 3578 | 3105 |
| Keyed read p50 / p95 / p99 (ms) | 0.24 / 0.43 / 1.13 | 0.27 / 0.65 / 1.48 |

Run-to-run variance on this hardware is roughly ±20% on throughput/p50 and
larger on p99/max (background load) — promotion comparisons must account for
that band.

### Promotion thresholds

Compare a candidate run (same pinned configuration) against the baseline
recorded above, on comparable hardware:

1. **Conformance first:** the conformance gate (§1) is a hard precondition —
   no performance number can compensate for a RED suite.
2. **Write ceiling:** sustained ops/sec must not regress by more than 50%
   vs. the baseline band (i.e. ≥ ~5 ops/sec on baseline-class hardware), and
   ingest p50 must stay ≤ 2× the baseline p50 band (≤ ~80 ms).
3. **Tail:** ingest p99 must stay ≤ 5× the baseline band (≤ ~9 s); a p99
   breach requires investigation before promotion (tail stalls dominate the
   agent flush experience).
4. **Read path:** keyed-read p50 must stay ≤ 2× the baseline band (≤ ~0.6
   ms); read regressions gate promotion just like write regressions.
5. **Re-baselining:** when the M1+ write path intentionally changes shape
   (new stage, new durability knob) or when the reference hardware changes,
   record a new baseline here (two runs), with the environment noted, and
   carry the thresholds forward explicitly. Never edit thresholds silently.

A change that breaches any threshold does not promote until the regression is
fixed or the re-baseline is recorded with its rationale.
