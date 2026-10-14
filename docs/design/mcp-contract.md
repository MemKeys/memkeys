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

**The tenant floor is server-derived and non-negotiable:**

1. **No tool argument can widen scope beyond the caller's tenant.** `scope`/`entity` arguments on any read tool
   are **intersect-only** — they narrow *within* the tenant floor the server already knows. There is no
   "global" value, no wildcard tenant, no argument the client can set to escape its tenant. A call that omits
   all finer keys runs against *everything the caller's tenant owns* (including `unkeyed` and sparsely-keyed
   facts, closing the R6 recall hole) — but still only that tenant.
2. **Write identity is bound to the principal.** On `memory.write`, `source.producer` and `entity.agent` are
   overwritten from the verified principal (§3.1 of the design). A client-asserted `agent` is advisory and
   discarded on conflict; `provenance` in the envelope is rejected-if-populated. The MCP tool does not expose
   any way to write a fact *as another agent*.
3. **Cross-tenant ids fail closed.** If a `memory.get` / `memory.traverse` call names an `entity.*` id or a
   `fact.id` / node id the caller's tenant does not own, the server returns `not_found` (§7) — **never** a
   `forbidden` that would leak the id's existence across the tenant boundary.
4. **Tenant scoping is applied before the ANN pass** for `search`/`recall` (§5.4.2 candidate pre-filter) — team
   A's vectors are never in team B's candidate set, so isolation holds even under semantic recall.

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
                    "type": "<decision|outcome|observation|learning|context>",  // optional, server-defaulted
                    "subject": "<string>",            // REQUIRED when `supersedes` is set
                    "event_time": "<RFC3339>",        // optional
                    "confidence": 0.0,                // optional; SHOULD set for type=learning
                    "supersedes": "<prior fact.id>" },// optional
    "attachments": [ "<ref>" ]                        // optional; refs only, never inlined
    // server-derived (NOT in schema): source.producer, entity.agent, provenance.*
  }
}
// → { "fact_id": "...", "keyed": true, "entity_keys_resolved": { ... }, "rejected_hints": [ ... ] }
```

Idempotent on the server-assigned `fact_id` derivation (retried flush = no duplicate, §6 stage 1). `write`
returns the resolved keys so a caller can cache them for subsequent `memory.get` without a lookup round-trip.

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
(§7.5) — the graph store carries no body text.

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
selectively via `memory.get`. `resolve` controls how much comes back (§6). When `memory.semantic.digest` is
off, `recall` **degrades to a fact-tier scope recall with a `degraded: "digest_disabled"` warning** (per §7.4),
not a hard error; when the whole `memory.semantic` module is off, it returns `feature_disabled`.

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
  **opaque token** encoding `(as_of snapshot pin, last sort key, tier/fusion state, page size)`.
- **`as_of` is pinned into the cursor** on the first page, so paging through a large result is a stable
  point-in-time snapshot even while writes continue. (A caller who wants to see newer facts starts a fresh
  query, not a continued page.)
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
| `forbidden` | Authenticated but action not allowed (e.g. write with a bad `source.kind`). | 403 | no | Never used for cross-tenant ids — those are `not_found` (§3). |
| `not_found` | Id not found *within the caller's tenant* (includes cross-tenant ids — fail-closed, §3). | 404 | no | Does not leak existence across tenant. |
| `invalid_envelope` | Write failed schema/anchor validation (missing anchor, `supersedes` without `subject`, supersedes target mismatch). | 422 | no | `detail` names the field; `rejected_hints` in the write response for soft key failures. |
| `invalid_argument` | Read arg malformed (no `entity.*` on `get`, bad `as_of`, depth over cap). | 400 | no | — |
| `bad_cursor` | Cursor opaque-decode failed / wrong tool / wrong tenant / expired snapshot. | 400 | no | Restart the query. |
| `feature_disabled` | Semantic module off for `search`; whole semantic module off for `recall`. | 501 | no | **Not** a silent empty result — lets callers degrade. Digest-off `recall` *degrades* with a warning instead (§4.5), not this error. |
| `rate_limited` / `budget_exceeded` | Caller over its request or token budget. | 429 | yes (backoff) | `detail` carries retry-after when known. |
| `backend_unavailable` | SoT (Grail) write/read path down. | 503 | yes | Only the SoT path. A **derived-view lag is never an error** — keyed reads still serve; a stale graph/vector returns results with a `freshness_lag` warning, not a failure (§5.4 invariant 2). |
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
   on a successful keyed response.

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
- **[Challenger]** Red-team the tenant floor (§3): can any combination of `scope`/`entity`/`as_of`/cursor
  arguments, across transports, surface a fact the caller's tenant doesn't own? The `not_found`-not-`forbidden`
  rule and the pre-ANN pre-filter are the two load-bearing claims to attack.
- **[Challenger]** Is `conflict`-returns-existing-`fact_id` on retry safe against a confused-deputy write (A
  retries B's fact id)? Validate against §3.1 identity binding.
- **[board/product]** stdio single-principal assumption: confirm no local deployment needs one subprocess to
  serve multiple tenants (the design assumes not; a shared local gateway might).
