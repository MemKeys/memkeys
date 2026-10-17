#!/usr/bin/env bash
# MemKeys M8 — scripted write-path benchmark + conformance run.
#
# Reproducible single-command run of the M8 harness against a Postgres
# instance: builds the release binaries, executes the conformance suite
# (must be GREEN), then the single-writer write-path benchmark with the
# pinned baseline configuration (docs/promotion-thresholds.md).
#
# Usage:
#   scripts/run-write-bench.sh [DATABASE_URL]
#
# The URL may also be supplied via MEMKEYS_DATABASE_URL or TEST_DATABASE_URL.
# Results (human summary on stdout + JSON report) are written to
# bench-results/<timestamp>.json (gitignored).

set -euo pipefail

cd "$(dirname "$0")/.."

URL="${1:-${MEMKEYS_DATABASE_URL:-${TEST_DATABASE_URL:-}}}"
if [ -z "$URL" ]; then
    echo "error: no database url (pass one, or set MEMKEYS_DATABASE_URL/TEST_DATABASE_URL)" >&2
    exit 2
fi

echo "=== memkeys M8 harness: build (release) ==="
cargo build --release -p memkeys-bench

echo
echo "=== conformance suite ==="
./target/release/conformance "$URL"

echo
echo "=== write-path benchmark (single-writer ceiling, pinned baseline config) ==="
mkdir -p bench-results
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
./target/release/write-bench \
    --url "$URL" \
    --ops 1000 --warmup 100 --read-ops 1000 \
    --seed 106112001380375 \
    --supersede-ratio 0.05 --key-cardinality 64 --body-bytes 256 \
    --json "bench-results/write-bench-${STAMP}.json"

echo
echo "JSON report: bench-results/write-bench-${STAMP}.json"
echo "Compare against the baseline and thresholds in docs/promotion-thresholds.md."
