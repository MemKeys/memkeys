-- MemKeys M1 — FactStore source of truth (Postgres).
--
-- Grounded in docs/architecture.md §5.1 (record shape) and §6 (pipeline stages 1-4):
-- append-only, bi-temporal (event_time = fact axis, ingested_at = transaction axis),
-- keyed dimensions as first-class columns, supersession as read-time collapse (§5.1:
-- a new version is a new record carrying fact.supersedes; "currently valid" is computed
-- at read time — no mutation of prior records).
--
-- tenant_id is the WS-4 Invariant T isolation floor (single column predicate, §1.1 of
-- the retrieval contract). fact.id is principal-salted (tenant_id + agent_id + caller
-- idempotency key as UUIDv5 derivation inputs) and is the idempotency key: a retried
-- flush resolves to the same row (no duplicate), a replay with a different body is a 409.

CREATE TABLE facts (
    id                    UUID PRIMARY KEY,
    tenant_id             TEXT NOT NULL,
    fact_type             TEXT NOT NULL
                          CHECK (fact_type IN
                          ('decision', 'outcome', 'observation', 'learning', 'context')),
    subject               TEXT,
    body                  TEXT NOT NULL,
    body_hash             TEXT NOT NULL,
    confidence            DOUBLE PRECISION
                          CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    supersedes            UUID,
    event_time            TIMESTAMPTZ NOT NULL,
    ingested_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    entity_project        TEXT,
    entity_issue          TEXT,
    entity_pr             TEXT,
    entity_agent          TEXT,
    entity_run            TEXT,
    entity_thread         TEXT,
    entity_service       TEXT,
    counterpart_agent     TEXT,
    keyed                 BOOLEAN NOT NULL,
    provenance_principal  TEXT NOT NULL,
    provenance_source_kind TEXT NOT NULL
                          CHECK (provenance_source_kind IN
                          ('gateway_flush', 'run_settle', 'a2a_exchange', 'telemetry_learning')),
    provenance_trace_id  TEXT,
    rejected_hints        JSONB NOT NULL DEFAULT '[]'::jsonb,
    attachments           JSONB NOT NULL DEFAULT '[]'::jsonb,
    -- D4 category facets are a derived projection (M6) written back onto this record;
    -- the column exists from day one so the facet surface is part of the SoT schema.
    categories            TEXT[] NOT NULL DEFAULT '{}'
);

-- Supersession resolution: "not superseded as of the transaction axis" is a correlated
-- lookup on the superseding record's ingested_at (WS-4 §3.4 F6 ordering).
CREATE INDEX facts_supersedes_idx ON facts (supersedes) WHERE supersedes IS NOT NULL;

-- Keyed fetch (op 1): partial indexes per keyed dimension, keyed rows only.
CREATE INDEX facts_tenant_project_idx ON facts (tenant_id, entity_project, event_time DESC)
    WHERE keyed;
CREATE INDEX facts_tenant_issue_idx   ON facts (tenant_id, entity_issue, event_time DESC)
    WHERE keyed;
CREATE INDEX facts_tenant_pr_idx      ON facts (tenant_id, entity_pr, event_time DESC)
    WHERE keyed;
CREATE INDEX facts_tenant_agent_idx   ON facts (tenant_id, entity_agent, event_time DESC)
    WHERE keyed;
CREATE INDEX facts_tenant_run_idx     ON facts (tenant_id, entity_run, event_time DESC)
    WHERE keyed;
CREATE INDEX facts_tenant_thread_idx  ON facts (tenant_id, entity_thread, event_time DESC)
    WHERE keyed;
CREATE INDEX facts_tenant_service_idx ON facts (tenant_id, entity_service, event_time DESC)
    WHERE keyed;

-- The tenant floor alone (browse by coarse key) still needs a tenant-wide time order.
CREATE INDEX facts_tenant_time_idx ON facts (tenant_id, event_time DESC, id DESC) WHERE keyed;
