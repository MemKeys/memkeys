-- MemKeys M7 — embedding-model stamp for the vector tiers (reconciler/backfill;
-- ISI-6081; docs/architecture.md §5.5 derivation versioning).
--
-- The LLM-derived tiers (derived_keys/digests/category_facets) already carry
-- `(model_id, prompt_version)` so the stage-5c/5d/5e `catch_up` scans treat a
-- config bump as backfill work (ADR Decision 5). The embedded tiers had NO such
-- stamp: `fact_vectors` and the `digests.embedding` were written with whatever
-- embedder was active, with nowhere to record WHICH one. So when the embedding
-- model changes (an operator swaps `MEMKEYS_EMBED_URL`/`MEMKEYS_EMBED_MODEL`, or
-- upgrades the model behind the same endpoint), the old vectors silently
-- persisted and the ANN then ranked across INCOMPATIBLE vector spaces — a
-- correctness-degrading hole the hole-only `catch_up` (missing-row scan) could
-- never detect. M7 closes this: an embedding-model stamp per embedded row + a
-- stale-stamp predicate in the reconciler's `catch_up` scans, so a model change
-- triggers a re-embed with no downtime (the derived row is rebuilt from the SoT
-- off the hot path; a stale vector costs freshness, never correctness — D3).
--
-- Backward compatibility: both columns default to '' (the empty sentinel). Any
-- pre-M7 row therefore reads back model_id='' , which differs from every real
-- embedder model id, so the first reconcile pass after upgrade re-embeds it —
-- the correct outcome (its true embedding provenance is unknown). No backfill
-- SQL is needed; the reconciler converges it.

-- fact_vectors (stage 5b): the embedder build that produced `embedding`.
-- `model_id <> active` is the stage-5b stale-embedding backfill trigger.
ALTER TABLE fact_vectors ADD COLUMN model_id TEXT NOT NULL DEFAULT '';

-- digests (stage 5c): the digest summary is itself embedded (one embedder, both
-- tiers — ADR Decision 3, RRF-fusable). `model_id`/`prompt_version` already
-- track the GENERATOR (summarizer); add the EMBEDDER build so an embedder-only
-- swap (generator unchanged) still re-rolls the digest and re-embeds its summary
-- — otherwise the digest vector space would drift out of sync with the fact
-- vector space the two are fused against.
ALTER TABLE digests ADD COLUMN embed_model_id TEXT NOT NULL DEFAULT '';
