---
title: What is MemKeys?
description: An open, attribute-keyed memory server for AI agents — facts keyed by what they are about, with temporal validity, and graph + vector as derived views.
sidebar:
  order: 1
---

# What is MemKeys?

**MemKeys is an open memory server for AI agents.** Agents produce *facts* — a
decision taken on a pull request, the outcome of a run, a learning derived from
telemetry, the context an agent flushes at the end of a session. MemKeys stores
each fact *keyed by what it is about* and serves it back deterministically to the
next agent that needs it.

The design goal is boring reliability: the overwhelming majority of what an agent
asks memory is **"what do we already know about X?"** — not a fuzzy search. "Pick
up issue 6397" wants the prior decisions, the last run outcome, and the handoff
note the previous agent left. That is a lookup by a key, not a similarity ranking.
MemKeys makes that lookup the default path and keeps the fuzzy parts at the edges
where they belong.

## The keyed-first invariant

This is the one idea the whole system hangs on:

> **Attribute keys bound the candidate set before any vector or graph work. The
> keyed / temporal store is the single source of truth; graph and vector are
> derived, rebuildable projections.**

Every fact is *about something* — a `project`, an `issue`, a `pr`, an `agent`, a
`run`, a `thread`, a `service`. Those "somethings" are the **entities**, supplied
as attributes by the producing agent, and they are the primary index into memory.
Because the default read is a keyed fetch, memory is:

- **Deterministic & reproducible** — the same key returns the same set. No ranking
  drift, no embedding-model dependence, no false neighbours.
- **Cheap** — a filtered scan over an indexed dimension, with no vector math on the
  hot path.
- **Portable** — any store that can "filter records by field + time range" can
  serve it. The semantic tier, the part that varies most across backends, is pushed
  to the edge as a replaceable index.
- **Honest about misses** — a keyed miss means "nothing recorded about X", not "the
  embedding didn't surface it". Discovery ambiguity is confined to the semantic tier
  where it belongs.

Semantic search still earns its place for the genuinely open question ("has anyone
hit a NetworkPolicy blackhole like this before?") — but it runs *within* a keyed
scope wherever one is known, never as the primary retrieval path.

## Facts and temporal validity

A fact is a small, immutable envelope: a `body` (the thing only the producer can
supply), a `type` (`decision` / `outcome` / `observation` / `learning` /
`context`), the entity keys it is about, and the time it happened. Agents push
**raw** facts; the server does the modeling — it normalizes the fact, validates and
enriches the entity keys, applies temporal semantics, and writes the authoritative
record.

MemKeys is **bitemporal**. Each fact carries both *when the thing happened* (event
time) and *when the server learned it* (transaction time). Records are append-only
and never mutated:

- A new version of a statement is a **new record** that `supersedes` the old one.
- "Currently valid" is **computed at read time** — the latest non-superseded record
  per `(entity, subject, type)`.
- The full history stays queryable, so you can ask a keyed read for the *current*
  value, or replay memory **as of** any past instant.

Nothing is ever overwritten, so an audit trail and point-in-time recall fall out of
the storage model for free.

## Graph and vector are derived views

MemKeys keeps a single source of truth — the keyed / temporal fact store — and
maintains two **derived, rebuildable** views off it, asynchronously, off the write
hot path:

- **Knowledge graph** — entity↔entity relationships, supersedes-chains, and
  cross-fact links, projected from the keyed store. It answers "what did this agent
  decide last sprint?" and "what superseded this fact?".
- **Vector index** — embeddings of fact bodies (and, optionally, of scope-level
  digest summaries) for similarity search. It is an **optional** module: a minimal
  deployment runs keyed + graph only and turns semantics on when it needs them.

Because both are projections, they can be rebuilt by replaying the keyed store. If a
derived view falls behind or is lost, the source of truth is untouched and a
reconciler catches it back up. A derived-view outage never blocks a write or a keyed
read — it only delays graph / semantic freshness.

Those projections feed **four retrieval operations**, in priority order:

1. **Fetch by keys** (hot, default) — fact → entity keys + time range → direct
   lookup. No embeddings, no graph traversal.
2. **Graph traversal** (warm) — navigate relations and supersedes-chains.
3. **Semantic search** (optional) — similarity over a *key-bounded* candidate set.
4. **Write with keys** (hot) — push a raw fact; the server does all the modeling.

The [Architecture](./architecture.md) page walks through the envelope, the
single-Postgres layout, the write pipeline, and the four-operation contract in
detail.

## MemKeys is not MemPalace

Despite the similar "mem-" naming, these are two different things:

- **MemKeys** — this project: an open-source, keyed memory *server* for agents.
  Facts are stored with agent-supplied attribute keys and temporal validity; graph
  and vector are derived views; the contract is open and the backend is pluggable.
- **MemPalace** — a separate, internal MCP shared-memory layer with a different
  design. MemKeys is **not** built on MemPalace, and MemPalace is **not** MemKeys.

If you are moving an agent off the MemPalace MCP, the tool surface is different on
purpose — it is a migration, not a drop-in shim. See
[`mempalace-to-memkeys-migration.md`](../mempalace-to-memkeys-migration.md) for the
operation mapping.

## Where to go next

- **[Architecture](./architecture.md)** — how a fact flows from the wire to storage
  and back out through the four operations.
- **[Getting started](./getting-started.md)** — bring up Postgres + MemKeys with
  Docker Compose and make your first `/memory/*` calls.
- **[CRD reference](./crd-reference.md)** — run MemKeys on Kubernetes with the
  `MemKeysInstance` operator.
