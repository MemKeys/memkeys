# ISI-6328 — Ph2 cutover: k8squad-test agents MemPalace → MemKeys

Owner: ProxOps (cluster/agent wiring) + Architect (MemKeys config). Parent: ISI-6307.
Status as of 2026-10-20: **transport validated; agent flip blocked on cross-owner decisions (below).**

## What "k8squad-test agents" actually are

Scanned the k8squad-test cluster: the in-cluster agent workloads (`goose`,
`goosetown`, `bmad-crew`, `bmad-squad`, `sympozium-squad`, `ksquad-team-*`) wire
**no memory MCP** — their only MCP configs are GitHub/Dynatrace. **No in-cluster
object references MemPalace.**

The agents that use MemPalace are the Paperclip **control-plane `claude_local`
agents** (company `615e9564…`), wired via a **host-global** `~/.claude.json`
`mcpServers.mempalace` entry (`ssh mempalace-mcp python3 -m mempalace.mcp_server`).
Per-agent `adapterConfig` is empty `{}` — the claude adapter does **not** inject
per-agent MCP, so every claude_local agent on the host inherits the same global
mempalace server. ISI-6331 already swept these agents' `AGENTS.md` to the MemKeys
4-op instructions — but the `memkeys` MCP server was never wired, so those
instructions currently reference tools that do not yet exist in-session.

## Transport: MemKeys MCP is stdio coupled to the full server

`crates/memkeys-server/src/main.rs` spawns `mcp::run_stdio` alongside the HTTP
listeners — the MCP server IS the full `memkeys-server` binary reading JSON-RPC
on stdin. There is **no thin remote MCP connector**. The MemPalace analogue
(`ssh host python3 -m mempalace.mcp_server`) is a lightweight remote stdio
process; MemKeys has no equivalent. A client therefore drives the in-pod binary
over a `kubectl exec -i` pipe (see `memkeys-mcp-launcher.sh`).

Principal is launch-bound from `MEMKEYS_RUN_TOKEN` (HS256 JWT). The launcher
passes the Paperclip-injected identity (`PAPERCLIP_COMPANY_ID/AGENT_ID/RUN_ID`)
through to an **in-pod** mint, so the token secret never leaves the cluster
while still carrying the real per-agent principal.

## Canary verification — PASSED (2026-10-20, from paperclip host → k8squad-test)

Tenant `canary-6328`, two independent MCP stdio sessions:

| op | session | result |
|----|---------|--------|
| `initialize` / `tools/list` | A | serverInfo `memkeys 0.1.0`; 4 tools: store/fetch/traverse/search |
| `memory_store` | A | `fact_id 979e748f-…`, `keyed:true`, principal-stamped `entity.agent` |
| `memory_fetch` | **B (fresh session)** | 1 fact, subject `isi6328-canary-write` → **recall across sessions** |
| `memory_traverse` | B | 2 nodes / 1 edge (`entity_project`→`entity_fact`) → **kg/graph ops** |
| `memory_search` | B | 1 hit (semantic, after the 30s projection tick) → **vector tier** |

Write, cross-session recall, graph traverse, and semantic search all round-trip
over the remote MCP transport. The dogfood 4-op surface is functional.

## Blocking decisions for the all-agents flip (cross-owner)

1. **MemKeys MCP transport weight → Architect.** stdio-coupled-to-full-server
   means every agent session spawns a full `memkeys-server` (HTTP listeners +
   M6 workers + reconciler + DB pool) via `kubectl exec`. N concurrent agents =
   N servers each running maintenance/reconcile loops against the one Postgres.
   Tolerable for a canary; wasteful at "all agents." Recommend a thin
   **MCP-stdio → HTTP `/memory/*`** connector (talks to the in-cluster service /
   VIP 10.0.0.220) so sessions are cheap and stateless. Tracked as a child issue.

2. **Per-agent / per-company MCP scoping + shared-host authorization → BigBoss.**
   `~/.claude.json` is host-global across ALL companies on 10.0.0.189, and
   Paperclip has no per-agent MCP override (`adapterConfig` empty). Options:
   (a) additive host-global `memkeys` entry gated by `PAPERCLIP_COMPANY_ID` in
   the launcher (implemented here) — keeps MemPalace for rollback, only this
   company's agents spawn a server; (b) Paperclip platform adds per-agent/company
   MCP config. Editing the shared-host file touches other companies' agents, so
   this needs board authorization before I apply it.

## Rollback

MemPalace stays wired throughout — the cutover is **additive** (`memkeys`
alongside `mempalace`). Rollback = remove the `memkeys` entry from the MCP
config; no data move (clean start, board-confirmed). Keep MemPalace reachable
for the full soak.
