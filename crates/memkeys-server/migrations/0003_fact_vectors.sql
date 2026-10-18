-- MemKeys M5 — fact-vector tier (docs/architecture.md §5.4.2, §7.4; ISI-6079/ISI-6167).
--
-- The semantic surface's PRECISION tier: one pgvector row per committed fact,
-- carrying the embedding of `fact.body` plus the fact's KEYS — and NEVER the
-- body text itself (§5.4.2 R1, the telephone-game invariant: a semantic hit is
-- a fact.id + score; the authoritative body is always hydrated from the keyed
-- `facts` store afterward, §7.5). Co-located with the SoT store, derived &
-- rebuildable off the hot path (stage 5b, §6/R2) — a lost/stale vector costs
-- freshness, never correctness.
--
-- Table name: the repo's SoT tables are `facts` / `edges` (the architecture
-- doc's aspirational `agentmesh_*` prefix was dropped in M1/M2); this tier
-- follows that convention as `fact_vectors`.
--
-- Multi-tenancy (§5.4.2 / §7.4 R6): `tenant_id` is the HARD isolation boundary.
-- Every ANN search is bounded to one tenant partition (the btree on tenant_id)
-- before the vector distance is ever computed — team A's facts can never
-- surface in team B's results. Finer `entity.*` scope narrows WITHIN that floor
-- (passed as a candidate id set by the service), never across it.
--
-- Idempotency: PK is `fact_id` — the stage-5b worker upserts (ON CONFLICT DO
-- UPDATE), so a replay re-embeds to the same vector (the embedder is
-- deterministic within a derivation version) and overwrites rather than
-- duplicating (§5.4.2).

CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE fact_vectors (
    fact_id        UUID PRIMARY KEY,          -- = facts.id; idempotency key (upsert target)
    tenant_id      TEXT        NOT NULL,      -- §7.4 R6 hard isolation floor
    embedding      vector(256) NOT NULL,      -- dim MUST match the embedder (HashEmbedder = 256)
    fact_type      TEXT        NOT NULL,      -- carried key (scope facet; never body)
    entity_project TEXT,
    entity_issue   TEXT,
    entity_pr      TEXT,
    entity_agent   TEXT,
    entity_run     TEXT,
    entity_thread  TEXT,
    entity_service TEXT
);

-- The tenant floor is applied as an equality predicate BEFORE the ANN pass, so
-- a btree on tenant_id keeps "this tenant's partition" an index lookup.
CREATE INDEX fact_vectors_tenant_idx ON fact_vectors (tenant_id);

-- ANN index on the embedding (HNSW, cosine ops — the service ranks by cosine
-- similarity `1 - (embedding <=> q)`, matching MemVectorIndex's cosine()). HNSW
-- over ivfflat: no training/list-count tuning, good recall out of the box, and
-- it degrades to an exact scan when a candidate-id pre-filter is present (the
-- scoped-search path) — correctness holds either way.
CREATE INDEX fact_vectors_embedding_idx ON fact_vectors
    USING hnsw (embedding vector_cosine_ops);
