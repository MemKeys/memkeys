#!/usr/bin/env bash
# ISI-6328 (Ph2 cutover): MemKeys MCP stdio launcher for Paperclip claude_local
# agents. Referenced as the `command` of a `memkeys` stdio MCP server in the
# agent MCP config (host ~/.claude.json or a per-agent/per-company override once
# Paperclip supports one — see ISI-6328 for the scoping dependency).
#
# MemKeys MCP is a stdio JSON-RPC 2.0 server coupled to the full memkeys-server
# binary (crates/memkeys-server/src/main.rs: mcp::run_stdio spawned alongside the
# HTTP listeners). There is no thin remote MCP connector yet, so this launcher
# drives the in-pod binary's stdio loop over a `kubectl exec -i` pipe. The
# agent's MCP client owns this process's stdin/stdout; closing stdin ends it.
#
# Principal binding (MemKeys derives tenant/agent from MEMKEYS_RUN_TOKEN, a
# launch-bound HS256 JWT): we pass the Paperclip-injected identity THROUGH to an
# in-pod mint, so the run-token SECRET never leaves the cluster while the token
# still carries the real per-agent principal:
#   company_id -> tenant floor   (PAPERCLIP_COMPANY_ID)
#   sub        -> entity.agent    (PAPERCLIP_AGENT_ID)
#   run_id     -> entity.run      (PAPERCLIP_RUN_ID)
#
# VALIDATED 2026-10-20 from the paperclip host (10.0.0.189) against k8squad-test:
# init/tools-list/store/fetch and, cross-session, fetch/traverse/search all
# round-tripped (tenant canary-6328). See ISI-6328-cutover-design.md.
set -euo pipefail

KUBECONFIG="${MEMKEYS_KUBECONFIG:-/home/hrexed/.config/capmox/k8squad-test.kubeconfig}"
export KUBECONFIG
NS="${MEMKEYS_NAMESPACE:-memkeys}"
DEPLOY="${MEMKEYS_DEPLOY:-deploy/memkeys}"

# Only this company's agents are in scope for the dogfood cutover. Other
# claude_local agents on the shared host must NOT spawn a full in-pod server.
TARGET_COMPANY="${MEMKEYS_TARGET_COMPANY:-615e9564-65df-4465-9b19-a5afb73446c6}"
if [ "${PAPERCLIP_COMPANY_ID:-}" != "$TARGET_COMPANY" ]; then
  # Not in scope: do nothing but hold the pipe open so the MCP client sees a
  # quiet (tool-less) server rather than a crash loop. Ends when stdin closes.
  exec cat >/dev/null
fi

COMPANY="${PAPERCLIP_COMPANY_ID}"
AGENT="${PAPERCLIP_AGENT_ID:-stdio-dev}"
RUN="${PAPERCLIP_RUN_ID:-}"
# Random high alt-ports so concurrent agent sessions don't collide on the
# in-pod server's HTTP + memory listeners (unused here, but bound at startup).
HP=$(( 20000 + (RANDOM % 20000) ))
MP=$(( HP + 1 ))

exec kubectl -n "$NS" exec -i "$DEPLOY" -- sh -c '
  b64url() { openssl base64 -A | tr "+/" "-_" | tr -d "="; }
  EXP=$(( $(date +%s) + 3600 ))
  H=$(printf "%s" "{\"alg\":\"HS256\",\"typ\":\"JWT\"}" | b64url)
  P=$(printf "%s" "{\"company_id\":\"'"$COMPANY"'\",\"sub\":\"'"$AGENT"'\",\"run_id\":\"'"$RUN"'\",\"exp\":$EXP}" | b64url)
  SIG=$(printf "%s" "$H.$P" | openssl dgst -sha256 -hmac "$MEMKEYS_RUN_TOKEN_SECRET" -binary | b64url)
  export MEMKEYS_RUN_TOKEN="$H.$P.$SIG"
  export MEMKEYS_HTTP_HOST=127.0.0.1 MEMKEYS_HTTP_PORT='"$HP"' MEMKEYS_MEMORY_HTTP_PORT='"$MP"' MEMKEYS_SEMANTIC=1
  # ISI-6401: bridge kubectl-exec stdin through a local pipe. When the server
  # inherits the kubelet stream as fd 0 it answers only the first stdin burst
  # (real MCP clients send initialize, await the reply, then tools/list — the
  # second request was never answered: 30s client timeout). `cat |` makes the
  # server read a local pipe, which handles delayed delivery correctly.
  cat | exec /usr/local/bin/memkeys-server 2>/dev/null
'
