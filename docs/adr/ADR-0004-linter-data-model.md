# ADR-0004: Golden Path linter — data model & storage

- **Status:** Accepted (implements board-accepted plan `cc7ee655`, ISI-7787)
- **Date:** 2026-11-25
- **Owner:** Architect (Winston)
- **Workstream:** Golden Path linters — Paperclip issue ISI-7811 (child A of ISI-7787)
- **Relates to:** [`docs/architecture.md`](../architecture.md) §3 (envelope), §5.1 (fact record), §7.2 (op-1 keyed fetch), §7.4 (op-4 semantic search); [ADR-0003](ADR-0003-task-state-resume-convention.md) (prior "config as a fact, no new op" precedent)
- **Credits:** Linter concept & the Solisol CLI rule schema this model aligns with — **Jessica Andersson** (Solisol). The MemKeys-native shape below is designed for round-trip interop with Solisol rule files.

---

## Context

The Golden Path initiative (ISI-7787) makes linters a first-class concern of
MemKeys: an agent fetching or searching memory should also receive the linter
rules that apply to its tenant and project, so the "golden path" for a task is
surfaced alongside the facts. The board accepted plan `cc7ee655` with three
defaults and **no override**:

1. **MemKeys-native linter rule shape** (evaluate Solisol CLI rule-schema interop).
2. **Advisory-only v1** — storage returns linters; no blocking/enforcement hook yet.
3. **New `FactType::Linter`** — a new fact type, *not* a 5th operation.

The hard constraint from the parent plan: **stay inside the four-op surface**
(`memory_store` / `memory_fetch` / `memory_traverse` / `memory_search`). No 5th
operation, no new MCP tool.

This ADR covers **child A only**: the data model and storage. The resolver that
folds applicable linters into fetch/search responses (`applicable_linters`) is
children B–G (ISI-7813+) and is explicitly out of scope here.

## Decision

### 1. A linter is a fact, typed `linter`

Add `FactType::Linter` (wire name `"linter"`) to the canonical fact types
(`model.rs`). It flows through the existing pipeline unchanged because every
stage already keys off `FactType::as_str()` / serde — the only storage change
is admitting `'linter'` into the FactStore `fact_type` CHECK domain
(migration `0008_linter_fact_type.sql`). **No new columns.**

### 2. Scope is read off the entity keys — not a new field

| Scope | Anchoring | Stored as | Read back via |
|-------|-----------|-----------|---------------|
| **global**  | tenant floor only, **no** `entity.*` | unkeyed fact (`keyed=false`) | op-4 `memory_search` (runs over the whole tenant floor, incl. unkeyed — R6) |
| **context** | `entity.project` | keyed fact on project | op-1 `memory_fetch` with `entity.project` + `type: linter` |

Both are written over the **`telemetry_learning`** source kind. That is a
deliberate reuse, not an accident: `telemetry_learning` is the *only* source
kind whose stage-2 anchor requirement tolerates an **unkeyed** fact
(`pipeline.rs` §3.1 — "unkeyed tolerated"), which is exactly what a global
linter needs. A context linter supplies `entity.project`, which also satisfies
that same anchor rule. One source kind serves both scopes; no new `SourceKind`.

### 3. The rule shape lives in the structured `fact.body`

`fact.body` already accepts a structured JSON payload (envelope §3). The linter
rule is that payload. This keeps the rule **semantically searchable** (the body
is what gets embedded for op-4) and round-trips with zero new storage machinery.

`fact.subject` **= `rule_id`**. Subject is the supersession grain
(`pipeline.rs` stage 3 / live-head key `H = (tenant, entity, subject, type)`),
so re-storing a rule under the same `rule_id` + scope **versions** the rule
(the newest is the live head; history is replayable via `valid=all` / `as_of`).

Canonical MemKeys-native body (superset-aligned with the Solisol CLI rule
schema — unknown Solisol fields are preserved verbatim, never dropped):

```jsonc
{
  "rule_id": "no-console-log",        // == fact.subject; stable id, supersession grain
  "title": "No console.log in committed code",
  "description": "Use the structured logger; console.log bypasses redaction.",
  "severity": "warn",                  // error | warn | info  (advisory in v1)
  "enabled": true,
  "scope": {                           // narrowing facets the resolver (B–G) filters on
    "language": "typescript",          // omit / "*" = any language
    "path": "src/**",                  // glob; omit = whole repo
    "paths_exclude": ["**/*.test.ts"]
  },
  "message": "console.log found — route through logger.info()",
  "match": {                           // Solisol-compatible matcher block
    "kind": "regex",                   // regex | ast | semgrep-ref | solisol-ref
    "pattern": "console\\.log\\(",
    "solisol_rule": null               // optional ref into an external Solisol ruleset
  },
  "source": "memkeys",                 // "memkeys" | "solisol"; provenance of the rule
  "fix_hint": "Replace with logger.info(...)."
}
```

**Advisory-only (v1):** storage neither validates nor enforces this shape.
`severity` and `match` are carried for the resolver and future enforcement
children; the FactStore treats the body as opaque JSON text. Validating the
schema at write time is intentionally deferred (YAGNI until a producer needs the
feedback) and would be a later, additive change.

**Attachments** remain refs-only per the architecture invariant (never inline
data): a context linter MAY attach a URI to an external Solisol ruleset file;
the inline narrowing facets live in `scope` above so the resolver needs no
second fetch. *(This is the one reconciliation with the plan's "attachment
metadata" wording — narrowing facets live in the searchable body, not in
attachments, so they round-trip through the standard full view and feed op-4.)*

### 4. Storage / indexing

Migration `0008`:
- extend the `fact_type` CHECK to admit `'linter'`;
- add `facts_linter_idx` — a partial index `WHERE fact_type = 'linter'` on
  `(tenant_id, entity_project, event_time DESC)`. It serves both scopes in one
  shape (context = project range-scan; global = `entity_project IS NULL`), stays
  tiny (linters are low-cardinality config), and never competes with the 0001
  keyed-fetch indexes. This is the index the resolver children read to assemble
  a tenant/project's applicable linter set.

## Consequences

- **Smallest possible surface.** One enum variant + one migration (one CHECK
  swap, one partial index) + schema-enum advertisement. Everything else is the
  existing write pipeline and keyed/semantic read paths, unchanged.
- **DoD met:** a global linter (store via `telemetry_learning`, no key →
  read back via op-4) and a context linter (store with `entity.project` → read
  back via op-1 `type: linter`) both round-trip through the four ops. Unit
  coverage lives in `mcp.rs` (store+fetch, store+search) and `model.rs` (serde).
- **Solisol interop** is a body-shape alignment, not a hard dependency. The
  `match.solisol_rule` / `source` fields and verbatim preservation of unknown
  body keys let a Solisol ruleset be ingested without loss; a formal
  bidirectional converter (if wanted) is a follow-up to coordinate with Jessica
  Andersson, not a blocker for storage.
- **Enforcement deferred.** v1 is read-only/advisory. A blocking hook, write-time
  schema validation, and the fetch/search `applicable_linters` fold-in are
  children B–G (ISI-7813+) and build on this model additively.
