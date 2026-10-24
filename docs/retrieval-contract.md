# Retrieval contract — the four operations

MemKeys exposes exactly four operations. All of them return the same **fact
shape** — the authoritative record hydrated from the keyed store — so callers
can mix operations and always get coherent results. Operations 3 and 4 never
carry body text from the graph or vector index; hydration from the keyed
store is always the final step.

The same four operations are available over two transports:

- **HTTP** on the memory listener (`/memory/*`, default port `3000`)
- **MCP over stdio** as the tools `memory_store`, `memory_fetch`,
  `memory_search`, `memory_traverse` (see [MCP transport](/memkeys/agent-mcp-transport/))

New to MemKeys? Start with [What is MemKeys](/memkeys/what-is-memkeys/) and
the [Quickstart](/memkeys/quickstart/).

## 1 — Store (write-with-keys)

```
POST /memory/facts
     { ...fact envelope... }
  → { "fact_id": "…", "keyed": true,
      "entity_keys_resolved": { "project": "…", "issue": "…" },
      "duplicate": false }
```

Push a raw envelope (see [Architecture — the fact envelope](/memkeys/architecture/#the-fact-envelope)).
The server does the modeling: it normalizes, validates and enriches keys,
applies temporal semantics, and commits the keyed record. The response
carries the server-resolved entity keys so the caller can cache them for
subsequent keyed reads. Retries with the same idempotency key are safe — a
duplicate push returns `duplicate: true` and writes nothing new.

## 2 — Fetch by keys (the default path)

```
GET /memory/facts
     ?entity.issue=…   &entity.pr=…   &entity.agent=…
     &entity.run=…     &entity.project=…  &entity.thread=…  &entity.service=…
     &type=decision|outcome|observation|learning|context   (optional)
     &as_of=<RFC3339>    (optional — point-in-time replay)
     &valid=current|all  (default: current — collapses supersession)
     &limit=…
  → { "facts": [ { "fact": { "id", "type", "subject", "body", "confidence",
                             "event_time" },
                   "entity": {…}, "provenance": {…} } ] }
```

At least one `entity.*` key is required; multiple keys are ANDed
(intersection). `valid=current` returns the latest non-superseded record per
`(entity, subject, type)`; `valid=all` returns the full history.
`as_of` replays the world as of a past instant on both temporal axes: only
facts whose event time is ≤ `as_of` **and** that had been recorded by then.

This operation is deterministic: the same keys return the same set. No
embeddings, no ranking, no drift.

## 3 — Semantic search (optional module, scoped)

```
POST /memory/search
     { "query": "<text>",
       "scope": {
         "entity.project": "…",
         "entity.issue":   "…",
         "entity.agent":   "…",
         "fact.type":      "…",
         "category":       "…"
       },
       "k":      10,
       "tiers":  "auto|fact|digest",
       "as_of":  "<RFC3339>"   (optional — scopes the candidate set)
     }
  → { "hits": [ { "fact": {…}, "score": 0.65,
                  "via": "fact|digest",
                  "digest_scope": {…}? } ] }
```

Search is **always bounded by the caller's tenant** — the floor is derived
from the verified principal, never supplied by the client. Finer `scope`
attributes are ANDed within that floor. Unknown scope keys and non-string
scope values are rejected (loudly, not ignored).

Results are ranked against both vector tiers when the digest tier is enabled
(fused with Reciprocal Rank Fusion); every digest hit resolves down to its
underlying raw facts. Hits are hydrated from the keyed store.

When the semantic module is disabled, the server answers
`501 feature_disabled` rather than an empty result, so callers can degrade
gracefully. Digest-only requests with the digest flag off degrade to the fact
tier with a warning header.

## 4 — Graph traversal

```
POST /memory/graph/traverse
     { "start": { "kind": "fact|entity_project|entity_issue|entity_agent|…",
                  "id": "<composite node id>" },
       "edges":     ["supersedes", "entity_fact", "produced_by", …],  // optional filter
       "direction": "out|in|both",
       "depth":     2,
       "hydrate":   true,     // fetch bodies from the keyed store for leaf nodes
       "deadline_ms": … }
  → { "nodes": […], "edges": [ { "from", "to", "edge_type" } ],
       "cursor": …, "truncated": false }
```

Walk relationships from a start node: supersession chains, entity links,
producer attribution. `hydrate: false` returns structure only (ids + edge
topology) for cheap introspection.

**Addressing rule:** `start.id` is the *composite node id*
`<tenant>|<dotted-kind>|<value>` — for example
`acme|entity.project|billing-service` — while `start.kind` is the snake_case
enum (`entity_project`). A raw value returns `not_found`, by design.
`truncated: true` (with a `cursor`) tells you the walk hit its budget;
truncation is surfaced, never hidden.

## Error shape

All errors share one taxonomy — `unauthenticated`, `forbidden`,
`not_found`, `invalid_request`, `feature_disabled`, `internal` — with
`retriable` flags and detail, on both HTTP and MCP. On MCP, an errored tool
call is a *successful* JSON-RPC reply with `isError: true` and the same error
body as content, so the two surfaces stay in parity.

## Inspector

The [Inspector UI](https://github.com/MemKeys/memkeys/tree/main/frontend)
(ships as `ghcr.io/memkeys/memkeys-inspector`) is a read-only browser for
this contract: a Facts Browser over fetch-by-keys and a force-directed Memory
Graph over traversal. It talks only to the public read surface, never to the
database, and it never writes.
