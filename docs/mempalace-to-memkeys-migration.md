# MemPalace → MemKeys MCP migration

**Status:** adopted (ISI-6331) · **Supersedes:** the MemPalace MCP surface for k8squad-test agents

MemKeys replaces the former MemPalace MCP as the shared organizational memory for all
agents. This document is the authoritative mapping from the 12-tool MemPalace surface to
the **four** MemKeys operations, plus the fold-down policy for MemPalace features that have
no MemKeys primitive.

## Why this migration

Phase 0b of the k8squad-test dogfood rollout (ISI-6326) evaluated two paths: (a) ship a
MemPalace-compatible shim in front of MemKeys, or (b) migrate agents directly onto the
native MemKeys 4-op surface. **Decision (b) was chosen** — no shim.

The deciding factor is the **clean start** (ISI-6307, decision C): the k8squad-test cutover
stands up a fresh MemKeys instance with **no data migration** from MemPalace. With no legacy
data to bridge, a compatibility shim carries cost without benefit — it would only preserve
call-site ergonomics for a tool surface we are deliberately retiring. Agents instead learn
the smaller, sharper 4-op vocabulary once.

> **No data is carried over.** Existing MemPalace memories, diary entries, drawers, and
> knowledge-graph edges are *not* imported. Agents start with an empty MemKeys graph and
> repopulate it through normal operation.

## The MemKeys surface: four operations

MemKeys is a **bitemporal fact graph** with semantic search. It exposes exactly four MCP
operations:

| Op | Purpose |
|----|---------|
| `memory_store` | Write a typed fact (envelope) into the graph. Also the mechanism for *updates* and *supersession* — bitemporality means you never mutate in place; you store a new version. |
| `memory_fetch` | Keyed lookup by `entity.*` identity (e.g. `entity.project`, `entity.issue`, `entity.pr`, `entity.agent`), optionally filtered by `type` and bitemporal selectors (`valid: "all"`, `as_of`). |
| `memory_search` | Natural-language semantic search over facts, optionally narrowed by `scope.entity.*`. |
| `memory_traverse` | Graph traversal from a `start` node (`{kind, id}`) over `edges` to discover cross-entity relationships. |

The tenant is bound **once at launch** from the agent's run principal — never passed as an
argument — and `entity.agent` is **stamped authoritatively** from that principal on every
write.

## Op mapping (MemPalace → MemKeys)

| MemPalace tool | MemKeys op | Notes |
|----------------|-----------|-------|
| `mempalace_search` | `memory_search` | Direct equivalent: semantic retrieval. Narrow with `scope.entity.*` instead of room/wing filters. |
| `mempalace_kg_query` | `memory_fetch` **or** `memory_search` | Use `memory_fetch` when you know the entity key(s); use `memory_search` for a natural-language query. |
| `mempalace_kg_add` | `memory_store` | Store a typed fact envelope. No separate "check duplicate" step — see fold-down below. |
| `mempalace_kg_invalidate` | `memory_store` (supersede) | Store the replacement fact with the **same `fact.subject`** (optionally `fact.supersedes`). Bitemporality closes the prior version automatically; nothing is deleted. Read history with `memory_fetch` + `valid: "all"` / `as_of`. |
| `mempalace_traverse` | `memory_traverse` | Direct equivalent. Start from `{kind, id}`, walk `edges`. |
| `mempalace_find_tunnels` / `mempalace_follow_tunnels` | `memory_traverse` | "Tunnels" are just edges in the fact graph; discovery and following both collapse into traversal. |
| `mempalace_list_tunnels` / `mempalace_create_tunnel` / `mempalace_delete_tunnel` | `memory_store` + `memory_traverse` | Relationships are facts. Create an edge by storing a fact that relates two entities; discover edges via traversal. There is no explicit tunnel-CRUD surface. |

## Fold-down policy (features with no MemKeys primitive)

MemKeys is deliberately smaller than MemPalace. Two classes of MemPalace features do not map
to a MemKeys op:

### Folded in — diary → typed facts

MemPalace's diary (`mempalace_diary_write`, `mempalace_diary_read`, and the AAAK spec via
`mempalace_get_aaak_spec`) has **no MemKeys equivalent as a distinct surface**. It folds into
the fact graph as typed `learning` / `context` facts:

- **Write** (`diary_write`) → `memory_store` an envelope with
  `source.kind: "telemetry_learning"`, `entity.project: "<project>"`, and
  `fact.type: "learning"` (what you did) or `fact.type: "context"` (state to resume next
  session). `entity.agent` is stamped from your principal automatically.
- **Read** (`diary_read`) → `memory_fetch` with `entity.agent: "<your role>"` and
  `type: "learning"` (and/or `type: "context"`). This is the heartbeat-resume recall.
- **AAAK format** → dropped. There is no structured AAAK envelope; a free-text `learning`
  fact body carries the same content without the ceremony.

### Dropped — not carried forward

These MemPalace features are **removed** and have no replacement. Agents must not rely on them:

| Dropped feature | MemPalace tools | Rationale |
|-----------------|-----------------|-----------|
| Taxonomy | `mempalace_get_taxonomy` | MemKeys has no fixed taxonomy; entities are typed keys (`project`/`issue`/`pr`/`agent`), not a managed category tree. |
| Spatial model (rooms/wings/drawers) | `mempalace_list_rooms`, `mempalace_list_wings`, `mempalace_list_drawers`, `mempalace_add_drawer`, `mempalace_get_drawer`, `mempalace_update_drawer`, `mempalace_delete_drawer` | The palace spatial metaphor is gone. Key-value "drawers" have no MemKeys primitive; use a typed fact on the relevant entity instead. |
| Timeline | `mempalace_kg_timeline` | MemKeys is bitemporal — temporal history is intrinsic. Query it with `memory_fetch` + `valid: "all"` / `as_of` rather than a dedicated timeline op. |
| Dedup pre-check | `mempalace_check_duplicate` | No separate call. Supersede-by-`fact.subject` makes re-storing a known subject idempotent at the version level. |
| Graph/house stats & status | `mempalace_graph_stats`, `mempalace_kg_stats`, `mempalace_status`, `mempalace_memories_filed_away`, `mempalace_reconnect`, `mempalace_hook_settings` | Operational/introspection surface, not agent-facing memory. Not carried forward into the agent MCP contract. |

## What agents should do

1. **On heartbeat start** — `memory_fetch` with your `entity.agent` and `type: "learning"`
   (and `type: "context"`) to recall prior sessions.
2. **Before answering about past work** — `memory_search` with a natural-language query,
   narrowed by `scope.entity.project` where relevant.
3. **After significant work** — `memory_store` a `learning` fact anchored on the project.
4. **When a decision is made or changes** — `memory_store` a `decision` fact with
   `fact.subject`; store the replacement under the same subject to supersede.
5. **To trace relationships** — `memory_traverse` from a known node.

The shared AGENTS.md "MemKeys — Shared Organizational Memory" block encodes exactly this.
All k8squad-test agent instruction blocks reference only `memory_store` / `memory_fetch` /
`memory_traverse` / `memory_search` after ISI-6331.

## Scope boundary

- The **per-agent MCP server config swap** (`mempalace` → `memkeys` endpoint) is **not** part
  of this document — it lands with the Ph2 cutover (ISI-6328).
- This document + the AGENTS.md sweep are the doc/config deliverables of ISI-6331, which
  gates Ph2.
