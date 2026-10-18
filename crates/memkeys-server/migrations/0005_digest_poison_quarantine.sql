-- MemKeys M6 follow-up (ISI-6192 B2) — durable poison quarantine for the
-- W-digest worker.
--
-- The digest tier has no per-fact `derivation_runs` success stamp (the
-- `digests` row IS its success marker), so before this migration a scope whose
-- rollup deterministically poisoned (schema-invalid summary from the HTTP
-- generator) had nowhere to record "attempted at this version, failed". The
-- catch_up hole-scan would therefore re-drive it every tick (real generator
-- cost), and — before the collect-and-continue fix — `?`-abort the tenant's
-- backfill, failing the boot-time scan in main().
--
-- Fix: allow a `digest_rollup` stamp whose `ref_id` is the stable digest_id
-- (uuid_v5 of memkeys/digest/<tenant>/<scope>/<scope_id>) so a poison scope is
-- quarantined at its (model_id, prompt_version) and skipped until a version
-- bump re-opens it (ADR Decision 8: poison STOPS reprocessing). The PK is
-- (kind, ref_id), so digest stamps never collide with the fact-keyed
-- 'llm_extract'/'categorize' stamps even though both use UUID ref_ids.
ALTER TABLE derivation_runs DROP CONSTRAINT IF EXISTS derivation_runs_kind_check;
ALTER TABLE derivation_runs
    ADD CONSTRAINT derivation_runs_kind_check
    CHECK (kind IN ('llm_extract', 'categorize', 'digest_rollup'));
