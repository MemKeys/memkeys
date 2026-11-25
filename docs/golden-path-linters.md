# Golden Path linters

A **linter** is an advisory rule that describes part of a project's "golden
path" — a convention an agent should follow while it works. Examples: "never
build this service in Rust", "route logs through the structured logger", or
"every write endpoint requires an idempotency key". MemKeys surfaces these rules
to agents as part of retrieval, so the guardrails travel with the memory.

> **Origin & credit.** The linter-to-agent concept — surfacing a project's
> guardrails to agents as a first-class part of memory retrieval — originates
> with **[Jessica Andersson](https://github.com/Solisol)** of **Solisol**. The
> MemKeys-native rule shape documented here is designed to round-trip with the
> [Solisol](https://github.com/Solisol) CLI rule schema. See the repository
> [`NOTICE`](../NOTICE) for attribution, and
> [ADR-0004](adr/ADR-0004-linter-data-model.md) for the data-model decision.

## The idea in one line

On every `memory_fetch` and `memory_search`, MemKeys returns the linters that
apply to the tenant and project **alongside** the facts — the "golden path"
arrives in the same round-trip as the memory an agent asked for.

Crucially, this is **not a fifth operation**. A linter is an ordinary fact,
typed `linter`, stored and read over the same four operations as everything else
(`memory_store` / `memory_fetch` / `memory_search` / `memory_traverse`). What
the retrieval contract adds is an additive `applicable_linters` block on the two
read operations.

## Two scopes

A linter applies at one of two scopes, expressed purely through entity keys — no
separate scope field:

| Scope | Applies to | Stored |
|-------|------------|--------|
| **global**  | the whole tenant | **unkeyed** — no `entity.*` keys |
| **context** | one project | keyed on `entity.project` |

On a read, the `applicable_linters` block is the union of (a) all global linters
and (b) the context linters whose `entity.project` appears among the facts the
read returned, de-duplicated by fact id and ordered **global-first, then by
severity** (`error` → `warn` → `info`).

## The rule shape

The rule itself lives in the fact `body` as structured JSON. It is **advisory
only in v1**: MemKeys stores it opaquely — it neither validates nor enforces the
shape, and unknown fields (for example extra Solisol keys) are preserved
verbatim. `fact.subject` is the stable `rule_id`, which is also the supersession
grain: re-storing the same `rule_id` at the same scope **versions** the rule.

```jsonc
{
  "rule_id":     "no-console-log",     // == fact.subject; stable id, supersession grain
  "title":       "No console.log in committed code",
  "description": "Use the structured logger; console.log bypasses redaction.",
  "severity":    "warn",               // error | warn | info  (advisory in v1)
  "enabled":     true,
  "scope": {                            // narrowing facets for the consumer
    "language":      "typescript",      // omit or "*" = any language
    "path":          "src/**",          // glob; omit = whole repo
    "paths_exclude": ["**/*.test.ts"]
  },
  "message":  "console.log found — route through logger.info()",
  "match": {                            // Solisol-compatible matcher block
    "kind":         "regex",            // regex | ast | semgrep-ref | solisol-ref
    "pattern":      "console\\.log\\(",
    "solisol_rule": null                // optional ref into an external Solisol ruleset
  },
  "source":   "memkeys",                // "memkeys" | "solisol" — provenance of the rule
  "fix_hint": "Replace with logger.info(...)."
}
```

Only `severity` is interpreted by MemKeys (lifted out for wire ordering and
echoed on each entry); every other field is carried through to the agent
untouched.

## How to author a linter

A linter is written with `memory_store` like any other fact. The only
distinguishing choices are the **type** (`linter`), the **scope** (through
entity keys), the **source kind** (`telemetry_learning`, the one kind whose
anchoring tolerates an unkeyed global fact), and the **body** (the rule).

### Store a global linter

No `entity.*` keys → the rule applies tenant-wide:

```jsonc
{
  "source": { "kind": "telemetry_learning" },
  "entity": { },
  "fact": {
    "type":    "linter",
    "subject": "no-console-log",
    "body": {
      "rule_id":  "no-console-log",
      "title":    "No console.log in committed code",
      "severity": "warn",
      "enabled":  true,
      "message":  "console.log found — route through logger.info()",
      "match":    { "kind": "regex", "pattern": "console\\.log\\(" },
      "source":   "memkeys"
    }
  }
}
```

### Store a context linter

Add the project key; everything else is the same:

```jsonc
{
  "source": { "kind": "telemetry_learning" },
  "entity": { "project": "billing-service" },
  "fact": {
    "type":    "linter",
    "subject": "require-idempotency-key",
    "body": {
      "rule_id":  "require-idempotency-key",
      "title":    "Write endpoints require an idempotency key",
      "severity": "error",
      "enabled":  true,
      "message":  "POST/PUT handler missing idempotency-key handling"
    }
  }
}
```

### Version or disable a rule

Re-store the same `rule_id` at the same scope to publish a new version (the
newest is the live head; earlier versions replay via `valid=all` / `as_of`). To
turn a rule off without deleting its history, re-store it with
`"enabled": false`.

## How agents receive linters

Any `memory_fetch` or `memory_search` automatically carries the additive
`applicable_linters` array next to its normal payload:

```jsonc
// GET /memory/facts?entity.issue=I1   (and likewise POST /memory/search)
{
  "facts": [ … ],                      // or "hits": [ … ] for search
  "applicable_linters": [
    {
      "scope":        "global",        // "global" | "context"
      "severity":     "error",
      "fact.id":      "…",
      "fact.subject": "no-console-log", // the rule_id
      "fact.body":    { … },            // the rule
      "entity":       { }               // resolved entity keys ({} for a global linter)
    }
  ]
}
```

The block is **backward-compatible**: a caller that ignores the field is
unaffected. Both transports (HTTP and MCP) emit the byte-for-byte identical
block. For the full wire contract — field-by-field shape, ordering guarantees,
and the opt-out flag — see
[retrieval-contract.md](retrieval-contract.md#golden-path-linters--the-applicable_linters-block).

A few behaviours worth knowing:

- **Linters never appear as search hits.** Op-4 recall deliberately excludes
  `fact.type = linter` from its `hits`: linters are policy, not recall, so they
  must not be gated on query relevance or pollute a user's results. They reach
  agents *only* through the deterministic `applicable_linters` resolver.
- **Opt out** with `?applicable_linters=false` (HTTP fetch) or
  `"applicable_linters": false` (search body / MCP argument) to get the legacy
  facts-only shape.
- **Management reads are not augmented.** A read that is itself scoped to
  linters (`type=linter` on fetch, `fact.type=linter` on search) is a read of
  the rules themselves, so it is never decorated with a second linter block.
  Read a project's rules directly with
  `GET /memory/facts?entity.project=…&type=linter`.

## Browsing linters in the Inspector

The [Inspector UI](inspector-ui.md) has a **Linters** view for managing rules:
list the global and per-project linters, see each rule's scope and severity, and
create, edit, or disable a rule. It talks only to the public read/write surface,
never to the database.

## Solisol interop

MemKeys interop with Solisol is a **body-shape alignment, not a hard
dependency**. The `match.solisol_rule` and `source` fields, plus verbatim
preservation of unknown body keys, let a Solisol ruleset be ingested without
loss. A formal bidirectional converter, if wanted, is a follow-up to coordinate
with Jessica Andersson — not a prerequisite for storing or reading linters
today.

## Status

- **v1 is advisory.** MemKeys *reports* applicable rules; nothing here blocks,
  enforces, or validates against them. Enforcement hooks (and write-time schema
  validation) are a later, additive change.

## See also

- [retrieval-contract.md](retrieval-contract.md#golden-path-linters--the-applicable_linters-block)
  — the `applicable_linters` wire shape, ordering, and opt-out.
- [ADR-0004](adr/ADR-0004-linter-data-model.md) — linter data model & storage decision.
- [NOTICE](../NOTICE) — attribution for the Solisol rule-schema alignment.
