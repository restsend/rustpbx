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
}

impl TokenInjector {
    pub fn new(auth: Arc<PhoneAuth>) -> Self {
        Self { auth }
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

        let is_register = match msg.cseq_header() {
            Ok(cseq) => {
                let cseq_str = format!("{:?}", cseq);
                cseq_str.to_uppercase().contains("REGISTER")
            }
            Err(_) => false,
        };

        if !is_register {
            return msg;
        }

        let agent_id = msg
            .contact_header()
            .ok()
            .and_then(|c| {
                let uri_str = format!("{:?}", c);
                extract_user_from_uri(&uri_str)
            })
            .or_else(|| {
                msg.to_header().ok().and_then(|t| {
                    let uri_str = format!("{:?}", t);
                    extract_user_from_uri(&uri_str)
                })
            });

        if let Some(agent_id) = agent_id {
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

fn extract_user_from_uri(uri_str: &str) -> Option<String> {
    let s = uri_str.trim();
    let s = s.trim_start_matches('<').trim_end_matches('>');
    let user_part = s.split(':').nth(1)?;
    let user = user_part.split('@').next()?;
    Some(user.to_string())
}
