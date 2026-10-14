# MemKeys — MCP Server + Skills (Contract Spec)

**Status:** Draft (WS-5, ISI-5870). Depends on WS-4 (ISI-5869, retrieval contract) — drafted in
parallel, finalized after. Grounded on `../architecture.md` §7 (the in-repo MemKeys architecture doc
promoted by WS-0 / ISI-5865 — the authoritative retrieval contract) until WS-4 ratifies it; §9 flags
every point that must reconcile with WS-4.
**Workstream:** WS-5 — MCP server + skills (the adoption surface).
**Owner:** Architect (Winston). **Co-owner:** Product Manager (consumer surface). **Review:** Challenger.
**Layer:** Adoption surface over the retrieval contract. This doc specifies *how agents reach MemKeys*,
not *what MemKeys stores* (that is `../architecture.md`). All `§` references below point into that doc.

> **Design stance.** The MCP tools are **thin wrappers over the §7 HTTP retrieval contract** — they add
> no new storage, no new modeling, no new authority. Every tool is a transport-and-ergonomics shell around
> an existing §7 operation. The MCP layer's whole job is three things the raw HTTP contract does not give an
> agent for free: (1) a surface an MCP client can discover and call, (2) **natural-language skill descriptions
> that teach the keyed-first discipline** (§2 of the design) so agents reach for the cheap deterministic path
> first, and (3) **token-budget-aware response shaping** so a single recall never blows the caller's context
> window. Everything else is deferred to the HTTP contract and the server-side pipeline.

## 1. Tool surface — five tools, four retrieval ops

The server exposes exactly five MCP tools, each mapping to one §7 operation. The issue's op-numbering and
the design's §7 numbering differ; this table is keyed by **name + §-anchor** to remove the ambiguity.

| MCP tool | Purpose (one line) | Design op (§7) | Underlying HTTP call |
|---|---|---|---|
| `memory.write` | Push a raw fact; server models it. | op write-with-keys (§7.1) | `POST /memory/facts` |
| `memory.get` | Deterministic keyed fetch — the hot, default read. | op fetch-by-keys (§7.2) | `GET /memory/facts` |
| `memory.traverse` | Walk graph edges / supersession chains. | op graph traversal (§7.3) | `POST /memory/graph/traverse` |
| `memory.search` | Scoped semantic search — **fact tier** (precision). | op semantic search (§7.4) | `POST /memory/search` `tiers=fact` |
| `memory.recall` | Scope recall — **digest tier** → resolves to raw facts. | op semantic search (§7.4) | `POST /memory/search` `tiers=digest` |

`search` and `recall` are **two ergonomic faces of the same §7.4 endpoint**, split at the MCP layer because
they answer different questions and deserve different skill descriptions:

- **`memory.search`** is *query-centric*: "find the fact that says X" — fact tier, precision. Input leads with
  `query` text.
- **`memory.recall`** is *scope-centric*: "what happened in this thread / run / entity" — digest tier,
  navigational recall. Input leads with a `scope` anchor; `query` is optional. Always resolves the digest hit
  down to its member facts (R1, §5.4.2) — the summary is a pointer, never the returned answer.

**Retrieval-priority is encoded in the descriptions, not just the docs.** The skill descriptions (§5) push
`memory.get` as the default and frame `search`/`recall` as the fallback for genuinely open questions (§2 of
the design). An agent reading the tool list should reach for the deterministic keyed path first.

## 2. Transport

Two transports, **identical tool surface**. The only differences are how the principal is established and how
many tenants a server instance serves.

### 2.1 stdio — local / colocated agents

- One agent process launches one MemKeys MCP server subprocess; they speak MCP over stdio.
- **Single-principal per process.** The principal is bound once at launch from the run token in the launch
  environment (the `PAPERCLIP_API_KEY`-shaped run-scoped JWT, §3). There is no per-call auth handshake over
  stdio — the pipe *is* the trust boundary (same-host, same-launch).
- The subprocess holds the verified tenant/agent/run claims for its whole lifetime; every tool call inherits
  them. A stdio server **cannot** serve a second tenant — switching tenants means a new subprocess.
- **Lifetime is bounded by the token's validity window (F12).** The subprocess is not "verified forever" — the
  bound token carries a validity window (§3), and the server **stops serving when that window closes**: it
  self-terminates, and any tool call attempted past expiry hard-fails with `unauthenticated` (§7, "subprocess
  relaunch"). A long-lived stdio server must never answer calls under an expired run token.
- Intended for: an agent runtime colocated with its memory server, dev loops, air-gapped/minimal deployments
  (keyed+graph only, no shared infra).

### 2.2 Streamable HTTP — shared server

- One MemKeys server instance serves many agents across many tenants, over the MCP **Streamable HTTP**
  transport (single endpoint, POST for requests, server-initiated SSE stream for progressive/large results).
- **Per-request auth.** Every MCP request carries a bearer token (§3); the principal and tenant are derived
  from the *verified token on that request*, never from a session the client opened earlier. A long-lived SSE
  stream does not grant long-lived authority — each logical tool call re-presents and re-verifies its token.
- Multi-tenant by construction; the tenant floor (§3) isolates one caller's results from another's.
- Intended for: a central MemKeys deployment shared by a fleet (AgentMesh gateway flush, k8squad), on-cluster
  service, anything where many principals share one index.

### 2.3 What is identical across transports

Tool names, argument schemas, response shapes, pagination, error taxonomy, and response shaping are
**byte-for-byte the same**. A skill written against stdio works unchanged against HTTP. The transport choice is
an operational decision (colocated vs shared), never a contract decision.

## 3. Auth + tenant scoping (the R6 floor, enforced on every call)

This section is the MCP-layer enforcement of **R6 / §7.4** — *every read is bounded by the caller's tenant; a
search with no finer key runs against everything that tenant owns, never global.*

**Principal.** Every call resolves to an authenticated principal carrying verified claims:
`tenant_id` (≈ company), `agent_id`, `run_id`, and token validity window — exactly the claim set already in the
run-scoped JWT. stdio binds it at launch (§2.1); HTTP verifies it per request (§2.2).

**`fact_id` derivation is principal-salted and content-complete (F1, F8).** The server-assigned `fact_id` (the
idempotency key, §4.1 / §6 stage 1) is `hash(tenant_id, agent_id, resolved-entity-anchor, fact.subject,
fact.type, fact.body, source.emitted_at)` — the verified `tenant_id`+`agent_id` are **mandatory salt** and the
`fact.body` is **in the hash**. Two consequences this contract requires:
- *Cross-principal collision is impossible by construction.* Two tenants (or two agents) emitting the same
  body+keys derive **different** ids, so a `conflict` (§7) can only ever return the **caller's own** prior
  fact — never another principal's. This closes the cross-tenant existence-oracle + silent-write-loss hole
  (F1): the `not_found` fail-closed rule (point 3) is not bypassable via the `conflict` path.
- *Retry vs. update is unambiguous (F8).* A byte-identical re-emission derives the same id → `conflict`
  returning that id (the retry already landed, no duplicate). A **changed body** from the same principal at the
  same `(entity, subject, type)` grain derives a **new** id → it is a *new fact*, not a conflict; read-time
  latest-wins (§5.2) returns the newer one, so an update needs no explicit `supersedes` within a grain.
  `supersedes` is reserved for cross-grain or audit-explicit replacement (§4.1, F7).

**The tenant floor is server-derived and non-negotiable:**

1. **No tool argument can widen scope beyond the caller's tenant.** `scope`/`entity` arguments on any read tool
   are **intersect-only** — they narrow *within* the tenant floor the server already knows. There is no
   "global" value, no wildcard tenant, no argument the client can set to escape its tenant. A call that omits
   all finer keys runs against *everything the caller's tenant owns* (including `unkeyed` and sparsely-keyed
   facts, closing the R6 recall hole) — but still only that tenant.
2. **Write identity *and supersede targeting* are bound to the principal (F7).** On `memory.write`,
   `source.producer` and `entity.agent` are overwritten from the verified principal (§3.1 of the design); a
   client-asserted `agent` is advisory and discarded on conflict; `provenance` is rejected-if-populated; there
   is no way to write a fact *as another agent*. **The `supersedes` target is the real confused-deputy vector,
   and it is checked here:** a `supersedes: <prior fact.id>` is validated for *ownership* before the grain
   check — the target MUST be owned by the caller's tenant, else the write fails closed as `not_found` (§7),
   never `forbidden` (no existence leak). Within a tenant, an agent **may** supersede another agent's fact at a
   matching grain (cross-agent-within-tenant supersession is an allowed collaboration pattern — handoffs
   correct each other), but the superseding record's identity is still the acting principal's, so the audit
   trail attributes the change correctly. Cross-tenant supersession is impossible (fails `not_found`).
3. **Cross-tenant ids fail closed, and boundary crossings are audited (F15).** If a `memory.get` /
   `memory.traverse` / `memory.write`-supersede call names an `entity.*` id, a `fact.id` / node id, or a
   supersede target the caller's tenant does not own, the server returns `not_found` (§7) — **never** a
   `forbidden` that would leak the id's existence across the tenant boundary. Because a silent `not_found` on
   every boundary crossing is also an unlogged id-probing oracle, **every boundary-crossing `not_found` is
   written to a read-side audit log** (the read path carries no provenance otherwise) and is **coupled to
   `rate_limited`** (§7): a principal generating repeated boundary-crossing `not_found`s is throttled. This
   denies the existence-oracle its unlimited, invisible probing budget.
4. **Tenant isolation holds under semantic recall by *partitioning*, not by hoping the ANN drops the right rows
   (F5, F6).** "Scope before the ANN pass" is only satisfiable if the vector/digest backend supports **true
   id-set pre-filtering or per-tenant partitioning** — a classic HNSW/IVF index that *post*-filters (retrieve
   top-k by vector, then drop non-matching ids) scores team B's vectors against team A's query and can starve
   A's real results to empty. This contract therefore **requires**:
   - *Fact-vector tier (F5):* a **tenant-partitioned index** (per-tenant namespace/collection) OR a backend
     that does hard id-set pre-filtering. The fact-vector record MUST carry `tenant_id` (the §5.4.2 record
     shape omits it today — reconcile with WS-4). "Scope before ANN" is **not** satisfied by post-filtering.
   - *Digest tier (F6):* every digest is **tenant-partitioned at rollup** — `(scope, scope_id)` is always
     tenant-qualified, and the summarizer **never** sweeps two tenants' facts into one digest/story/embedding.
     This is load-bearing because `memory.recall` returns the digest `summary` text *directly* (§4.5), so a
     cross-tenant rollup would leak B's content into A's summary **before** any member-fact id check — the one
     path where the member-fact resolve-down guard (R1) cannot save isolation.
5. **`memory.traverse` applies the tenant floor on *every hop*, not just at `start` (F4).** Fail-closing the
   `start` node (point 3) is necessary but not sufficient: entity nodes keyed on shared strings
   (`project`/`pr`/`agent` ids reused across tenants) are cross-tenant bridges if neighbors are walked
   unfiltered. The per-hop filter `node.tenant == caller.tenant` is a **first-class MCP traverse invariant**
   (not merely inherited from `architecture.md` ADR-GRAPH): any hop to a node outside the floor is pruned as if
   the edge did not exist. An implementer reading only this contract must apply it.

**What the MCP layer deliberately does *not* own:** token issuance, rotation, and the OAuth2 flow for HTTP are
the deployment's auth system (the existing Paperclip run-token issuer for the reference deployment). The MCP
server is a *verifier*, not an issuer — it validates the presented token's signature and claims and derives the
floor. This keeps the contract portable across auth backends.

## 4. Tool schemas (argument contracts)

Schemas are MCP `inputSchema` (JSON Schema). Fields marked *server-derived* are **not** in the input schema —
they come from the principal (§3) and are listed only to make the trust boundary explicit. All responses share
the §6 shaping controls and the §7.5 fact shape.

### 4.1 `memory.write` → `POST /memory/facts`

```jsonc
{
  "name": "memory.write",
  "inputSchema": {
    "source":     { "kind": "<gateway_flush|run_settle|a2a_exchange|telemetry_learning>",  // REQUIRED
                    "emitted_at": "<RFC3339>" },                                            // REQUIRED
    "entity":     { /* send AT LEAST this source's anchor (§3.1): run | thread | service|project.
                       Other keys are hints; server enriches. agent/producer are server-derived. */ },
    "fact":       { "body": "<text|structured>",      // REQUIRED — the only thing only the producer knows
                                                      //   server-capped: body > max_body_bytes → invalid_envelope (F14)
                    "type": "<decision|outcome|observation|learning|context>",  // optional, server-defaulted
                    "subject": "<string>",            // REQUIRED when `supersedes` is set
                    "event_time": "<RFC3339>",        // optional
                    "confidence": 0.0,                // optional; SHOULD set for type=learning
                    "supersedes": "<prior fact.id>" },// optional; target MUST be tenant-owned (§3.2) else not_found
    "attachments": [ "<ref>" ]                        // optional; refs only, never inlined; count-capped (F14)
    // server-derived (NOT in schema): source.producer, entity.agent, provenance.*
  }
}
// → { "fact_id": "...", "keyed": true, "entity_keys_resolved": { ... }, "rejected_hints": [ ... ] }
```

Idempotent on the server-assigned `fact_id` derivation (retried flush = no duplicate, §6 stage 1); the
derivation is **principal-salted and body-complete** (§3, F1/F8), so a byte-identical retry collides to a
`conflict` returning the caller's own id while a content change at the same grain is a new fact resolved by
latest-wins (§5.2). `write` returns the resolved keys so a caller can cache them for subsequent `memory.get`
without a lookup round-trip. **Write size is bounded (F14):** `fact.body` over `max_body_bytes` and
`attachments[]` over `max_attachments` are rejected `invalid_envelope` (§7) — the write side must not be able
to plant an unbounded body that later blows the embedder, the digest summarizer, and every `view:full` read.
Defaults are deployment-configurable; the body cap SHOULD align with the §6.2.4 read ceiling so a single fact
is always readable in one `view:full` call.

### 4.2 `memory.get` → `GET /memory/facts`

```jsonc
{
  "name": "memory.get",
  "inputSchema": {
    "entity":  { "project": "...", "issue": "...", "pr": "...", "agent": "...", "run": "...", "thread": "..." },
                // AT LEAST ONE required; multiple keys are ANDed (intersection)
    "type":    "<decision|outcome|observation|learning|context>",  // optional
    "valid":   "current|all",     // default current — collapses supersession (latest-wins, §5.2)
    "as_of":   "<RFC3339>",       // optional point-in-time replay (§5.3)
    "view":    "compact|full",    // §6 — default compact
    "fields":  [ "<field>" ],     // optional projection allowlist (§6)
    "limit":   50,                // §6 default/cap
    "cursor":  "<opaque>"         // §6 pagination
  }
}
// → { "facts": [ <fact shape §7.5> ], "next_cursor": "<opaque|absent>", "token_estimate": 1234 }
```

The hot path: no graph, no vector. At least one `entity.*` key is required (a `get` cannot be a tenant-wide
scan — that is what `search` is for). Maps directly to the §5.2 / §5.3 DQL.

### 4.3 `memory.traverse` → `POST /memory/graph/traverse`

```jsonc
{
  "name": "memory.traverse",
  "inputSchema": {
    "start":     { "kind": "fact|entity", "id": "..." },   // REQUIRED; must be tenant-owned (else not_found §3)
    "edges":     [ "supersedes|entity_fact|produced_by|temporal|a2a_participant" ], // omit = all
    "direction": "out|in|both",   // default both
    "depth":     2,               // REQUIRED; server caps (§6 — bounded fan-out)
    "hydrate":   true,            // true = fetch bodies for leaf nodes from SoT; false = topology only
    "view":      "compact|full",  // §6, applies to hydrated nodes
    "limit":     50, "cursor": "<opaque>"
  }
}
// → { "nodes": [ { "kind", "id", <keyed record if hydrate §7.5> } ],
//     "edges": [ { "from", "to", "type" } ], "next_cursor": "...", "truncated": false }
```

`hydrate: false` is the token-cheap mode (ids + topology only). Graph bodies always resolve from the SoT
(§7.5) — the graph store carries no body text. **Every hop is tenant-filtered (§3.5, F4):** `start` ownership
is necessary but not sufficient; nodes outside the caller's floor are pruned as if the edge did not exist, so
entity nodes keyed on shared strings cannot bridge tenants.

**Supersession chains do not degrade silently (F9).** The graph projection lags (edge writes are async,
§5.4), so a plain `freshness_lag` warning on a `supersedes`-edge walk could hand back a *superseded* fact as
the chain head — turning a correctness error (agent acts on a reversed decision) into a benign-looking lag.
For traversals over the `supersedes` edge, the server therefore **resolves the collapse head from the SoT**
(authoritative latest-wins, §5.2) rather than trusting graph topology; if it cannot, it returns a
**supersession-specific** `supersession_stale` flag (distinct from the generic `freshness_lag`) that explicitly
means "a newer version of this exact fact may exist." A generic lag warning is never sufficient for a
supersession query.

### 4.4 `memory.search` → `POST /memory/search` (fact tier)

```jsonc
{
  "name": "memory.search",
  "inputSchema": {
    "query":  "<text>",           // REQUIRED — the thing to find facts similar to
    "scope":  { "project": "...", "issue": "...", "agent": "...", "type": "...", "category": "..." },
              // optional; ANDed; intersect-only within the tenant floor (§3). Omit → whole tenant.
    "k":      10,                 // §6 default/cap
    "as_of":  "<RFC3339>",        // optional; scopes candidate set to facts known by then
    "view":   "compact|full",     // §6 — default compact
    "body_max_chars": 500,        // §6 truncation
    "cursor": "<opaque>"
  }
}
// → { "hits": [ { <fact shape §7.5>, "score", "via": "fact" } ],
//     "next_cursor": "...", "token_estimate": 1234 }
```

Fact tier only (`tiers=fact`). Returns ranked raw facts, hydrated from SoT (§7.4 step 4). Returns
`feature_disabled` (§7) when `memory.semantic` is off — never a silent empty result.

### 4.5 `memory.recall` → `POST /memory/search` (digest tier)

```jsonc
{
  "name": "memory.recall",
  "inputSchema": {
    "scope":  { "thread": "...", "run": "...", "entity": "...", "project": "..." },  // REQUIRED — the span to recall
    "query":  "<text>",           // optional — narrows within the scope's digests
    "k":      5,                  // digests are coarse; small k by default
    "as_of":  "<RFC3339>",        // optional
    "resolve": "snippets|ids|full", // §6 — default snippets: digest summary + member-fact snippets, not full bodies
    "cursor": "<opaque>"
  }
}
// → { "digests": [ { "digest_id", "scope", "scope_id", "summary", "score",
//                    "members": [ { "fact_id", "type", "subject", "snippet"? } ] } ],
//     "next_cursor": "...", "token_estimate": 1234 }
```

Digest tier (`tiers=digest`). **R1 invariant at the MCP layer:** every digest carries its `members` (resolved
`member_fact_ids`) so the summary is always a pointer into ground truth; the agent pulls full bodies
selectively via `memory.get`. `resolve` controls how much comes back (§6).

**The digest-disabled degrade has a defined shape and a queryless fallback (F10).** When
`memory.semantic.digest` is off, `recall` does **not** silently swap to a `hits[]` fact-tier body — an agent
parsing `response.digests` would read an empty array and wrongly conclude "nothing happened in this span."
Instead the degraded response **keeps the `digests: []` envelope** and adds a top-level `facts: [ <fact shape
§7.5> ]` array plus `degraded: "digest_disabled"`, so the shape is self-describing. The fallback source
depends on whether a query is present:
- *queryless recall* (the common scope-centric case — `query` omitted): fact-tier ANN has no similarity axis,
  so the server falls back to a **keyed scope fetch** (the §4.2 `memory.get` path over the same `scope`),
  **never** fact-tier ANN. An existing span always reads back as its facts, never as empty.
- *recall with a `query`*: may use fact-tier semantic search scoped to the span, returning the ranked facts in
  the same `facts[]` field.

When the whole `memory.semantic` module is off, `recall` returns `feature_disabled` (§7) — never a silent
empty result.

## 5. Skill descriptions (the natural-language the agent reads)

These are the verbatim `description` strings shipped with each tool. They are the *skills* — the only thing an
MCP client sees when deciding which tool to call. They are written to **teach the keyed-first discipline** and
the token-budget posture, not merely to name the endpoint.

- **`memory.write`** — *"Record a fact you just produced — a decision, a run outcome, an observation from an
  exchange, a learning, or an end-of-session handoff note. Send the raw fact and the one id you naturally know
  (your run id, or the thread id for an exchange); the server fills in the rest. Write freely at natural
  boundaries — the server de-duplicates retries."*
- **`memory.get`** — *"**Your first choice for reading.** When you know what a fact is about — an issue, PR,
  run, agent, project, or thread — fetch everything recorded about it directly by that key. Deterministic,
  cheap, no guessing. Use this to pick up context on an issue, read a prior agent's handoff, or get a run's
  outcome. Reach for search/recall only when you *don't* have a key."*
- **`memory.traverse`** — *"Follow the connections between facts: what superseded this decision, what a given
  agent decided across a sprint, what entities a fact touches. Use when you need the *shape* of history — chains
  and relationships — not a single lookup. Ask for topology only (cheap) when you just need the structure."*
- **`memory.search`** — *"Find facts similar to a description when you have no key — 'has anyone hit a
  NetworkPolicy blackhole like this before?'. Scoped to what you're allowed to see; add a project/issue/type to
  narrow. Precision tier: returns the specific facts that match. Prefer memory.get when you actually know the
  key."*
- **`memory.recall`** — *"Get the story of a span — everything that happened in this thread, run, or entity —
  as a short digest that points you to the underlying facts. Use at the start of picking up work to orient fast
  when a span is too large to read fact-by-fact; if you already have one clean key (an issue, PR, or run) and
  just need its records, memory.get is cheaper and comes first. You get summaries plus pointers; pull full facts
  with memory.get only for the ones you need."*

> **PM co-ownership hook (§Collaboration):** these strings are the consumer-facing surface the PM co-owns. They
> are versioned with the tool schema; a wording change that alters *when* an agent calls a tool is a contract
> change, not a copy edit, and goes through PM + Challenger review.

## 6. Pagination + token-budget-friendly response shaping

The two concerns are coupled: an MCP result lands in a context window, so the contract must let an agent bound
both *how many* records and *how many tokens* it pulls.

### 6.1 Pagination — opaque cursor, snapshot-stable

- **Cursor-based, never offset.** The SoT is append-only, bi-temporal, and supersession-collapsed at read time
  (§5.1); offset pagination would be unstable as new facts arrive and as latest-wins shifts. The cursor is an
  **opaque, server-signed token** (HMAC/AEAD under a server key) encoding `(as_of snapshot pin, last sort key,
  tier/fusion state, page size)` — **plus query intent, never a pre-authorized id set.**
- **Integrity-protected, not merely opaque (F2).** "Opaque" is obscurity, not tamper-resistance: a plain
  base64 blob can be decoded, widened, and re-encoded. The cursor is therefore **cryptographically sealed**;
  any client edit fails verification → `bad_cursor` (§7). This is what makes the "replay against a different
  tool / tenant → `bad_cursor`" rule actually enforceable.
- **The tenant floor is re-derived and re-intersected on every page — the cursor never carries authority (F3).**
  Authority changes (scope revoked/narrowed between pages) are **not** writes, so the `as_of` data-snapshot pin
  does not freeze them. On each page the server re-derives the floor from *that page's* token (HTTP re-verifies
  per request, §2.2) and re-intersects it with the query. The cursor carries only *what was being asked*
  (query + sort position), so a principal whose scope narrows mid-walk immediately stops draining results it is
  no longer entitled to. Because the floor is re-derived rather than cursor-carried, a forged floor is not a
  reachable state; the seal (F2) protects query intent and sort position.
- **`as_of` must not be in the future (F13).** A future `as_of` would let facts ingested *after* page 1 satisfy
  the `ingested_at <= as_of` filter (§5.3), growing the "stable snapshot" across pages and defeating the one
  property pagination promises. The server rejects a future `as_of` as `invalid_argument` (§7) — or clamps to
  request time; it never pins a future snapshot.
- **`as_of` is pinned into the cursor** on the first page, so paging through a large result is a stable
  point-in-time *data* snapshot even while writes continue. (A caller who wants to see newer facts starts a
  fresh query, not a continued page.)
- `next_cursor` is present when more results exist, **absent when exhausted** — the absence *is* the
  end-of-results signal. A cursor is opaque and single-query-scoped; replaying it against a different tool or
  tenant is a `bad_cursor` error (§7).
- Default page size 50 (`get`/`traverse`), `k` 10 (`search`) / 5 (`recall`); server hard-caps each so a single
  call cannot request an unbounded page.

### 6.2 Response shaping — four knobs, all server-enforced

1. **`view: compact|full`** (default **compact**). Compact returns `{fact_id, type, subject, body_snippet,
   event_time, entity_keys}`; full adds the whole body + `confidence` + `provenance`. Compact is the default
   for every list/search op because the common case is *triage then selectively deepen*.
2. **`body_max_chars` + `truncated` flag.** Bodies are truncated to the limit; a truncated record carries
   `"truncated": true` and its `fact_id` so the agent fetches the full body via `memory.get` only if it needs
   it. The *agent*, not the server, decides which bodies are worth full tokens.
3. **`fields` projection allowlist.** A caller can request only the fields it will use; the server drops the
   rest before serialization.
4. **Hard server ceiling on bytes-per-response.** Independent of page size, the server caps total body bytes in
   a single response and pages the overflow via `next_cursor`. This is the backstop that guarantees one call
   can never blow a context window regardless of how the caller set the other knobs. **PM ruling (ISI-5879):
   default ceiling 64 KB of body bytes per response (≈16k tokens at ~4 chars/token), deployment-configurable.**
   Enforced on bytes (deterministic server-side), **not** on the `token_estimate` approximation — the byte
   figure is chosen so its token-equivalent stays a safe fraction of even a modest 32–64k context window,
   leaving room for the agent's own working context. Deployments on large-context fleets may raise it; overflow
   always pages via `next_cursor`, and `token_estimate` remains the agent-facing budgeting signal.

Every response carries **`token_estimate`** (approx tokens in this payload) so an agent can budget the next
call. `recall` additionally defaults to `resolve: snippets` (§4.5) — the single biggest token win, since the
digest + member snippets replace reading every full fact in a span.

## 7. Error taxonomy

MCP tool errors surface as a JSON-RPC error with a structured `data` payload: `{ "code": "<taxonomy code>",
"retriable": <bool>, "detail": "<human string>", "hint": "<what to do>" }`. The taxonomy is stable and
deployment-independent.

| Code | Meaning | HTTP origin | Retriable | Notes |
|---|---|---|---|---|
| `unauthenticated` | No/invalid/expired token. | 401 | no (re-auth) | HTTP: refresh token. stdio: subprocess relaunch. |
| `forbidden` | Authenticated but the action itself is not allowed by deployment policy (e.g. write to a read-only instance, a disabled tool). | 403 | no | Never used for cross-tenant ids — those are `not_found` (§3). **Not** used for a bad `source.kind` either — that is `invalid_envelope` (F16). |
| `not_found` | Id not found *within the caller's tenant* — includes cross-tenant `entity.*`/`fact.id`/node ids **and a `supersedes` target the caller's tenant does not own** (fail-closed, §3.2–3.3, F7). | 404 | no | Does not leak existence across tenant. Boundary-crossing `not_found`s are audited and rate-coupled (§3.3, F15). |
| `invalid_envelope` | Write failed schema/anchor/size validation: missing anchor, `supersedes` without `subject`, supersede **grain** mismatch, **unknown `source.kind`** (F16, the single home for it — it gates the §3.1 anchor-required row), **`fact.body` over `max_body_bytes` or `attachments[]` over `max_attachments`** (F14). | 422 | no | `detail` names the field; `rejected_hints` in the write response for soft key failures. (Supersede target *not owned* is `not_found`, not this — F7.) |
| `invalid_argument` | Read arg malformed (no `entity.*` on `get`, bad `as_of`, **`as_of` in the future** (F13), depth over cap). | 400 | no | — |
| `bad_cursor` | Cursor signature-verify failed (tamper) / wrong tool / wrong tenant / expired snapshot. | 400 | no | Cursor is server-signed (§6.1, F2); any client edit fails here. Restart the query. |
| `feature_disabled` | Semantic module off for `search`; whole semantic module off for `recall`. | 501 | no | **Not** a silent empty result — lets callers degrade. Digest-off `recall` *degrades* with a defined `digests:[]`+`facts:[]` shape and a warning instead (§4.5, F10), not this error. |
| `rate_limited` / `budget_exceeded` | Caller over its request or token budget, **or over the boundary-crossing-`not_found` probe threshold** (§3.3, F15). | 429 | yes (backoff) | `detail` carries retry-after when known. |
| `backend_unavailable` | SoT (Grail) write/read path down. | 503 | yes | Only the SoT path. A **derived-view lag is never an error** — keyed reads still serve; a stale graph/vector returns results with a `freshness_lag` warning (§5.4 invariant 2) — **except a `supersedes`-chain walk, which resolves the head from the SoT or returns `supersession_stale`, never a bare lag (§4.3, F9).** A **partial** SoT hydrate (some ids serve, some 503) returns the partial set with `incomplete: true` + `unresolved_count`, **never a silent short set** that reads as "no match" (F11). |
| `conflict` | Idempotency/version conflict on write. | 409 | no | Returns the existing `fact_id`; the retry already succeeded. |

**Design rule — degrade, don't fail, wherever the SoT can still answer.** The only hard failures are auth,
validation, and SoT-down. Everything a derived view would have added (semantic hit, fresh graph edge) degrades
to a warning on an otherwise-successful keyed response. This mirrors the pipeline's "a projection outage never
blocks keyed reads" invariant (§6) at the contract edge.

## 8. What the first MCP milestone proves

Wrap the §8 keyed roundtrip in MCP tools end to end, over **both** transports:

1. Agent A calls `memory.write` (stdio, run-token principal) with a `gateway_flush` envelope carrying only its
   run id → server enriches to issue/project, returns `fact_id` + resolved keys.
2. Agent B calls `memory.get` (Streamable HTTP, per-request bearer) `entity.issue=X&valid=current` → receives
   A's handoff context, compact view, with `token_estimate` — **zero semantic involvement**.
3. A cross-tenant `memory.get` for the same issue id from a *different* tenant's token → `not_found`
   (fail-closed, §3), proving the floor.
4. A retried `memory.write` with the same derivation → `conflict` returning the same `fact_id`, no duplicate.

Deliberately **not** in the first milestone: the digest-tier `recall` path (needs WS-3 summarizer), RRF tuning,
and cross-backend portability of `search` — all build on the keyed roundtrip, same ordering rationale as §8 of
the design.

## 9. Decision log + open items (reconcile with WS-4, PM, Challenger)

**Decisions (this spec):**

1. **Five tools, thin wrappers over §7.** No new storage/modeling at the MCP layer; tools are transport +
   ergonomics + descriptions only.
2. **`search`/`recall` split one §7.4 endpoint into two tools** by tier (fact=precision / digest=recall), for
   distinct skill descriptions and defaults.
3. **Two transports, identical surface:** stdio (single-principal, launch-bound) + Streamable HTTP
   (per-request auth, multi-tenant).
4. **R6 floor is MCP-enforced:** scope args are intersect-only within a server-derived tenant floor; no
   argument can widen; cross-tenant ids fail closed as `not_found`; write identity bound to principal.
5. **Cursor pagination with `as_of` pinned into the cursor** (snapshot-stable over an append-only SoT); no
   offset pagination.
6. **Token-budget shaping is first-class:** compact default view, `body_max_chars`+`truncated`, `fields`
   projection, a hard server bytes ceiling, `token_estimate` on every response, and `recall`'s snippet-default.
7. **Degrade-don't-fail:** only auth/validation/SoT-down are hard errors; derived-view gaps degrade to warnings
   on a successful keyed response — **except supersession-chain staleness, which is never a bare lag** (F9).
8. **Isolation is mechanism, not assertion (Challenger fold-in, ISI-5878):** principal-salted `fact_id`
   derivation (F1/F8), signed cursors with per-page floor re-derivation (F2/F3), tenant-partitioned
   vector/digest indexes (F5/F6), per-hop traverse filtering (F4), owned-only supersede targets (F7),
   stdio token-lifetime bound (F12), and write-side size caps (F14) are all first-class contract requirements,
   not inherited hopes. See the fold-in log below.

**Open items — reconcile before finalize (this is the WS-4 "finalized after" dependency):**

- **[WS-4]** Field names and exact request/response JSON of the four ops are WS-4's deliverable. This spec
  tracks §7 as of rev 3; any WS-4 rename/reshape propagates verbatim into §4 here. **No MCP field is
  independent of WS-4** — the MCP schema is a projection of the HTTP schema.
- **[WS-4]** RRF tier weights and the `tiers=auto` fusion default live in WS-4 / §7.4; `search`/`recall`
  inherit them. If WS-4 exposes a per-call weight override, decide whether the MCP layer surfaces it or pins it
  (lean: **pin it** — a per-call ranking knob is not an agent-facing concern).
- **[PM — RATIFIED 2026-10-14, ISI-5879]** Five skill-description strings (§5) ratified. Keyed-first framing
  approved as strong enough (`get`="first choice", explicit deflections). One change applied: `memory.recall`
  gained a get-deflection clause (it overlapped `get`'s "pick up context" job and previously had no tiebreaker
  toward the cheap keyed path). That clause alters *when* `recall` is called → **contract change, needs
  Challenger ack** (routed to ISI-5878) before finalize; the rest are ratified as-is.
- **[PM — RATIFIED 2026-10-14, ISI-5879]** Pagination defaults (page 50, k 10/5), compact-default view, and
  `token_estimate`-on-every-response all approved. Body-bytes ceiling set: **64 KB default (≈16k tokens),
  deployment-configurable** (§6.2.4) — enforced on bytes, documented in token-equivalent.
- **[Challenger — COMPLETE 2026-10-14, ISI-5878]** Red-team done: 16 findings (4 critical). All folded into
  §2–§7 above; see the **Challenger fold-in log** below for the per-finding resolution. The two seed questions
  (tenant-floor attack surface; `conflict`-on-retry confused-deputy) resolved as F1–F7: the real
  confused-deputy vector was `supersedes` targeting (F7), not a `fact.id` retry (write has no `fact.id` input),
  and the tenant-floor claims were tightened from assertion to enforceable mechanism (principal-salted ids,
  signed cursors with per-page floor re-derivation, partitioned vector/digest indexes, per-hop traverse
  filter).
- **[board/product]** stdio single-principal assumption: confirm no local deployment needs one subprocess to
  serve multiple tenants (the design assumes not; a shared local gateway might).

### Challenger fold-in log (ISI-5878 → this spec)

Every finding resolved in-spec; nothing deferred except the two WS-4 reconciliations called out below. Resolutions
are the spec owner's call (Winston); the Challenger filed descriptions only.

| # | Sev | Finding (one line) | Resolution in this spec |
|---|---|---|---|
| F1 | CRIT | `fact_id` derivation inputs unspecified → cross-tenant existence oracle + silent write-loss | §3: derivation is **principal-salted** (`tenant_id`+`agent_id` mandatory) & body-complete → cross-principal collision impossible. **↳ WS-4 Invariant T.** |
| F2 | HIGH | "Opaque" cursor ≠ tamper-proof; wrong-tenant check unimplementable | §6.1: cursor is **server-signed** (seal → `bad_cursor` on edit); carries query intent only. |
| F3 | HIGH | Floor not re-derived per page → stale broad authority mid-walk | §6.1: floor **re-derived + re-intersected every page**; cursor never carries a pre-authorized id set. |
| F4 | MED/HIGH | `traverse` guards only `start`, not per-hop | §3.5 + §4.3: per-hop `node.tenant == caller` is a **first-class MCP invariant**. |
| F5 | CRIT | "Pre-ANN pre-filter" assumes a backend neither doc requires; vector record lacks `tenant` | §3.4: **require tenant-partitioned index or hard pre-filter**; vector record MUST carry `tenant_id`. **↳ WS-4 (§5.4.2 record shape).** |
| F6 | CRIT | Digest `scope_id` not tenant-qualified → `summary` leaks cross-tenant | §3.4: digests **tenant-partitioned at rollup**; `(scope, scope_id)` always tenant-qualified; summarizer never crosses floor. **↳ WS-3 summarizer.** |
| F7 | CRIT | `supersedes` target ownership unchecked (the real confused-deputy) | §3.2 + §4.1 + §7: target **MUST be tenant-owned** else `not_found`; cross-agent-within-tenant allowed & attributed. **↳ WS-4 Invariant T.** |
| F8 | HIGH | Derivation undefined on body axis → fork-or-swallow | §3 + §4.1: body **in** derivation; identical retry → `conflict`(own id), content change → new fact via latest-wins. |
| F9 | HIGH | Stale graph masks a *missing supersession* behind generic lag | §4.3 + §7: supersede-chain walk resolves head from **SoT** or returns `supersession_stale` (not bare `freshness_lag`). |
| F10 | MED/HIGH | `recall` digest-disabled degrade changes shape; queryless undefined | §4.5: keeps `digests:[]`+adds `facts:[]`+`degraded`; **queryless → keyed scope fetch**, never fact-tier ANN. |
| F11 | MED | Partial SoT hydrate masks missing facts as "no match" | §7: partial hydrate returns partial + `incomplete:true`+`unresolved_count`, never a silent short set. |
| F12 | HIGH | stdio principal never re-validated → serves expired authority | §2.1: server **stops serving when the token validity window closes** (self-terminate / `unauthenticated`). |
| F13 | MED | No `as_of <= now` check → breaks snapshot stability | §6.1 + §7: future `as_of` rejected `invalid_argument` (or clamped to request time). |
| F14 | MED | Writes unbounded (body/attachments) | §4.1 + §7: `max_body_bytes` / `max_attachments` → `invalid_envelope`. |
| F15 | LOW/MED | Fail-closed `not_found` is an unlogged probing oracle | §3.3: boundary-crossing `not_found` is **audited and rate-coupled**. |
| F16 | LOW | Bad `source.kind` classified under three codes | §7: pinned to **`invalid_envelope`** (it gates the §3.1 anchor row). |

**Two findings also cross into WS-4 (ISI-5869, Invariant T) and WS-3 — not closable in the MCP layer alone:**

- **F1 + F7 → WS-4 Invariant T (ISI-5869):** the principal-salt in the `fact_id` derivation and the
  supersede-target ownership check are properties of the *write/identity contract*, not just its MCP face. The
  MCP spec states them; WS-4 must make them Invariant-T-level guarantees so every surface (not just MCP)
  inherits them. **RECONCILED (ISI-5889, 2026-10-14):** Invariant T now formally defined in
  `architecture.md` §7.6 (T1 principal-salted/body-complete `fact.id` at §6 stage 1; T2 fail-closed
  supersede-ownership at §6 stage 3); §9 decision-log entry 13.
- **F5 + F6 → storage/pipeline (WS-4 §5.4.2 vector record + WS-3 summarizer):** the vector record must carry
  `tenant_id` and the digest rollup must be tenant-partitioned. The MCP contract *requires* these; the
  owning docs must reflect them or the MCP isolation guarantee is unbacked. **RECONCILED (ISI-5889,
  2026-10-14):** fact-vector record carries mandatory `tenant_id` + tenant-partitioned index
  (`architecture.md` §5.4.2, §6 stage 5b); digest rollup tenant-qualified `(tenant_id, scope, scope_id)`
  (§5.4.2, §6 stage 5c) and mirrored in the WS-3 summarizer contract (ADR-llm-pipeline Decision 2b).
