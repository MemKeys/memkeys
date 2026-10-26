<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="branding/_unpacked/memkeys-branding/kit/memkeys-mark-reverse-512.png">
    <img src="branding/_unpacked/memkeys-branding/kit/memkeys-mark-512.png" alt="MemKeys mark" width="220">
  </picture>
</p>

# MemKeys

The attribute-based memory server: facts keyed by agent-supplied attributes, temporal validity, graph+vector as derived views, open contract, pluggable backends (Grail plugin as reference).

The name is final: **MemKeys** (camelCase) was locked in on 2026-10-14 after a full naming and collision sweep — it is no longer a working name. The brand kit (mark, wordmark lockups, palette, favicon set) lives in [`branding/`](branding/_unpacked/memkeys-branding/).

**Website & docs:** <https://memkeys.github.io/memkeys-website/> — landing page plus the published docs from this repo's [`docs/`](docs/) tree (what MemKeys is, architecture, configuration, retrieval contract, MCP transport, Kubernetes, and verified [connect-your-agents recipes](https://memkeys.github.io/memkeys-website/docs/connect-your-agents/)). CI republishes the site when `docs/` changes on `main`.

**Keyed-first invariant:** attribute keys bound the candidate set before any vector/graph work; the keyed/temporal store is the single source of truth; graph and vector are derived, rebuildable projections.

**Benchmarks:** on an agentic end-to-end benchmark (agents solving real tasks, scored on answer correctness), MemKeys ranks **#1 of 5** backends on the single-agent SRE scenario — highest warm accuracy and largest cold→warm memory payoff. Full results, methodology, and reproduction steps: [`BENCHMARKS.md`](BENCHMARKS.md).

## MemKeys is not MemPalace

Despite the similar "mem-" naming, these are two different things:

- **MemKeys** — this project: an open-source, keyed memory server for agents. Facts are stored with agent-supplied attribute keys and temporal validity; graph and vector are derived views.
- **MemPalace** — a separate internal MCP shared-memory layer. A different project, with a different design; MemKeys is not built on MemPalace, and MemPalace is not MemKeys.

See [`docs/architecture.md`](docs/architecture.md) for the architecture baseline — the landing pad for the WS-1…WS-6 ADRs.

## Inspector UI

The Inspector is a read-only React SPA for browsing memory. See the full documentation:

- [`docs/inspector-ui.md`](docs/inspector-ui.md) — What the Inspector is, how to run it, and how to visualize memory with Facts Browser and Memory Graph.
