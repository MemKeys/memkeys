# Connect your agents

MemKeys speaks MCP over stdio, so wiring an agent is a config block, not an
integration project. This page collects the recipes for the common hosts —
the transport details and principal model live in
[MCP transport](/memkeys/agent-mcp-transport/).

All recipes use the four tools (`memory_store`, `memory_fetch`,
`memory_traverse`, `memory_search`) and the shared error surface. If your
client speaks MCP, it works.

## Claude Code

```bash
# A local server as a stdio child
claude mcp add-json memkeys '{
  "type": "stdio",
  "command": "memkeys-server",
  "env": {
    "MEMKEYS_DATABASE_URL": "postgres://memkeys:memkeys@localhost:5432/memkeys",
    "MEMKEYS_RUN_TOKEN_SECRET": "<issuer-secret>",
    "MEMKEYS_RUN_TOKEN": "<minted-token>"
  }
}'

# Health check — connects and runs initialize + tools/list
claude mcp get memkeys     # expect: Status: ✔ Connected

# Cleanup
claude mcp remove memkeys
```

Equivalent shorthand:

```bash
claude mcp add memkeys \
  -e MEMKEYS_DATABASE_URL=postgres://memkeys:memkeys@localhost:5432/memkeys \
  -- memkeys-server
```

The same shape works against a shared cluster deployment by pointing the
command at the MCP-only exec path — see
[reaching the server](/memkeys/agent-mcp-transport/#reaching-the-server).

## Claude Desktop

Add the block to `claude_desktop_config.json`
(`~/Library/Application Support/Claude/` on macOS, `%APPDATA%\Claude\` on
Windows):

```json
{
  "mcpServers": {
    "memkeys": {
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

## goose

[goose](https://github.com/block/goose) loads MCP extensions of
`type: stdio` from `~/.config/goose/config.yaml`:

```yaml
extensions:
  memkeys:
    name: MemKeys
    type: stdio
    cmd: memkeys-server
    envs:
      MEMKEYS_DATABASE_URL: postgres://memkeys:memkeys@localhost:5432/memkeys
      MEMKEYS_RUN_TOKEN_SECRET: <issuer-secret>
      MEMKEYS_RUN_TOKEN: <minted-token>
    enabled: true
    timeout: 300
```

Then `goose session` starts the extension and the four `memory_*` tools
appear.

## kagent

[kagent](https://kagent.dev) attaches MCP tool servers to Kubernetes-native
agents:

```yaml
apiVersion: kagent.dev/v1alpha1
kind: MCPServer
metadata:
  name: memkeys
  namespace: kagent
spec:
  stdio:
    command: memkeys-server
    env:
      MEMKEYS_DATABASE_URL: postgres://memkeys:memkeys@memkeys:5432/memkeys
      MEMKEYS_RUN_TOKEN_SECRET: <issuer-secret>
      MEMKEYS_RUN_TOKEN: <minted-token>
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

The `MCPServer`/`Agent` API version varies across kagent releases — check
`kubectl get crd | grep kagent` on your install, and run the stdio command
where it can reach your Postgres.

## Any other MCP client

Spawn `memkeys-server` (or the in-cluster MCP-only exec) as a stdio child
and speak newline-delimited JSON-RPC 2.0. The minimal client is ~20 lines of
Python — see the [raw wire reference](/memkeys/agent-mcp-transport/#raw-wire-reference)
for every exchange, verbatim, from `initialize` through a four-operation
round-trip.

## What your agents can do once connected

- **Store at session end** (`gateway_flush`): the agent's run id is the only
  key it needs to send — the server enriches the rest.
- **Pick up work** (`memory_fetch` by issue or project): everything known
  about X, deterministic, including the previous agent's handoff.
- **Ask open questions** (`memory_search`, scoped): "similar past incidents
  on this service".
- **Explain a decision** (`memory_traverse`): walk the supersession chain —
  what was decided, what it replaced, and who decided it.

## Recipe status

Claude Code and the raw JSON-RPC path are exercised against live servers on
every release. The goose and kagent blocks match their documented stdio
contracts but have not been live-tested end-to-end — if you run them, PRs
amending this page with what you find are welcome.
