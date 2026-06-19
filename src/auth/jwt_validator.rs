use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::JwtAuthConfig;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug)]
pub enum JwtError {
    Malformed,
    InvalidSignature,
    Expired,
    InvalidIssuer,
    InvalidAudience,
    MissingUserId,
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JwtError::Malformed => write!(f, "malformed token"),
            JwtError::InvalidSignature => write!(f, "invalid signature"),
            JwtError::Expired => write!(f, "token expired"),
            JwtError::InvalidIssuer => write!(f, "invalid issuer"),
            JwtError::InvalidAudience => write!(f, "invalid audience"),
            JwtError::MissingUserId => write!(f, "missing user id claim"),
        }
    }
}

impl std::error::Error for JwtError {}

pub struct JwtValidator {
    secret: Vec<u8>,
    issuer: Option<String>,
    audience: Option<String>,
    user_id_claim: String,
}

impl JwtValidator {
    pub fn new(config: &JwtAuthConfig) -> Self {
        Self {
            secret: config.secret.as_bytes().to_vec(),
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
            user_id_claim: config.user_id_claim.clone(),
        }
    }

    pub fn validate(&self, token: &str) -> Result<Value, JwtError> {
        let token = token.trim();
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(JwtError::Malformed);
        }

        let signing_input = format!("{}.{}", parts[0], parts[1]);

        let mut mac =
            HmacSha256::new_from_slice(&self.secret).map_err(|_| JwtError::InvalidSignature)?;
        mac.update(signing_input.as_bytes());

        let provided_sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| JwtError::Malformed)?;

        mac.verify_slice(&provided_sig)
            .map_err(|_| JwtError::InvalidSignature)?;

        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| JwtError::Malformed)?;
        let claims: Value = serde_json::from_slice(&payload).map_err(|_| JwtError::Malformed)?;

        if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if now > exp {
                return Err(JwtError::Expired);
            }
        }

        if let Some(ref expected_iss) = self.issuer {
            let actual = claims.get("iss").and_then(|v| v.as_str());
            if actual != Some(expected_iss.as_str()) {
                return Err(JwtError::InvalidIssuer);
            }
        }

        if let Some(ref expected_aud) = self.audience {
            let actual = claims.get("aud").and_then(|v| v.as_str());
            if actual != Some(expected_aud.as_str()) {
                return Err(JwtError::InvalidAudience);
            }
        }

        Ok(claims)
    }

    pub fn extract_user_id(&self, claims: &Value) -> Option<String> {
        let val = claims.get(&self.user_id_claim)?;
        if let Some(s) = val.as_str() {
            return Some(s.to_string());
        }
        if let Some(n) = val.as_i64() {
            return Some(n.to_string());
        }
        if let Some(n) = val.as_u64() {
            return Some(n.to_string());
        }
        None
    }
}

pub fn generate_hs256_jwt(claims: &Value, secret: &str) -> String {
    let header = serde_json::json!({"alg": "HS256", "typ": "JWT"});
    let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&header).unwrap_or_default());
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap_or_default());
    let signing_input = format!("{}.{}", header_b64, payload_b64);

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(signing_input.as_bytes());
    let sig = mac.finalize().into_bytes();

    format!(
        "{}.{}",
        signing_input,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(secret: &str, claim: &str) -> JwtAuthConfig {
        JwtAuthConfig {
            enabled: true,
            secret: secret.to_string(),
            user_id_claim: claim.to_string(),
            issuer: None,
            audience: None,
            sip_header_name: "X-Auth-Token".to_string(),
            check_local_user: false,
            ws_token_param: "token".to_string(),
        }
    }

    #[test]
    fn test_valid_jwt() {
        let config = make_config("test-secret", "userId");
        let validator = JwtValidator::new(&config);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = serde_json::json!({
            "userId": "1001",
            "name": "Alice",
            "exp": now + 3600,
        });
        let token = generate_hs256_jwt(&claims, "test-secret");

        let result = validator.validate(&token).unwrap();
        assert_eq!(result["userId"], "1001");

        let uid = validator.extract_user_id(&result).unwrap();
        assert_eq!(uid, "1001");
    }

    #[test]
    fn test_expired_jwt() {
        let config = make_config("test-secret", "userId");
        let validator = JwtValidator::new(&config);

        let claims = serde_json::json!({
            "userId": "1001",
            "exp": 1,
        });
        let token = generate_hs256_jwt(&claims, "test-secret");

        let result = validator.validate(&token);
        assert!(matches!(result, Err(JwtError::Expired)));
    }

    #[test]
    fn test_tampered_signature() {
        let config = make_config("test-secret", "userId");
        let validator = JwtValidator::new(&config);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = serde_json::json!({ "userId": "1001", "exp": now + 3600 });
        let token = generate_hs256_jwt(&claims, "wrong-secret");

        let result = validator.validate(&token);
        assert!(matches!(result, Err(JwtError::InvalidSignature)));
    }

    #[test]
    fn test_numeric_user_id() {
        let config = make_config("s", "sub");
        let validator = JwtValidator::new(&config);

        let claims = serde_json::json!({ "sub": 1001 });
        assert_eq!(validator.extract_user_id(&claims).unwrap(), "1001");
    }

    #[test]
    fn test_missing_claim() {
        let config = make_config("s", "userId");
        let validator = JwtValidator::new(&config);

        let claims = serde_json::json!({ "foo": "bar" });
        assert!(validator.extract_user_id(&claims).is_none());
    }

    #[test]
    fn test_issuer_validation() {
        let mut config = make_config("s", "userId");
        config.issuer = Some("expected-iss".to_string());
        let validator = JwtValidator::new(&config);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims_ok = serde_json::json!({
            "userId": "1001", "exp": now + 3600, "iss": "expected-iss"
        });
        let token_ok = generate_hs256_jwt(&claims_ok, "s");
        assert!(validator.validate(&token_ok).is_ok());

        let claims_bad = serde_json::json!({
            "userId": "1001", "exp": now + 3600, "iss": "wrong-iss"
        });
        let token_bad = generate_hs256_jwt(&claims_bad, "s");
        assert!(matches!(
            validator.validate(&token_bad),
            Err(JwtError::InvalidIssuer)
        ));
    }
}
