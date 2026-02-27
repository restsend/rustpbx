/// Outbound SIP REGISTER client for trunk registration.
///
/// When a trunk has `register_enabled = true`, this module will periodically
/// send REGISTER requests to the trunk's SIP server via rsipstack's
/// `Registration` client, handle digest authentication challenges, and
/// refresh the registration before it expires.
///
/// On reload, registrations for removed trunks are cancelled (un-REGISTER with
/// Expires: 0) and new/changed trunks with registration enabled are started.
use crate::proxy::routing::TrunkConfig;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rsip::StatusCode;
use rsipstack::{
    dialog::{authenticate::Credential, registration::Registration},
    transaction::endpoint::EndpointInnerRef,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Default REGISTER expiry in seconds.
const DEFAULT_REGISTER_EXPIRES: u32 = 300;
/// Safety margin: refresh the registration this many seconds before it expires.
const REFRESH_MARGIN_SECS: u32 = 30;

/// Runtime status for a single trunk registration.
#[derive(Debug, Clone, Serialize)]
pub struct TrunkRegistrationStatus {
    pub trunk_name: String,
    pub registered: bool,
    pub last_register_at: Option<DateTime<Utc>>,
    pub expires: Option<u32>,
    pub error: Option<String>,
    pub remote_addr: Option<String>,
}

/// Manages outbound trunk registrations.
pub struct TrunkRegistrar {
    statuses: Arc<RwLock<HashMap<String, TrunkRegistrationStatus>>>,
    cancel_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>,
    /// Global cancel token for the registrar (parent for all per-trunk tokens).
    parent_cancel: CancellationToken,
    /// SIP endpoint reference for creating transactions (set after server build).
    endpoint: RwLock<Option<EndpointInnerRef>>,
}

impl TrunkRegistrar {
    pub fn new() -> Self {
        Self {
            statuses: Arc::new(RwLock::new(HashMap::new())),
            cancel_tokens: Arc::new(RwLock::new(HashMap::new())),
            parent_cancel: CancellationToken::new(),
            endpoint: RwLock::new(None),
        }
    }

    /// Set the SIP endpoint reference.  Must be called after the SipServer is
    /// built, before the first `reconcile`.
    pub fn set_endpoint(&self, endpoint: EndpointInnerRef) {
        *self.endpoint.write().unwrap() = Some(endpoint);
    }

    /// Get the registration status of all trunks.
    pub fn get_statuses(&self) -> HashMap<String, TrunkRegistrationStatus> {
        self.statuses.read().unwrap().clone()
    }

    /// Get the registration status of a specific trunk.
    pub fn get_status(&self, trunk_name: &str) -> Option<TrunkRegistrationStatus> {
        self.statuses.read().unwrap().get(trunk_name).cloned()
    }

    /// Reconcile registrations with the current set of trunks.
    ///
    /// - Starts registration for new trunks with `register_enabled`.
    /// - Sends un-REGISTER for trunks that were removed or had registration disabled.
    /// - Re-starts registration for trunks whose config changed.
    pub async fn reconcile(&self, trunks: &HashMap<String, TrunkConfig>) {
        let endpoint = { self.endpoint.read().unwrap().clone() };
        let Some(endpoint) = endpoint else {
            debug!("trunk registrar: no endpoint set yet, skipping reconcile");
            return;
        };

        let current_names: Vec<String> =
            { self.cancel_tokens.read().unwrap().keys().cloned().collect() };

        // Determine which trunks need registration.
        let mut desired: HashMap<String, &TrunkConfig> = HashMap::new();
        for (name, config) in trunks.iter() {
            if config.register_enabled.unwrap_or(false)
                && config.disabled.map(|d| !d).unwrap_or(true)
            {
                desired.insert(name.clone(), config);
            }
        }

        // Cancel registrations for removed / disabled trunks.
        for name in &current_names {
            if !desired.contains_key(name) {
                info!(trunk = %name, "cancelling trunk registration (removed or disabled)");
                self.stop_registration(name).await;
            }
        }

        // Start / restart registrations for desired trunks.
        for (name, config) in &desired {
            let already_running = self
                .cancel_tokens
                .read()
                .unwrap()
                .contains_key(name.as_str());
            if !already_running {
                info!(trunk = %name, dest = %config.dest, "starting trunk registration");
                self.start_registration(name.clone(), (*config).clone(), endpoint.clone());
            }
        }
    }

    /// Stop all registrations (used on shutdown).
    pub async fn stop_all(&self) {
        self.parent_cancel.cancel();
        let names: Vec<String> = self.cancel_tokens.read().unwrap().keys().cloned().collect();
        for name in names {
            self.stop_registration(&name).await;
        }
    }

    /// Stop registration for a single trunk, sending un-REGISTER.
    async fn stop_registration(&self, name: &str) {
        let token = { self.cancel_tokens.write().unwrap().remove(name) };
        if let Some(token) = token {
            token.cancel();
        }
        self.statuses.write().unwrap().remove(name);
    }

    /// Spawn a background task for trunk registration.
    fn start_registration(
        &self,
        name: String,
        config: TrunkConfig,
        endpoint: EndpointInnerRef,
    ) {
        let child_token = self.parent_cancel.child_token();
        {
            self.cancel_tokens
                .write()
                .unwrap()
                .insert(name.clone(), child_token.clone());
        }

        let statuses = self.statuses.clone();

        tokio::spawn(async move {
            registration_loop(name, config, child_token, statuses, endpoint).await;
        });
    }
}

/// Normalize a trunk destination string into an `rsip::Uri`.
///
/// Handles bare `host:port`, strips angle brackets, and prepends `sip:` if no
/// scheme is present.  The `user@` part (if any) is kept — `Registration`
/// overwrites `to.uri.auth` from the credential anyway.
fn parse_server_uri(dest: &str) -> Result<rsip::Uri> {
    let trimmed = dest.trim().trim_matches(|c| c == '<' || c == '>');
    let uri_str = if trimmed.starts_with("sip:") || trimmed.starts_with("sips:") {
        trimmed.to_string()
    } else {
        format!("sip:{}", trimmed)
    };
    rsip::Uri::try_from(uri_str.as_str())
        .map_err(|e| anyhow::anyhow!("invalid SIP URI '{}': {}", uri_str, e))
}

/// Long-running loop that maintains registration for a single trunk.
async fn registration_loop(
    name: String,
    config: TrunkConfig,
    cancel: CancellationToken,
    statuses: Arc<RwLock<HashMap<String, TrunkRegistrationStatus>>>,
    endpoint: EndpointInnerRef,
) {
    let expires = config.register_expires.unwrap_or(DEFAULT_REGISTER_EXPIRES);
    let refresh_interval = if expires > REFRESH_MARGIN_SECS {
        Duration::from_secs((expires - REFRESH_MARGIN_SECS) as u64)
    } else {
        Duration::from_secs(expires as u64 / 2)
    };

    // Build server URI.
    let server_uri = match parse_server_uri(&config.dest) {
        Ok(uri) => uri,
        Err(err) => {
            error!(trunk = %name, error = %err, "failed to parse trunk destination for registration");
            update_status(
                &statuses,
                &name,
                false,
                None,
                Some(format!("URI parse error: {err}")),
                None,
            );
            return;
        }
    };

    // Build credential if the trunk has auth configured.
    let credential = match (&config.username, &config.password) {
        (Some(user), Some(pass)) => Some(Credential {
            username: user.clone(),
            password: pass.clone(),
            realm: None, // extracted from server challenge
        }),
        (Some(user), None) => Some(Credential {
            username: user.clone(),
            password: String::new(),
            realm: None,
        }),
        _ => None,
    };

    let mut registration = Registration::new(endpoint, credential);

    let remote_str = server_uri.to_string();

    loop {
        let result = do_register(&name, &mut registration, &server_uri, expires).await;

        match result {
            Ok(actual_expires) => {
                update_status(
                    &statuses,
                    &name,
                    true,
                    Some(actual_expires),
                    None,
                    Some(remote_str.clone()),
                );
                info!(trunk = %name, expires = actual_expires, "trunk registration successful");
            }
            Err(err) => {
                update_status(
                    &statuses,
                    &name,
                    false,
                    None,
                    Some(err.to_string()),
                    Some(remote_str.clone()),
                );
                warn!(trunk = %name, error = %err, "trunk registration failed");
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                // Send un-REGISTER on cancellation.
                info!(trunk = %name, "sending un-REGISTER before shutdown");
                let _ = do_register(&name, &mut registration, &server_uri, 0).await;
                update_status(&statuses, &name, false, None, Some("cancelled".to_string()), None);
                return;
            }
            _ = sleep(refresh_interval) => {
                debug!(trunk = %name, "refreshing trunk registration");
            }
        }
    }
}

/// Perform a single REGISTER transaction via rsipstack's `Registration`.
///
/// Returns the effective expires value from the server response on success.
async fn do_register(
    trunk_name: &str,
    registration: &mut Registration,
    server_uri: &rsip::Uri,
    expires: u32,
) -> Result<u32> {
    let response = registration
        .register(server_uri.clone(), Some(expires))
        .await
        .map_err(|e| anyhow::anyhow!("REGISTER transaction error: {e}"))?;

    match response.status_code {
        StatusCode::OK => {
            let actual_expires = registration.expires();
            debug!(trunk = %trunk_name, actual_expires, "REGISTER 200 OK");
            Ok(actual_expires)
        }
        code => Err(anyhow::anyhow!("REGISTER rejected: {}", code)),
    }
}

fn update_status(
    statuses: &Arc<RwLock<HashMap<String, TrunkRegistrationStatus>>>,
    name: &str,
    registered: bool,
    expires: Option<u32>,
    error: Option<String>,
    remote_addr: Option<String>,
) {
    let mut map = statuses.write().unwrap();
    let entry = map
        .entry(name.to_string())
        .or_insert_with(|| TrunkRegistrationStatus {
            trunk_name: name.to_string(),
            registered: false,
            last_register_at: None,
            expires: None,
            error: None,
            remote_addr: None,
        });
    entry.registered = registered;
    entry.last_register_at = Some(Utc::now());
    entry.expires = expires;
    entry.error = error;
    if let Some(addr) = remote_addr {
        entry.remote_addr = Some(addr);
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_server_uri() {
        let uri = parse_server_uri("sip:provider.com").unwrap();
        assert_eq!(uri.to_string(), "sip:provider.com");

        let uri = parse_server_uri("sip:provider.com:5060").unwrap();
        assert_eq!(uri.to_string(), "sip:provider.com:5060");

        let uri = parse_server_uri("sip:user@provider.com").unwrap();
        assert_eq!(uri.to_string(), "sip:user@provider.com");

        let uri = parse_server_uri("10.0.0.1:5060").unwrap();
        assert_eq!(uri.to_string(), "sip:10.0.0.1:5060");

        let uri = parse_server_uri("provider.com").unwrap();
        assert_eq!(uri.to_string(), "sip:provider.com");
    }

    #[test]
    fn test_trunk_registrar_creation() {
        let registrar = TrunkRegistrar::new();
        let statuses = registrar.get_statuses();
        assert!(statuses.is_empty());
        assert!(registrar.get_status("nonexistent").is_none());
    }

    #[test]
    fn test_update_status() {
        let statuses = Arc::new(RwLock::new(HashMap::new()));

        update_status(
            &statuses,
            "trunk1",
            true,
            Some(300),
            None,
            Some("sip:1.2.3.4:5060".to_string()),
        );

        let map = statuses.read().unwrap();
        let s = map.get("trunk1").unwrap();
        assert!(s.registered);
        assert_eq!(s.expires, Some(300));
        assert!(s.error.is_none());
        assert_eq!(s.remote_addr.as_deref(), Some("sip:1.2.3.4:5060"));
        assert!(s.last_register_at.is_some());
    }

    #[test]
    fn test_update_status_error() {
        let statuses = Arc::new(RwLock::new(HashMap::new()));

        update_status(
            &statuses,
            "trunk2",
            false,
            None,
            Some("timeout".to_string()),
            Some("sip:1.2.3.4:5060".to_string()),
        );

        let map = statuses.read().unwrap();
        let s = map.get("trunk2").unwrap();
        assert!(!s.registered);
        assert_eq!(s.error.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn test_reconcile_without_endpoint_is_noop() {
        let registrar = TrunkRegistrar::new();

        let mut trunks = HashMap::new();
        trunks.insert(
            "test-trunk".to_string(),
            TrunkConfig {
                dest: "sip:192.0.2.1:5060".to_string(),
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                register_enabled: Some(true),
                register_expires: Some(60),
                ..Default::default()
            },
        );

        // Without endpoint set, reconcile should be a no-op.
        registrar.reconcile(&trunks).await;
        {
            let tokens = registrar.cancel_tokens.read().unwrap();
            assert!(tokens.is_empty());
        }
    }
}
