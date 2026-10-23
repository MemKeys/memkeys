---
title: Connect your agents
description: Point MCP clients — Claude Code, kagent, goose, or any raw JSON-RPC client — at MemKeys over the stdio MCP transport, with per-run tokens and verified recipes.
slug: docs/connect-your-agents
editUrl: https://github.com/MemKeys/memkeys/edit/main/docs/connect-your-agents.md
sidebar:
  order: 4
---

# Connect your agents

MemKeys speaks the **Model Context Protocol (MCP)** over **stdio**: one
newline-delimited JSON-RPC 2.0 pipe, four tools, and a launch-bound principal.
Any MCP client — Claude Code, kagent, goose, or a twenty-line script — can use
it. This page gives one recipe per client, each marked with how it was
verified.

Before wiring an agent, skim [Getting started](./getting-started.md) so you
have a running server + Postgres. The authoritative design doc for this
transport (and the per-run token policy) is
[`docs/agent-mcp-transport.md`](https://github.com/MemKeys/memkeys/blob/main/docs/agent-mcp-transport.md);
agents migrating from the old MemPalace MCP should read
[`docs/mempalace-to-memkeys-migration.md`](https://github.com/MemKeys/memkeys/blob/main/docs/mempalace-to-memkeys-migration.md)
for the operation mapping.

> **Verification labels used on this page** (see the appendix for evidence):
> **verified** = the exact recipe was executed against a live MemKeys endpoint
> in the MemKeys test cluster; **control-plane verified** = the Kubernetes
> objects were applied and accepted by the live controller; **documented** =
> grounded in the transport contract but not yet executed end-to-end.

## 1. How the transport works

The MCP server is part of the `memkeys-server` binary (`mcp::run_stdio` in
`crates/memkeys-server/src/mcp.rs`). When the process starts with the memory
transport enabled (i.e. **not** `MEMKEYS_ROLE=worker`), it reads
newline-delimited JSON-RPC 2.0 from stdin and writes one JSON response per
line to stdout. Protocol version `2024-11-05`; the server identifies itself
as `memkeys`.

\`\`\`mermaid
sequenceDiagram
    autonumber
    participant A as Agent (MCP client)
    participant L as Launcher / wrapper
    participant S as memkeys-server (stdio)
    A->>L: spawn command
    L->>L: mint per-run JWT (HS256)
    L->>S: kubectl exec -i (or direct exec), env MEMKEYS_RUN_TOKEN
    A->>S: initialize (protocolVersion 2024-11-05)
    S-->>A: serverInfo memkeys, capabilities.tools
    A->>S: notifications/initialized
    A->>S: tools/list
    S-->>A: memory_store / memory_fetch / memory_traverse / memory_search
    A->>S: tools/call memory_store (envelope)
    S-->>A: fact_id + resolved entity keys
    A->>S: tools/call memory_fetch / memory_traverse / memory_search
    S-->>A: facts / graph / ranked hits
\`\`\`

### The four tools

| Tool | Purpose | Key arguments |
|---|---|---|
| `memory_store` | Store a fact envelope; returns fact id + resolved entity keys | `envelope` (see below) |
| `memory_fetch` | Fetch facts by entity keys | `entity.project`, `entity.issue`, `entity.pr`, `entity.agent`, `entity.run`, `entity.thread`, `entity.service`; `type`, `valid`, `as_of`, `limit` |
| `memory_traverse` | Bounded BFS over the memory graph | `start` (`kind` + opaque composite `id`), `edges`, `direction`, `depth`, `hydrate`, `deadline_ms` |
| `memory_search` | Semantic search over the fact-vector tier | `query`, `scope`, `k`, `tiers`, `as_of` |

Argument details live in the `tools/list` response — the schemas above are
taken verbatim from it. Things that repeatedly surprise first-time users:

- **`memory_fetch` keys are flat and dotted** (`"entity.project": "demo"`),
  not nested objects.
- **`memory_traverse` start ids are opaque composites**
  `tenant|entity.project|value` (dotted kind in the middle), **not** the raw
  value. Traversing from the raw id returns `not_found`. A fetch response's
  graph ids (and the store response's resolved keys) are the source of truth.
- **`memory_search` needs the semantic module on** (`MEMKEYS_SEMANTIC=1` with
  the same embedder as the primary server); otherwise it returns
  `feature_disabled`. Freshly stored facts take ~30s to project into the
  vector tier, so a search immediately after a store may miss.
- The **tenant floor is never a tool argument** — it is derived from the
  launch-bound principal (see §2).

A minimal `memory_store` envelope:

\`\`\`json
{
  "source": { "kind": "gateway_flush", "emitted_at": "2026-10-22T11:13:09Z" },
  "entity": { "project": "demo", "agent": "my-agent", "run": "run-123" },
  "fact":   { "body": "Deployed v1.2.3 to staging", "subject": "release" }
}
\`\`\`

## 2. Identity: the per-run token

The stdio pipe is the trust boundary. At launch the server binds **one
principal** for the process lifetime:

- **`MEMKEYS_RUN_TOKEN` + `MEMKEYS_RUN_TOKEN_SECRET`** → the token is verified
  once at launch and **re-checked on every tool call** (a token that expires
  mid-session fails cleanly, it does not linger).
- Claims: `company_id` (tenant floor, server-derived — callers cannot assert a
  tenant), `sub` (agent id — stamped as `entity.agent` on every write),
  optional `run_id`, and `exp`. HS256.
- **No token / no secret** (dev loopback only): the server binds an
  UNVERIFIED principal for tenant `MEMKEYS_STDIO_TENANT` (default
  `default`, agent id `stdio-dev`) and logs a warning. Fine for a laptop;
  never fine off loopback.

Mint a session token inline (no extra tooling needed):

\`\`\`sh
b64() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
now=$(date +%s); exp=$((now + 3600))
hdr=$(printf '{"alg":"HS256","typ":"JWT"}' | b64)
pl=$(printf '{"company_id":"%s","sub":"%s","iat":%s,"exp":%s}' "$TENANT" "$AGENT" "$now" "$exp" | b64)
sig=$(printf '%s.%s' "$hdr" "$pl" | openssl dgst -sha256 -hmac "$MEMKEYS_RUN_TOKEN_SECRET" -binary | b64)
TOKEN="$hdr.$pl.$sig"
\`\`\`

## 3. Recipe: Claude Code — verified

Claude Code talks stdio MCP natively. The proven pattern runs the server
**in-pod** through `kubectl exec -i`, minting the token at session launch so
no long-lived token is written to disk.

**1. Wrapper script** on the host Claude runs from (this is the in-pod
launcher pattern from
[`docs/agent-mcp-transport.md` §3](https://github.com/MemKeys/memkeys/blob/main/docs/agent-mcp-transport.md);
pass identity through the environment):

\`\`\`sh
#!/bin/sh
export PATH="$HOME/.local/bin:$PATH"          # kubectl lives here on some hosts
export KUBECONFIG=/path/to/cluster.kubeconfig
export PAPERCLIP_COMPANY_ID=my-tenant          # -> company_id claim (tenant floor)
export PAPERCLIP_AGENT_ID=my-agent-id          # -> sub claim (entity.agent)
exec bash /path/to/memkeys-mcp-launcher.sh
\`\`\`

The launcher mints the JWT in-pod (secret never leaves the cluster), picks
random alt-ports for the unused HTTP listeners, and `exec`s the server binary
so stdin/stdout go straight to the MCP loop. If your launcher sits on a
**noexec mount** (e.g. a NAS), invoke it via `bash` as above — direct exec
fails with *Permission denied*, which Claude surfaces as
`CONNECTION_CLOSED`.

**2. Register it:**

\`\`\`sh
claude mcp add-json --scope user memkeys '{"type":"stdio","command":"/path/to/memkeys-wrapper.sh","env":{}}'
\`\`\`

or in `~/.claude.json`:

\`\`\`json
{
  "mcpServers": {
    "memkeys": {
      "type": "stdio",
      "command": "/path/to/memkeys-wrapper.sh",
      "env": {}
    }
  }
}
\`\`\`

**3. Verify the handshake:**

\`\`\`sh
claude mcp list
# memkeys: /path/to/memkeys-wrapper.sh - ✔ Connected
\`\`\`

Tools are then available as `mcp__memkeys__memory_store` etc. Two caveats
verified the hard way: the all-in-one binary prints a few startup log lines to
stdout before the JSON-RPC stream (Claude tolerates them; the planned
`MEMKEYS_ROLE=mcp` launch mode removes them — see the transport doc §1), and
concurrent sessions each spawn their own in-pod server, which is why the
launcher randomizes ports.

## 4. Recipe: kagent — verified

kagent exposes MCP servers as the `MCPServer` CRD (`kagent.dev/v1alpha1`,
`transportType: stdio`); kagent runs the server container itself and agents
reference it by name. Verified shape:

\`\`\`yaml
apiVersion: kagent.dev/v1alpha1
kind: MCPServer
metadata:
  name: memkeys-mcp
  namespace: kagent
spec:
  transportType: stdio
  stdioTransport: {}
  timeout: 60s
  deployment:
    image: <your-registry/memkeys:tag>
    cmd: /usr/local/bin/memkeys-server
    port: 3000
    replicas: 1
    env:
      MEMKEYS_DATABASE_URL: "postgres://… (point at the same Postgres as the primary)"
      MEMKEYS_STDIO_TENANT: "kagent-demo"   # dev principal; prefer a minted token in prod
      MEMKEYS_SEMANTIC: "1"                 # keep memory_search on
      MEMKEYS_HTTP_HOST: 127.0.0.1
      MEMKEYS_PG_MAX_CONNECTIONS: "2"       # one small pool per MCP session
    resources:
      requests: {cpu: 50m, memory: 64Mi}
      limits:   {cpu: 300m, memory: 256Mi}
\`\`\`

Wire it into an Agent (`kagent.dev/v1alpha2`; strict decoder — no
`description`/`type` fields inside `declarative`):

\`\`\`yaml
apiVersion: kagent.dev/v1alpha2
kind: Agent
metadata:
  name: memkeys-demo
  namespace: kagent
spec:
  declarative:
    runtime: python
    modelConfig: mac-studio-ollama
    systemMessage: |
      You are an agent whose memory lives in MemKeys via MCP tools
      (memory_store, memory_fetch, memory_traverse, memory_search).
    tools:
      - type: McpServer
        mcpServer:
          apiGroup: kagent.dev
          kind: MCPServer
          name: memkeys-mcp
          toolNames: [memory_store, memory_fetch, memory_traverse, memory_search]
\`\`\`

Check acceptance: `kubectl -n kagent get mcpservers` →
`READY True`, `kubectl -n kagent get agents` → `READY/ACCEPTED True`.
Tool negotiation happens when a session starts. Mind node pressure: MCP server
pods get evicted like any other pod and kagent reschedules them.

Session-level verification (2026-10-23): a raw JSON-RPC client piped through
the **kagent-hosted `memkeys-mcp` pod** (same image, env, and Postgres the
MCPServer spec above provides) completed `initialize` → `tools/list` →
`memory_store` (fact id returned) → `memory_fetch` → `memory_traverse`
(hydrated `entity_fact` edge) — the full handshake, not just control-plane
acceptance.

## 5. Recipe: goose — documented

goose (Block's open-source agent, now under the AAIF) supports stdio MCP
servers as **extensions** in `~/.config/goose/config.yaml` (goose 1.x reads
**YAML, not the old config.toml** — a `.toml` file is silently ignored):

\`\`\`yaml
extensions:
  memkeys:
    type: stdio
    command: /path/to/memkeys-wrapper.sh
    env: {}
    enabled: true
\`\`\`

goose needs a configured provider before it will start a session
(`GOOSE_PROVIDER`/`GOOSE_MODEL`, e.g. a local Ollama). goose 1.x loads
extensions lazily — the tools appear when the model reaches for them, not at
startup.

> **Status:** registered-config handshake against goose 1.55 is **not yet
> verified end-to-end** (registration appears to require the interactive
> `goose configure` flow in 1.55; the declarative YAML above did not attach
> the tools in our test). The wrapper itself is transport-verified via the
> generic recipe. Treat this recipe as the starting point and re-verify with
> your goose version; see the appendix.

## 6. Recipe: any client — raw JSON-RPC (verified)

No SDK required — the whole contract is printf-over-stdio. This is the exact
driver used for verification (run it in-pod, or point it at any launcher):

\`\`\`sh
#!/bin/sh
# mint $TOKEN as in §2, export MEMKEYS_RUN_TOKEN="$TOKEN"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"raw-client","version":"1"}}}'
LIST='{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
STORE='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_store","arguments":{"envelope":{"source":{"kind":"gateway_flush","emitted_at":"'"$NOW"'"},"entity":{"project":"demo","agent":"raw-client","run":"raw-client-1"},"fact":{"body":"hello from the raw client","subject":"smoke"}}}}}'
FETCH='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_fetch","arguments":{"entity.project":"demo"}}}'
TRAVERSE='{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_traverse","arguments":{"start":{"kind":"entity_project","id":"demo|entity.project|demo"},"edges":["entity_fact"],"direction":"out","depth":2,"hydrate":true}}}'
SEARCH='{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"hello from the raw client","k":5,"scope":{"entity.project":"demo"}}}}'
export MEMKEYS_HTTP_HOST=127.0.0.1 MEMKEYS_HTTP_PORT=18080 MEMKEYS_MEMORY_HTTP_PORT=13000
{ printf '%s\n' "$INIT" "$LIST" "$STORE" "$FETCH" "$TRAVERSE"; sleep 35; printf '%s\n' "$SEARCH"; } \
  | memkeys-server 2>/dev/null | grep '"jsonrpc"'
\`\`\`

(The 35s pause lets the maintenance tick project the fresh fact into the
vector tier before `memory_search`.) With an SDK, spawn the same wrapper as
the child process — e.g. the official TypeScript
`@modelcontextprotocol/sdk` `Client` + `StdioClientTransport`, or Python
`mcp.client.stdio` — and call the four tools by name.

## 7. Troubleshooting

- **`memory_traverse` → `not_found`**: you passed a raw entity value as
  `start.id`. Use the opaque composite `tenant|entity.project|value`.
- **`memory_search` → `feature_disabled`**: `MEMKEYS_SEMANTIC` is off on the
  process serving your session, or its embedder differs from the primary's
  (vectors land in a different ANN space). Set `MEMKEYS_SEMANTIC=1` and the
  same `MEMKEYS_EMBED_URL`/model.
- **Search misses a fact you just stored**: the vector tier projects on the
  ~30s maintenance tick. Wait, or verify the write with `memory_fetch` first.
- **Claude `CONNECTION_CLOSED` on connect**: usually the wrapper died
  instantly — check the launcher is executable (noexec mounts: run it via
  `bash`) and `kubectl` is on the wrapper's `PATH`.
- **`unauthenticated` mid-session**: the run token expired (re-checked every
  call). Re-mint with a TTL sized to the session, or restart the session.
- **Port collisions between concurrent in-pod sessions**: each session spawns
  its own all-in-one server; keep the randomized-alt-ports pattern until
  `MEMKEYS_ROLE=mcp` (bind-less MCP-only mode) ships.

## Appendix: how each recipe was verified (2026-10-22/23, ISI-6401)

| Recipe | Result | Evidence |
|---|---|---|
| Raw JSON-RPC driver | **verified** — initialize, tools/list, store (`fact_id` returned), fetch, traverse (hydrated `entity_fact` edge), search (score 0.60) | in-pod run on the `k8squad-test` cluster, tenant `isi6401-verify`, live Postgres |
| Claude Code 2.1.239 | **verified** — `claude mcp list` health check `✔ Connected` through the launcher | registered via `claude mcp add-json` on the agent host, same cluster |
| kagent | **verified** — control-plane: `MCPServer/memkeys-mcp` Ready, `Agent/memkeys-demo` READY/ACCEPTED, pod 1/1; session-level (2026-10-23): full stdio handshake through the kagent-hosted pod — initialize, tools/list, store (`fact_id f70e0f87…`), fetch, traverse (`entity_fact` edge) | applied live on the `memory-benchmark` cluster with the real memkeys image |
| goose 1.55.0 | **documented** — config-registered but tools did not attach in a non-interactive session; provider (Ollama) reachable, model responded | attempt log in the ISI-6401 run; re-verify with `goose configure` |

Raw transcripts are in the ISI-6401 run log; the driver and manifests are
reproducible from this page verbatim.
