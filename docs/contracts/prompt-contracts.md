---
contract: prompt-contracts
title: "Prompt contracts — MemKeys generative workers (WS-3)"
status: proposed
author: Architect (Winston)
date: 2026-10-14
workstream: WS-3
issue: ISI-5868
governs: [W-extract, W-digest, W-categorize]
adr: ADR-llm-pipeline
---

# Prompt contracts — generative workers (WS-3)

Companion to [`ADR-llm-pipeline.md`](../adr/ADR-llm-pipeline.md). One contract per generative
task (`extract`, `summarize`, `categorize`). Each contract is the versioned unit behind
`prompt_version` in `derivation_version = (model_id, prompt_version)` (ADR §5.5 / Decision 4).

## Contract rules (apply to all three)

1. **Input is untrusted.** The model reads `fact.body` (and, for digests, member bodies) — agent
   content. It is **data, not instructions**. Prompts wrap it in an explicit delimiter and
   instruct the model to treat anything inside as content to be processed, never as commands.
2. **Structured-output only.** Each worker forces a JSON schema (tool/structured-output mode).
   Output is **validated against the schema before any write**; a parse/schema failure is a
   **poison** condition (ADR Decision 8), never a silent bad write.
3. **R8 injection containment — the model never emits authority.** No contract may emit
   `provenance`, `entity.tenant`/principal, `fact.id`, timestamps, or trust markers. The model
   proposes *content* (keys, summaries, labels); the **server** stamps all authority, keys as
   low-trust/derived, and validates before widening any candidate set.
4. **Determinism knobs.** `temperature = 0` (or lowest supported) for extract/categorize;
   low temperature for summarize. Fixed `max_tokens` per contract. No tools other than the
   structured-output schema.
5. **Versioning.** Any wording, schema, or constraint change bumps `prompt_version` (monotonic
   string, e.g. `extract/v3`). A bump is a change-log backfill event (ADR Decision 5).
6. **Token budget.** Each contract declares an input cap; over-cap inputs are truncated with a
   documented strategy (head+tail for bodies; most-recent-first for member sets) and the
   truncation is recorded so a later-version re-run is reproducible.

---

## Contract: `extract` — entity-key proposal (W-extract)

**prompt_version:** `extract/v1`

**Purpose.** Propose candidate entity keys from a sparsely-keyed fact's body, to be **validated**
by the server (§4 verify) and written as low-trust `derived_key[]` (ADR Decision 2a). Enriches
findability; never authoritative.

**Input (server-assembled):**
```json
{
  "fact_body": "<untrusted text, delimited>",
  "existing_keys": { "entity.project": "…", "entity.issue": "…" },
  "allowed_key_space": ["entity.project","entity.issue","entity.pr","entity.agent",
                        "entity.run","entity.thread","entity.entity"]
}
```

**Output schema:**
```json
{
  "proposed_keys": [
    { "key": "entity.issue", "value": "ISI-1234", "confidence": 0.0-1.0,
      "evidence_span": "<verbatim substring of fact_body supporting this>" }
  ]
}
```

**Constraints.**
- `key` MUST be one of `allowed_key_space`. Any other key → dropped by the server (not an error).
- MUST NOT propose a value for a key already present in `existing_keys` (no overwrite path exists).
- `evidence_span` MUST be a verbatim substring of `fact_body` — the server checks this; a
  non-substring span invalidates that proposal (anti-hallucination guard).
- No provenance, no tenant, no free-form keys. Empty `proposed_keys` is a valid (common) answer.

**Server post-processing.** Validate each proposal against the entity registry/scope (§4 verify);
accepted ones written as `derived_key{source:"llm-extract", confidence, derivation_version}`.
Rejected proposals are logged, not written.

---

## Contract: `summarize` — scope rollup + story (W-digest)

**prompt_version:** `summarize/v1`

**Purpose.** Produce one digest per `(scope, scope_id)` — a navigational recall surface, **not a
record** (R1). Digest hits always resolve down to `member_fact_ids`; the summary is never the
final answer (§5.4.2).

**Input (server-assembled):**
```json
{
  "scope": "thread|run|entity",
  "scope_id": "…",
  "member_facts": [
    { "fact_id": "…", "fact_type": "decision|observation|…",
      "event_time": "<RFC3339>", "body": "<untrusted, delimited>" }
  ]
}
```
(Member set ordered most-recent-first; truncated per rule 6 when over the input cap.)

**Output schema:**
```json
{
  "summary": "<= ~512 tokens, what happened across this scope, faithful to member facts>",
  "story":   "<short processing-lineage note: 'rolled up N facts on <scope> <scope_id> spanning <range>'>"
}
```

**Constraints.**
- `summary` MUST be grounded only in `member_facts` — no outside knowledge, no invented facts,
  no fact_ids in prose. It is embedded as the digest-tier vector; faithfulness drives recall.
- `story` is a **server-lineage artifact distinct from envelope provenance** (ISI-5801, R7):
  how the server derived this digest, never who/what/trust of a fact.
- MUST NOT emit member bodies verbatim as "the answer" or any authority fields.

**Server post-processing.** Store `{digest.id, scope, scope_id, member_fact_ids[], story_id,
derivation_version}`; embed `summary` via `Embedder.embed(task=passage)` → digest tier. Idempotent
on `(scope, scope_id, scope_version, model_id, prompt_version)`.

---

## Contract: `categorize` — emergent labels, schema-constrained (W-categorize)

**prompt_version:** `categorize/v1`

**Purpose.** Propose emergent category labels over `fact.body`, later folded into a **per-project
controlled vocabulary** by a separate (non-LLM) consolidation job, then written back as `category[]`
facets (§5.4.3, D4). Emergent → controlled; never a free-tag swamp.

**Input (server-assembled):**
```json
{
  "fact_body": "<untrusted, delimited>",
  "fact_type": "decision|observation|…",
  "known_vocabulary": ["rollout-risk","perf-regression","…"]
}
```
(`known_vocabulary` = the current project-scoped controlled vocabulary, to bias reuse over churn.)

**Output schema:**
```json
{
  "labels": [
    { "label": "<kebab-case, <=3 words>", "confidence": 0.0-1.0,
      "reused_from_vocabulary": true|false }
  ]
}
```

**Constraints.**
- `label` MUST be a short kebab-case string (schema-enforced shape) — never a structured key,
  never provenance (R8). Max labels per fact capped (e.g. 5).
- The model is **constrained to emit from / minimally extend** `known_vocabulary`: prefer reuse;
  propose a new label only when no existing one fits (`reused_from_vocabulary:false`).
- Confidence below a server threshold → dropped before consolidation.

**Server post-processing.** Emergent labels feed the periodic **consolidation job** (not an LLM):
near-duplicate/low-frequency labels merged/pruned into the project vocabulary (R4); survivors
written back as `category[]` derived-attribute facets, stamped `derivation_version`, available as a
pre-filter in keyed fetch and the semantic scope step (§5.4.3). Idempotent on
`(fact.id, model_id, prompt_version)`.

---

## Change-log

| Date | Contract | prompt_version | Change |
|---|---|---|---|
| 2026-10-14 | extract | extract/v1 | initial |
| 2026-10-14 | summarize | summarize/v1 | initial |
| 2026-10-14 | categorize | categorize/v1 | initial |
