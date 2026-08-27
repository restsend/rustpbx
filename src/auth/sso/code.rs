//! Stateless artifacts for the SSO broker (cluster-friendly).
//!
//! Instead of server-side stores, both the authorization flow and the
//! authorization code travel as **sealed envelopes** — compact HMAC-SHA256
//! signed payloads in the standard JWT shape (`header.payload.signature`,
//! reused from `jwt_validator::generate_hs256_jwt`). Every node holding the
//! same `[sso]`/`[proxy.jwt_auth]` secret can verify them locally, so
//! `/authorize`, `/callback` and `/token` may land on different nodes behind
//! a load balancer **with no shared storage whatsoever**.
//!
//! Envelopes are short-lived (`exp` enforced) and bound to the PKCE
//! challenge, mirroring the security properties of the server-side stores
//! they replaced (single-use is approximated: an exchanged code yields only
//! its own bearer token, and PKCE keeps interception useless without the
//! verifier).

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Issuer stamped on every sealed envelope so validation rejects foreign
/// tokens accidentally presented as codes/states.
pub const ENVELOPE_ISSUER: &str = "rustpbx-sso";

/// Verify an S256 `code_verifier` against the stored `code_challenge`
/// (`BASE64URL(SHA256(verifier)) == challenge`, RFC 7636 §4.6).
pub fn verify_pkce_s256(challenge: &str, verifier: &str) -> bool {
    // RFC 7636 §4.1: verifier is 43..=128 chars from the unreserved set.
    if verifier.len() < 43 || verifier.len() > 128 {
        return false;
    }
    if !verifier
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    {
        return false;
    }
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_slice()) == challenge
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Sealing
// ---------------------------------------------------------------------------

/// Sign arbitrary claims into a tamper-proof, expiring one-line artifact.
pub fn seal(claims: &Value, secret: &str) -> String {
    crate::auth::jwt_validator::generate_hs256_jwt(claims, secret)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsealError {
    Malformed,
    BadSignature,
    Expired,
    WrongKind,
}

impl std::fmt::Display for UnsealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsealError::Malformed => write!(f, "malformed"),
            UnsealError::BadSignature => write!(f, "bad signature"),
            UnsealError::Expired => write!(f, "expired"),
            UnsealError::WrongKind => write!(f, "wrong kind"),
        }
    }
}

fn envelope_validator(secret: &str) -> crate::auth::jwt_validator::JwtValidator {
    crate::auth::jwt_validator::JwtValidator::new(&crate::config::JwtAuthConfig {
        enabled: true,
        secret: secret.to_string(),
        user_id_claim: "sub".to_string(),
        issuer: Some(ENVELOPE_ISSUER.to_string()),
        audience: None,
        sip_header_name: "X-Auth-Token".to_string(),
        check_local_user: false,
        ws_token_param: "token".to_string(),
        dev_mint_enabled: false,
    })
}

/// Open a sealed envelope: verifies signature, expiry, issuer and kind tag,
/// returning the raw claims.
pub fn unseal(
    artifact: &str,
    secret: &str,
    kind: &str,
) -> Result<Value, UnsealError> {
    let validator = envelope_validator(secret);
    use crate::auth::jwt_validator::JwtError as E;
    let claims = match validator.validate(artifact.trim()) {
        Ok(claims) => claims,
        Err(E::Expired) => return Err(UnsealError::Expired),
        Err(E::InvalidSignature | E::InvalidIssuer | E::InvalidAudience) => {
            return Err(UnsealError::BadSignature)
        }
        Err(_) => return Err(UnsealError::Malformed),
    };
    if claims.get("k").and_then(|v| v.as_str()) != Some(kind) {
        return Err(UnsealError::WrongKind);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_roundtrip() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier));
        assert!(verify_pkce_s256(&challenge, verifier));
        assert!(!verify_pkce_s256(&challenge, "different-verifier-at-least-43-chars-long!!!!!"));
        assert!(!verify_pkce_s256(&challenge, "short"));
    }

    #[test]
    fn envelope_roundtrip_and_kind() {
        let claims = json!({
            "iss": ENVELOPE_ISSUER, "k": "flow",
            "cst": "abc", "chl": "xyz", "exp": now_epoch() + 60,
        });
        let sealed = seal(&claims, "sec");
        let opened = unseal(&sealed, "sec", "flow").unwrap();
        assert_eq!(opened["cst"], "abc");

        // wrong kind rejected
        assert!(matches!(
            unseal(&sealed, "sec", "code"),
            Err(UnsealError::WrongKind)
        ));
        // wrong key rejected
        assert!(matches!(
            unseal(&sealed, "other", "flow"),
            Err(UnsealError::BadSignature)
        ));
        // tampered payload rejected
        let mut parts: Vec<String> = sealed.split('.').map(str::to_string).collect();
        parts[1] = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"k":"flow","forged":true}"#);
        let forged = parts.join(".");
        assert!(matches!(
            unseal(&forged, "sec", "flow"),
            Err(UnsealError::BadSignature)
        ));
    }

    #[test]
    fn envelope_expiry() {
        let claims = json!({"iss": ENVELOPE_ISSUER, "k": "code", "exp": now_epoch() - 1});
        let sealed = seal(&claims, "sec");
        assert!(matches!(
            unseal(&sealed, "sec", "code"),
            Err(UnsealError::Expired)
        ));
    }
}
