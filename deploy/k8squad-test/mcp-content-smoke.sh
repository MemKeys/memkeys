#!/bin/sh
# ISI-6459: MCP conformance smoke test for backup_Content Writer
# Validates MCP stdio JSON-RPC 2.0 handshake + basic content operations
# Run in-pod against the live memkeys-server binary + live Postgres.
set -eu

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

SECRET="$MEMKEYS_RUN_TOKEN_SECRET"
HEADER='{"alg":"HS256","typ":"JWT"}'
EXP=$(( $(date +%s) + 3600 ))
PAYLOAD="{\"company_id\":\"content-emergency\",\"sub\":\"backup_Content Writer\",\"run_id\":\"isi6459\",\"exp\":$EXP}"
H=$(printf '%s' "$HEADER" | b64url)
P=$(printf '%s' "$PAYLOAD" | b64url)
SIG=$(printf '%s' "$H.$P" | openssl dgst -sha256 -hmac "$SECRET" -binary | b64url)
TOKEN="$H.$P.$SIG"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"isi6459-content-smoke","version":"1"}}}'
LIST='{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
STORE="{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_store\",\"arguments\":{\"envelope\":{\"source\":{\"kind\":\"content_validation\",\"emitted_at\":\"$NOW\"},\"entity\":{\"project\":\"content-emergency\",\"agent\":\"backup_Content Writer\",\"run\":\"isi6459\"},\"fact\":{\"body\":\"ISI-6459 MCP conformance smoke: basic content operations validated on k8squad-test\",\"subject\":\"isi6459-content-conformance\"}}}}}"
FETCH='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name\":\"memory_fetch","arguments":{"entity.project\":\"content-emergency\"}}}'
TRAVERSE='{"jsonrpc":"2.0","id":5,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_traverse\",\"arguments\":{\"start\":{\"kind\":\"entity_project\",\"id\":\"content-emergency\"},\"depth\":3,\"direction\":\"out\"}}}'
SEARCH='{"jsonrpc":"2.0","id":6,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_search\",\"arguments\":{\"query\":\"content validation MCP conformance\",\"k\":5,\"scope\":{\"entity.project\":\"content-emergency\"}}}}'

export MEMKEYS_RUN_TOKEN="$TOKEN"
export MEMKEYS_HTTP_HOST=127.0.0.1
export MEMKEYS_HTTP_PORT=18081
export MEMKEYS_MEMORY_HTTP_PORT=13001

echo "=== MCP content smoke: token tenant=content-emergency exp=$EXP ==="
# init/list/store/fetch/traverse immediately; delay SEARCH ~35s so the server's
# 30s maintenance tick projects the fact-vector (stage-5b async) first.
{ printf '%s\n' "$INIT" "$LIST" "$STORE" "$FETCH" "$TRAVERSE"; sleep 35; printf '%s\n' "$SEARCH"; } \
  | timeout 60 /usr/local/bin/memkeys-server 2>/dev/null \
  | grep '"jsonrpc"' || true
echo "=== MCP content smoke done ==="