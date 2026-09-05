use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{SipMessage, headers::Header};
use rsipstack::transaction::endpoint::MessageInspector;
use rsipstack::transport::SipAddr;

use super::TokenValidator;

const TOKEN_TTL_SECS: u64 = 3600;
const DEFAULT_SECRET: &str = "cc-phone-auth-secret-change-in-production";

fn default_secret() -> String {
    std::env::var("CC_PHONE_AUTH_SECRET").unwrap_or_else(|_| DEFAULT_SECRET.to_string())
}

#[derive(Debug, Clone)]
pub struct AgentToken {
    pub agent_id: String,
    pub token: String,
    pub expires_at: Instant,
}

pub struct PhoneAuth {
    tokens: RwLock<Vec<AgentToken>>,
    secret: RwLock<String>,
}

impl PhoneAuth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tokens: RwLock::new(Vec::new()),
            secret: RwLock::new(default_secret()),
        })
    }

    pub fn with_secret(secret: String) -> Arc<Self> {
        Arc::new(Self {
            tokens: RwLock::new(Vec::new()),
            secret: RwLock::new(secret),
        })
    }

    pub fn set_secret(&self, secret: String) {
        if let Ok(mut s) = self.secret.try_write() {
            *s = secret;
        }
    }

    pub async fn set_secret_async(&self, secret: String) {
        *self.secret.write().await = secret;
    }

    pub fn generate_token(&self, agent_id: &str) -> String {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let payload = format!("{}:{}", agent_id, ts);
        let sig = self.sign(&payload);
        let token = format!("{}.{}", payload, sig);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token);

        let expires_at = Instant::now() + Duration::from_secs(TOKEN_TTL_SECS);
        let now = Instant::now();

        if let Ok(mut tokens) = self.tokens.try_write() {
            // Drop any expired entries so agents that log in once and never
            // return do not accumulate stale tokens for the lifetime of the
            // process. This is purely opportunistic GC; correctness is
            // unaffected because expired tokens are rejected at validate time.
            tokens.retain(|t| t.agent_id == agent_id || t.expires_at > now);
            // The retain above kept the old entry for this agent (if any);
            // drop it so the new token replaces it.
            tokens.retain(|t| t.agent_id != agent_id);
            tokens.push(AgentToken {
                agent_id: agent_id.to_string(),
                token: encoded.clone(),
                expires_at,
            });
        }

        encoded
    }

    pub fn validate(&self, raw: &str) -> Option<String> {
        if let Ok(tokens) = self.tokens.try_read() {
            for t in tokens.iter() {
                if t.token == raw {
                    if Instant::now() < t.expires_at {
                        return Some(t.agent_id.clone());
                    }
                }
            }
        }

        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .ok()
            .and_then(|v| String::from_utf8(v).ok())?;

        let (payload, sig) = decoded.rsplit_once('.')?;

        if !self.verify_signature(payload, sig) {
            return None;
        }

        let (agent_id, ts_str) = payload.split_once(':')?;
        let ts: u64 = ts_str.parse().ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now > ts + TOKEN_TTL_SECS {
            return None;
        }

        Some(agent_id.to_string())
    }

    fn sign(&self, payload: &str) -> String {
        let secret = self
            .secret
            .try_read()
            .map(|s| s.clone())
            .unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        hasher.update(secret.as_bytes());
        let result = hasher.finalize();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(result)
    }

    fn verify_signature(&self, payload: &str, expected_sig: &str) -> bool {
        let computed = self.sign(payload);
        computed.as_bytes() == expected_sig.as_bytes()
    }
}

impl TokenValidator for PhoneAuth {
    fn validate_token(&self, token: &str) -> Option<String> {
        self.validate(token)
    }
}

pub struct TokenInjector {
    auth: Arc<PhoneAuth>,
    /// Optional gate: when set, X-Agent-Token is only injected for users the
    /// validator accepts (e.g. known CC agents). Sync to fit MessageInspector.
    agent_check: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
}

impl TokenInjector {
    pub fn new(auth: Arc<PhoneAuth>) -> Self {
        Self {
            auth,
            agent_check: None,
        }
    }

    /// Only issue X-Agent-Token for registrants the validator accepts (e.g. a
    /// CC agent registry lookup). Kept optional so non-CC deployments retain
    /// the previous unconditional behaviour.
    pub fn with_agent_validator(
        auth: Arc<PhoneAuth>,
        check: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            auth,
            agent_check: Some(check),
        }
    }
}

impl MessageInspector for TokenInjector {
    fn before_send(&self, mut msg: SipMessage, _dest: Option<&SipAddr>) -> SipMessage {
        if !msg.is_response() {
            return msg;
        }

        let resp = match &msg {
            SipMessage::Response(r) => r,
            _ => return msg,
        };
        if resp.status_code().code() != 200 {
            return msg;
        }

        let is_register = msg.cseq_header().ok().and_then(|cseq| cseq.method().ok())
            == Some(rsipstack::sip::Method::Register);

        if !is_register {
            return msg;
        }

        // REGISTER's To URI identifies the account being registered. Contact
        // identifies a device binding; SIP.js deliberately uses a random user.
        let agent_id = msg
            .to_header()
            .ok()
            .and_then(|to| to.uri().ok())
            .and_then(|uri| uri.auth.map(|auth| auth.user))
            .filter(|user| !user.is_empty());

        if let Some(agent_id) = agent_id {
            // Gate to CC agents only when a validator is configured.
            if let Some(check) = &self.agent_check {
                if !check(&agent_id) {
                    return msg;
                }
            }
            let token = self.auth.generate_token(&agent_id);
            use rsipstack::sip::message::HasHeaders;
            msg.headers_mut()
                .push(Header::Other("X-Agent-Token".to_string(), token));
        }

        msg
    }

    fn after_received(&self, msg: SipMessage, _from: Option<&SipAddr>) -> SipMessage {
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_injector_rejects_invalid_registration_identity() {
        use rsipstack::sip::{HasHeaders, Response, StatusCode};

        let auth = PhoneAuth::with_secret("test-secret".to_string());
        let injector = TokenInjector::with_agent_validator(auth, Arc::new(|id| id == "2001"));
        for (status, cseq, to) in [
            (
                StatusCode::OK,
                "2 REGISTER",
                Some("<sip:alice@example.com>"),
            ),
            (StatusCode::OK, "2 REGISTER", Some("<sip:example.com>")),
            (StatusCode::OK, "2 REGISTER", Some("invalid To header")),
            (StatusCode::OK, "2 REGISTER", None),
            (
                StatusCode::Unauthorized,
                "2 REGISTER",
                Some("<sip:2001@example.com>"),
            ),
            (StatusCode::OK, "2 INVITE", Some("<sip:2001@example.com>")),
            (
                StatusCode::OK,
                "invalid CSeq",
                Some("<sip:2001@example.com>"),
            ),
        ] {
            let mut response = Response {
                status_code: status.clone(),
                headers: vec![
                    Header::CSeq(cseq.into()),
                    // A known agent in Contact must never substitute for To.
                    Header::Contact("<sip:2001@device.invalid>".into()),
                ]
                .into(),
                ..Default::default()
            };
            if let Some(to) = to {
                response.headers.push(Header::To(to.into()));
            }
            let output = injector.before_send(SipMessage::Response(response), None);
            assert!(
                !output.headers().iter().any(|header| {
                    matches!(header, Header::Other(name, _) if name == "X-Agent-Token")
                }),
                "unexpected token for status={status}, CSeq={cseq}, To={to:?}"
            );
        }
    }

    #[tokio::test]
    async fn generate_token_evicts_expired_entries_for_other_agents() {
        // Regression: `tokens` used to only retain-by-agent on each
        // generate_token. Expired entries for OTHER agents accumulated
        // forever. Verify that generating a new token sweeps them.
        let auth = PhoneAuth::with_secret("test-secret".to_string());

        // Issue a token for agent-a; then back-date it so it is expired.
        auth.generate_token("agent-a");
        {
            let mut tokens = auth.tokens.write().await;
            assert_eq!(tokens.len(), 1);
            tokens[0].expires_at = Instant::now() - Duration::from_secs(1);
        }

        // Generate a token for agent-b — the expired agent-a entry must be
        // swept, leaving only the new agent-b entry.
        auth.generate_token("agent-b");

        let tokens = auth.tokens.read().await;
        assert_eq!(tokens.len(), 1, "expired entries should be evicted");
        assert_eq!(tokens[0].agent_id, "agent-b");
    }

    #[tokio::test]
    async fn generate_token_replaces_existing_for_same_agent() {
        // Existing behaviour must be preserved: a fresh token for the same
        // agent replaces the previous one (no duplicates).
        let auth = PhoneAuth::with_secret("test-secret".to_string());
        auth.generate_token("agent-a");
        auth.generate_token("agent-a");
        let tokens = auth.tokens.read().await;
        assert_eq!(
            tokens.iter().filter(|t| t.agent_id == "agent-a").count(),
            1,
            "exactly one entry per agent"
        );
    }
}
