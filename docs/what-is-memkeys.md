# What is MemKeys?

MemKeys is an open-source, attribute-based memory server for AI agents. Agents
store each fact with the keys that describe it — a project, an issue, a pull
request, a run — plus a validity window for when it was true. Retrieval then
starts from **what you know**: the keys. You get the fact you meant, not the
one that happened to be nearest.

That is the whole idea in one line: **every fact has an address, and you reach
it by key.**

## The problem it solves

Most agent memory today is a pile of embeddings. You write text, you get back
whatever is *nearest* at query time, and you hope. That works well enough for
open-ended discovery. It works badly for the questions agents actually ask
most often:

- "What do we know about issue X?"
- "What did the last run on this service conclude?"
- "What was true when we made this decision?"

These are not similarity questions. They are address lookups. A vector store
answers them approximately, at ranking-drift risk, with no opinion about time.
MemKeys answers them exactly — deterministically, by key, with the timeline
attached.

## Facts, entities, keys

Agents produce *facts*: a decision taken on a pull request, the outcome of a
run, a learning derived from telemetry, the context an agent flushes at the
end of a session. Every fact is *about something* — a project, an issue, a PR,
an agent, a run, a thread. Those "somethings" are the **entities**, expressed
as agent-supplied attribute keys, and they are the primary index into memory:

```
entity.project · entity.issue · entity.pr
entity.agent  · entity.run    · entity.thread · entity.service
```

Clients push facts with the keys they natively know — the server does the
modeling: it validates and enriches keys, applies temporal semantics, and
maintains the derived indexes. Clients stay simple.

## One source of truth, two derived views

- **Keyed / temporal store — the source of truth.** Every fact lives here
  once: append-only, bi-temporal (it knows when it was true *and* when it was
  recorded). This is the only authoritative copy.
- **Knowledge graph — derived.** Entity relationships, supersession chains,
  and cross-fact links projected from the keyed store. Rebuildable by replay.
- **Vector index — derived, optional.** Embeddings for semantic recall, always
  scoped by the same keys, so similarity search is *bounded* by the candidate
  set the keys define. Rebuildable by replay.

Because graph and vector are projections, they can never drift out of
authority: lose them and you lose nothing permanent — replay rebuilds them
from the keyed core.

## Four retrieval operations

The whole retrieval surface is four operations over that core:

1. **Store** a fact with its keys and validity.
2. **Fetch** by the keys you know — exact, deterministic.
3. **Search** semantically *within* a keyed scope.
4. **Traverse** the graph from a start node to see how entities connect.

The same four operations are exposed over both an HTTP API and the Model
Context Protocol (MCP), so any MCP-capable agent — Claude Code, goose, kagent,
or your own client — can use MemKeys as its memory server today.

## Where MemKeys fits

MemKeys is a standalone server. It is attribute-agnostic: it does not care
whether your keys are `project/issue/pr` or your own namespace convention. Any
agent platform is a *consumer* of its contract.

- **Agent platforms** give every agent durable, findable memory.
- **Human-in-the-loop review** gets a queryable audit trail: what was
  recorded, when, by whom, and what it superseded.
- **Anything where "what was true then" matters** gets point-in-time replay
  for free — ask for the state of the world as of any past instant.

## What MemKeys is not

- **Not a vector database.** Similarity search exists, but it is a derived,
  scoped view — never the primary retrieval path.
- **Not a graph database.** The graph is a projection of keyed facts, not a
  store you write to directly.
- **Not a chat product or an agent framework.** MemKeys is the memory layer
  underneath whatever agents you already run.
- **Not MemPalace.** A separate, unrelated project shares the "mem-" prefix;
  MemKeys is not built on it, and it is not built on MemKeys.

## Next steps

- [Why MemKeys](/memkeys/why/) — the keyed-first inversion, in depth.
- [Quickstart](/memkeys/quickstart/) — store your first fact in five minutes.
- [Architecture](/memkeys/architecture/) — the contract, the envelope, the
  derived views, and the write pipeline.
