//! Token-budget-friendly response shaping + snapshot-stable pagination
//! (WS-5 §6). The two concerns are coupled: an MCP/HTTP result lands in a
//! context window, so the contract bounds both *how many* records and *how
//! many tokens* a single call returns.
//!
//! - **View** (§6.2 knob 1): `compact` (default) returns a snippet + keys;
//!   `full` adds the whole body, confidence, provenance.
//! - **`body_max_chars` + `truncated`** (§6.2 knob 2): per-body truncation.
//! - **Hard bytes ceiling** (§6.2 knob 4): the backstop — one call can never
//!   blow a context window regardless of the other knobs; overflow pages via
//!   `next_cursor`.
//! - **`token_estimate`** on every response (§6.2).
//! - **Signed cursor** (§6.1): opaque, server-sealed (HMAC), `as_of` pinned in,
//!   tenant-bound, future-`as_of` rejected (F2/F3/F13).

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::model::FactRecord;
use crate::wire::{ErrorCode, ShapedFact, WireError};

type HmacSha256 = Hmac<Sha256>;

/// §6.2 knob 1. Compact is the default for every list/search op (triage then
/// selectively deepen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Compact,
    Full,
}

impl View {
    pub fn parse(s: &str) -> Result<View, WireError> {
        match s {
            "compact" => Ok(View::Compact),
            "full" => Ok(View::Full),
            other => Err(WireError::new(
                ErrorCode::InvalidArgument,
                format!("unknown view '{other}' (want compact|full)"),
            )),
        }
    }
}

/// Default body-snippet cap for the compact view (§6.2). A deployment knob;
/// chosen so a compact list stays well under the §6.2.4 response ceiling.
pub const DEFAULT_BODY_MAX_CHARS: usize = 500;

/// Hard per-response body-bytes ceiling (§6.2.4, PM ruling ISI-5879): 64 KB
/// (~16k tokens). Enforced on bytes (deterministic), overflow pages via cursor.
pub const DEFAULT_RESPONSE_CEILING_BYTES: usize = 64 * 1024;

/// Shape one authoritative record into the §6.2 wire fact. `body_max_chars`
/// caps the snippet on both views; `full` additionally carries the whole body,
/// confidence, and provenance.
pub fn shape_fact(record: &FactRecord, view: View, body_max_chars: usize) -> ShapedFact {
    let (snippet, truncated) = snippet(&record.body, body_max_chars);
    let mut entity_keys = std::collections::BTreeMap::new();
    for (name, value) in record.entity_keys().resolved() {
        entity_keys.insert(name.to_string(), value.to_string());
    }
    ShapedFact {
        fact_id: record.id,
        fact_type: record.fact_type.clone(),
        subject: record.subject.clone(),
        body_snippet: snippet,
        body: match view {
            View::Full => Some(record.body.clone()),
            View::Compact => None,
        },
        truncated,
        event_time: record.event_time,
        entity_keys,
        confidence: match view {
            View::Full => record.confidence,
            View::Compact => None,
        },
        provenance: match view {
            View::Full => Some(record.provenance_principal.clone()),
            View::Compact => None,
        },
    }
}

/// Truncate to `max_chars` on a char boundary (never mid-codepoint). Returns
/// `(snippet, was_truncated)`.
fn snippet(body: &str, max_chars: usize) -> (String, bool) {
    if body.chars().count() <= max_chars {
        return (body.to_string(), false);
    }
    let cut: String = body.chars().take(max_chars).collect();
    (cut, true)
}

/// Approximate tokens in a serialized payload (§6.2 `token_estimate`): ~4 chars
/// per token. An approximation for agent budgeting — the hard ceiling is
/// enforced on bytes, never on this figure.
pub fn token_estimate(serialized_bytes: usize) -> u64 {
    (serialized_bytes as u64).div_ceil(4)
}

/// Enforce the §6.2.4 bytes ceiling over an already-shaped fact list: keep the
/// prefix that fits under `ceiling` (always at least one fact, so a single
/// large fact is never un-returnable), and report whether the list was cut
/// (the caller emits `next_cursor` when so).
pub fn enforce_ceiling(facts: &mut Vec<ShapedFact>, ceiling_bytes: usize) -> bool {
    let mut running = 0usize;
    let mut keep = 0usize;
    for f in facts.iter() {
        let sz = serde_json::to_vec(f).map(|v| v.len()).unwrap_or(0);
        if keep > 0 && running + sz > ceiling_bytes {
            break;
        }
        running += sz;
        keep += 1;
    }
    let cut = keep < facts.len();
    facts.truncate(keep);
    cut
}

/// Validate an optional `as_of` RFC3339 argument (§6.1, F13). A future `as_of`
/// would grow the "stable snapshot" across pages and defeat pagination's one
/// promise, so it is rejected `invalid_argument` (this deployment rejects
/// rather than clamps, for an explicit caller signal).
pub fn parse_as_of(
    as_of: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, WireError> {
    let Some(s) = as_of else { return Ok(None) };
    let parsed = chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|_| WireError::new(ErrorCode::InvalidArgument, "as_of is not valid RFC3339"))?
        .with_timezone(&chrono::Utc);
    if parsed > now {
        return Err(WireError::new(
            ErrorCode::InvalidArgument,
            "as_of must not be in the future",
        ));
    }
    Ok(Some(parsed))
}

/// The sealed-cursor claims (§6.1). Carries *query intent and sort position
/// only* — never a pre-authorized id set and never the tenant floor as
/// authority (the floor is re-derived per page, F3). The `tenant` field is a
/// binding check so a cursor replayed under another tenant fails `bad_cursor`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CursorClaims {
    /// Tenant the cursor was issued to (binding check, §6.1 "wrong tenant").
    pub tenant: String,
    /// Tool the cursor was issued for (binding check, §6.1 "wrong tool").
    pub tool: String,
    /// Pinned point-in-time data snapshot (§6.1) — RFC3339, or empty = none.
    pub as_of: String,
    /// Opaque sort position: the last record's `(event_time_millis, id)`.
    pub last_event_millis: i64,
    pub last_id: String,
    /// Fingerprint of the query body so a cursor can't be replayed against a
    /// different query (§6.1).
    pub query_fp: String,
    /// Page size pinned on the first page.
    pub page_size: u32,
}

/// Server key for sealing cursors (§6.1). Opaque bytes held by the server only.
pub struct CursorSealer {
    key: Vec<u8>,
}

impl CursorSealer {
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self { key: key.into() }
    }

    /// Seal claims into an opaque, tamper-evident token: `base64(json).base64(hmac)`.
    /// "Opaque" is not enough — the HMAC makes any client edit fail `open` (F2).
    pub fn seal(&self, claims: &CursorClaims) -> String {
        let payload = serde_json::to_vec(claims).expect("cursor claims serialize");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload);
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("hmac key");
        mac.update(b64.as_bytes());
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{b64}.{sig}")
    }

    /// Open + verify a cursor. Fails `bad_cursor` on any tamper, a tenant
    /// mismatch, or a tool mismatch (§6.1, F2/F3). The caller re-derives the
    /// tenant floor from *this request's* token regardless — the cursor never
    /// carries authority.
    pub fn open(
        &self,
        token: &str,
        expected_tenant: &str,
        expected_tool: &str,
    ) -> Result<CursorClaims, WireError> {
        let bad = || WireError::new(ErrorCode::BadCursor, "cursor is invalid; restart the query");
        let mut parts = token.split('.');
        let (b64, sig) = match (parts.next(), parts.next(), parts.next()) {
            (Some(b), Some(s), None) => (b, s),
            _ => return Err(bad()),
        };
        let expected_sig = {
            let mut mac = HmacSha256::new_from_slice(&self.key).expect("hmac key");
            mac.update(b64.as_bytes());
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        };
        // Constant-time-ish compare via the library on decoded bytes.
        let got = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(sig)
            .map_err(|_| bad())?;
        let want = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&expected_sig)
            .map_err(|_| bad())?;
        if got != want {
            return Err(bad());
        }
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(b64)
            .map_err(|_| bad())?;
        let claims: CursorClaims = serde_json::from_slice(&payload).map_err(|_| bad())?;
        if claims.tenant != expected_tenant || claims.tool != expected_tool {
            // A valid cursor for a DIFFERENT tenant/tool is still `bad_cursor`
            // (§6.1) — never an existence signal.
            return Err(bad());
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn rec(body: &str) -> FactRecord {
        FactRecord {
            id: uuid::Uuid::nil(),
            fact_type: "context".into(),
            subject: Some("s".into()),
            body: body.into(),
            confidence: Some(0.9),
            supersedes: None,
            event_time: Utc.timestamp_opt(1000, 0).unwrap(),
            ingested_at: Utc.timestamp_opt(1000, 0).unwrap(),
            entity_project: Some("p".into()),
            entity_issue: Some("I-1".into()),
            entity_pr: None,
            entity_agent: Some("a".into()),
            entity_run: None,
            entity_thread: None,
            entity_service: None,
            keyed: true,
            provenance_principal: "agent-a".into(),
        }
    }

    #[test]
    fn compact_hides_body_full_reveals_it() {
        let r = rec("hello world");
        let compact = shape_fact(&r, View::Compact, DEFAULT_BODY_MAX_CHARS);
        assert!(compact.body.is_none());
        assert!(compact.confidence.is_none());
        assert_eq!(compact.body_snippet, "hello world");
        let full = shape_fact(&r, View::Full, DEFAULT_BODY_MAX_CHARS);
        assert_eq!(full.body.as_deref(), Some("hello world"));
        assert_eq!(full.confidence, Some(0.9));
        assert_eq!(full.provenance.as_deref(), Some("agent-a"));
    }

    #[test]
    fn snippet_truncates_and_flags() {
        let r = rec("abcdefghij");
        let s = shape_fact(&r, View::Compact, 4);
        assert_eq!(s.body_snippet, "abcd");
        assert!(s.truncated);
    }

    #[test]
    fn ceiling_keeps_at_least_one_and_reports_cut() {
        let big = "x".repeat(2000);
        let mut facts: Vec<ShapedFact> = (0..10)
            .map(|_| shape_fact(&rec(&big), View::Full, 100_000))
            .collect();
        let cut = enforce_ceiling(&mut facts, 1024);
        assert!(cut);
        assert_eq!(facts.len(), 1); // one oversized fact is still returnable
    }

    #[test]
    fn future_as_of_rejected() {
        let now = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let future = "2099-01-01T00:00:00Z";
        assert_eq!(
            parse_as_of(Some(future), now).unwrap_err().error_code(),
            ErrorCode::InvalidArgument
        );
        assert!(parse_as_of(None, now).unwrap().is_none());
    }

    #[test]
    fn cursor_seal_open_roundtrip_and_tamper_fails() {
        let sealer = CursorSealer::new(b"server-key".to_vec());
        let claims = CursorClaims {
            tenant: "t1".into(),
            tool: "memory.get".into(),
            as_of: String::new(),
            last_event_millis: 1000,
            last_id: "id-9".into(),
            query_fp: "fp".into(),
            page_size: 50,
        };
        let token = sealer.seal(&claims);
        let got = sealer.open(&token, "t1", "memory.get").unwrap();
        assert_eq!(got, claims);

        // Wrong tenant → bad_cursor (never an existence signal).
        assert_eq!(
            sealer.open(&token, "t2", "memory.get").unwrap_err().error_code(),
            ErrorCode::BadCursor
        );
        // Wrong tool → bad_cursor.
        assert_eq!(
            sealer.open(&token, "t1", "memory.search").unwrap_err().error_code(),
            ErrorCode::BadCursor
        );
        // Tampered payload → bad_cursor (HMAC fails).
        let mut bytes = token.into_bytes();
        bytes[0] ^= 0x01;
        let tampered = String::from_utf8_lossy(&bytes).to_string();
        assert_eq!(
            sealer.open(&tampered, "t1", "memory.get").unwrap_err().error_code(),
            ErrorCode::BadCursor
        );
    }
}
