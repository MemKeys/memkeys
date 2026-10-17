# WS-3 / ISI-5914 — T3 live-head CAS gate + T4 A2A resolution: implementation notes

Status: **implemented** (realization (b)); `entity_identity`/`H` composition
**ratified by the Architect (ISI-6131, §7.6 T3-pin)**:
`H = (tenant_id, {project,issue,pr,thread,service}, subject, type)` — `agent`
and `run` excluded (`run` is lifted by §4 enrichment into project/issue/pr
before `H`; `thread` stays as the durable a2a anchor-of-record); an empty
composite ⇒ unkeyed-for-`H` ⇒ gate bypassed. Q2: stage-3 grain is unified on
`H`-equality (the "shares ≥1 anchor" rule is retired as authoritative and
remains only as an early pre-filter). Q3: realization (b); `fact.id` ⊥ `H`
(M1's shipped `fact.id` is unchanged — it already meets the T1 security floor).
Owner: WS-3 pipeline. Reference contract: `docs/architecture.md`
§7.6 (Invariant T), §6 stage 3–4, §5.1 — as pinned on
`isi-5906/live-head-uniqueness` @ `583082a` and polished by ISI-6053
(`8d6d165`: *H excludes `entity.agent`*).

## 0. What landed (this branch)

- `migrations/0002_live_head.sql` — immutable `entity_identity` column +
  head-lookup index (no `superseded` flag, no write-back).
- `pg.rs` — `commit_fact` rewritten to the realization-(b) gate: one
  transaction, `pg_advisory_xact_lock(hash(H))`, idempotency short-circuit
  under the lock, current-head CAS over the `supersedes` chain, auto-supersede
  on plain writes, `conflict` on a stale/foreign explicit `supersedes`. Plus
  `entity_identity()` / `live_head_lock_key()` helpers and 6 DB-free unit tests.
- `tests/integration.rs` — fork-prevention, stale-supersede-conflict, and
  subject-null-coexist tests (TEST_DATABASE_URL-gated; CI runs them on PG 18).

Verified by CI on the PR (fmt + clippy `-D warnings` + `cargo test --workspace`
against a Postgres service) — this environment has no local Rust toolchain/PG.
The **only** open item is Architect ratification of the §3 `H` composition.

This note is the handoff artifact for the T3 pipeline change. It records the
chosen realization and exact write-path logic (ready to code), and the single
blocking question that must be resolved first because the shipped M1 write
pipeline diverged from the §7.6 T1 identity model.

---

## 1. What is already in place (M1, merged to `main`)

- Append-only `facts` table, bi-temporal, keyed dimensions as columns
  (`migrations/0001_factstore.sql`). Supersession is **read-time collapse** via
  the `supersedes` chain: `fetch_by_keys` with `valid=current` drops any `f`
  for which `∃ s: s.supersedes = f.id AND s.tenant_id = f.tenant_id`
  (`pg.rs`, the `Validity::Current` branch). This already works and agrees with
  realization (b) below — **no read change is needed**.
- Stage 3 (`service.rs`) validates an **explicit** `supersedes`:
  tenant-scoped `get_fact` (T2 tenant-ownership, fail-closed `not_found`) then
  `validate_supersedes_target` — subject match, type match, and *shares ≥1
  anchor* entity key.
- `commit_fact` (`pg.rs`) is a single idempotent `INSERT … ON CONFLICT (id) DO
  NOTHING`; same-id/same-body → `Duplicate`, same-id/diff-body → `Conflict`.

**Not yet implemented (the T3 gap):** live-head *uniqueness*. Two plain writes
to the same statement each have `supersedes = None`, both pass stage 3 (the
`supersedes` check never fires), and both land live — a **fork** that read-time
collapse cannot repair (neither names the other). There is also no
"explicit `supersedes` must name the **current** head" check, so superseding a
stale/already-superseded id is silently accepted.

## 2. Chosen realization — (b) per-key write serialization, not the index flag

§7.6 T3 offers two backend-portable realizations:
(a) a Postgres partial-unique index `UNIQUE (tenant_id, entity_identity,
subject, type) WHERE NOT superseded`; or (b) key-level write serialization on
`H` with a compare-and-set on the current head id.

**Pick (b).** Rationale:

- Realization (a) needs a `superseded BOOLEAN` column that is **flipped on the
  prior head** when a new head lands — a write-back to a prior record. §6
  stage 3 (ISI-6053 R2) states "**No write-back to prior records —
  supersession is read-time**". (a) is in tension with that line and with the
  append-only SoT framing; (b) honors it.
- (b) needs **no migration** and **no new column**: it reuses the existing
  `supersedes` chain for liveness (which the read path already collapses on)
  and a Postgres transaction-scoped advisory lock for the critical section.
- Blast radius is one function (`commit_fact`) plus a small stage-3 contract
  note. Lower risk for a correctness-critical concurrency change.

### 2.1 Exact write-path logic (ready to code in `commit_fact`)

For a **subject-bearing, keyed** fact (subject-null or unkeyed → unchanged
idempotent insert path; they legitimately share `(entity, null, type)`):

```
BEGIN;                                              -- the stage 3–4 critical section
  -- Serialize all writers on this statement's live-head key H.
  SELECT pg_advisory_xact_lock(hashtext($H_canonical));   -- $H_canonical = canonical(H)

  -- Idempotency first (unchanged semantics): same id already present?
  --   same body_hash  -> COMMIT; return Duplicate
  --   diff body_hash  -> ROLLBACK; return Conflict{existing_id: fact.id}

  -- Current head of H = the record on H with no successor (≤1 under the lock):
  --   SELECT f.id FROM facts f
  --   WHERE f.tenant_id=$t AND <entity_identity(f)=entity_identity(fact)>
  --     AND f.subject=$s AND f.fact_type=$ty
  --     AND NOT EXISTS (SELECT 1 FROM facts x
  --                     WHERE x.supersedes=f.id AND x.tenant_id=f.tenant_id)
  --   FOR UPDATE;
  let head = <above>;

  let effective_supersedes = match (fact.supersedes, head) {
      (Some(explicit), Some(h)) if explicit == h   => Some(h),      -- names current head: OK
      (Some(_),        _)                           => CONFLICT,     -- stale/foreign/no head: reject
      (None,           Some(h))                     => Some(h),      -- plain write: AUTO-SUPERSEDE head
      (None,           None)                        => None,         -- first head on H
  };

  INSERT the new fact with supersedes = effective_supersedes;       -- the SoT write (stage 4)
COMMIT;
```

Properties: **no forks** (the advisory lock serializes concurrent writers to
`H`; the second observes the first's head and supersedes it — one live head
survives); **no cycles** (`supersedes` only ever advances from the current
head, so the chain is a strict line); **no prior-record write-back**
(liveness stays read-time). `effective_supersedes == CONFLICT` returns
`IngestError::Conflict{existing_id: current_head}` so the caller learns the
real head.

The advisory-lock key is `hashtext(canonical(H))`; `canonical(H)` is a
delimiter-joined, length-prefixed serialization of `(tenant_id,
entity_identity, subject, fact_type)` to avoid the lock-key collisions a naive
concatenation would allow. Stage-3 `validate_supersedes_target` stays as
defense-in-depth for the grain/error-message path; the current-head decision
is authoritative and lives in the locked section.

## 3. The blocker — `entity_identity` (`H`) is undefined against shipped M1

§7.6 T1 defines `fact.id` as a hash over `(tenant_id, agent_id,
entity-identity-keys, type, subject, body)`, and `H` reuses that
*entity-identity-keys* composite minus `entity.agent` (ISI-6053 R1). **The
shipped M1 pipeline does not implement that model.** `pipeline.rs::normalize`
derives:

```
fact.id = UUIDv5(NAMESPACE_URL, "{tenant_id}|{principal}|{idem_input}")
idem_input = caller idempotency_key, else "body:" + sha256(body ++ event_time ++ first_anchor)
```

So the shipped id encodes **neither** `type`, `subject`, nor an
entity-identity-keys composite — only `tenant`, `principal(=agent)`, and a
single `first_anchor` (and only when no idempotency_key is supplied).
`entity_identity` therefore has **no operational definition** in the code T3
must extend, and three sub-questions are unresolved:

- **Q1 — composition.** Which of `{project, issue, pr, run, thread, service}`
  (agent excluded) compose `entity_identity`? The existing integration test
  `supersession_collapses_at_read_time` supersedes **across `run r1 → r2`
  sharing only `issue`**. `first_anchor` for `gateway_flush` is `run`, so *any*
  `H` built from the single resolved anchor — or from a full composite that
  includes `run` — makes v1 and v2 land on **different `H`**, which both
  breaks that test (explicit supersedes would become cross-`H` → Conflict) and
  fails to model the intended "latest-wins on the same statement". `H` must be
  pinned to the subset that makes same-`issue`, same-`subject`, same-`type`
  the same statement regardless of incidental `run`.
- **Q2 — grain reconciliation.** Stage 3 today uses *shares ≥1 anchor*
  (looser than `entity_identity` equality). Under the gate, an explicit
  `supersedes` must name the current head of the **new fact's `H`**. If grain
  stays "shares ≥1 anchor" but `H` is a composite, a `supersedes` that shares
  `issue` yet differs on `run` is grain-valid but cross-`H` — the two rules
  contradict. They must be unified on the pinned `H`.
- **Q3 — identity model.** Does T3 land as realization (b) keyed on the pinned
  `H` while M1's idempotency-key-based `fact.id` stays as-is (recommended —
  the two are orthogonal: `fact.id` = retry idempotency, `H` = statement
  identity), or must T1's identity-hash `fact.id` be adopted first? If the
  former, confirm `fact.id` and `H` are explicitly decoupled in §7.6.

Guessing `H` would either break the shipped test (Q1) or silently fail to
prevent forks. This is an architecture/owner decision, not an implementation
choice.

## 4. T4 (A2A resolution) status

Largely satisfied at the SoT layer: `counterpart_agent` is a column, stamped
from the server-corroborated value (hint stays a hint). The append-only
`a2a_counterpart_resolution {fact_id, counterpart_agent, tenant_id,
resolved_at, basis}` record for *later* corroboration, and the §6 stage-5a
`a2a_participant` edge built from the persisted value only, are **graph-tier
(M2 / ISI-6076)** work — out of scope for this write-path change. No blocker
here; tracked with M2.

## 5. Test matrix (write once `H` is pinned; integration-gated on `TEST_DATABASE_URL`)

1. **Fork prevention.** Two plain writes, same `H`, different body → read
   `valid=current` returns exactly one; the later body wins; `valid=all`
   returns both.
2. **Explicit current-head supersede** → accepted; one live head.
3. **Explicit stale supersede** (name an already-superseded id) → `Conflict`.
4. **Cross-tenant supersede target** → `not_found` (T2, existence-opaque).
5. **Subject-null** two writes same `(entity, null, type)` → both live (gate
   skipped).
6. **Concurrency** (serial proxy for the advisory lock): interleaved writes to
   one `H` never leave two live heads.
7. **Regression:** `supersession_collapses_at_read_time` still passes under the
   pinned `H` (this is the Q1 acceptance check).
