# MemKeys

The attribute-based memory server: facts keyed by agent-supplied attributes, temporal validity, graph+vector as derived views, open contract, pluggable backends (Grail plugin as reference).

**Keyed-first invariant:** attribute keys bound the candidate set before any vector/graph work; the keyed/temporal store is the single source of truth; graph and vector are derived, rebuildable projections.

See [`docs/architecture.md`](docs/architecture.md) for the architecture baseline — the landing pad for the WS-1…WS-6 ADRs.
