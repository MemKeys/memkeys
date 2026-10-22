#!/bin/sh
# ISI-6333 in-cluster SLO: concurrency sweep on the heavy depth-3 recall path,
# to separate engine latency from single-node 500m-CPU-limit saturation.
set -u
URL=http://memkeys.memkeys.svc.cluster.local:3000/memory/graph/traverse
RECALL='{"start":{"kind":"entity_issue","id":"slo-6333|entity.issue|issue-0"},"edges":["entity_fact","supersedes","produced_by"],"direction":"out","depth":3,"deadline_ms":500}'
STRESS='{"start":{"kind":"entity_project","id":"slo-6333|entity.project|slo-6333"},"edges":["entity_fact","produced_by","supersedes"],"direction":"both","depth":3,"deadline_ms":500}'
echo "target=$URL depth=3 (server pod cpu limit=500m)"
curl -s -o /tmp/r.json -w "recall sanity: http=%{http_code} bytes=%{size_download}\n" "$URL" -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$RECALL"
echo "--- RECALL concurrency sweep ---"
sh /scripts/load.sh "$URL" 400  1  "RECALL c1"  "$RECALL"
sh /scripts/load.sh "$URL" 800  4  "RECALL c4"  "$RECALL"
sh /scripts/load.sh "$URL" 1600 8  "RECALL c8"  "$RECALL"
sh /scripts/load.sh "$URL" 3200 16 "RECALL c16" "$RECALL"
sh /scripts/load.sh "$URL" 3200 32 "RECALL c32" "$RECALL"
echo "--- STRESS (project both) sweep ---"
sh /scripts/load.sh "$URL" 1600 8  "STRESS c8"  "$STRESS"
sh /scripts/load.sh "$URL" 3200 32 "STRESS c32" "$STRESS"
echo "--- done ---"
