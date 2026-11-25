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

## Golden Path linters

A **linter** is an advisory rule that captures part of a project's "golden
path" — a convention an agent should follow while it works (for example,
"never build this service in Rust" or "route logs through the structured
logger"). MemKeys stores linters as ordinary facts (typed `linter`) and, on
every `memory_fetch` and `memory_search`, returns the linters that apply to the
tenant and project **alongside** the facts — so an agent receives the golden
path in the same round-trip as the memory it asked for. No fifth operation: it
stays within the four-op surface.

The linter-to-agent concept — surfacing project guardrails to agents as part of
retrieval — originates with **[Jessica Andersson](https://github.com/Solisol)**
of **Solisol**. The MemKeys-native rule shape is designed to round-trip with the
[Solisol](https://github.com/Solisol) CLI rule schema. See [`NOTICE`](NOTICE)
for attribution.

- [`docs/golden-path-linters.md`](docs/golden-path-linters.md) — what linters
  are, how to author global and context rules, and how to read them back
  (usage & how-to).
- [`docs/retrieval-contract.md`](docs/retrieval-contract.md#golden-path-linters--the-applicable_linters-block) — the
  `applicable_linters` wire shape on fetch/search, ordering, and opt-out.
- [`docs/adr/ADR-0004-linter-data-model.md`](docs/adr/ADR-0004-linter-data-model.md) — the data model & storage decision.

## Inspector UI

The Inspector is a read-only React SPA for browsing memory. See the full documentation:

- [`docs/inspector-ui.md`](docs/inspector-ui.md) — What the Inspector is, how to run it, and how to visualize memory with Facts Browser and Memory Graph.
