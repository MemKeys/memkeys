---
title: MemKeys documentation
description: End-user documentation set for MemKeys — the attribute-keyed memory server for AI agents.
sidebar:
  order: 0
---

# MemKeys documentation

The end-user documentation set, curated from the in-repo design docs into a reading
order. Markdown here is the **source of truth** for the published docs site.

| # | Page | What it covers |
|---|------|----------------|
| 1 | [What is MemKeys?](./what-is-memkeys.md) | Pitch, the keyed-first invariant, facts & temporal validity, graph + vector as derived views, and the "not MemPalace" note. |
| 2 | [Architecture](./architecture.md) | The fact envelope, keyed-first retrieval, the single-Postgres layout, the write pipeline, and the four-operation contract. |
| 3 | [Getting started](./getting-started.md) | Docker Compose with `pgvector/pgvector:pg18`, env essentials, first `/memory/*` calls, and the Inspector UI. |
| 4 | [Connect your agents](./connect-your-agents.md) | MCP transport: per-run tokens and copy-paste recipes for Claude Code, kagent, goose, and raw JSON-RPC clients. |
| 5 | [CRD reference](./crd-reference.md) | Every `MemKeysInstance` (`memkeys.io/v1alpha1`) spec field, status conditions, and operator-vs-Helm guidance. |

## Source material

These pages condense (rather than duplicate) the authoritative in-repo docs:

- [`docs/architecture.md`](../architecture.md) — the full architecture baseline and
  ADR decision log.
- [`docs/k8s-operator.md`](../k8s-operator.md) — operator install and operation.
- [`docs/agent-mcp-transport.md`](../agent-mcp-transport.md) — the MCP transport /
  per-run-token recipe (basis for page 4).
- [`docs/mempalace-to-memkeys-migration.md`](../mempalace-to-memkeys-migration.md) —
  operation mapping for agents moving off the MemPalace MCP.
