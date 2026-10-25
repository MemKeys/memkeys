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
#
# ISI-6401 (2026-10-22): stdin bridging fix. When the server inherits the
# kubelet exec stream as fd 0 it answers only the FIRST stdin burst — real MCP
# clients send initialize, await the reply, then tools/list, so the second
# request hung 30s and timed out (reproduced with 0.5s/2s/3s burst gaps; kubectl
# itself streams delayed stdin fine — `cat` bridging fixed it). Keep the
# `cat |` bridge below in any custom wrapper. See docs/agent-mcp-transport.md.
#
# DELIBERATE REPO HOME (ISI-6552): this is operational dogfood tooling and lives
# here alongside values.yaml / postgres.yaml. It was twice stripped as a
# "leaked" file by public-facing scrubbing passes (ISI-6395 commit 9baf679,
# ISI-6514 af8199a), which broke ALL agents' memkeys MCP because the host
# wrapper execs this exact path. Do NOT remove it in a docs/website scrub: it
# carries no secret (the run-token SECRET is read in-pod via env; the company
# UUID is the public canary tenant, already gitleaks-allowlisted).
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
# in-pod server's HTTP + memory listeners. Harmless no-ops once the pod image
# supports MEMKEYS_ROLE=mcp (no binds in MCP-only mode), still protective on
# older images where the role env is inert and the all-in-one server binds.
HP=$(( 20000 + (RANDOM % 20000) ))
MP=$(( HP + 1 ))

exec kubectl -n "$NS" exec -i "$DEPLOY" -- sh -c '
  b64url() { openssl base64 -A | tr "+/" "-_" | tr -d "="; }
  EXP=$(( $(date +%s) + 3600 ))
  H=$(printf "%s" "{\"alg\":\"HS256\",\"typ\":\"JWT\"}" | b64url)
  P=$(printf "%s" "{\"company_id\":\"'"$COMPANY"'\",\"sub\":\"'"$AGENT"'\",\"run_id\":\"'"$RUN"'\",\"exp\":$EXP}" | b64url)
  SIG=$(printf "%s" "$H.$P" | openssl dgst -sha256 -hmac "$MEMKEYS_RUN_TOKEN_SECRET" -binary | b64url)
  export MEMKEYS_RUN_TOKEN="$H.$P.$SIG"
  # ISI-6552: MCP-only role + pool cap 2 — each agent session must not exec a
  # full all-in-one server (boot reconcile + maintenance loop + worker pool)
  # into the pod; that saturated Postgres (see ISI-6549 RCA). Requires a pod
  # image with the ISI-6337 role support; on older images the env is inert and
  # the alt ports above keep listeners from colliding.
  export MEMKEYS_ROLE=mcp MEMKEYS_PG_MAX_CONNECTIONS=2
  export MEMKEYS_HTTP_HOST=127.0.0.1 MEMKEYS_HTTP_PORT='"$HP"' MEMKEYS_MEMORY_HTTP_PORT='"$MP"' MEMKEYS_SEMANTIC=1
  # ISI-6401: bridge kubectl-exec stdin through a local pipe. When the server
  # inherits the kubelet stream as fd 0 it answers only the first stdin burst
  # (real MCP clients send initialize, await the reply, then tools/list — the
  # second request was never answered: 30s client timeout). `cat |` makes the
  # server read a local pipe, which handles delayed delivery correctly.
  cat | exec /usr/local/bin/memkeys-server 2>/dev/null
'
