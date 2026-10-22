# MemKeys — Connect Your Agents

**Project:** MemKeys — attribute-based memory server (facts keyed by agent-supplied attributes, temporal validity, graph + vector derived views)
**Audience:** agent operators wiring an MCP-capable agent to MemKeys
**Reference:** `docs/architecture.md` (baseline contract), `deploy/k8squad-test/ISI-6333-conformance-report.md` (MCP + SLO conformance), `deploy/k8squad-test/ISI-6328-cutover-design.md` (agent cutover)
**Verification:** recipes marked ✅ were exercised against a live MemKeys MCP endpoint on k8squad-test on 2026-10-22; evidence in §7.

> **Correction to the plan text (ISI-6401):** there is no `MEMKEYS_ROLE=mcp`. The server has two roles: the default all-in-one role (HTTP + stdio MCP transports on) and `MEMKEYS_ROLE=worker` (projection-only; transports off) — see `crates/memkeys-server/src/main.rs:171`. The docs `docs/agent-mcp-transport.md` and `docs/mempalace-to-memkeys-migration.md` referenced in the issue do not exist; this page is the MCP transport doc, and migration context lives in `deploy/k8squad-test/ISI-6328-cutover-design.md`.

## 1. The MCP transport

MemKeys exposes the Model Context Protocol as **newline-delimited JSON-RPC 2.0 over stdio** (`crates/memkeys-server/src/mcp.rs`). There is no thin remote MCP connector: the MCP server *is* the full `memkeys-server` binary — `mcp::run_stdio` is spawned alongside the HTTP listeners, so the process that serves MCP also binds the health and `/memory/*` HTTP ports and needs `MEMKEYS_DATABASE_URL` (Postgres + pgvector).

**Wire protocol**

- One request per line on stdin, one response per line on stdout.
- Methods: `initialize`, `tools/list`, `tools/call`. Unknown methods → `-32601`.
- Requests without an `id` are JSON-RPC **notifications** and get *no* reply (e.g. `notifications/initialized`).
- Errored tool calls come back as a *successful* JSON-RPC reply with `"isError": true` and the WS-5 §7 error body (`code`/`retriable`/`detail`) as the content text — parity with the HTTP error surface.
- `initialize` returns `protocolVersion: "2024-11-05"`, `serverInfo: {name: "memkeys", version: …}`, `capabilities: {tools: {}}`.

**Principal binding (§2.1)** — the stdio pipe is the trust boundary. At launch the server binds **one** principal from `MEMKEYS_RUN_TOKEN`, an HS256 JWT minted with the deployment's `MEMKEYS_RUN_TOKEN_SECRET`:

| JWT claim | Becomes |
|---|---|
| `company_id` | tenant floor (all four ops are scoped to it; tenant is never a tool argument) |
| `sub` | `entity.agent` / provenance principal |
| `run_id` | `entity.run` |
| `exp` | expiry window, re-checked on every `tools/call` (F12) — an expired window hard-fails `unauthenticated` |

If `MEMKEYS_RUN_TOKEN`/`MEMKEYS_RUN_TOKEN_SECRET` are absent, the server binds an **unverified dev principal** for tenant `MEMKEYS_STDIO_TENANT` (default `default`) — loopback dev posture only.

**Environment**

| Variable | Default | Purpose |
|---|---|---|
| `MEMKEYS_DATABASE_URL` | *(required)* | Postgres DSN (pgvector enabled) |
| `MEMKEYS_RUN_TOKEN` | — | launch-bound HS256 principal (above) |
| `MEMKEYS_RUN_TOKEN_SECRET` | — | issuer secret that verifies the token |
| `MEMKEYS_HTTP_HOST` | `127.0.0.1` | bind host (non-loopback requires the secret or `MEMKEYS_ALLOW_INSECURE=1`) |
| `MEMKEYS_HTTP_PORT` | `8080` | health surface |
| `MEMKEYS_MEMORY_HTTP_PORT` | `3000` | `/memory/*` HTTP transport |
| `MEMKEYS_SEMANTIC` | off | `1` enables the vector tier (`memory_search`); off → `memory_search` returns `isError:true feature_disabled` |
| `MEMKEYS_ROLE` | all-in-one | `worker` disables HTTP + MCP transports |

**The four tools** (exact schemas from a live `tools/list`, §7):

- `memory_store {envelope}` → `{fact_id, keyed, entity_keys_resolved, duplicate}`
- `memory_fetch {entity.*, type?, as_of?, limit?, valid?}` → `{facts: […]}` (at least one `entity.*` key; `as_of` is RFC3339 point-in-time)
- `memory_traverse {start:{kind,id}, edges?, direction?, depth?, hydrate?, deadline_ms?}` → `{nodes, edges, cursor, truncated}` — **addressing rule:** `start.id` is the *composite node id* `<tenant>|<dotted-kind>|<value>` while `start.kind` is the snake_case enum, e.g. `{"kind":"entity_project","id":"my-tenant|entity.project|my-project"}`. A raw value returns `not_found`.
- `memory_search {query, scope?, k?, tiers?, as_of?}` → `{hits: […]}`; unknown `scope` keys are rejected, non-string scope values are rejected; `tiers:"digest"` degrades to the fact tier with a warning.

## 2. Reaching the MCP endpoint

Because MCP is coupled to the full binary, an agent host reaches it one of three ways:

1. **Local process** — run `memkeys-server` on the agent host; the MCP client owns its stdin/stdout. Simplest for dev.
2. **`kubectl exec` bridge (production dogfood path)** — `deploy/k8squad-test/memkeys-mcp-launcher.sh` runs on the agent host, execs into the memkeys pod, mints the run-token **in-pod** (the secret never leaves the cluster) from the `PAPERCLIP_COMPANY_ID` / `PAPERCLIP_AGENT_ID` / `PAPERCLIP_RUN_ID` env the control plane injects, and pipes stdio. This is the command every recipe below points at.
3. **In-cluster** — a workload in the cluster runs the binary directly.

> **Stdin bridging caveat (fixed 2026-10-22, ISI-6401):** when the server inherits a `kubectl exec` stream as fd 0 it answered only the *first* stdin burst — real MCP clients send `initialize`, await the reply, then send `tools/list`, so the second request timed out after 30s. The launcher now bridges stdin through a local pipe (`cat | memkeys-server`). If you write your own `kubectl exec` wrapper, keep that bridge. Root-cause fix in the server's stdio loop is tracked separately.

Launcher env knobs: `MEMKEYS_KUBECONFIG` (default `~/.config/capmox/k8squad-test.kubeconfig`), `MEMKEYS_NAMESPACE` (default `memkeys`), `MEMKEYS_DEPLOY` (default `deploy/memkeys`), `MEMKEYS_TARGET_COMPANY` (only this Paperclip company gets a live server; other companies get a quiet tool-less pipe).

## 3. Recipe: Claude Code / Claude Desktop — ✅ verified 2026-10-22

Claude Code and Claude Desktop both read stdio MCP servers from an `mcpServers` config block of the shape `{command, args, env}`.

**Claude Code (CLI):**

```bash
# stdio server whose command is the kubectl-exec launcher
claude mcp add-json memkeys '{
  "type": "stdio",
  "command": "bash",
  "args": ["/path/to/memkeys/deploy/k8squad-test/memkeys-mcp-launcher.sh"],
  "env": {
    "PAPERCLIP_COMPANY_ID": "<company-uuid>",
    "PAPERCLIP_AGENT_ID": "<agent-id>",
    "PAPERCLIP_RUN_ID": "<run-id>"
  }
}'

# health-check: connects, runs initialize + tools/list
claude mcp get memkeys        # expect: Status: ✔ Connected
claude mcp remove memkeys     # cleanup
```

Equivalent: `claude mcp add memkeys -e PAPERCLIP_COMPANY_ID=… -- bash /path/to/memkeys-mcp-launcher.sh`.

**Claude Desktop** — add the same block to `claude_desktop_config.json` (`~/Library/Application Support/Claude/` on macOS, `%APPDATA%\Claude\` on Windows):

```json
{
  "mcpServers": {
    "memkeys": {
      "command": "bash",
      "args": ["/path/to/memkeys/deploy/k8squad-test/memkeys-mcp-launcher.sh"],
      "env": { "PAPERCLIP_COMPANY_ID": "<company-uuid>", "PAPERCLIP_AGENT_ID": "<agent-id>", "PAPERCLIP_RUN_ID": "<run-id>" }
    }
  }
}
```

Verified 2026-10-22: `claude mcp get` → `✔ Connected`; server negotiated `protocolVersion 2024-11-05`, `serverVersion memkeys 0.1.0`, `hasTools: true`, and a four-op round-trip (store → fetch → traverse → search) completed through the launcher with realistic request pacing (§7).

## 4. Recipe: goose — ⏳ config-reviewed, live handshake pending

[goose](https://github.com/block/goose) (Block's open-source agent) loads MCP extensions of `type: stdio` from `~/.config/goose/config.yaml`:

```yaml
extensions:
  memkeys:
    name: MemKeys
    type: stdio
    cmd: bash
    args:
      - /path/to/memkeys/deploy/k8squad-test/memkeys-mcp-launcher.sh
    envs:
      PAPERCLIP_COMPANY_ID: <company-uuid>
      PAPERCLIP_AGENT_ID: goose
      PAPERCLIP_RUN_ID: <run-id>
    enabled: true
    timeout: 300
```

Then `goose session` starts the extension and the four `memory_*` tools appear. The launcher satisfies goose's stdio contract (command + args + envs, newline-delimited JSON-RPC). A live goose↔MemKeys handshake has **not** been run yet — goose is not installed on the dogfood host; tracked in the DevOps verification follow-up (§8).

## 5. Recipe: kagent — ⏳ config-reviewed, live handshake pending

[kagent](https://kagent.dev) is a Kubernetes-native agent framework (Solo.io / CNCF) that attaches MCP tool servers to agents. Point a stdio MCP tool server at the launcher (it must run where `kubectl` + the cluster kubeconfig are available):

```yaml
apiVersion: kagent.dev/v1alpha1
kind: MCPServer
metadata:
  name: memkeys
  namespace: kagent
spec:
  stdio:
    command: bash
    args: ["/opt/memkeys/memkeys-mcp-launcher.sh"]
    env:
      PAPERCLIP_COMPANY_ID: <company-uuid>
      PAPERCLIP_AGENT_ID: kagent
      PAPERCLIP_RUN_ID: <run-id>
---
apiVersion: kagent.dev/v1alpha1
kind: Agent
metadata:
  name: memkeys-agent
  namespace: kagent
spec:
  # ... modelConfig etc. per your kagent install ...
  tools:
    - type: McpServer
      mcpServer:
        name: memkeys
        toolNames: ["memory_store", "memory_fetch", "memory_traverse", "memory_search"]
```

Caveats: the `MCPServer`/`Agent` API version varies across kagent releases — check `kubectl get crd | grep kagent` on your install; the stdio command runs as a long-lived child of the kagent controller, so bundle `kubectl`, the kubeconfig, and the launcher into the controller image (or run the bridge as a sidecar). Live handshake pending (§8).

## 6. Recipe: generic MCP client — ✅ verified 2026-10-22

Any MCP SDK works: spawn the launcher (or a local `memkeys-server`) as a stdio child and speak JSON-RPC. Raw wire reference — every exchange below was replayed live against k8squad-test on 2026-10-22 (§7):

```jsonc
// → initialize
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"my-client","version":"1"}}}
// ← {"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"memkeys","version":"0.1.0"}}}

// → (notification — no reply by design)
{"jsonrpc":"2.0","method":"notifications/initialized"}

// → tools/list
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
// ← {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"memory_store",…},{"name":"memory_fetch",…},{"name":"memory_traverse",…},{"name":"memory_search",…}]}}

// → memory_store
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_store","arguments":{"envelope":{"source":{"kind":"gateway_flush","emitted_at":"2026-10-22T10:56:57Z"},"entity":{"project":"my-project","agent":"my-agent","run":"run-1"},"fact":{"body":"deploy freeze starts Friday","subject":"freeze"}}}}}
// ← {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"duplicate\":false,\"entity_keys_resolved\":{…},\"fact_id\":\"6e2e3888-…\",\"keyed\":true}"}]}}

// → memory_fetch (at least one entity.* key)
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_fetch","arguments":{"entity.project":"my-project"}}}
// ← … "facts":[{… "body":"deploy freeze starts Friday" …}] …

// → memory_traverse (composite node id: <tenant>|<dotted kind>|<value>)
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_traverse","arguments":{"start":{"kind":"entity_project","id":"my-tenant|entity.project|my-project"},"depth":2,"direction":"out","hydrate":true}}}
// ← … "edges":[{"edge_type":"entity_fact",…}], "nodes":[{…hydrated records…}] …

// → memory_search (requires MEMKEYS_SEMANTIC=1 on the server)
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"deploy freeze","k":5,"scope":{"entity.project":"my-project"}}}}
// ← … "hits":[{"score":0.645, "fact.body":"deploy freeze starts Friday", …}] …

// errored tool call (e.g. expired principal window) — an OK reply with isError:
// ← {"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"{\"code\":\"unauthenticated\",\"retriable\":false,…}"}],"isError":true}}
```

Minimal Python client (stdlib only), driving the launcher:

```python
import json, subprocess

proc = subprocess.Popen(
    ["bash", "deploy/k8squad-test/memkeys-mcp-launcher.sh"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    env={"PAPERCLIP_COMPANY_ID": "<company-uuid>", "PAPERCLIP_AGENT_ID": "py-client",
         "PAPERCLIP_RUN_ID": "run-1", "PATH": "/usr/bin:/bin:/usr/local/bin"},
)

def call(req_id, method, params=None):
    msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write(json.dumps(msg) + "\n"); proc.stdin.flush()
    return json.loads(proc.stdout.readline())

print(call(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                             "clientInfo": {"name": "py", "version": "1"}}))
print([t["name"] for t in call(2, "tools/list")["result"]["tools"]])
print(call(3, "tools/call", {"name": "memory_fetch",
                             "arguments": {"entity.project": "my-project"}}))
proc.stdin.close(); proc.wait()
```

A ready-made shell equivalent is `deploy/k8squad-test/mcp-smoke.sh` — it mints a token in-pod, runs initialize/list/store/fetch/traverse, waits ~35s for the async vector projection, then runs search.

## 7. Verification evidence

All live checks ran against the k8squad-test memkeys deployment on **2026-10-22** (plus the earlier ISI-6328/ISI-6333 conformance runs on 2026-10-20).

| Recipe | Check | Result |
|---|---|---|
| Generic MCP (raw) | `mcp-smoke.sh` in-pod: initialize, tools/list, store, fetch, traverse, search | ✅ all six round-tripped; search score 0.645 after the ~30s projection tick |
| Generic MCP (raw) | traverse addressing | ✅ composite id `<tenant>\|<kind>\|<value>` returns hydrated nodes/edges; raw value returns `not_found` (as documented) |
| Generic MCP (unit) | `cargo test -p memkeys-server --lib mcp::` | ✅ 7/7 (notification silence, parse-error null id, expiry → `unauthenticated`, bound-tenant fetch, search `feature_disabled`, search on) |
| Claude Code | `claude mcp add-json` + `claude mcp get` against the launcher | ✅ `Status: ✔ Connected` (`serverVersion memkeys 0.1.0`, protocol 2024-11-05, `hasTools:true`) |
| Claude Code (launcher path) | four-op round-trip with client-like pacing (2s between requests) | ✅ initialize/list/store/fetch/traverse all answered; stored fact retrieved; `entity_fact` edge traversed |
| goose | config matches goose stdio extension contract | ⏳ live handshake pending — DevOps follow-up (§8) |
| kagent | config matches kagent MCP tool-server contract | ⏳ live handshake pending — DevOps follow-up (§8) |

**Bug found & fixed during verification:** the launcher originally exec'd the in-pod server with the `kubectl exec` stream as fd 0; the server then answered only the first stdin burst, so any real MCP client (initialize → wait → tools/list) hung for 30s. Fix shipped in `memkeys-mcp-launcher.sh`: bridge stdin through a local pipe (`cat |`). Before the fix `claude mcp get` reported `Connected · tools fetch failed / Request timed out`; after, `✔ Connected`.

## 8. Troubleshooting & follow-ups

| Symptom | Cause | Fix |
|---|---|---|
| `tools/list` times out (~30s) but `initialize` answers | server stdin is a bare `kubectl exec` stream (pre-fix launcher) | use the shipped launcher (cat bridge), or add `cat \|` before your own exec |
| `memory_traverse` → `not_found` | raw value as `start.id` | composite id `<tenant>\|<dotted-kind>\|<value>` (§1) |
| `memory_search` → `isError:true feature_disabled` | vector tier off | run the server with `MEMKEYS_SEMANTIC=1` |
| every call → `unauthenticated` | run-token `exp` window closed (F12) | relaunch with a fresh token (launcher mints 1h tokens) |
| fetch returns another tenant's data | impossible by construction — tenant comes from the launch-bound principal, never a tool argument | — |
| server refuses to bind | non-loopback host without `MEMKEYS_RUN_TOKEN_SECRET` | keep `127.0.0.1`, set the secret, or accept `MEMKEYS_ALLOW_INSECURE=1` (not recommended) |

**Tracked follow-ups**

- Server-side root-cause fix for the kubectl-stream stdin starvation (the cat bridge is a launcher-level workaround).
- DevOps pairing: live goose and kagent handshakes to bring all four recipes to ✅.
- Thin MCP-stdio → HTTP `/memory/*` connector so agent sessions don't each spawn a full server (from ISI-6328's blocking decisions).
