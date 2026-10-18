//! Principal + tenant-floor derivation (WS-5 §3). The MCP/HTTP layer is a
//! token *verifier*, never an issuer (§3 "deliberately does not own"): it
//! validates a presented bearer token's signature and claims and derives the
//! R6 tenant floor. Token issuance/rotation belongs to the deployment's auth
//! system (the Paperclip run-token issuer for the reference deployment).
//!
//! stdio binds the principal once at launch from the run token (§2.1); HTTP
//! verifies it per request (§2.2). Both resolve to the same [`Principal`].

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::wire::{ErrorCode, WireError};

/// The verified caller (WS-5 §3). Carries exactly the claim set already in the
/// run-scoped JWT: `tenant_id` (≈ company), `agent_id`, `run_id`, and the
/// token validity window. The tenant floor is `tenant_id` and is never
/// supplied by, nor omittable by, the client.
#[derive(Debug, Clone)]
pub struct Principal {
    /// The R6 isolation floor (≈ company id). Opaque.
    pub tenant_id: String,
    /// The authenticated producer agent = `entity.agent` (§3.1).
    pub agent_id: String,
    /// Run scope, for custody/audit.
    pub run_id: Option<String>,
    /// Unix seconds the token expires (§2.1 lifetime bound, F12).
    pub expires_at: i64,
    /// The W3C trace id to stamp onto writes (custody), when present.
    pub trace_id: Option<String>,
}

impl Principal {
    /// The [`CallerContext`](crate::pipeline::CallerContext) this principal
    /// stamps onto a write — the trust root for keys (§4 rule 1).
    pub fn caller_context(&self) -> crate::pipeline::CallerContext {
        crate::pipeline::CallerContext {
            tenant_id: self.tenant_id.clone(),
            principal: self.agent_id.clone(),
            trace_id: self.trace_id.clone(),
        }
    }
}

/// Verifies a presented bearer token into a [`Principal`]. A trait so a
/// deployment can swap the token format and tests can inject a fixed principal
/// without minting JWTs.
pub trait TokenVerifier: Send + Sync {
    /// Verify the raw bearer token value (no `Bearer ` prefix) at `now`
    /// (unix seconds). Returns `unauthenticated` on any signature/claim/expiry
    /// failure — never a distinct error that would reveal *why* (§3/§7).
    fn verify(&self, token: &str, now: i64) -> Result<Principal, WireError>;
}

/// HS256 verifier for the Paperclip-shaped run-scoped JWT (the reference
/// deployment, §3). Claims: `sub` → `agent_id`, `company_id` → `tenant_id`,
/// `run_id`, `exp`. The server holds the shared secret; it is a *verifier*,
/// not an issuer (§3).
pub struct JwtVerifier {
    secret: Vec<u8>,
}

impl JwtVerifier {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
        }
    }
}

type HmacSha256 = Hmac<Sha256>;

impl TokenVerifier for JwtVerifier {
    fn verify(&self, token: &str, now: i64) -> Result<Principal, WireError> {
        let unauth = |d: &str| WireError::new(ErrorCode::Unauthenticated, d.to_string());
        let mut parts = token.split('.');
        let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(unauth("malformed token")),
        };

        // Verify the HS256 signature over `header.payload` before trusting any
        // claim. A bad signature is `unauthenticated` — never a parse of the
        // (forgeable) payload.
        let signing_input = format!("{h}.{p}");
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| unauth("bad signature encoding"))?;
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .map_err(|_| unauth("verifier misconfigured"))?;
        mac.update(signing_input.as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| unauth("bad signature"))?;

        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(p)
            .map_err(|_| unauth("bad payload encoding"))?;
        let claims: serde_json::Value =
            serde_json::from_slice(&payload).map_err(|_| unauth("bad payload json"))?;

        let tenant_id = claims
            .get("company_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| unauth("missing tenant claim"))?
            .to_string();
        let agent_id = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| unauth("missing subject claim"))?
            .to_string();
        let expires_at = claims
            .get("exp")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| unauth("missing exp claim"))?;
        // F12: a long-lived server must never answer under an expired token.
        if now >= expires_at {
            return Err(unauth("token expired"));
        }
        let run_id = claims
            .get("run_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        Ok(Principal {
            tenant_id,
            agent_id,
            run_id,
            expires_at,
            trace_id: None,
        })
    }
}

/// A fixed-principal verifier for stdio single-principal binding (§2.1) and for
/// tests. The principal is bound once; `verify` ignores the token value (the
/// stdio pipe is the trust boundary) but still enforces the expiry window (F12).
pub struct StaticVerifier {
    principal: Principal,
}

impl StaticVerifier {
    pub fn new(principal: Principal) -> Self {
        Self { principal }
    }
}

impl TokenVerifier for StaticVerifier {
    fn verify(&self, _token: &str, now: i64) -> Result<Principal, WireError> {
        if now >= self.principal.expires_at {
            return Err(WireError::new(
                ErrorCode::Unauthenticated,
                "token validity window closed",
            ));
        }
        Ok(self.principal.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mint(secret: &[u8], claims: &serde_json::Value) -> String {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let header = b64(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64(serde_json::to_vec(claims).unwrap().as_slice());
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(signing_input.as_bytes());
        let sig = b64(&mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    #[test]
    fn verifies_valid_token_and_derives_floor() {
        let secret = b"test-secret";
        let token = mint(
            secret,
            &serde_json::json!({"sub":"agent-a","company_id":"tenant-1","run_id":"run-9","exp":2000}),
        );
        let p = JwtVerifier::new(secret.to_vec())
            .verify(&token, 1000)
            .unwrap();
        assert_eq!(p.tenant_id, "tenant-1");
        assert_eq!(p.agent_id, "agent-a");
        assert_eq!(p.run_id.as_deref(), Some("run-9"));
    }

    #[test]
    fn rejects_expired_token() {
        let secret = b"test-secret";
        let token = mint(
            secret,
            &serde_json::json!({"sub":"a","company_id":"t","exp":500}),
        );
        let err = JwtVerifier::new(secret.to_vec())
            .verify(&token, 1000)
            .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Unauthenticated);
    }

    #[test]
    fn rejects_tampered_signature() {
        let token = mint(
            b"real-secret",
            &serde_json::json!({"sub":"a","company_id":"t","exp":2000}),
        );
        // Verifier holds a different secret → signature mismatch.
        let err = JwtVerifier::new(b"other-secret".to_vec())
            .verify(&token, 1000)
            .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Unauthenticated);
    }

    #[test]
    fn rejects_alg_none_forgery() {
        // A forged token with a valid-looking payload but no real signature
        // must fail: we HMAC-verify `header.payload`, so an empty/garbage sig
        // never passes regardless of the header's claimed alg.
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let header = b64(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = b64(br#"{"sub":"attacker","company_id":"victim","exp":9999999999}"#);
        let forged = format!("{header}.{payload}.");
        let err = JwtVerifier::new(b"secret".to_vec())
            .verify(&forged, 1000)
            .unwrap_err();
        assert_eq!(err.error_code(), ErrorCode::Unauthenticated);
    }
}
