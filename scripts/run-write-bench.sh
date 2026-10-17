#!/usr/bin/env bash
# MemKeys M8 — scripted, reproducible single-writer write-path benchmark run.
#
# Builds the release profile, then runs `write-bench` with the recorded default
# workload (seed 0x6082_2026_1017, 1000 ops, 100 warmup, 5% supersede ratio,
# 64 key cardinality, 256-byte bodies) against a Postgres instance, writing a
# JSON report plus a console summary. Identical seed + op count => identical
# workload; reruns never collide (unique tenant per run).
#
# Usage:
#   scripts/run-write-bench.sh [URL] [--ops N] [--warmup N] [--read-ops N] ...
#
# Database URL resolution: first positional argument, else MEMKEYS_DATABASE_URL,
# else TEST_DATABASE_URL (same order as the binary). Extra flags are forwarded
# to write-bench verbatim, so --ops/--json/... work as on the binary.
#
# Report: bencher-reports/write-bench-<UTC timestamp>.json (plus stdout
# summary). Exit status mirrors write-bench (0 ok, 1 failed phase, 2 usage).

set -euo pipefail

cd "$(dirname "$0")/.."

URL="${1:-}"
EXTRA=()
if [ $# -gt 0 ]; then shift; EXTRA=("$@"); fi
if [ -z "$URL" ]; then
  URL="${MEMKEYS_DATABASE_URL:-${TEST_DATABASE_URL:-}}"
fi
if [ -z "$URL" ]; then
  echo "no database url: pass it as the first argument or set MEMKEYS_DATABASE_URL/TEST_DATABASE_URL" >&2
  exit 2
fi

echo "=== memkeys M8: single-writer write-path benchmark (scripted run) ===" >&2
cargo build --release -p memkeys-bench >&2

mkdir -p bencher-reports
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT="bencher-reports/write-bench-${STAMP}.json"

exec ./target/release/write-bench \
  --url "$URL" \
  --json "$REPORT" \
  "${EXTRA[@]}"
