//! Token issuance for the SSO broker.
//!
//! Two token modes (RFC 6749 shape, provider decides which applies):
//! - `passthrough`: the upstream enterprise JWT **is** the access_token.
//!   No refresh grant — the client re-runs `/authorize` when it expires
//!   (silent when the upstream session cookie is still alive).
//! - `minted`: rustpbx signs its own HS256 JWT with normalized claims and
//!   also issues a rotating refresh_token.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::response::{IntoResponse, Response};
use axum::Json;
use dashmap::DashMap;
use serde_json::Value;

pub const TOKEN_MODE_PASSTHROUGH: &str = "passthrough";
pub const TOKEN_MODE_MINTED: &str = "minted";

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// RFC 6749 §5.2 error responses
// ---------------------------------------------------------------------------

pub fn token_error(error: &str) -> Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        [("cache-control", "no-store"), ("pragma", "no-cache")],
        Json(serde_json::json!({ "error": error })),
    )
        .into_response()
}

/// RFC 6749 §5.1 success response (`Cache-Control: no-store` mandatory).
pub fn token_success(body: Value) -> Response {
    (
        axum::http::StatusCode::OK,
        [("cache-control", "no-store"), ("pragma", "no-cache")],
        Json(body),
    )
        .into_response()
}

/// Identity bundle handed from `/callback` into the token endpoint logic.
#[derive(Debug, Clone, Copy)]
pub struct IssuanceInput<'a> {
    /// Upstream JWT (passthrough mode returns it verbatim).
    pub access_token: &'a str,
    pub user_id: &'a str,
    /// Full validated upstream claims.
    pub upstream_claims: &'a Value,
    /// Server-side default lifetime used when the upstream JWT carries no
    /// `exp` claim (otherwise `expires_in` would collapse to 0).
    pub fallback_ttl_secs: u64,
}

/// Build the JSON body for a passthrough response: the upstream JWT is
/// returned verbatim; expiry comes from its own `exp` claim, falling back to
/// the configured token TTL when absent.
pub fn passthrough_response(entry: IssuanceInput<'_>) -> Value {
    let expires_at = exp_of(entry.upstream_claims);
    let now = now_epoch();
    let expires_in = if expires_at > now {
        expires_at - now
    } else {
        entry.fallback_ttl_secs as i64
    };
    serde_json::json!({
        "access_token": entry.access_token,
        "token_type": "Bearer",
        "expires_in": expires_in,
    })
}

// ---------------------------------------------------------------------------
// Minted tokens + refresh store
// ---------------------------------------------------------------------------

/// Claims copied from the upstream token into a minted token (normalized).
const COPIED_CLAIMS: &[&str] = &["email", "name", "agent_id", "agentId", "mis_id"];

#[derive(Debug, Clone)]
struct RefreshEntry {
    sid: String,
    subject: String,
    claims: Value,
    expires_at_epoch: i64,
}

fn refresh_expired(now: i64) -> impl Fn(&RefreshEntry) -> bool {
    move |e| e.expires_at_epoch <= now
}

/// In-process refresh-token store with rotation semantics.
#[derive(Debug, Clone, Default)]
pub struct RefreshStore {
    inner: Arc<DashMap<String, RefreshEntry>>,
}

impl RefreshStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &self,
        sid: String,
        subject: String,
        claims: Value,
        ttl: Duration,
    ) -> Option<String> {
        let token = format!("rt_{}", uuid::Uuid::new_v4().simple());
        self.inner.retain(|_, v| !refresh_expired(now_epoch())(v));
        self.inner.insert(
            token.clone(),
            RefreshEntry {
                sid,
                subject,
                claims,
                expires_at_epoch: now_epoch() + ttl.as_secs() as i64,
            },
        );
        Some(token)
    }

    /// Rotate: consume the presented refresh_token, mint a fresh row keyed by
    /// a brand-new opaque value. The old value can never be reused.
    pub fn rotate(&self, token: &str) -> Option<(String, String, String, Value)> {
        let (_, entry) = self.inner.remove(token)?;
        if refresh_expired(now_epoch())(&entry) {
            return None;
        }
        let new_sid = entry.sid.clone();
        let new_subject = entry.subject.clone();
        let new_claims = entry.claims.clone();
        let new_token = self.insert(
            new_sid.clone(),
            new_subject.clone(),
            new_claims.clone(),
            Duration::from_secs(entry.expires_at_epoch.saturating_sub(now_epoch()) as u64),
        )?;
        Some((new_token, new_sid, new_subject, new_claims))
    }
}

/// Mint an HS256 access token signed by rustpbx itself. `secret` is shared
/// with `[proxy.jwt_auth].secret` so SIP/WS validation accepts it unchanged;
/// `user_id_claim` mirrors `[proxy.jwt_auth].user_id_claim` so
/// `JwtAuthBackend::extract_user_id` works on the minted token too.
pub fn mint_access_token(
    secret: &str,
    subject: &str,
    user_id_claim: &str,
    upstream_claims: &Value,
    sid: &str,
    ttl_secs: u64,
) -> Result<(String, i64), String> {
    let now = now_epoch();
    let exp = now + ttl_secs as i64;
    let mut claims = serde_json::Map::new();
    claims.insert("iss".into(), Value::String("rustpbx".into()));
    claims.insert("sub".into(), Value::String(subject.to_string()));
    claims.insert(user_id_claim.to_string(), Value::String(subject.to_string()));
    claims.insert("sid".into(), Value::String(sid.to_string()));
    claims.insert("iat".into(), Value::from(now));
    claims.insert("exp".into(), Value::from(exp));
    for key in COPIED_CLAIMS {
        if let Some(v) = upstream_claims.get(*key) {
            claims.insert((*key).to_string(), v.clone());
        }
    }
    let token = crate::auth::jwt_validator::generate_hs256_jwt(&Value::Object(claims), secret);
    Ok((token, exp))
}

/// Parse epoch seconds out of an `exp` claim (0 when absent/invalid).
pub fn exp_of(claims: &Value) -> i64 {
    claims
        .get("exp")
        .and_then(|v| v.as_i64())
        .or_else(|| claims.get("exp").and_then(|v| v.as_f64()).map(|f| f as i64))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_claims() -> Value {
        serde_json::json!({
            "userId": "1001",
            "email": "a@b.c",
            "name": "Alice",
            "exp": now_epoch() + 600,
        })
    }

    #[test]
    fn refresh_rotation_single_use() {
        let store = RefreshStore::new();
        let rt1 = store
            .insert("sid-1".into(), "1001".into(), valid_claims(), Duration::from_secs(3600))
            .unwrap();

        let (rt2, sid, subject, _) = store.rotate(&rt1).expect("first rotation");
        assert_eq!(sid, "sid-1");
        assert_eq!(subject, "1001");
        assert_ne!(rt1, rt2);

        // old refresh token is dead (rotation)
        assert!(store.rotate(&rt1).is_none());

        // rotated one works once more
        assert!(store.rotate(&rt2).is_some());
    }

    #[test]
    fn refresh_expiry_rejected() {
        let store = RefreshStore::new();
        let rt = store
            .insert("s".into(), "u".into(), valid_claims(), Duration::ZERO)
            .unwrap();
        assert!(store.rotate(&rt).is_none());
    }

    #[test]
    fn minted_token_validates_and_carries_claims() {
        let (token, exp) =
            mint_access_token("sec", "1001", "userId", &valid_claims(), "sid-9", 300).unwrap();
        assert!(exp > now_epoch());

        // Validate via the stock JwtValidator path (same as JwtAuthBackend).
        let cfg = crate::config::JwtAuthConfig {
            enabled: true,
            secret: "sec".to_string(),
            user_id_claim: "userId".to_string(),
            issuer: Some("rustpbx".to_string()),
            audience: None,
            sip_header_name: "X-Auth-Token".to_string(),
            check_local_user: false,
            ws_token_param: "token".to_string(),
            dev_mint_enabled: false,
        };
        let validator = crate::auth::jwt_validator::JwtValidator::new(&cfg);
        let decoded = validator.validate(&token).expect("minted token must validate");
        assert_eq!(decoded["sub"], "1001");
        assert_eq!(decoded["email"], "a@b.c");
        assert_eq!(decoded["sid"], "sid-9");
        assert_eq!(
            validator.extract_user_id(&decoded).unwrap(),
            "1001",
            "user id claim aligned with [proxy.jwt_auth].user_id_claim"
        );
    }

    #[test]
    fn passthrough_response_shape() {
        let claims = valid_claims();
        let body = passthrough_response(IssuanceInput {
            access_token: "e-jwt",
            user_id: "1001",
            upstream_claims: &claims,
            fallback_ttl_secs: 3600,
        });
        assert_eq!(body["access_token"], "e-jwt");
        assert_eq!(body["token_type"], "Bearer");
        assert!(!body.get("refresh_token").is_some());
        let expires_in = body["expires_in"].as_i64().unwrap();
        assert!((500..=600).contains(&expires_in), "expires_in={expires_in}");
    }

    #[test]
    fn passthrough_falls_back_when_no_exp() {
        // Upstream JWT without exp: must not yield expires_in=0 (which would
        // make the client re-run SSO immediately).
        let claims = serde_json::json!({"userId": "1001"});
        let body = passthrough_response(IssuanceInput {
            access_token: "e-jwt",
            user_id: "1001",
            upstream_claims: &claims,
            fallback_ttl_secs: 3600,
        });
        assert_eq!(body["expires_in"], 3600);
    }
}
