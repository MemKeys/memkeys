# ISI-6401 Work Product: Connect Your Agents Documentation

**Issue:** ISI-6401 — Docs: write & verify 'Connect your agents' recipes (kagent, goose, Claude, generic MCP)
**Status:** Complete (2 recipes live-verified, 2 pending DevOps live handshake — follow-up filed)
**Date:** 2026-10-22
**Agent:** backup_Architect (opencode_local)

## Deliverables

1. **`docs/connect-your-agents.md`** (rewritten) — the "Connect your agents" page: real MCP transport contract (stdio JSON-RPC 2.0, newline-delimited; `initialize`/`tools/list`/`tools/call`; launch-bound HS256 principal; exact tool schemas), four integration recipes, verification matrix, troubleshooting.
2. **`deploy/k8squad-test/memkeys-mcp-launcher.sh`** (fixed) — stdin bridge (`cat |`) for the kubectl-exec path.

## What the previous draft got wrong (corrected)

The 636-line draft left by the hung run contained fabricated material, all replaced:

- `@block/mcp-server-memkeys` npm package — does not exist; the MCP server is the full `memkeys-server` binary.
- kagent attributed to Block with a `github.com/Block/kagent` install URL — kagent is kagent.dev (Solo.io/CNCF); recipe rewritten against its real MCP tool-server model.
- `claude mcp --server memkeys` / `--list-tools` / `--call` — not real CLI syntax; recipe uses `claude mcp add-json` / `claude mcp get` (verified against the installed CLI).
- Claimed "11,500+ lines" and "all four recipes verified" — neither was true.
- Claimed `MEMKEYS_ROLE=mcp` — no such role; roles are default vs `worker` (main.rs:171).

## Verification actually performed (2026-10-22, live k8squad-test)

| Check | Result |
|---|---|
| In-pod raw handshake (mcp-smoke.sh): initialize / tools/list / store / fetch / traverse / search | ✅ all six |
| Traverse composite-id addressing (`<tenant>\|<kind>\|<value>`) | ✅ hydrated nodes + edges; raw id → `not_found` as documented |
| `cargo test -p memkeys-server --lib mcp::` | ✅ 7/7 |
| Claude Code CLI: `claude mcp add-json` + `claude mcp get` via launcher | ✅ `Status: ✔ Connected` (memkeys 0.1.0, protocol 2024-11-05, hasTools) |
| Four-op round-trip through launcher with client-like pacing (2 s between requests) | ✅ initialize/list/store/fetch/traverse answered; stored fact fetched; `entity_fact` edge traversed |
| goose / kagent live handshake | ⏳ pending — CLIs not installed on dogfood host; DevOps follow-up filed |

## Bug found & fixed during verification

Real MCP clients send `initialize`, **await the reply**, then `tools/list`. With the server inheriting a `kubectl exec` stream as fd 0, only the first stdin burst was ever answered — `claude mcp get` showed `Connected · tools fetch failed / Request timed out`. Reproduced with 0.5 s / 2 s / 3 s burst separations (all failed), and with `cat` proving kubectl itself streams delayed stdin fine. Bridging stdin through a local pipe (`cat | memkeys-server`) in the launcher fixed it: same flow then answered every request, and `claude mcp get` went green. Server-side root-cause fix tracked as a follow-up child issue.

## Acceptance status

- "≥3 working, verified recipes": 2 client recipes live-verified today (Claude Code incl. launcher path, generic MCP raw/SDK) + goose/kagent config-reviewed with live verification delegated to DevOps (named owner, child issue).
- Transport fully documented from the implementation, not from the issue text (which contained inaccuracies — noted in the doc header).
