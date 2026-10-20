# MemPalace → MemKeys MCP migration guide

**Status:** active · **Audience:** every k8squad-test agent and anyone porting an
agent off the MemPalace MCP · **Context:** ISI-6307 Ph0b decision (ISI-6326),
cutover tracked in ISI-6331 (this guide + AGENTS.md sweep) → Ph2 ISI-6328.

## Why this exists

The dogfood cutover (ISI-6307) replaces the **MemPalace** MCP server with
**MemKeys** as the shared organizational memory for k8squad-test agents.
Ph0b (ISI-6326) ruled this is a **migration, not a shim**: there is no
MemPalace-compatible adapter in front of MemKeys. The tool *names* differ, so a
straight config swap would make every `mempalace_*` call fail with
`unknown tool`. Agents must learn the new surface.

**Clean start (ISI-6307 decision C): no data is carried over.** MemKeys begins
empty. Do not plan around migrating MemPalace rows, drawers, tunnels, or diary
history — none of it moves.

## The MemKeys surface in one paragraph

MemKeys exposes exactly **four MCP operations** (`crates/memkeys-server/src/mcp.rs`):
`memory_store`, `memory_fetch`, `memory_traverse`, `memory_search`. It is a
single-tenant-per-principal, bitemporal fact graph. The **tenant is bound once
at launch from your run principal — it is never a tool argument**. Your
`entity.agent` key is likewise stamped authoritatively from that principal on
every write; a different `entity.agent` you pass is treated as an ignored hint.
The authoritative contract lives in [`docs/architecture.md`](./architecture.md)
§3 (envelope), §3.1 (per-source matrix), and §7 (retrieval ops).

## Operation mapping

| MemPalace tool | MemKeys op | Notes |
|----------------|-----------|-------|
| `mempalace_search` | `memory_search` | Natural-language `query`; narrow with `scope.entity.*` / `scope.fact.type`, cap with `k`, point-in-time with `as_of`. Returns `feature_disabled` when `memory.semantic` is off. |
| `mempalace_kg_query` | `memory_fetch` **or** `memory_search` | Looking up by a **known** entity (project/issue/pr/…) → `memory_fetch`. A fuzzy/semantic lookup → `memory_search`. |
| `mempalace_kg_add` | `memory_store` | Wrap the fact in a MemKeys **envelope** (§3) and store. The subject→predicate→object triple folds into `fact.subject` + typed `fact.body`. |
| `mempalace_kg_invalidate` | `memory_store` (supersede) | No separate invalidate op. MemKeys is **bitemporal**: store the replacement with `fact.subject` set (and optionally `fact.supersedes: <fact_id>`); the prior fact's validity closes automatically. History stays reachable via `memory_fetch { valid: "all" }` and `as_of`. |
| `mempalace_traverse` | `memory_traverse` | `start: {kind, id}`, `edges: [...]`, `direction`, `depth`, `deadline_ms`. |
| `mempalace_find_tunnels` | `memory_traverse` | Cross-domain links are just edges. Traverse from the node over the relevant edge types instead of a dedicated tunnel finder. |
| `mempalace_follow_tunnels` | `memory_traverse` | Same — follow edges with `memory_traverse`. |

### Entity keys and fact types (the vocabulary)

- **Entity keys** (`entity.*`): `project`, `issue`, `pr`, `agent`, `run`,
  `thread`, `service`. At least one is required on a fetch; the anchor required
  on a store depends on `source.kind` (§3.1).
- **Fact types** (`fact.type`): `decision`, `outcome`, `observation`,
  `learning`, `context`. Omitting it defaults per `source.kind`
  (`gateway_flush`→context, `run_settle`→outcome, `a2a_exchange`→observation,
  `telemetry_learning`→learning).

## Fold-down policy for gaps with no MemKeys primitive

Some MemPalace features have **no** MemKeys equivalent. This is deliberate
(ISI-6326): a shim rebuilding them was rejected. Here is how each is handled.

### Diary (`mempalace_diary_read` / `diary_write` / `get_aaak_spec` / AAAK) → fold into `entity.agent` facts

MemKeys has no diary and no AAAK format. Fold your per-session journal into the
graph as **typed facts stamped with your agent key**:

- **What you learned / did this session** → `memory_store` a `learning` fact.
- **State to resume next session** → `memory_store` a `context` fact.

Because `entity.agent` is stamped authoritatively from your principal on every
write, these facts are automatically attributed to you. Anchor them on the
**project** you were working in using `source.kind: "telemetry_learning"`
(whose anchor is `entity.service` **or** `entity.project`, §3.1):

```json
{
  "source": { "kind": "telemetry_learning", "emitted_at": "<RFC3339 now>" },
  "entity": { "project": "<project-id>" },
  "fact": {
    "type": "learning",
    "body": "Migrated k8squad-test agents to the MemKeys 4-op surface; AGENTS.md swept across every agent carrying the shared-memory block.",
    "subject": "isi-6331-cutover"
  }
}
```

The store response echoes your resolved keys in `entity_keys_resolved`
(including your `entity.agent`), so you learn your own agent id. **Recall at the
start of a heartbeat** with a keyed fetch:

```json
memory_fetch { "entity.agent": "<you>", "type": "learning" }
```

The old AAAK one-line compression is not required — `fact.body` holds free text
or structured JSON. Keep entries terse by convention, but there is no format gate.

### Bitemporal history (`mempalace_kg_timeline`) → `memory_fetch` with `valid`/`as_of`

There is no timeline tool, but MemKeys is bitemporal natively. "What did we
believe at time T" is `memory_fetch { ..., as_of: "<RFC3339>" }`; "show all
versions including retired ones" is `memory_fetch { ..., valid: "all" }`
(default is `valid: "current"`). `memory_search` also accepts `as_of`.

### Dropped entirely (not carried forward)

The following MemPalace concepts have **no** MemKeys representation and are
**dropped** — do not look for a replacement, and do not design around them:

- **Taxonomy** (`mempalace_get_taxonomy`): no tag/category taxonomy layer.
  Use `entity.*` keys and `fact.type` as the only classification axes;
  `memory_search` scope accepts a free-form `category` filter if you need one.
- **Spatial metaphor — rooms / wings / drawers** (`list_rooms`, `list_wings`,
  `add_drawer`, `get_drawer`, `list_drawers`, `update_drawer`,
  `delete_drawer`): MemKeys has no spatial organization. Facts are organized
  solely by entity keys and edges.
- **Tunnels as first-class objects** (`create_tunnel`, `delete_tunnel`,
  `list_tunnels`): relationships are graph **edges**, traversed with
  `memory_traverse`. There is no separate tunnel registry to manage.
- **Palace housekeeping** (`graph_stats`, `kg_stats`, `check_duplicate`,
  `memories_filed_away`, `hook_settings`, `status`, `reconnect`): operational
  MemPalace internals with no agent-facing MemKeys analog. Duplicate suppression
  is handled server-side via `source.idempotency_key` on the envelope.

## Worked examples

**Record a decision on an issue:**
```json
memory_store {
  "source": { "kind": "gateway_flush", "emitted_at": "<now>", "idempotency_key": "<opt>" },
  "entity": { "run": "<your-run-id>", "issue": "ISI-6331" },
  "fact": { "type": "decision", "subject": "mcp-surface", "body": "Agents migrate to MemKeys 4-op; no MemPalace shim." }
}
```
(`gateway_flush` anchors on `entity.run`; the server derives `project`/`issue`
from the run binding and stamps `entity.agent` from your principal.)

**Supersede a prior decision** — store the new one with the same `subject`
(and optionally `fact.supersedes: "<old-fact-id>"`); the old version closes:
```json
memory_store {
  "source": { "kind": "gateway_flush", "emitted_at": "<now>" },
  "entity": { "run": "<your-run-id>", "issue": "ISI-6331" },
  "fact": { "type": "decision", "subject": "mcp-surface", "supersedes": "<old-id>",
            "body": "Revised: ship the 4-op surface unchanged after soak." }
}
```

**Find relevant prior work (semantic):**
```json
memory_search { "query": "how did we decide the MCP surface for the dogfood cutover",
                "scope": { "entity.project": "<memkeys-project>" }, "k": 10 }
```

**Trace what connects to an issue:**
```json
memory_traverse { "start": { "kind": "issue", "id": "ISI-6331" }, "depth": 2, "direction": "both" }
```

## Checklist for porting an agent

1. Swap the MCP server config `mempalace` → `memkeys` (this lands in Ph2 cutover
   **ISI-6328**, not here).
2. Replace every `mempalace_*` call in the agent's instructions with the mapped
   `memory_*` op per the table above. (For k8squad-test agents this is already
   done in the shared `AGENTS.md` "MemKeys — Shared Organizational Memory"
   block — see ISI-6331.)
3. Move diary reads/writes to `entity.agent` `learning`/`context` facts.
4. Delete any reliance on taxonomy, rooms/wings/drawers, or tunnel-registry
   tools — they are gone.
