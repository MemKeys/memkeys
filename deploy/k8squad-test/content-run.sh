#!/bin/sh
# ISI-6459: Content writer process health check
# Basic monitoring for backup_Content Writer processes
set -u

echo "=== Content writer health check ==="

# Check if basic content operations are working
curl -s -o /dev/null -w "health: http=%{http_code} time=%{time_total}\n" \
  "http://memkeys.memkeys.svc.cluster.local:3000/memory/store" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"envelope":{"source":{"kind":"health_check","emitted_at":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'"},"entity":{"project":"content-emergency","agent":"backup_Content Writer","run":"isi6459"},"fact":{"body":"Content writer health check performed","subject":"content-health"}}}' \
  | grep -v "^$" || true

# Check if content operations are responsive  
curl -s -o /dev/null -w "fetch: http=%{http_code} time=%{time_total}\n" \
  "http://memkeys.memkeys.svc.cluster.local:3000/memory/fetch" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"entity.project":"content-emergency"}' \
  | grep -v "^$" || true

echo "=== Content health check done ==="