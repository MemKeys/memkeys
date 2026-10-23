#!/bin/sh
# ISI-6333: MCP stdio JSON-RPC 2.0 handshake + four-op conformance smoke,
# run in-pod against the live memkeys-server binary + live Postgres.
# Launches a SECOND server instance on loopback alt-ports (health 18080,
# memory 13000) so it never collides with the primary listeners; drives the
# MCP stdio loop by piping newline-delimited JSON-RPC requests to its stdin.
set -eu

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

SECRET="$MEMKEYS_RUN_TOKEN_SECRET"
HEADER='{"alg":"HS256","typ":"JWT"}'
EXP=$(( $(date +%s) + 3600 ))
PAYLOAD="{\"company_id\":\"conformance-6333\",\"sub\":\"testing-architect\",\"run_id\":\"isi6333\",\"exp\":$EXP}"
H=$(printf '%s' "$HEADER" | b64url)
P=$(printf '%s' "$PAYLOAD" | b64url)
SIG=$(printf '%s' "$H.$P" | openssl dgst -sha256 -hmac "$SECRET" -binary | b64url)
TOKEN="$H.$P.$SIG"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"isi6333-smoke","version":"1"}}}'
LIST='{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
STORE="{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_store\",\"arguments\":{\"envelope\":{\"source\":{\"kind\":\"gateway_flush\",\"emitted_at\":\"$NOW\"},\"entity\":{\"project\":\"conformance-6333\",\"agent\":\"testing-architect\",\"run\":\"isi6333\"},\"fact\":{\"body\":\"ISI-6333 MCP conformance smoke: four-op handshake validated on k8squad-test\",\"subject\":\"isi6333-conformance\"}}}}}"
FETCH='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_fetch","arguments":{"entity.project":"conformance-6333"}}}'
TRAVERSE='{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_traverse","arguments":{"start":{"kind":"entity_project","id":"conformance-6333"},"depth":3,"direction":"out"}}}'
SEARCH='{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"MCP conformance four-op handshake","k":5,"scope":{"entity.project":"conformance-6333"}}}}'

export MEMKEYS_RUN_TOKEN="$TOKEN"
export MEMKEYS_HTTP_HOST=127.0.0.1
export MEMKEYS_HTTP_PORT=18080
export MEMKEYS_MEMORY_HTTP_PORT=13000

echo "=== MCP smoke: token tenant=conformance-6333 exp=$EXP ==="
# init/list/store/fetch/traverse immediately; delay SEARCH ~35s so the server's
# 30s maintenance tick projects the fact-vector (stage-5b async) first.
{ printf '%s\n' "$INIT" "$LIST" "$STORE" "$FETCH" "$TRAVERSE"; sleep 35; printf '%s\n' "$SEARCH"; } \
  | timeout 60 /usr/local/bin/memkeys-server 2>/dev/null \
  | grep '"jsonrpc"' || true
echo "=== MCP smoke done ==="
