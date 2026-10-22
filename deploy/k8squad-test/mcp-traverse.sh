#!/bin/sh
# ISI-6333: confirm memory_traverse over MCP using the opaque composite node id
# (tenant|entity.project|value) — NOT the raw value. Re-uses the fact stored by
# mcp-smoke.sh (entity_fact edge already present).
set -eu
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
SECRET="$MEMKEYS_RUN_TOKEN_SECRET"
EXP=$(( $(date +%s) + 3600 ))
H=$(printf '%s' '{"alg":"HS256","typ":"JWT"}' | b64url)
P=$(printf '%s' "{\"company_id\":\"conformance-6333\",\"sub\":\"testing-architect\",\"run_id\":\"isi6333\",\"exp\":$EXP}" | b64url)
SIG=$(printf '%s' "$H.$P" | openssl dgst -sha256 -hmac "$SECRET" -binary | b64url)
export MEMKEYS_RUN_TOKEN="$H.$P.$SIG"
export MEMKEYS_HTTP_HOST=127.0.0.1 MEMKEYS_HTTP_PORT=18081 MEMKEYS_MEMORY_HTTP_PORT=13001

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
# opaque id: tenant|entity.project|value (kind.as_str() uses the dotted form)
TRAVERSE='{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_traverse","arguments":{"start":{"kind":"entity_project","id":"conformance-6333|entity.project|conformance-6333"},"edges":["entity_fact"],"depth":3,"direction":"out","hydrate":true}}}'

{ printf '%s\n' "$INIT" "$TRAVERSE"; sleep 2; } \
  | timeout 25 /usr/local/bin/memkeys-server 2>/dev/null \
  | grep '"jsonrpc"' || true
