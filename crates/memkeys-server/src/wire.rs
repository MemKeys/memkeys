//! Wire contract for the M3 adoption surface — the shapes that cross the
//! HTTP `/memory/*` boundary (WS-4 §7) and the MCP tool boundary (WS-5
//! `docs/design/mcp-contract.md`). Both transports share these types verbatim:
//! the MCP tools are thin wrappers over the same request/response JSON (WS-5
//! §2.3 "byte-for-byte the same").
//!
//! The error taxonomy (WS-5 §7) is the single source of truth for how every
//! failure — on either transport — maps to a stable code, an HTTP status, and
//! a `retriable` flag. `forbidden`/`not_found` collapse (§3.3) and the
//! degrade-don't-fail rule (§7) are encoded here, not at the call sites.

use serde::{Deserialize, Serialize};

/// The stable, deployment-independent error taxonomy (WS-5 §7). Every variant
/// carries a fixed wire `code`, an HTTP origin status, and whether a client
/// should retry. MCP surfaces these as a JSON-RPC error `data` payload; HTTP
/// surfaces them as the status + a JSON body of the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// No/invalid/expired token (401). HTTP: refresh; stdio: relaunch.
    Unauthenticated,
    /// Authenticated but the action is disallowed by deployment policy (403).
    /// NEVER used for cross-tenant ids (those are `NotFound`, §3.3) nor a bad
    /// `source.kind` (that is `InvalidEnvelope`, F16).
    Forbidden,
    /// Id not found *within the caller's tenant* — includes every cross-tenant
    /// id and an unowned `supersedes` target (fail-closed, §3.2–3.3, F7). Does
    /// not leak existence across the tenant boundary.
    NotFound,
    /// Write failed schema/anchor/size validation (422): missing anchor,
    /// `supersedes` without `subject`, unknown `source.kind` (F16), body over
    /// `max_body_bytes` or attachments over `max_attachments` (F14).
    InvalidEnvelope,
    /// Read argument malformed (400): no `entity.*` on `get`, bad/future
    /// `as_of` (F13), depth over cap.
    InvalidArgument,
    /// Cursor signature-verify failed / wrong tool / wrong tenant / expired
    /// snapshot (400). The cursor is server-signed (§6.1, F2).
    BadCursor,
    /// Semantic module off for `search`; whole semantic module off for
    /// `recall` (501). Never a silent empty result.
    FeatureDisabled,
    /// Over the request/token budget OR over the boundary-crossing-`not_found`
    /// probe threshold (429, §3.3/F15). Retriable with backoff.
    RateLimited,
    /// SoT write/read path down (503). Retriable. A derived-view lag is NEVER
    /// this — keyed reads still serve with a `freshness_lag` warning (§7).
    BackendUnavailable,
    /// Idempotency/version conflict on write (409). Returns the existing id.
    Conflict,
}

impl ErrorCode {
    /// The stable wire string (WS-5 §7 table, column 1).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unauthenticated => "unauthenticated",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::NotFound => "not_found",
            ErrorCode::InvalidEnvelope => "invalid_envelope",
            ErrorCode::InvalidArgument => "invalid_argument",
            ErrorCode::BadCursor => "bad_cursor",
            ErrorCode::FeatureDisabled => "feature_disabled",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::BackendUnavailable => "backend_unavailable",
            ErrorCode::Conflict => "conflict",
        }
    }

    /// HTTP origin status (WS-5 §7 table, "HTTP origin" column).
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::Unauthenticated => 401,
            ErrorCode::Forbidden => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::InvalidEnvelope => 422,
            ErrorCode::InvalidArgument => 400,
            ErrorCode::BadCursor => 400,
            ErrorCode::FeatureDisabled => 501,
            ErrorCode::RateLimited => 429,
            ErrorCode::BackendUnavailable => 503,
            ErrorCode::Conflict => 409,
        }
    }

    /// Whether a client should retry (WS-5 §7 table, "Retriable" column).
    pub fn retriable(self) -> bool {
        matches!(self, ErrorCode::RateLimited | ErrorCode::BackendUnavailable)
    }
}

/// The structured error body carried on both transports (WS-5 §7). On MCP this
/// is the JSON-RPC error `data`; on HTTP it is the response JSON with the
/// status set from [`ErrorCode::http_status`].
#[derive(Debug, Clone, Serialize)]
pub struct WireError {
    pub code: &'static str,
    pub retriable: bool,
    pub detail: String,
    /// What the caller should do (WS-5 §7 "hint"); omitted when there is
    /// nothing actionable beyond the code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Retry-after seconds when known (WS-5 §7 `rate_limited`/`backend_unavailable`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
}

impl WireError {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code: code.as_str(),
            retriable: code.retriable(),
            detail: detail.into(),
            hint: None,
            retry_after_secs: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// The [`ErrorCode`] this error carries (parsed back from the wire string).
    pub fn error_code(&self) -> ErrorCode {
        match self.code {
            "unauthenticated" => ErrorCode::Unauthenticated,
            "forbidden" => ErrorCode::Forbidden,
            "not_found" => ErrorCode::NotFound,
            "invalid_envelope" => ErrorCode::InvalidEnvelope,
            "invalid_argument" => ErrorCode::InvalidArgument,
            "bad_cursor" => ErrorCode::BadCursor,
            "feature_disabled" => ErrorCode::FeatureDisabled,
            "rate_limited" => ErrorCode::RateLimited,
            "backend_unavailable" => ErrorCode::BackendUnavailable,
            "conflict" => ErrorCode::Conflict,
            _ => ErrorCode::BackendUnavailable,
        }
    }
}

/// `memory.write` response (WS-5 §4.1 / WS-4 §7.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResponse {
    pub fact_id: uuid::Uuid,
    pub keyed: bool,
    /// Server-resolved entity keys, `entity.*` wire name → value (§7.1).
    pub entity_keys_resolved: std::collections::BTreeMap<String, String>,
    /// Hint keys that failed validation — recorded, never silently dropped (§4).
    #[serde(default)]
    pub rejected_hints: Vec<String>,
    /// True when a retried flush resolved to the existing row (no duplicate).
    #[serde(default)]
    pub duplicate: bool,
}

/// `memory.get` response (WS-5 §4.2 / WS-4 §7.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResponse {
    pub facts: Vec<ShapedFact>,
    /// Present when more results exist, absent when exhausted (§6.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Approximate tokens in this payload, for caller budgeting (§6.2).
    pub token_estimate: u64,
    /// Derived-view/degrade warnings on an otherwise-successful keyed read
    /// (§7 degrade-don't-fail). Empty on the hot path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// A fact in the WS-5 §6.2 response shape. Compact (default) returns a snippet
/// and the keys; full adds the whole body, confidence, and provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShapedFact {
    pub fact_id: uuid::Uuid,
    #[serde(rename = "type")]
    pub fact_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Compact: `body_snippet` (truncated to `body_max_chars`). Full: the whole
    /// body is in `body` and this is the same leading snippet for convenience.
    pub body_snippet: String,
    /// Full view only (§6.2 knob 1): the complete body text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// True when `body_snippet` was cut at `body_max_chars` (§6.2 knob 2) — the
    /// agent fetches the full body via `memory.get` full view only if needed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub event_time: chrono::DateTime<chrono::Utc>,
    /// `entity.*` wire name → value (resolved keys, §7.5).
    pub entity_keys: std::collections::BTreeMap<String, String>,
    /// Full view only: confidence (§6.2 knob 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Full view only: provenance principal (§6.2 knob 1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `memory.traverse` response (WS-5 §4.3 / WS-4 §7.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseResponse {
    pub nodes: Vec<TraverseNode>,
    pub edges: Vec<TraverseEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub truncated: bool,
    /// The §4.3 supersession-specific staleness flag — set only on a
    /// `supersedes`-edge walk the server could not resolve from the SoT (F9).
    /// Distinct from a generic `freshness_lag` warning.
    #[serde(default, skip_serializing_if = "is_false")]
    pub supersession_stale: bool,
    pub token_estimate: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseNode {
    pub kind: String,
    pub id: String,
    /// The hydrated record (§7.5), present only when `hydrate=true` succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<ShapedFact>,
    /// Hydrate requested but the keyed fetch missed (projection hole) — topology
    /// kept, body omitted, never served stale (§7.5).
    #[serde(default, skip_serializing_if = "is_false")]
    pub unhydratable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseEdge {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub edge_type: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_status_and_retriable_match_spec() {
        // WS-5 §7 table spot-checks.
        assert_eq!(ErrorCode::NotFound.http_status(), 404);
        assert!(!ErrorCode::NotFound.retriable());
        assert_eq!(ErrorCode::RateLimited.http_status(), 429);
        assert!(ErrorCode::RateLimited.retriable());
        assert_eq!(ErrorCode::BackendUnavailable.http_status(), 503);
        assert!(ErrorCode::BackendUnavailable.retriable());
        assert_eq!(ErrorCode::FeatureDisabled.http_status(), 501);
        assert_eq!(ErrorCode::Conflict.http_status(), 409);
        assert_eq!(ErrorCode::InvalidEnvelope.http_status(), 422);
    }

    #[test]
    fn wire_error_round_trips_code() {
        for c in [
            ErrorCode::Unauthenticated,
            ErrorCode::Forbidden,
            ErrorCode::NotFound,
            ErrorCode::InvalidEnvelope,
            ErrorCode::InvalidArgument,
            ErrorCode::BadCursor,
            ErrorCode::FeatureDisabled,
            ErrorCode::RateLimited,
            ErrorCode::BackendUnavailable,
            ErrorCode::Conflict,
        ] {
            let e = WireError::new(c, "x");
            assert_eq!(e.error_code(), c);
            assert_eq!(e.retriable, c.retriable());
        }
    }
}
