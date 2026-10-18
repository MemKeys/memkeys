//! M6 — the generative-model port (ADR-llm-pipeline Decision 1, WS-3).
//!
//! Two model roles sit behind two interfaces; the embedder role shipped in M5
//! ([`Embedder`](crate::semantic::Embedder), `embed.rs`). This module is the
//! second role: the **generative** endpoint — `Generator.generate(task, input,
//! schema) → structured output` — serving the three prompt contracts
//! (`extract` / `summarize` / `categorize`, `docs/contracts/prompt-contracts.md`)
//! from ONE configured endpoint (fewer moving parts; one place to version, one
//! cost meter).
//!
//! Two concrete generators sit behind the port, mirroring the embedder split:
//! - [`TemplateGenerator`] — the deterministic, dependency-free default. Lets a
//!   minimal deployment (and CI) run stages 5c-5e with no model server. It is
//!   explicitly NOT a language model: fixed heuristics with the same output
//!   schemas. Deterministic ⇒ idempotent re-runs.
//! - [`HttpGenerator`] — calls a real generative endpoint over HTTP. A thin
//!   shim a deployment adapts to its model server; no Python/core dependency.
//!
//! **R8 injection containment (hard rule, both adapters).** `fact.body` is
//! untrusted agent content: the model's output is *validated against the
//! contract schema* before any write ([`validate_extract`] /
//! [`validate_summarize`] / [`validate_categorize`]), proposed keys are never
//! authoritative, and the model never emits provenance. A parse/schema failure
//! is a POISON condition (ADR Decision 8) — mapped to
//! [`GenError::InvalidOutput`] — never a silent bad write.
//!
//! **OTel (signal catalog, WS-3).** Every generative call goes through
//! [`generate`] — the single helper — which emits the gen_ai CLIENT span
//! `"{gen_ai.operation.name} {gen_ai.request.model}"` with the pinned
//! experimental semconv attributes (never prompt/completion content) and
//! records `gen_ai.client.token.usage` / `gen_ai.client.operation.duration`.

use std::sync::Arc;

use opentelemetry::trace::{Span, SpanKind, Status};

use crate::telemetry::{self, Instruments};

/// Environment variable naming the generative endpoint. Unset ⇒
/// [`TemplateGenerator`].
pub const GEN_URL_ENV: &str = "MEMKEYS_GEN_URL";
/// Environment variable naming the generative model id (HTTP adapter).
pub const GEN_MODEL_ENV: &str = "MEMKEYS_GEN_MODEL";

/// `gen_ai.system` for the HTTP adapter (the endpoint is self-hosted / generic).
pub const GEN_AI_SYSTEM_HTTP: &str = "memkeys-http-adapter";
/// `gen_ai.system` for the deterministic template default.
pub const GEN_AI_SYSTEM_TEMPLATE: &str = "memkeys-template";

// ---------------------------------------------------------------------------
// Prompt contracts (docs/contracts/prompt-contracts.md) — the versioned unit
// behind `prompt_version` in `derivation_version = (model_id, prompt_version)`.
// ---------------------------------------------------------------------------

/// The three generative tasks. Each selects its prompt contract; the
/// `prompt_version` values are monotonic strings bumped whenever wording,
/// schema, or constraints change (a bump is a change-log backfill event,
/// ADR Decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenTask {
    /// Entity-key proposal (W-extract, stage 5e) — `extract/v1`.
    Extract,
    /// Scope rollup + story (W-digest, stage 5c) — `summarize/v1`.
    Summarize,
    /// Emergent category labels (W-categorize, stage 5d) — `categorize/v1`.
    Categorize,
}

impl GenTask {
    /// The active `prompt_version` of this task's contract.
    pub fn prompt_version(self) -> &'static str {
        match self {
            GenTask::Extract => "extract/v1",
            GenTask::Summarize => "summarize/v1",
            GenTask::Categorize => "categorize/v1",
        }
    }

    /// The wire name used on the HTTP adapter's request.
    pub fn wire_name(self) -> &'static str {
        match self {
            GenTask::Extract => "extract",
            GenTask::Summarize => "summarize",
            GenTask::Categorize => "categorize",
        }
    }
}

/// `extract` input — server-assembled; `fact_body` is untrusted content.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractInput {
    pub fact_body: String,
    pub existing_keys: std::collections::HashMap<String, String>,
    pub allowed_key_space: Vec<String>,
}

/// One proposed key (`extract` output). Never authoritative — the server
/// validates before widening any candidate set.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProposedKey {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub evidence_span: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ExtractOutput {
    #[serde(default)]
    pub proposed_keys: Vec<ProposedKey>,
}

/// One member fact of a scope rollup (`summarize` input). Deserialize too —
/// the template adapter round-trips the assembled input.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemberFact {
    pub fact_id: uuid::Uuid,
    pub fact_type: String,
    pub event_time: String,
    pub body: String,
}

/// `summarize` input — server-assembled from the scope's current member facts.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SummarizeInput {
    pub scope: String,
    scope_id: String,
    pub member_facts: Vec<MemberFact>,
}

impl SummarizeInput {
    pub fn new(
        scope: impl Into<String>,
        scope_id: impl Into<String>,
        members: Vec<MemberFact>,
    ) -> Self {
        Self {
            scope: scope.into(),
            scope_id: scope_id.into(),
            member_facts: members,
        }
    }

    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }
}

/// `summarize` output — the rollup summary + the server "story" lineage note.
/// The model proposes content only; the SERVER stamps ids / member ids /
/// versions (R8: the model never emits authority).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SummarizeOutput {
    pub summary: String,
    #[serde(default)]
    pub story: Option<String>,
}

/// `categorize` input — one fact body (batched per call where the contract
/// allows; M6 runs per fact).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CategorizeInput {
    pub fact_body: String,
}

/// One emergent label (`categorize` output): a bounded label string +
/// optional confidence. The model never emits keys or provenance (R8).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CategorizeLabel {
    pub label: String,
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct CategorizeOutput {
    #[serde(default)]
    pub labels: Vec<CategorizeLabel>,
}

// ---------------------------------------------------------------------------
// Validation — the schema guard (ADR Decision 8). Runs on EVERY adapter's
// output before any write; a violation is a poison condition.
// ---------------------------------------------------------------------------

/// Hard cap on label text (bounded output; a runaway categorizer would
/// otherwise write unbounded strings into the facet surface).
const LABEL_MAX_LEN: usize = 64;
/// Hard cap on a proposal value (same reasoning, on the derived-key surface).
const KEY_VALUE_MAX_LEN: usize = 128;
/// Hard cap on the summary (contract rule 6 token budget, byte form).
const SUMMARY_MAX_LEN: usize = 8_192;
/// Hard cap on the story lineage note.
const STORY_MAX_LEN: usize = 2_048;

/// Failure of the generative port. `InvalidOutput` is the POISON condition
/// (schema-invalid generative output — never a silent bad write).
#[derive(Debug, thiserror::Error)]
pub enum GenError {
    #[error("generative endpoint unavailable: {0}")]
    Unavailable(String),
    #[error("generative output failed contract validation: {0}")]
    InvalidOutput(String),
}

/// Validate an `extract` output against the contract:
/// - `key` MUST be in the allowed key space (anything else is dropped, not an
///   error — the contract says the server drops unknown keys),
/// - MUST NOT propose a key already in `existing_keys` (no overwrite path),
/// - `evidence_span` MUST be a verbatim substring of `fact_body` (the
///   anti-hallucination guard — a non-substring span invalidates the proposal),
/// - value/confidence bounds.
///
/// The typed structs simply cannot carry provenance/tenant authority (unknown
/// JSON fields are ignored at deserialization, never persisted). Returns the
/// ACCEPTED proposals; a proposal set whose every member fails is a valid,
/// empty answer.
pub fn validate_extract(
    output: &ExtractOutput,
    fact_body: &str,
    existing_keys: &std::collections::HashMap<String, String>,
    allowed_key_space: &[String],
) -> Vec<(ProposedKey, f64)> {
    output
        .proposed_keys
        .iter()
        .filter_map(|p| {
            if !allowed_key_space.iter().any(|k| k == &p.key) {
                return None; // unknown key → dropped by the server, not an error
            }
            if existing_keys.contains_key(&p.key) {
                return None; // no overwrite path exists (R8)
            }
            if p.value.trim().is_empty() || p.value.len() > KEY_VALUE_MAX_LEN {
                return None;
            }
            let evidence = p.evidence_span.as_deref()?;
            if evidence.trim().is_empty() || !fact_body.contains(evidence) {
                return None; // anti-hallucination: MUST be a verbatim substring
            }
            let confidence = p.confidence.unwrap_or(1.0).clamp(0.0, 1.0);
            Some((p.clone(), confidence))
        })
        .collect()
}

/// Validate a `summarize` output: non-empty bounded summary (a digest is a
/// navigational pointer, R1 — never the answer), optional bounded story.
pub fn validate_summarize(output: &SummarizeOutput) -> Result<(), GenError> {
    if output.summary.trim().is_empty() {
        return Err(GenError::InvalidOutput("empty summary".into()));
    }
    if output.summary.len() > SUMMARY_MAX_LEN {
        return Err(GenError::InvalidOutput("summary over contract cap".into()));
    }
    if let Some(story) = &output.story {
        if story.len() > STORY_MAX_LEN {
            return Err(GenError::InvalidOutput("story over contract cap".into()));
        }
    }
    Ok(())
}

/// Validate a `categorize` output: bounded, deduplicated, lowercase-normalized
/// labels with clamped confidence. The controlled-vocabulary consolidation is
/// the M7 reconciler's job (§5.4.3); stage 5d only produces emergent labels.
pub fn validate_categorize(output: &CategorizeOutput) -> Vec<(String, f64)> {
    let mut seen = std::collections::BTreeSet::new();
    output
        .labels
        .iter()
        .filter_map(|l| {
            let label = l.label.trim().to_lowercase();
            if label.is_empty() || label.len() > LABEL_MAX_LEN {
                return None;
            }
            if label.chars().any(|c| c.is_control()) {
                return None;
            }
            if !seen.insert(label.clone()) {
                return None; // dedup
            }
            let confidence = l.confidence.unwrap_or(1.0).clamp(0.0, 1.0);
            Some((label, confidence))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The port + the single OTel-instrumented call helper.
// ---------------------------------------------------------------------------

/// The generative-model port (ADR Decision 1). One endpoint, three prompt
/// contracts. Implementations MUST be deterministic within a
/// `(model_id, prompt_version)` so retries/backfills are idempotent.
#[async_trait::async_trait]
pub trait Generator: Send + Sync {
    /// Identifies the generator build — the `model_id` half of
    /// `derivation_version` stamped on every derived artifact.
    fn model_id(&self) -> &str;

    /// The `gen_ai.system` attribute (which adapter is talking).
    fn system(&self) -> &'static str;

    /// One raw generative call: task-selected prompt contract, JSON in, JSON
    /// out. The SERVER validates the output against the contract schema before
    /// any write (Decision 8) — adapters never persist directly.
    async fn generate(
        &self,
        task: GenTask,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, GenError>;
}

/// The single instrumented call helper (signal catalog: the gen_ai semconv is
/// "pinned, isolated in a single helper"). Emits the CLIENT span
/// `"{gen_ai.operation.name} {gen_ai.request.model}"` (= `chat <model>`) with
/// the experimental gen_ai attributes — NEVER prompt/completion content — plus
/// the `gen_ai.client.token.usage` / `gen_ai.client.operation.duration`
/// metrics. Token counts are byte/4 estimates (documented approximation until
/// adapters surface real usage on the wire).
pub async fn generate(
    generator: &dyn Generator,
    task: GenTask,
    input: &serde_json::Value,
    instruments: &Instruments,
) -> Result<serde_json::Value, GenError> {
    let model = generator.model_id();
    let system = generator.system();
    let input_tokens = estimate_tokens(input);
    // Span name per the semconv convention "{operation} {model}" (dynamic —
    // the model id is configuration, not a compile-time constant).
    let mut span = telemetry::start_dynamic_span(
        format!("chat {model}"),
        SpanKind::Client,
        vec![
            telemetry::gen_attr("gen_ai.operation.name", "chat"),
            telemetry::gen_attr("gen_ai.system", system),
            telemetry::gen_attr("gen_ai.request.model", model.to_string()),
        ],
    );
    let t = std::time::Instant::now();
    let outcome = generator.generate(task, input).await;
    let seconds = t.elapsed().as_secs_f64();
    let output_tokens = outcome.as_ref().map(estimate_tokens).unwrap_or(0);
    match &outcome {
        Ok(_) => span.set_status(Status::Ok),
        Err(e) => span.set_status(Status::Error {
            description: e.to_string().into(),
        }),
    }
    span.set_attribute(telemetry::gen_attr(
        "gen_ai.response.model",
        model.to_string(),
    ));
    span.set_attribute(telemetry::gen_attr(
        "gen_ai.usage.input_tokens",
        input_tokens,
    ));
    span.set_attribute(telemetry::gen_attr(
        "gen_ai.usage.output_tokens",
        output_tokens,
    ));
    span.end();
    instruments.llm_token_usage(model, system, input_tokens, output_tokens);
    instruments.llm_operation_duration(model, system, seconds);
    outcome
}

/// Rough token estimate (bytes/4) for adapters that do not report usage.
fn estimate_tokens(v: &serde_json::Value) -> i64 {
    (serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) / 4) as i64
}

// ---------------------------------------------------------------------------
// The deterministic default.
// ---------------------------------------------------------------------------

/// The deterministic, dependency-free generative default (minimal deployments
/// and CI). Same output schemas as the real contracts; fixed heuristics
/// instead of a model. Deterministic means stage re-runs are idempotent.
#[derive(Debug, Clone)]
pub struct TemplateGenerator {
    model_id: String,
}

impl TemplateGenerator {
    pub fn new() -> Self {
        Self {
            model_id: "memkeys-template-v1".into(),
        }
    }
}

/// One deterministic extraction heuristic: after a case-insensitive prefix
/// (`isi-`, `pr`, `project:`, …), capture a bounded value (digits or ident).
struct ExtractPattern {
    key: &'static str,
    prefixes: &'static [&'static str],
    /// `true` → capture a digit run; `false` → capture an ident token.
    digits: bool,
    /// Emit the value uppercased (issue ids: `ISI-6080`).
    upper: bool,
}

const EXTRACT_PATTERNS: &[ExtractPattern] = &[
    ExtractPattern {
        key: "entity.issue",
        prefixes: &["isi-"],
        digits: true,
        upper: true,
    },
    ExtractPattern {
        key: "entity.pr",
        prefixes: &["pr ", "pr#", "pull request ", "#"],
        digits: true,
        upper: false,
    },
    ExtractPattern {
        key: "entity.project",
        prefixes: &["project"],
        digits: false,
        upper: false,
    },
    ExtractPattern {
        key: "entity.agent",
        prefixes: &["agent"],
        digits: false,
        upper: false,
    },
    ExtractPattern {
        key: "entity.run",
        prefixes: &["run"],
        digits: false,
        upper: false,
    },
    ExtractPattern {
        key: "entity.thread",
        prefixes: &["thread"],
        digits: false,
        upper: false,
    },
];

/// Head+tail truncation (contract rule 6: over-cap inputs are truncated with a
/// documented strategy — head+tail for bodies).
fn head_tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let head = cap / 2;
    // char-boundary-safe head
    let mut h = head.min(text.len());
    while h > 0 && !text.is_char_boundary(h) {
        h -= 1;
    }
    // char-boundary-safe tail: walk back from cap total
    let mut t = text.len().saturating_sub(cap - h);
    while t < text.len() && !text.is_char_boundary(t) {
        t += 1;
    }
    format!("{}…{}", &text[..h], &text[t..])
}

/// Case-insensitive `find` that returns the BYTE index in the ORIGINAL string
/// (to_lowercase can change byte lengths, so matching must be per-char).
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let nchars: Vec<char> = needle.chars().collect();
    for (i, c) in hay.char_indices().skip_while(|(i, _)| *i < from) {
        if c.to_lowercase().next() != nchars[0].to_lowercase().next() {
            continue;
        }
        let mut hi = i + c.len_utf8();
        let mut matched = true;
        for nc in &nchars[1..] {
            // advance one haystack char (multi-byte safe)
            let Some(hc) = hay[hi..].chars().next() else {
                matched = false;
                break;
            };
            if hc.to_lowercase().next() != nc.to_lowercase().next() {
                matched = false;
                break;
            }
            hi += hc.len_utf8();
        }
        if matched {
            return Some(i);
        }
    }
    None
}

/// The deterministic label vocabulary for the template categorizer.
const LABEL_VOCABULARY: &[(&str, &[&str])] = &[
    (
        "decision",
        &["decided", "decision", "we will", "going with", "agreed to"],
    ),
    ("risk", &["risk", "danger", "concern", "caveat"]),
    ("blocker", &["blocked", "blocker", "blocking", "stuck"]),
    (
        "performance",
        &["latency", "throughput", "p95", "benchmark", "slow"],
    ),
    (
        "security",
        &[
            "auth",
            "security",
            "token",
            "secret",
            "vulnerability",
            "injection",
        ],
    ),
    (
        "rollout",
        &["deploy", "rollout", "release", "helm", "kubernetes"],
    ),
    ("bug", &["bug", "broken", "failing", "regression", "crash"]),
    ("learning", &["learned", "lesson", "insight", "takeaway"]),
];

#[async_trait::async_trait]
impl Generator for TemplateGenerator {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn system(&self) -> &'static str {
        GEN_AI_SYSTEM_TEMPLATE
    }

    async fn generate(
        &self,
        task: GenTask,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, GenError> {
        match task {
            GenTask::Extract => {
                let body = input["fact_body"].as_str().unwrap_or_default();
                let existing: std::collections::HashMap<String, String> =
                    serde_json::from_value(input["existing_keys"].clone()).unwrap_or_default();
                let allowed: Vec<String> =
                    serde_json::from_value(input["allowed_key_space"].clone()).unwrap_or_default();
                let mut proposed = Vec::new();
                for pat in EXTRACT_PATTERNS {
                    if existing.contains_key(pat.key) {
                        continue; // no overwrite path (contract)
                    }
                    if !allowed.is_empty() && !allowed.iter().any(|k| k == pat.key) {
                        continue;
                    }
                    if let Some(m) = template_extract_one(body, pat) {
                        proposed.push(serde_json::json!({
                            "key": pat.key,
                            "value": m.value,
                            "confidence": 0.8,
                            "evidence_span": m.evidence,
                        }));
                    }
                }
                Ok(serde_json::json!({ "proposed_keys": proposed }))
            }
            GenTask::Summarize => {
                let members: Vec<MemberFact> =
                    serde_json::from_value(input["member_facts"].clone())
                        .map_err(|e| GenError::InvalidOutput(e.to_string()))?;
                if members.is_empty() {
                    return Err(GenError::InvalidOutput("no member facts".into()));
                }
                let n = members.len();
                let scope = input["scope"].as_str().unwrap_or("scope");
                let scope_id = input["scope_id"].as_str().unwrap_or_default();
                let first = members.first().map(|m| m.event_time.as_str()).unwrap_or("");
                let last = members.last().map(|m| m.event_time.as_str()).unwrap_or("");
                // Deterministic template: member bodies joined head+tail —
                // the digest is a pointer into ground truth (R1), never the answer.
                let joined = members
                    .iter()
                    .map(|m| head_tail(&m.body, 240))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let summary = head_tail(
                    &format!(
                        "Rollup of {n} facts on {scope} {scope_id} ({first} to {last}): {joined}"
                    ),
                    SUMMARY_MAX_LEN,
                );
                let story = head_tail(
                    &format!(
                        "rolled up {n} facts on {scope} {scope_id} spanning {first} to {last}"
                    ),
                    STORY_MAX_LEN,
                );
                Ok(serde_json::json!({ "summary": summary, "story": story }))
            }
            GenTask::Categorize => {
                let body = input["fact_body"].as_str().unwrap_or_default();
                let lower = body.to_lowercase();
                let mut labels = Vec::new();
                for (label, needles) in LABEL_VOCABULARY {
                    if needles.iter().any(|n| lower.contains(n)) {
                        labels.push(serde_json::json!({ "label": label, "confidence": 0.7 }));
                    }
                }
                Ok(serde_json::json!({ "labels": labels }))
            }
        }
    }
}

impl Default for TemplateGenerator {
    fn default() -> Self {
        Self::new()
    }
}

struct TemplateMatch {
    value: String,
    evidence: String,
}

/// Apply one [`ExtractPattern`]: find the first prefix occurrence (word-start
/// bounded), then capture the following digits or `ident` token. The evidence
/// span is the verbatim matched text, so the substring guard holds by
/// construction.
fn template_extract_one(body: &str, pat: &ExtractPattern) -> Option<TemplateMatch> {
    for prefix in pat.prefixes {
        let mut from = 0usize;
        while let Some(start) = find_ci(body, prefix, from) {
            from = start + 1;
            // Word-start bound: the char before the prefix must not be
            // alphanumeric (the `\b` in the real contract patterns).
            let word_start_ok = start == 0
                || !body[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric());
            if !word_start_ok {
                continue;
            }
            let mut i = start + prefix.len();
            // For ident patterns a separator run is REQUIRED between the
            // keyword and the value (`project: x`, `run x`) — without it the
            // keyword merely occurs inside another word (`runtime`), which is
            // not a key mention at all.
            if !pat.digits {
                let mut separators = 0usize;
                while i < body.len() && matches!(body.as_bytes()[i], b':' | b'=' | b' ' | b'\t') {
                    i += 1;
                    separators += 1;
                }
                if separators == 0 {
                    continue;
                }
            }
            let value_start = i;
            let bytes = body.as_bytes();
            let mut end = i;
            if pat.digits {
                // `pr 19` / `pr#19` / `#19`: skip an optional '#' before digits
                while end < bytes.len() && bytes[end] == b'#' {
                    end += 1;
                }
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
            } else {
                if end < bytes.len() && !bytes[end].is_ascii_alphanumeric() {
                    continue; // separator ran to nothing usable
                }
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'.'
                        || bytes[end] == b'_'
                        || bytes[end] == b'-')
                {
                    end += 1;
                }
            }
            let raw_value = &body[value_start..end];
            // The value must itself start with a digit/ident (strip a leading '#').
            let value = raw_value.trim_start_matches('#');
            if value.is_empty() {
                continue;
            }
            // Issue-style ids embed the prefix (`ISI-6080`); plain digit/ident
            // values stay verbatim.
            let value = if pat.upper {
                body[start..end].to_uppercase()
            } else {
                value.to_string()
            };
            // Evidence = the verbatim span from prefix start to value end.
            let evidence = &body[start..end];
            if evidence.len() > KEY_VALUE_MAX_LEN * 2 {
                continue; // degenerate span — drop rather than write a huge row
            }
            return Some(TemplateMatch {
                value,
                evidence: evidence.to_string(),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The pluggable HTTP endpoint adapter.
// ---------------------------------------------------------------------------

/// An HTTP generative-endpoint adapter — the pluggable endpoint path
/// (ADR/plan: the generative role is a configured endpoint, never a code
/// dependency; no Python in the core).
///
/// Wire contract (thin shim, same philosophy as `HttpEmbedder`):
/// `POST <url>` with `{"task": "extract", "model": "<model_id>", "input":
/// {...}}` → `{"output": {...}}`. The endpoint owns prompting/tool mode; the
/// server validates the returned output against the contract schema before
/// any write (Decision 8).
pub struct HttpGenerator {
    url: String,
    model_id: String,
    client: reqwest::Client,
}

#[derive(serde::Serialize)]
struct GenRequest<'a> {
    task: &'a str,
    model: &'a str,
    input: &'a serde_json::Value,
}

#[derive(serde::Deserialize)]
struct GenResponse {
    output: serde_json::Value,
}

impl HttpGenerator {
    pub fn new(url: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            model_id: model_id.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl Generator for HttpGenerator {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn system(&self) -> &'static str {
        GEN_AI_SYSTEM_HTTP
    }

    async fn generate(
        &self,
        task: GenTask,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, GenError> {
        let resp = self
            .client
            .post(&self.url)
            .json(&GenRequest {
                task: task.wire_name(),
                model: &self.model_id,
                input,
            })
            .send()
            .await
            .map_err(|e| GenError::Unavailable(format!("generative request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(GenError::Unavailable(format!(
                "generative endpoint returned {}",
                resp.status()
            )));
        }
        let body: GenResponse = resp.json().await.map_err(|e| {
            // A malformed endpoint response is a POISON condition, not a
            // transient one (Decision 8 schema guard) — retriable only on the
            // next derivation_version bump.
            GenError::InvalidOutput(format!("generative response decode failed: {e}"))
        })?;
        Ok(body.output)
    }
}

/// The deployment generative-endpoint factory (the single wiring point).
/// `MEMKEYS_GEN_URL` set ⇒ [`HttpGenerator`] (model id from
/// [`GEN_MODEL_ENV`], default `memkeys-generative-default`); unset ⇒ the
/// deterministic [`TemplateGenerator`] fallback so a minimal deployment runs
/// the LLM pipeline stages with no model server.
pub fn from_env() -> Arc<dyn Generator> {
    match std::env::var(GEN_URL_ENV).ok().filter(|s| !s.is_empty()) {
        Some(url) => {
            let model = std::env::var(GEN_MODEL_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "memkeys-generative-default".into());
            tracing::info!(
                target: "memkeys",
                "memkeys.generative.http: using HTTP generative endpoint at {url} (model {model})"
            );
            Arc::new(HttpGenerator::new(url, model))
        }
        None => {
            tracing::info!(
                target: "memkeys",
                "memkeys.generative.template: MEMKEYS_GEN_URL unset — using the deterministic \
                 TemplateGenerator fallback"
            );
            Arc::new(TemplateGenerator::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str =
        "We decided to ship the rollout on thread t-42; see ISI-6080 for the plan and PR 19.";

    #[test]
    fn prompt_versions_are_declared() {
        assert_eq!(GenTask::Extract.prompt_version(), "extract/v1");
        assert_eq!(GenTask::Summarize.prompt_version(), "summarize/v1");
        assert_eq!(GenTask::Categorize.prompt_version(), "categorize/v1");
    }

    #[tokio::test]
    async fn template_extract_proposes_verbatim_evidence_keys() {
        let g = TemplateGenerator::new();
        let input = serde_json::json!({
            "fact_body": BODY,
            "existing_keys": {},
            "allowed_key_space": ["entity.issue", "entity.pr", "entity.thread"],
        });
        let out: ExtractOutput =
            serde_json::from_value(g.generate(GenTask::Extract, &input).await.unwrap()).unwrap();
        let accepted = validate_extract(
            &out,
            BODY,
            &std::collections::HashMap::new(),
            &[
                "entity.issue".to_string(),
                "entity.pr".to_string(),
                "entity.thread".to_string(),
            ],
        );
        assert!(accepted
            .iter()
            .any(|(p, _)| p.key == "entity.issue" && p.value == "ISI-6080"));
        assert!(accepted
            .iter()
            .any(|(p, _)| p.key == "entity.pr" && p.value == "19"));
        assert!(accepted
            .iter()
            .any(|(p, _)| p.key == "entity.thread" && p.value == "t-42"));
        // evidence must be a verbatim substring (anti-hallucination guard)
        for (p, _) in &accepted {
            assert!(BODY.contains(p.evidence_span.as_deref().unwrap()));
        }
    }

    #[tokio::test]
    async fn template_extract_respects_existing_keys() {
        let g = TemplateGenerator::new();
        let mut existing = std::collections::HashMap::new();
        existing.insert("entity.issue".to_string(), "ISI-1".to_string());
        let input = serde_json::json!({
            "fact_body": BODY,
            "existing_keys": { "entity.issue": "ISI-1" },
            "allowed_key_space": ["entity.issue", "entity.pr", "entity.thread"],
        });
        let out: ExtractOutput =
            serde_json::from_value(g.generate(GenTask::Extract, &input).await.unwrap()).unwrap();
        assert!(!out.proposed_keys.iter().any(|p| p.key == "entity.issue"));
    }

    #[test]
    fn validate_extract_drops_unknown_keys_overwrites_and_bad_evidence() {
        let mut existing = std::collections::HashMap::new();
        existing.insert("entity.issue".to_string(), "ISI-1".to_string());
        let out = ExtractOutput {
            proposed_keys: vec![
                ProposedKey {
                    key: "entity.tenant".into(), // not in allowed space → dropped
                    value: "acme".into(),
                    confidence: Some(1.0),
                    evidence_span: Some("acme".into()),
                },
                ProposedKey {
                    key: "entity.issue".into(), // already keyed → dropped
                    value: "ISI-2".into(),
                    confidence: Some(1.0),
                    evidence_span: Some("ISI-2".into()),
                },
                ProposedKey {
                    key: "entity.pr".into(), // evidence NOT a substring → dropped
                    value: "19".into(),
                    confidence: Some(1.0),
                    evidence_span: Some("PR 19 exists somewhere else".into()),
                },
                ProposedKey {
                    key: "entity.pr".into(), // valid
                    value: "19".into(),
                    confidence: Some(2.0), // clamped
                    evidence_span: Some("PR 19".into()),
                },
            ],
        };
        let accepted = validate_extract(
            &out,
            "see PR 19 merged",
            &existing,
            &["entity.pr".to_string()],
        );
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].0.key, "entity.pr");
        assert_eq!(accepted[0].1, 1.0); // clamped from 2.0
    }

    #[tokio::test]
    async fn template_summarize_is_deterministic_and_valid() {
        let g = TemplateGenerator::new();
        let members = vec![
            MemberFact {
                fact_id: uuid::Uuid::new_v4(),
                fact_type: "decision".into(),
                event_time: "2026-10-18T10:00:00Z".into(),
                body: "chose postgres".into(),
            },
            MemberFact {
                fact_id: uuid::Uuid::new_v4(),
                fact_type: "outcome".into(),
                event_time: "2026-10-18T11:00:00Z".into(),
                body: "p95 under 150ms".into(),
            },
        ];
        let input = serde_json::to_value(SummarizeInput::new("thread", "t-1", members)).unwrap();
        let a: SummarizeOutput =
            serde_json::from_value(g.generate(GenTask::Summarize, &input).await.unwrap()).unwrap();
        let b: SummarizeOutput =
            serde_json::from_value(g.generate(GenTask::Summarize, &input).await.unwrap()).unwrap();
        assert_eq!(a.summary, b.summary); // deterministic ⇒ idempotent re-roll
        assert!(a.summary.contains("2 facts"));
        assert!(a.story.as_deref().unwrap().contains("rolled up 2 facts"));
        assert!(validate_summarize(&a).is_ok());
    }

    #[test]
    fn validate_summarize_rejects_empty_and_oversized() {
        assert!(validate_summarize(&SummarizeOutput {
            summary: "   ".into(),
            story: None,
        })
        .is_err());
        assert!(validate_summarize(&SummarizeOutput {
            summary: "x".repeat(SUMMARY_MAX_LEN + 1),
            story: None,
        })
        .is_err());
    }

    #[tokio::test]
    async fn template_categorize_emits_vocabulary_labels() {
        let g = TemplateGenerator::new();
        let input =
            serde_json::json!({ "fact_body": "The rollout is blocked by a security token bug." });
        let out: CategorizeOutput =
            serde_json::from_value(g.generate(GenTask::Categorize, &input).await.unwrap()).unwrap();
        let labels = validate_categorize(&out);
        let names: Vec<&str> = labels.iter().map(|(l, _)| l.as_str()).collect();
        assert!(names.contains(&"blocker"));
        assert!(names.contains(&"security"));
        assert!(names.contains(&"rollout"));
    }

    #[test]
    fn validate_categorize_normalizes_and_dedups() {
        let out = CategorizeOutput {
            labels: vec![
                CategorizeLabel {
                    label: "  Rollout ".into(),
                    confidence: Some(0.9),
                },
                CategorizeLabel {
                    label: "rollout".into(), // dup after normalize
                    confidence: Some(0.5),
                },
                CategorizeLabel {
                    label: "x".repeat(80), // over cap → dropped
                    confidence: Some(1.0),
                },
            ],
        };
        let labels = validate_categorize(&out);
        assert_eq!(labels, vec![("rollout".to_string(), 0.9)]);
    }

    #[test]
    fn head_tail_truncates_at_char_boundary() {
        let s = "abcde";
        assert_eq!(head_tail(s, 10), s);
        let long = "x".repeat(100);
        let cut = head_tail(&long, 10);
        assert_eq!(cut.len(), 13); // 5 head + 3-byte ellipsis + 5 tail
                                   // Unicode safety: no panic on multi-byte boundaries.
        let uni = "é".repeat(50);
        assert!(!head_tail(&uni, 7).is_empty());
    }

    #[test]
    fn find_ci_is_index_safe() {
        assert_eq!(find_ci("see PR 19 merged", "pr ", 0), Some(4));
        assert_eq!(find_ci("see pr 19 merged", "PR ", 0), Some(4));
        assert_eq!(find_ci("nothing here", "pr", 0), None);
    }

    #[tokio::test]
    async fn template_extract_from_ident_style_bodies() {
        let g = TemplateGenerator::new();
        let body = "Agent codex finished run run-77 for project memkeys";
        let input = serde_json::json!({
            "fact_body": body,
            "existing_keys": {},
            "allowed_key_space": ["entity.agent", "entity.run", "entity.project"],
        });
        let out: ExtractOutput =
            serde_json::from_value(g.generate(GenTask::Extract, &input).await.unwrap()).unwrap();
        let accepted = validate_extract(
            &out,
            body,
            &std::collections::HashMap::new(),
            &[
                "entity.agent".to_string(),
                "entity.run".to_string(),
                "entity.project".to_string(),
            ],
        );
        assert!(accepted
            .iter()
            .any(|(p, _)| p.key == "entity.agent" && p.value == "codex"));
        assert!(accepted
            .iter()
            .any(|(p, _)| p.key == "entity.run" && p.value == "run-77"));
        assert!(accepted
            .iter()
            .any(|(p, _)| p.key == "entity.project" && p.value == "memkeys"));
    }
}
