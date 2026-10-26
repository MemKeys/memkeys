# Benchmark readiness: making a just-stored fact deterministically searchable

> Scope: the fact-vector (semantic) tier, stage 5b. ISI-6677, follow-up to
> ISI-6671. Addresses the ISI-6183 Set A warm-pass recall methodology.

## The race

Stage 5b (embed the fact body → upsert `fact_vectors`) is **async, off the ack
path** (§6/R2 — a projection failure never blocks stages 1-4). The `store`
receipt is gated on stage 4 (the commit) only. Embedding lands later, when the
`VectorProjectionWorker` drains its queue (the server's maintenance loop ticks
every 30s) — the whole point of stage 5b is to keep the embedding-model call
off the hot write path.

Consequence for a **single-agent benchmark harness** that writes a fact and then
immediately queries it semantically: the fact is committed and keyed, but its
vector is not in the index yet, so op-4 (`semantic_search`) returns an empty hit
set until the worker catches up. That perturbs warm-pass recall measurements
with a timing artifact that has nothing to do with retrieval quality.

## Decision

We do **not** make synchronous embedding the default, and we do **not** add a
per-write sync flag that couples the commit path to the model server. Instead we
expose an **opt-in readiness / flush primitive** the harness calls explicitly:

- In-process (the ISI-6183 bench harness drives `MemKeysService` directly):
  `MemKeysService::flush_semantic(tenant) -> Result<usize, QueryError>`.
- Over HTTP (an out-of-process harness): `POST /memory/flush` (no body; tenant
  from the verified principal) → `{ "tier": "fact_vector", "embedded": <n> }`.

Both run the stage-5b **SoT convergence scan** (`VectorProjectionWorker::catch_up`):
they embed every committed fact for the tenant that has no current-model vector
yet, then return. After the call returns, the fact-vector tier is **converged**
for that tenant, so the store→search round-trip is deterministic.

`catch_up` (not merely draining the in-process queue) is used deliberately: it
closes holes the lost-queue, module-just-enabled, and cross-pod cases would
otherwise leave, so the readiness guarantee holds regardless of which worker
instance enqueued the write.

When the semantic module is off, flush returns `feature_disabled` (HTTP 501) —
the same seam as op-4 — so a harness never mistakes "disabled" for "converged"
and then queries an empty tier.

### Why not the alternatives

- **Synchronous embed on the write path (default):** rejected. Couples every
  commit to the embedding-model server, defeating stage 5b. (A per-request
  opt-in sync flag was considered; a flush primitive is simpler, covers batch
  writes in one call, and needs no change to the ingest path.)
- **Keep async, no primitive, document a sleep:** rejected. A fixed sleep is
  both flaky (under-waits under load) and slow (over-waits normally). A
  convergence scan is deterministic and returns as soon as the tier is caught up.

## Harness usage

```text
for fact in workload:            # write phase
    store(fact)                  # stage-4 ack; stage 5b still async
flush_semantic(tenant)           # drain the tier to convergence (opt-in)
for q in queries:                # warm pass: deterministic, no race
    semantic_search(tenant, q)
```

Call `flush_semantic` once after the write phase (it is tenant-scoped and
convergent — a second call with nothing outstanding returns `0`), not per write.
