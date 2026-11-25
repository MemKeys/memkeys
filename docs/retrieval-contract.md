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

Fetch responses also carry an additive
[`applicable_linters`](#golden-path-linters--the-applicable_linters-block)
block by default (the golden-path rules for the returned facts' projects); opt
out with `?applicable_linters=false`.

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

Like fetch, search responses carry an additive
[`applicable_linters`](#golden-path-linters--the-applicable_linters-block)
block by default (opt out with `"applicable_linters": false`). Linter facts are
excluded from the `hits` themselves — they are policy, not recall.

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

## Golden Path linters — the `applicable_linters` block

A **linter** is an advisory rule that describes part of a project's "golden
path" — a convention an agent should follow while it works. Linters are not a
fifth operation: they are ordinary facts, typed `linter`, stored and read over
the same four ops (see [storing a linter](#storing-a-linter) below). What the
retrieval contract adds is that **fetch (op 2) and search (op 3) also return the
linters that apply to the facts they return**, so an agent gets the golden path
in the same round-trip as the memory it asked for.

Both responses carry an **additive** `applicable_linters` array alongside their
normal payload (`facts` for fetch, `hits` for search). It is purely additive and
backward-compatible: a caller that ignores the field is unaffected.

```jsonc
// GET /memory/facts?entity.issue=I1   (and likewise POST /memory/search)
{
  "facts": [ … ],                      // or "hits": [ … ] for search
  "applicable_linters": [
    {
      "scope":        "global",        // "global" | "context"
      "severity":     "error",         // "error" | "warn" | "info" | null
      "fact.id":      "…",
      "fact.subject": "no-console-log", // the rule_id
      "fact.body":    { … },            // the rule (structured JSON; raw string if unparseable)
      "entity":       { }               // resolved entity keys ({} for a global linter)
    }
  ]
}
```

**What gets resolved.** The block is the union of two scopes, in one store
round-trip, de-duplicated by `fact.id`:

- **global** linters — unkeyed, tenant-floor rules that always apply; and
- **context** linters — rules keyed on an `entity.project` that appears among
  the returned facts/hits (the distinct `entity.project` set of the payload).

The entries are ordered **global-first, then by severity** (`error` → `warn` →
`info` → unknown), then by `rule_id` for a stable, deterministic wire order.
Both transports (HTTP and MCP) emit the byte-for-byte identical block.

**Linters never appear as search hits.** Op 4 recall deliberately excludes
`fact.type = linter` from its `hits`: linters are policy, not recall, and must
not be gated on query relevance or pollute a user's results. They reach callers
*only* through the deterministic `applicable_linters` resolver. (A global
linter's body is still embedded, so it remains addressable by op 4 in principle,
but recall filters it out.)

**Opt out.** The block is on by default. To get the legacy facts-only / hits-only
shape, set the flag:

| Transport | Operation | How to opt out |
|-----------|-----------|----------------|
| HTTP  | fetch  | query param `?applicable_linters=false` |
| HTTP  | search | body field `"applicable_linters": false` |
| MCP   | `memory_fetch` / `memory_search` | argument `"applicable_linters": false` (boolean, default `true`) |

The resolver is also **skipped automatically when the read is itself scoped to
linters** (`type=linter` on fetch, `fact.type=linter` on search) — that is a
management read of the rules themselves, not a golden-path read, so it is never
augmented with a second linter block.

**Advisory only (v1).** The block *reports* the applicable rules; nothing here
blocks, enforces, or validates against them. Enforcement hooks are a later,
additive change.

### Storing a linter

A linter is written with `memory_store` like any other fact — the only
distinguishing choices are the fact **type**, the **scope** (expressed through
entity keys), and the **body** (the rule itself). Its lineage: the rule shape
and storage model are **ADR-0004**; the `applicable_linters` resolver is Golden
Path B (ISI-7813). The Solisol CLI rule schema this body aligns with is credited
to **Jessica Andersson (Solisol)**.

- **Type** — `fact.type` is `"linter"`.
- **Subject** — `fact.subject` is the stable `rule_id`. Subject is the
  supersession grain, so re-storing the same `rule_id` at the same scope
  **versions** the rule (newest is the live head; history replays via
  `valid=all` / `as_of`).
- **Scope** — read off the entity keys, *not* a separate field:
  - **global** (applies tenant-wide): store **unkeyed** — no `entity.*` keys.
  - **context** (applies to one project): key the fact on `entity.project`.
- **Source kind** — both scopes are written over `telemetry_learning`; it is the
  one source kind whose anchoring tolerates an unkeyed fact, which a global
  linter needs, and it equally accepts the `entity.project` of a context linter.
- **Body** — the rule, as structured JSON (below). Stored opaquely in v1: the
  server neither validates nor enforces the shape, and unknown fields
  (e.g. extra Solisol keys) are preserved verbatim.

Store a **global** linter (MCP `memory_store`, or `POST /memory/facts`):

```jsonc
{
  "source":   { "kind": "telemetry_learning" },
  "entity":   { },                     // no entity.* keys → global / tenant-floor
  "fact": {
    "type":    "linter",
    "subject": "no-console-log",        // == rule_id; supersession grain
    "body": {
      "rule_id":     "no-console-log",  // == fact.subject
      "title":       "No console.log in committed code",
      "description": "Use the structured logger; console.log bypasses redaction.",
      "severity":    "warn",            // error | warn | info  (advisory in v1)
      "enabled":     true,
      "scope": {                         // narrowing facets for the resolver/consumer
        "language":      "typescript",   // omit or "*" = any language
        "path":          "src/**",       // glob; omit = whole repo
        "paths_exclude": ["**/*.test.ts"]
      },
      "message":  "console.log found — route through logger.info()",
      "match": {                         // Solisol-compatible matcher block
        "kind":         "regex",         // regex | ast | semgrep-ref | solisol-ref
        "pattern":      "console\\.log\\(",
        "solisol_rule": null             // optional ref into an external Solisol ruleset
      },
      "source":   "memkeys",             // "memkeys" | "solisol" — provenance of the rule
      "fix_hint": "Replace with logger.info(...)."
    }
  }
}
```

Store a **context** linter by adding the project key; everything else is the
same:

```jsonc
{
  "source": { "kind": "telemetry_learning" },
  "entity": { "project": "billing-service" },   // keyed → applies to this project
  "fact":   { "type": "linter", "subject": "require-idempotency-key", "body": { … } }
}
```

`severity` is the only body field the resolver interprets (it is lifted out for
the wire ordering and echoed on each entry); everything else in `body` is
carried through to consumers untouched. Read a project's linters back directly
with op 2 (`GET /memory/facts?entity.project=…&type=linter`) — this is the
management read that is *not* augmented with a second `applicable_linters` block.

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
