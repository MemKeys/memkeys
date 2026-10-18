-- MemKeys M6 — LLM pipeline projections (ADR-llm-pipeline; docs/architecture.md
-- §5.4.2 digest tier, §5.4.3 category facets, §5.5 derivation versioning; stage
-- 5c/5d/5e workers).
--
-- All three tables are DERIVED, REBUILDABLE projections of the `facts` source of
-- truth (D3): a lost or stale row costs freshness, never correctness — the M7
-- reconciler/backfill rebuilds them by replaying the SoT. Every row carries
-- `derivation_version = (model_id, prompt_version)` (§5.5 / ADR Decision 4) so a
-- config bump marks stale artifacts as backfill work (ADR Decision 5).
--
-- R8 injection containment: the generative model never emits authority. Rows in
-- `derived_keys` are low-trust proposals (source='llm-extract'), never overwrite
-- client/server-stamped keys, and carry no provenance/tenant beyond the floor
-- inherited from the owning fact.

-- Stage 5e (W-extract): LLM-proposed candidate entity keys for sparsely-keyed
-- facts, validated before write. Idempotency per ADR Decision 6:
-- (fact.id, model_id, prompt_version[extract]) — the PK (fact_id, key) makes a
-- re-run an upsert, never a duplicate.
CREATE TABLE derived_keys (
    fact_id        UUID NOT NULL,
    tenant_id      TEXT NOT NULL,           -- inherited floor from the owning fact
    key            TEXT NOT NULL CHECK (key LIKE 'entity.%'),
    value          TEXT NOT NULL,
    confidence     DOUBLE PRECISION
                   CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    source         TEXT NOT NULL DEFAULT 'llm-extract' CHECK (source = 'llm-extract'),
    model_id       TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    applied_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (fact_id, key)
);
CREATE INDEX derived_keys_tenant_idx ON derived_keys (tenant_id);

-- Stage 5c (W-digest): one digest per (tenant, scope, scope_id) — the
-- §5.4.2 CONTEXT tier. `member_fact_ids` is the R1 resolve-down path (a digest
-- hit always hydrates to its underlying raw facts; the summary is a pointer,
-- never the answer). `scope_version` is monotonic per scope (ADR Decision 6) —
-- a late rollup for an older version is a no-op against the newer one.
-- The embedding sits in the digest row itself (the digest store IS derived; no
-- SoT separation needed) at the same width as migration 0003 (vector(256)).
CREATE TABLE digests (
    digest_id       UUID PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    scope           TEXT NOT NULL CHECK (scope IN ('thread', 'run', 'entity')),
    scope_id        TEXT NOT NULL,
    scope_version   BIGINT NOT NULL,
    summary         TEXT NOT NULL,
    story           TEXT NOT NULL,          -- server lineage artifact (§5.5)
    member_fact_ids UUID[] NOT NULL,
    embedding       vector(256) NOT NULL,
    model_id        TEXT NOT NULL,
    prompt_version  TEXT NOT NULL,
    ingested_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, scope, scope_id)
);
CREATE INDEX digests_tenant_idx ON digests (tenant_id);
CREATE INDEX digests_embedding_idx ON digests
    USING hnsw (embedding vector_cosine_ops);

-- Stage 5d (W-categorize): emergent labels as `category[]` facets (§5.4.3).
-- Facet rows are idempotent on (fact_id, label); the controlled-vocabulary
-- consolidation is the M7 periodic job, which folds these into the
-- project-scoped vocabulary and mirrors them onto `facts.categories`.
CREATE TABLE category_facets (
    fact_id        UUID NOT NULL,
    tenant_id      TEXT NOT NULL,
    label          TEXT NOT NULL,
    confidence     DOUBLE PRECISION
                   CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    model_id       TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    applied_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (fact_id, label)
);
CREATE INDEX category_facets_tenant_idx ON category_facets (tenant_id);

-- Per-(kind, fact) derivation stamps: marks that a generative worker RAN for a
-- fact even when it produced an empty result (zero accepted proposals / zero
-- labels), so the catch-up hole scan does not re-process facts forever, and a
-- (model_id, prompt_version) bump marks the stamp stale → backfill (ADR D5).
CREATE TABLE derivation_runs (
    kind           TEXT NOT NULL CHECK (kind IN ('llm_extract', 'categorize')),
    ref_id         UUID NOT NULL,           -- facts.id
    tenant_id      TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    applied_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, ref_id)
);
CREATE INDEX derivation_runs_tenant_idx ON derivation_runs (kind, tenant_id);
