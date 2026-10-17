# M8 — Write-path benchmark & conformance: promotion thresholds

Status: recorded 2026-10-17 (M8, ISI-6082). This document is the promotion
gate reference: what the harness measures, the recorded baseline numbers, and
the thresholds a milestone must meet before it promotes to `main`.

## 1. What runs

Both binaries live in `crates/memkeys-bench` and talk to a real Postgres
through the real `MemKeysService` (M1+ surfaces — never mocks of the pipeline):

| Tool | What it proves | Gate |
|---|---|---|
| `conformance` | 21 numbered checks of the §3/§3.1 envelope contract, §4 key extraction, §5.1 supersession, §5.3 point-in-time replay, §6 stages 1–4, §7.2 keyed read, WS-4 Invariant T | **Hard**: all 21 PASS (exit 0) or the milestone does not promote |
| `write-bench` | Single-writer ceiling: sustained ops/sec + p50/p95/p99 ingest latency of the sequential stages 1–4 write path (ack on stage 4), plus a supplementary op-1 keyed-read phase | **Relative**: within the regression thresholds of §4 below |

The scripted reproducible run is `scripts/run-write-bench.sh` — it builds the
harness, runs the conformance suite, then runs the benchmark with the pinned
workload (fixed seed `0x6082_2026_1017` = `106112001380375`, 1000 write ops,
100 warmup excluded, 1000 keyed reads, 5% supersession ratio, 64 distinct
issue keys, 256-byte bodies) and writes a JSON report. Same seed + op count
always produces the byte-identical workload, so runs are comparable across
hosts and over time.

## 2. Recorded baseline (2026-10-17, ISI-6082)

Environment: PostgreSQL 18.1, linux/x86_64, memkeys v0.1.0 (M1 write path),
single-writer sequential, local Postgres on the shared dev host (`/` on ext4).
Two durability configurations were measured; both are recorded because the
ceiling is dominated by the WAL fsync, not the pipeline:

**Durable (default `fsync=on`)** — the deployment-representative number:

| Metric | Run A | Run B |
|---|---|---|
| write ops/sec | 13 | 10 |
| ingest p50 (ms) | 39.6 | 45.9 |
| ingest p95 (ms) | 143.7 | 219.4 |
| ingest p99 (ms) | 1499.8 | 1184.5 |
| keyed read ops/sec | 4166 | 2589 |
| read p50 (ms) | 0.213 | 0.246 |

**Non-durable (`fsync=off`)** — isolates the pipeline cost from disk commit:

| Metric | Value |
|---|---|
| write ops/sec | **1184** |
| ingest p50 (ms) | 0.230 |
| ingest p95 (ms) | 0.565 |
| ingest p99 (ms) | 3.078 |
| keyed read ops/sec | 1291 |
| read p50 (ms) | 0.654 |

Reading of the baseline: the stages 1–4 pipeline itself costs well under a
millisecond at p50 (~0.23 ms per ingest without disk commit); the durable
ceiling is entirely fsync-bound and, on a shared host, high-variance (p99 in
the 1–1.5 s band from WAL/fsync storms). The keyed-read path is not
fsync-bound (~0.2–0.7 ms p50).

Because absolute durable numbers are host- and storage-specific, promotion
decisions use the **relative** thresholds below against a same-host baseline,
re-recorded at each milestone via the same scripted run.

## 3. Conformance gate (hard)

- `scripts/run-write-bench.sh` (CI: the conformance job) must report
  `21/21` checks passed (`CONFORMANCE GREEN`, exit 0).
- Any FAIL blocks promotion until fixed or the conformance check is revised
  with an accepted plan change — the suite is the executable form of the
  ratified §3/§3.1 contract (ISI-5801 PM ruling) plus the WS-4 invariants.
- M8 already exercised this gate: it caught C01 (gateway_flush accepted
  without the mandatory `entity.run` anchor) — fixed in the M1 pipeline as
  part of M8.

## 4. Regression thresholds (relative, same host, same seed/config)

A milestone promotes to `main` only if its scripted run, on the same host as
the last recorded baseline, satisfies all of:

| Metric | Threshold | Rationale |
|---|---|---|
| write ops/sec (durable) | >= **70%** of baseline | Tolerates host noise; catches pipeline-level regressions (e.g. a per-op extra round-trip) |
| ingest p95 (durable) | <= **1.5x** baseline | Latency SLO guard |
| write ops/sec (fsync=off) | >= **70%** of baseline | Isolates pipeline-only regressions from storage noise |
| keyed-read p95 | <= **2x** baseline | Read path guard |
| conformance | 21/21 green | Hard gate |

Gross regressions (any threshold missed) block promotion and get an issue; the
baseline is only re-recorded (this doc updated) when a milestone deliberately
changes the write path's cost structure, with the reason noted here.

## 5. CI wiring

`.github/workflows/ci.yml` runs, against the postgres service container:

1. fmt + clippy + unit/integration tests (unchanged),
2. the full conformance suite (`cargo run -p memkeys-bench --bin conformance`),
3. a benchmark smoke run (small op count) proving the harness executes and
   produces its report — CI is not a latency reference (shared runners are
   too noisy); latency baselines come from the scripted run on a designated
   host.
