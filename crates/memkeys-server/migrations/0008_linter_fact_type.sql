-- MemKeys Golden Path A — linter as a first-class fact type (ISI-7811, parent
-- ISI-7787; docs/adr/ADR-0004-linter-data-model.md).
--
-- A Golden Path linter is advisory config stored as an ordinary fact so the
-- four-op surface reads and writes it with no new operation and no new source
-- kind (see model.rs `FactType::Linter`). The ONLY schema change the FactStore
-- needs is to admit 'linter' into the fact_type domain — the rule shape lives in
-- the structured `body` (JSONB-as-text) and `subject` is the stable rule_id, so
-- no new columns are required. Scope is read off the existing entity keys:
--   * global  linter = unkeyed (no entity.*), tenant floor;
--   * context linter = keyed on entity_project.
--
-- The 0001 CHECK was declared inline (unnamed → Postgres auto-name
-- `facts_fact_type_check`). Drop it by that conventional name (IF EXISTS keeps
-- this idempotent across environments that may already differ) and re-add it,
-- now named, with 'linter' admitted.
ALTER TABLE facts DROP CONSTRAINT IF EXISTS facts_fact_type_check;
ALTER TABLE facts
    ADD CONSTRAINT facts_fact_type_check
    CHECK (fact_type IN
        ('decision', 'outcome', 'observation', 'learning', 'context', 'linter'));

-- Linter lookup index. Serves both scopes within a tenant in one shape:
--   * context linters: (tenant_id, entity_project) range-scan, newest first;
--   * global linters:  the same scan with entity_project IS NULL.
-- Partial on fact_type = 'linter' so it stays tiny (linters are low-cardinality
-- config, not the hot fact traffic) and never competes with the keyed-fetch
-- indexes of 0001. This is what the Golden Path resolver children (ISI-7813+)
-- read to assemble a tenant/project's applicable linter set.
CREATE INDEX facts_linter_idx
    ON facts (tenant_id, entity_project, event_time DESC)
    WHERE fact_type = 'linter';
