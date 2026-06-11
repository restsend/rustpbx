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

        if let Ok(mut tokens) = self.tokens.try_write() {
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

    fn after_received(&self, msg: SipMessage, _from: &SipAddr) -> SipMessage {
        msg
    }
}

fn extract_user_from_uri(uri_str: &str) -> Option<String> {
    let s = uri_str.trim();
    let s = s.trim_start_matches('<').trim_end_matches('>');
    let user_part = s.split(':').nth(1)?;
    let user = user_part.split('@').next()?;
    Some(user.to_string())
}
