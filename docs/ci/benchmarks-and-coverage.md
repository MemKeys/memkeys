# Benchmarks & coverage gating (ISI-6094 P2)

This documents the perf-regression harness and the coverage fail-threshold
introduced for the PR CI suite (ISI-6094). P2 = ISI-6100.

## Criterion hot-path benches

`crates/memkeys-bench/benches/hot_paths.rs` micro-benchmarks the CPU-bound hot
paths of `memkeys-server`, with no database required:

| Group / bench            | Hot path                                                             |
| ------------------------ | ------------------------------------------------------------------- |
| `write_pipeline/normalize`   | Stage 1: envelope validation, SHA-256 body hash, UUIDv5 idempotent id |
| `write_pipeline/key_stage`   | Stage 2: entity-key resolution + anchor enforcement (`NoopResolver`) |
| `write_pipeline/temporalize` | Stage 3: supersession-grain guard                                   |
| `write_pipeline/stages_1_3`  | Stages 1-3 end to end (full per-op synchronous compute)             |
| `keyed_lookup/{1000,8000}`   | Attribute key lookup/index: `MemStore::fetch_by_keys` over a corpus |

Run locally:

```bash
cargo bench -p memkeys-bench --bench hot_paths
# machine-readable, for tracking:
cargo bench -p memkeys-bench --bench hot_paths -- --output-format bencher
```

These isolate code regressions from database I/O. The end-to-end single-writer
ceiling against a real Postgres stays in the `write-bench` binary and its
`ci.yml` smoke step.

## Perf-regression tracking in CI

`.github/workflows/bench.yml` runs the criterion benches **off the PR hot path**
— on pushes to `main`, nightly (03:17 UTC), and manual `workflow_dispatch`. It
never triggers on `pull_request`, so the PR gate stays fast.

Each run:

1. produces the bencher-format report and the full `target/criterion` tree,
   uploaded as the `criterion-report-<sha>` artifact (the comparable baseline/
   report artifact);
2. restores the rolling baseline from the Actions cache (no `gh-pages` branch or
   push secret required), compares via
   `benchmark-action/github-action-benchmark` (tool `cargo`), and reports any
   bench >1.5× slower than baseline as a workflow annotation + job summary
   (`fail-on-alert: false` — report-only, does not gate);
3. saves the updated baseline back to the cache keyed by commit SHA.

## Coverage fail-threshold

**Status: pending P0 (ISI-6097).** P0 adds a report-only `cargo-llvm-cov`
coverage job to `ci.yml` (lcov report + PR summary, no threshold). Once that job
has merged and produced a baseline, enforce the threshold by adding the
fail-under flag to that same job:

```yaml
# in the coverage job introduced by ISI-6097:
- name: Coverage (fail under threshold)
  run: cargo llvm-cov --workspace --fail-under-lines 70
```

`cargo-llvm-cov` exits non-zero with a clear message
(`error: coverage (NN.NN%) is less than the minimum 70%`) when coverage drops
below the threshold. 70% is the default proposal; adjust once the P0 baseline is
known. This change is intentionally **not** applied here to avoid colliding with
P0's in-flight `ci.yml` edits and because there is no coverage job to gate yet.
