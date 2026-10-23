# MCP transport

MemKeys speaks the [Model Context Protocol](https://modelcontextprotocol.io)
as **newline-delimited JSON-RPC 2.0 over stdio**. Any MCP-capable client —
Claude Code, Claude Desktop, goose, kagent, or your own — can use MemKeys as
its memory server with zero glue code.

There is no thin remote connector: the MCP server *is* the `memkeys-server`
binary. Depending on its role, the same process either serves MCP alongside
the HTTP listeners (`MEMKEYS_ROLE` unset — the all-in-one default) or serves
MCP **only** (`MEMKEYS_ROLE=mcp`) — no HTTP binds, no boot reconcile, no
maintenance loop. The MCP-only role exists so per-agent sessions can share
one Postgres-backed deployment without colliding on ports or double-running
the projection workers.

## Wire protocol

- One JSON-RPC request per line on stdin, one response per line on stdout.
- Methods: `initialize`, `tools/list`, `tools/call`. Unknown methods return
  `-32601`.
- Requests without an `id` are JSON-RPC **notifications** and get no reply
  (e.g. `notifications/initialized`).
- `initialize` returns `protocolVersion: "2024-11-05"`, `serverInfo:
  {name: "memkeys", version: …}`, `capabilities: {tools: {}}`.
- Errored tool calls come back as a *successful* JSON-RPC reply with
  `"isError": true` and the standard error body (`code` / `retriable` /
  `detail`) as the content text — parity with the HTTP error surface, so
  clients need one error path for both transports.
- Stdout carries only JSON-RPC. Server logs and traces never target stdout.

## The four tools

The four operations of the [retrieval contract](/memkeys/retrieval-contract/),
as tools:

| Tool | Arguments (essentials) | Returns |
|---|---|---|
| `memory_store` | `{envelope}` — the fact envelope | `{fact_id, keyed, entity_keys_resolved, duplicate}` |
| `memory_fetch` | `{entity.*, type?, as_of?, limit?, valid?}` — at least one `entity.*` key | `{facts: […]}` |
| `memory_traverse` | `{start:{kind,id}, edges?, direction?, depth?, hydrate?, deadline_ms?}` | `{nodes, edges, cursor, truncated}` |
| `memory_search` | `{query, scope?, k?, tiers?, as_of?}` — requires the semantic module | `{hits: […]}` |

**Traversal addressing rule:** `start.id` is the *composite node id*
`<tenant>|<dotted-kind>|<value>` while `start.kind` is the snake_case enum —
e.g. `{"kind":"entity_project","id":"acme|entity.project|billing"}`. A raw
value returns `not_found`, by design.

**Search parity:** the MCP search path runs the same semantic read pipeline
as HTTP. A `MEMKEYS_ROLE=mcp` process must enable the semantic module and
point at the **same embedder** as the primary deployment — otherwise query
vectors land in a different embedding space than the stored vectors. When you
exec into the server pod, the pod env already carries this; keep it that way.

## Principal binding

The stdio pipe is the trust boundary. At launch the server binds **one**
principal from `MEMKEYS_RUN_TOKEN`, an HS256 JWT minted with the deployment's
`MEMKEYS_RUN_TOKEN_SECRET`:

| JWT claim | Becomes |
|---|---|
| `company_id` | tenant floor — all four operations are scoped to it; tenant is never a tool argument |
| `sub` | `entity.agent` and the provenance principal |
| `exp` | expiry window, re-checked on every `tools/call`; an expired token fails `unauthenticated` |

If `MEMKEYS_RUN_TOKEN` / `MEMKEYS_RUN_TOKEN_SECRET` are absent, the server
binds an unverified dev principal for tenant `MEMKEYS_STDIO_TENANT`
(default `default`) — loopback dev only.

Mint a token with nothing but `openssl`:

```sh
b64() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
now=$(date +%s); exp=$((now + 3600))
hdr=$(printf '{"alg":"HS256","typ":"JWT"}' | b64)
pl=$(printf '{"company_id":"%s","sub":"%s","iat":%s,"exp":%s}' \
      "$TENANT" "$AGENT_ID" "$now" "$exp" | b64)
sig=$(printf '%s.%s' "$hdr" "$pl" \
      | openssl dgst -sha256 -hmac "$MEMKEYS_RUN_TOKEN_SECRET" -binary | b64)
TOKEN="$hdr.$pl.$sig"
```

Size the TTL to the expected session length plus headroom — the server
re-checks `exp` on every call, so a session that outlives its token fails
cleanly rather than silently. Never write long-lived tokens into client
config files; mint per session.

## Reaching the server

**Local process (dev).** Run the server on the agent host; the MCP client
owns its stdin/stdout:

```json
{
  "mcpServers": {
    "memkeys": {
      "type": "stdio",
      "command": "memkeys-server",
      "env": {
        "MEMKEYS_DATABASE_URL": "postgres://memkeys:memkeys@localhost:5432/memkeys",
        "MEMKEYS_RUN_TOKEN_SECRET": "<issuer-secret>",
        "MEMKEYS_RUN_TOKEN": "<minted-token>"
      }
    }
  }
}
```

**In-cluster (shared deployment).** Exec into the server pod in the MCP-only
role and hand the pipe to the server binary — no extra ports, no second
worker stack, and a small Postgres footprint:

```json
{
  "mcpServers": {
    "memkeys": {
      "type": "stdio",
      "command": "kubectl",
      "args": [
        "exec", "-i", "-n", "memkeys", "deploy/memkeys", "--",
        "sh", "-c",
        "exec env MEMKEYS_ROLE=mcp MEMKEYS_PG_MAX_CONNECTIONS=2 MEMKEYS_RUN_TOKEN=\"$TOKEN\" memkeys-server"
      ],
      "env": {
        "KUBECONFIG": "/path/to/kubeconfig",
        "TOKEN": "<minted-run-token>"
      }
    }
  }
}
```

Notes:

- `MEMKEYS_ROLE=mcp` is the point of this shape: the exec'd process binds
  nothing and reconcils nothing; the primary all-in-one pod stays the sole
  owner of projection and maintenance.
- `MEMKEYS_PG_MAX_CONNECTIONS=2` caps the session's Postgres pool — the
  default 8, multiplied across a fleet of sessions, exhausts Postgres
  `max_connections`.
- `MEMKEYS_DATABASE_URL`, the secret, and the embedder env are inherited
  from the pod; only the role, pool size, and fresh token are per session.

If you write your own `kubectl exec` wrapper, keep a stdin bridge (e.g.
`cat | memkeys-server`): a bare exec stream can deliver only the first stdin
burst and starve subsequent requests.

## Raw wire reference

Every exchange, verbatim:

```jsonc
// → initialize
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"my-client","version":"1"}}}
// ← {"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}},"protocolVersion":"2024-11-05","serverInfo":{"name":"memkeys","version":"0.1.0"}}}

// → notification — no reply, by design
{"jsonrpc":"2.0","method":"notifications/initialized"}

// → tools/list
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
// ← {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"memory_store",…},{"name":"memory_fetch",…},{"name":"memory_traverse",…},{"name":"memory_search",…}]}}

// → memory_store
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_store","arguments":{"envelope":{"source":{"kind":"gateway_flush","emitted_at":"2026-10-22T10:56:57Z"},"entity":{"project":"billing","agent":"triage-bot","run":"run-1"},"fact":{"body":"deploy freeze starts Friday","subject":"freeze"}}}}}
// ← {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"duplicate\":false,\"entity_keys_resolved\":{…},\"fact_id\":\"…\",\"keyed\":true}"}]}}

// → memory_fetch (at least one entity.* key)
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_fetch","arguments":{"entity.project":"billing"}}}
// ← … "facts":[{… "body":"deploy freeze starts Friday" …}] …

// → memory_traverse (composite id: <tenant>|<dotted kind>|<value>)
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_traverse","arguments":{"start":{"kind":"entity_project","id":"acme|entity.project|billing"},"depth":2,"direction":"out","hydrate":true}}}
// ← … "edges":[{"edge_type":"entity_fact",…}], "nodes":[{…hydrated records…}] …

// → memory_search (requires the semantic module on the server)
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"deploy freeze","k":5,"scope":{"entity.project":"billing"}}}}
// ← … "hits":[{"score":0.645, "fact":{"body":"deploy freeze starts Friday"}, …}] …

// errored tool call — an OK reply with isError:
// ← {"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"{\"code\":\"unauthenticated\",\"retriable\":false,…}"}],"isError":true}}
```

Minimal client in Python (stdlib only), against a local server:

```python
import json, subprocess

proc = subprocess.Popen(
    ["memkeys-server"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
    env={"MEMKEYS_DATABASE_URL": "postgres://memkeys:memkeys@localhost:5432/memkeys",
         "PATH": "/usr/bin:/bin:/usr/local/bin"},
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
                             "arguments": {"entity.project": "billing"}}))
proc.stdin.close(); proc.wait()
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `tools/list` times out but `initialize` answers | your exec wrapper hands the server a bare exec stream as fd 0 | bridge stdin (`cat \| memkeys-server`) or use `MEMKEYS_ROLE=mcp` exec directly |
| `memory_traverse` → `not_found` | raw value as `start.id` | composite id `<tenant>\|<dotted-kind>\|<value>` |
| `memory_search` → `isError:true feature_disabled` | semantic module off | run the server with `MEMKEYS_SEMANTIC=1` (and the digest sub-flag for digests) |
| every call → `unauthenticated` | run-token `exp` window closed | relaunch with a fresh token |
| fetch returns another tenant's data | not possible by construction — the tenant comes from the launch-bound principal, never a tool argument | — |
| server refuses to bind | non-loopback host without `MEMKEYS_RUN_TOKEN_SECRET` | keep `127.0.0.1`, set the secret, or set `MEMKEYS_ALLOW_INSECURE=1` (dev only) |

## Roadmap

The long-term fleet shape is **MCP over streamable HTTP** at the existing
`/memory/*` listener — no `kubectl`, no exec, no per-session process, just a
per-request bearer. Until it lands, the MCP-only stdio role is the supported
per-agent transport.
