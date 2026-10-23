<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="branding/_unpacked/memkeys-branding/kit/memkeys-mark-reverse-512.png">
    <img src="branding/_unpacked/memkeys-branding/kit/memkeys-mark-512.png" alt="MemKeys mark" width="220">
  </picture>
</p>

# MemKeys

The attribute-based memory server: facts keyed by agent-supplied attributes, temporal validity, graph+vector as derived views, open contract, pluggable backends (Grail plugin as reference).

**Website:** <https://memkeys.github.io/memkeys-website/> — landing page, Why MemKeys, quickstart, and the published docs. The docs are authored in [`docs/`](docs/) in this repo and ingested at build time by the [website repo](https://github.com/MemKeys/memkeys-website); merges to `main` that touch `docs/` trigger an automatic republish.

The name is final: **MemKeys** (camelCase) was locked in on 2026-10-14 after a full naming and collision sweep — it is no longer a working name. The brand kit (mark, wordmark lockups, palette, favicon set) lives in [`branding/`](branding/_unpacked/memkeys-branding/).

**Keyed-first invariant:** attribute keys bound the candidate set before any vector/graph work; the keyed/temporal store is the single source of truth; graph and vector are derived, rebuildable projections.

## MemKeys is not MemPalace

Despite the similar "mem-" naming, these are two different things:

- **MemKeys** — this project: an open-source, keyed memory server for agents. Facts are stored with agent-supplied attribute keys and temporal validity; graph and vector are derived views.
- **MemPalace** — a separate internal MCP shared-memory layer. A different project, with a different design; MemKeys is not built on MemPalace, and MemPalace is not MemKeys.

See [`docs/architecture.md`](docs/architecture.md) for the architecture baseline — the landing pad for the WS-1…WS-6 ADRs.

## Inspector UI

A read-only React SPA for browsing memory. It talks **only** to the public
read surface (`GET /memory/facts`, `POST /memory/graph/traverse`) — never to the
database, and it never writes. Lives in [`frontend/`](frontend/).

- **Facts Browser** — keyed retrieval by entity attributes (`project`, `issue`,
  `pr`, `agent`, `run`, `thread`, `service`), with `type` / `valid` / `as_of`
  filters and a fact-detail view.
- **Memory Graph** — a d3 force-directed view of a traversal from a start node
  (`kind` + id), with direction / depth / edge-type controls; truncation and
  dangling-edge gaps are surfaced, never silently hidden.

### Quickstart

```bash
# 1. Run the memory server on :3000 (loopback dev mode self-asserts the tenant)
cargo run -p memkeys-server

# 2. In another shell, run the Inspector UI (dev server on :3001, proxies /memory → :3000)
cd frontend
npm install
npm run dev
```

Open <http://localhost:3001>, set the **Tenant** field in the top bar (and a
bearer token if the server runs with a verifier configured), then search facts
or traverse the graph.

Build for production with `npm run build` (static assets in `frontend/dist/`);
serve them from the same origin as the API so the relative `/memory` base
resolves. `npm run lint` and `npm test` gate the build in CI
(`.github/workflows/frontend.yml`).

<!-- TODO(ISI-6083): embed frontend/docs/inspector-ui.png once captured in a
     browser-capable environment: `npm run build && npm run preview` then
     screenshot http://localhost:4173. The build/lint/test gate is green; the
     screenshot asset is a doc artifact, not a code dependency. -->

