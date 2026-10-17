-- MemKeys T3 — live-head key (WS-4 Invariant T §7.6 T3; ISI-5914).
--
-- Adds `entity_identity`: the durable entity component of the live-head key
--   H = (tenant_id, entity_identity, subject, fact_type)
-- for subject-bearing keyed facts. It is computed by the write path (pg.rs
-- `entity_identity()`) and written once at commit — IMMUTABLE, never rewritten
-- (append-only SoT is preserved; this is not a supersession flag).
--
-- Composition (§7.6 T3-pin, ratified ISI-6131): the DURABLE keyed dimensions
-- project/issue/pr/thread/service — the keys that identify a *statement*. It
-- EXCLUDES:
--   * entity.agent — the head is per statement, not per producing agent
--                    (ISI-6053 R1); the agent stays in the fact.id salt only.
--   * entity.run   — episodic. §4 enrichment lifts a resolvable run into
--                    project/issue/pr BEFORE H is computed, so a run keeps
--                    chaining the same durable statement; a run that was not
--                    lifted leaves an empty composite (below). (thread stays —
--                    it is the durable a2a anchor-of-record.)
-- An EMPTY composite => the fact is unkeyed for H and NULL here: it skips the
-- live-head gate (also NULL for subject-null facts).
--
-- Uniqueness of the live head is enforced in the write path by per-key
-- serialization (a transaction-scoped advisory lock on hash(H)) plus a head
-- compare-and-set over the existing `supersedes` chain (§7.6 T3 realization
-- (b)). No `WHERE NOT superseded` flag is used, so there is no write-back to
-- prior records — liveness stays a read-time property of the supersedes chain.
-- This index only accelerates the in-gate head lookup.

ALTER TABLE facts ADD COLUMN entity_identity TEXT;

CREATE INDEX facts_live_head_idx
    ON facts (tenant_id, entity_identity, fact_type, subject)
    WHERE entity_identity IS NOT NULL;
