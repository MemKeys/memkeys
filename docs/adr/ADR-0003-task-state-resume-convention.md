# ADR-0003: Resume support — `task_state` convention over the 4-op surface

- **Status:** Proposed (pending board / Architect alignment)
- **Date:** 2026-10-28
- **Owner:** Architect (Winston)
- **Workstream:** Benchmark enablement — Paperclip issue ISI-6808 (source: ISI-5581 KubeCon Set B)
- **Relates to:** [`docs/architecture.md`](../architecture.md) (§3 envelope, §7.2 keyed fetch), [`docs/agent-mcp-transport.md`](../agent-mcp-transport.md), [`docs/mempalace-to-memkeys-migration.md`](../mempalace-to-memkeys-migration.md) (4-op contract), ISI-6326 / ISI-6331 (four-op decision)
- **Deciders:** Architect (author), DevOps Engineer (adapter wiring), Research Engineer (benchmark gate), Challenger (review)

---

## Context

The ISI-5581 KubeCon Set B benchmark scores memkeys **resume = 0.00** across all
three independent runs: when a kagent BMAD crew is suspended and relaunched, no
agent reconstructs its prior task context from memkeys. `factchange` scores 0.50 in
the same runs — so **store and retrieval work for new facts**; the gap is purely
that a relaunched agent reads back a pile of individual facts with **no way to know
where it left off**. zep (0.52) and mempalace (0.33) score nonzero because they
surface a richer context an agent can query to reconstruct state.

This is a **design gap, not a retrieval-accuracy bug.** Two sub-problems:

1. **No structured task record.** Crew agents wrote individual facts (decisions,
   observations) but never a single "here is my in-flight task, progress, and next
   step" record that a relaunched agent can read as a resume point.
2. **No relaunch-stable retrieval key.** Even if such a record existed, the agent
   must be able to *find it again* after a relaunch. This is where the memkeys key
   model bites, and it drives the whole decision:
   - Per [`envelope.rs`](../../crates/memkeys-server/src/envelope.rs),
     **`entity.agent` is always overwritten by the authenticated producer
     principal** — so if crew agents share one MCP principal it does not discriminate
     between roles, and it is not producer-controlled.
   - Per the §3.1 per-source anchor matrix
     ([`pipeline.rs:327`](../../crates/memkeys-server/src/pipeline.rs)):

     | `source.kind` | **required anchor** | default `fact.type` |
     |---|---|---|
     | `gateway_flush` / `run_settle` | **`entity.run`** | context / outcome |
     | `a2a_exchange` | **`entity.thread`** | observation |
     | `telemetry_learning` | **`entity.service` OR `entity.project`** (unkeyed tolerated) | learning |

     `entity.run` is a *new* run id on every launch, so `gateway_flush` — the
     "obvious" context source — is **wrong** for resume (its record would be anchored
     on a run that no longer exists on relaunch). The only source whose required
     anchor survives a relaunch is **`telemetry_learning`** (`service`/`project` are
     producer-controlled and stable). This is also the exact pattern the agent
     journaling convention already blesses: *"fold your session journal into the
     graph — `source.kind: telemetry_learning`, `entity.project`, `fact.type:
     context` (state to resume next session)."*

## Decision

**Do not add `save_task_state` / `restore_task_state` MCP tools.** Adopt a
`task_state` **convention over the existing 4-op surface** (`memory_store`,
`memory_fetch`). The four-op contract is a deliberate, hard-won invariant
(ISI-6326 chose "migrate agents to 4-op, no shim"; ISI-6331 shipped the sweep across
26 agents). A fifth/sixth tool would re-fork the surface we just unified, for a
capability the existing primitives already express. **Zero server changes.**

### The `task_state` record

A task_state record is one fact:

| Field | Value | Why |
|-------|-------|-----|
| `source.kind` | `telemetry_learning` | The only source whose required anchor (`service`/`project`) survives a relaunch (§3.1 matrix above). An agent recording its own working state *is* a telemetry/learning emission. |
| `fact.type` | `context` (set **explicitly**) | The fact type meaning "state to resume later" (model.rs:29). `telemetry_learning` defaults to `learning`, so the producer **must** set `type: "context"`; the explicit type is honored (pipeline.rs:152). |
| **anchor** `entity.service` | the **crew session id** (stable across suspend→relaunch) | Producer-controlled hint, **not** overwritten by the principal, and a valid `telemetry_learning` anchor. This is the relaunch-stable restore key. |
| `entity.project` | the benchmark scenario / project | Also a valid anchor and a secondary filter; stable. Provide both. |
| `fact.subject` | `task_state:<agent_role>` | Supersession grain + per-role discriminator when several crew roles share one session. A re-save under the same subject is newer, so restore recovers it. |
| `fact.body` | structured JSON (schema below) | The resume payload. |

`entity.agent` is left to be auto-stamped by the principal — it is an *extra*
filter, never the discriminator, precisely because the server overwrites it.

### `fact.body` schema (structured task state)

```json
{
  "task_title":        "string  — the task currently in flight",
  "progress_summary":  "string  — what has been done so far",
  "pending_decisions": ["string — open questions / decisions not yet taken"],
  "key_facts":         ["string — facts discovered that matter on resume"],
  "next_step":         "string  — the single next action to take on wake",
  "updated_at":        "RFC3339 — producer clock (also set source.emitted_at)"
}
```

### Save protocol (on crew suspend)

Per crew agent, write one `memory_store`:

```json
{
  "envelope": {
    "source": { "kind": "telemetry_learning", "emitted_at": "<RFC3339>" },
    "entity": { "service": "<crew_session_id>", "project": "<scenario>" },
    "fact": {
      "type": "context",
      "subject": "task_state:<agent_role>",
      "body": { "...": "the schema above" }
    }
  }
}
```

### Restore protocol (on crew relaunch — FIRST op, before any other tool)

```json
{ "entity.service": "<crew_session_id>", "type": "context", "limit": 10 }
```

`memory_fetch` returns newest-first (`ORDER BY event_time DESC, id DESC`,
[`pg.rs:562`](../../crates/memkeys-server/src/pg.rs); `MemStore` mirrors it,
[`memstore.rs:115`](../../crates/memkeys-server/src/memstore.rs)), so the newest
record per `subject` is the live state — **correct even without relying on
supersession**. The adapter selects the record whose `subject` matches the
relaunching agent's role and injects `fact.body` into the agent's system prompt /
first turn. This full round-trip (save v1 → save v2 → restore recovers v2, not v1) is
proven by the in-repo self-check `mcp::tests::task_state_round_trips_and_newest_wins`.

### Why this is sufficient (and minimal)

- **Structured record** → solves sub-problem 1.
- **`entity.service` anchor on `telemetry_learning`** → relaunch-stable, producer-controlled, not overwritten, and a *valid* anchor (unlike `entity.run`) → solves sub-problem 2.
- **newest-first fetch** → "where did I leave off" is the first row; no new query semantics.
- **No new tool, no server change** → the 4-op contract holds.

## Scope / ownership

| # | Work | Owner | Lives in |
|---|------|-------|----------|
| 1 | task_state schema + convention (this ADR) | Architect | this repo |
| 2 | In-repo self-check: store→fetch-newest round-trip over MCP dispatch | Architect | `crates/memkeys-server/src/mcp.rs` tests |
| 3 | kagent adapter: write task_state per agent on suspend | DevOps Engineer | kagent `mcp-memory-server` repo |
| 4 | kagent adapter: `restore` fetch as first op on relaunch, inject into prompt | DevOps Engineer | kagent `mcp-memory-server` repo |
| 5 | Benchmark gate: Set B resume ≥ 0.33 on a fresh N=3 run | Research Engineer | `memory-benchmark/setb-runner` |

Items 3–5 are delegated as child issues of ISI-6808; they are the live
continuation path to the benchmark gate.

## Consequences

- **Positive:** no server change; the 4-op contract is preserved; the convention is
  reusable by *any* agent (not just the benchmark crew) to persist and resume
  working state. Bitemporal history is free — `memory_fetch valid:"all"` /
  `as_of` replays how task_state evolved.
- **Negative / risk:** the convention is only honored if the adapter actually writes
  on suspend and reads *first* on relaunch; a crew that skips the restore op still
  scores 0.00. The gate (item 5) is what proves the wiring, not this ADR.
- **Rejected alternative — new tool pair:** re-forks the 4-op surface, needs a server
  release + a re-sweep of all agent configs, and buys nothing the `context` fact type
  + `entity.thread` anchor does not already provide.
- **Open question for the adapter owner:** confirm whether the crew session id is
  stable across the suspend→relaunch boundary and exposed to the MCP client. If it is
  not, the adapter must mint and persist a stable id out-of-band (e.g. in the kagent
  crew spec) and pass it as `entity.service`. This is the single assumption the gate
  validates.
