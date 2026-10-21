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
use dashmap::DashMap;
use rsipstack::sip::StatusCode;
use rsipstack::{
    dialog::{authenticate::Credential, registration::Registration},
    transaction::endpoint::EndpointInnerRef,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle, time::{sleep, timeout}};
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

struct ActiveRegistration {
    config: TrunkConfig,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// Manages outbound trunk registrations.
pub struct TrunkRegistrar {
    statuses: Arc<DashMap<String, TrunkRegistrationStatus>>,
    registrations: DashMap<String, ActiveRegistration>,
    /// Serialize reloads and shutdown while old tasks are being stopped.
    lifecycle: Mutex<()>,
    /// Global cancel token for the registrar (parent for all per-trunk tokens).
    parent_cancel: CancellationToken,
    /// SIP endpoint reference for creating transactions (set after server build).
    endpoint: RwLock<Option<EndpointInnerRef>>,
}

impl Drop for TrunkRegistrar {
    fn drop(&mut self) {
        self.parent_cancel.cancel();
    }
}

impl Default for TrunkRegistrar {
    fn default() -> Self {
        Self::new()
    }
}

impl TrunkRegistrar {
    pub fn new() -> Self {
        Self {
            statuses: Arc::new(DashMap::new()),
            registrations: DashMap::new(),
            lifecycle: Mutex::new(()),
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
        self.statuses
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Get the registration status of a specific trunk.
    pub fn get_status(&self, trunk_name: &str) -> Option<TrunkRegistrationStatus> {
        self.statuses.get(trunk_name).map(|r| r.value().clone())
    }

    /// Reconcile registrations with the current set of trunks.
    ///
    /// - Starts registration for new trunks with `register_enabled`.
    /// - Sends un-REGISTER for trunks that were removed or had registration disabled.
    /// - Re-starts registration for trunks whose config changed.
    pub async fn reconcile(&self, trunks: &HashMap<String, TrunkConfig>) {
        let _guard = self.lifecycle.lock().await;
        if self.parent_cancel.is_cancelled() {
            return;
        }
        let endpoint = { self.endpoint.read().unwrap().clone() };
        let Some(endpoint) = endpoint else {
            debug!("trunk registrar: no endpoint set yet, skipping reconcile");
            return;
        };

        let current_names: Vec<String> =
            { self.registrations.iter().map(|r| r.key().clone()).collect() };

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
            let unchanged = self.registrations.get(name.as_str()).is_some_and(|active| {
                let previous = &active.config;
                !active.task.is_finished()
                    && previous.dest == config.dest
                    && previous.outbound_proxy == config.outbound_proxy
                    && previous.username == config.username
                    && previous.password == config.password
                    && previous.register_expires.unwrap_or(DEFAULT_REGISTER_EXPIRES)
                        == config.register_expires.unwrap_or(DEFAULT_REGISTER_EXPIRES)
                    && previous.register_extra_headers == config.register_extra_headers
            });
            if !unchanged {
                self.stop_registration(name).await;
                info!(trunk = %name, dest = %config.dest, "starting trunk registration");
                self.start_registration(name.clone(), (*config).clone(), endpoint.clone());
            }
        }
    }

    /// Stop all registrations (used on shutdown).
    pub async fn stop_all(&self) {
        self.parent_cancel.cancel();
        let _guard = self.lifecycle.lock().await;
        let names: Vec<String> = self.registrations.iter().map(|r| r.key().clone()).collect();
        for name in names {
            self.stop_registration(&name).await;
        }
    }

    /// Stop registration for a single trunk, sending un-REGISTER.
    async fn stop_registration(&self, name: &str) {
        if let Some((_, mut active)) = self.registrations.remove(name) {
            active.cancel.cancel();
            // Allow a best-effort un-REGISTER, but do not block reload indefinitely
            // when the previous registrar is unreachable. Join before replacing it
            // so the old task cannot overwrite the new registration's status.
            if timeout(Duration::from_secs(3), &mut active.task).await.is_err() {
                active.task.abort();
                let _ = active.task.await;
            }
        }
        self.statuses.remove(name);
    }

    /// Spawn a background task for trunk registration.
    fn start_registration(&self, name: String, config: TrunkConfig, endpoint: EndpointInnerRef) {
        let child_token = self.parent_cancel.child_token();
        let task_config = config.clone();
        let task_name = name.clone();
        let task_cancel = child_token.clone();
        let statuses = self.statuses.clone();

        let task = crate::utils::spawn(async move {
            registration_loop(task_name, task_config, task_cancel, statuses, endpoint).await;
        });
        self.registrations.insert(name, ActiveRegistration {
            config,
            cancel: child_token,
            task,
        });
    }
}

/// Normalize a trunk destination string into an `rsipstack::sip::Uri`.
///
/// Handles bare `host:port`, strips angle brackets, and prepends `sip:` if no
/// scheme is present.  The `user@` part (if any) is kept — `Registration`
/// overwrites `to.uri.auth` from the credential anyway.
pub(crate) fn parse_server_uri(dest: &str) -> Result<rsipstack::sip::Uri> {
    let trimmed = dest.trim().trim_matches(|c| c == '<' || c == '>');
    let uri_str = if trimmed.starts_with("sip:") || trimmed.starts_with("sips:") {
        trimmed.to_string()
    } else {
        format!("sip:{}", trimmed)
    };
    rsipstack::sip::Uri::try_from(uri_str.as_str())
        .map_err(|e| anyhow::anyhow!("invalid SIP URI '{}': {}", uri_str, e))
}

/// Resolve the trunk's outbound proxy (`host[:port]` or SIP URI) to the
/// address REGISTERs are sent to. `None` when no proxy is configured.
async fn resolve_outbound_proxy(proxy: Option<&str>) -> Result<Option<SocketAddr>> {
    let Some(proxy) = proxy.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let uri = parse_server_uri(proxy)?;
    let port = uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);
    let host = uri.host().to_string();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| anyhow::anyhow!("cannot resolve outbound proxy '{proxy}': {e}"))?
        .next()
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("outbound proxy '{proxy}' resolved to no address"))
}

/// Long-running loop that maintains registration for a single trunk.
async fn registration_loop(
    name: String,
    config: TrunkConfig,
    cancel: CancellationToken,
    statuses: Arc<DashMap<String, TrunkRegistrationStatus>>,
    endpoint: EndpointInnerRef,
) {
    let expires = config.register_expires.unwrap_or(DEFAULT_REGISTER_EXPIRES);
    // Initial guess from the requested expires; recomputed from the
    // server-granted lifetime after every successful REGISTER.
    let mut refresh_after = if expires > REFRESH_MARGIN_SECS {
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
            auth_username: None, // falls back to `username` for the digest
            password: pass.clone(),
            realm: None, // extracted from server challenge
        }),
        (Some(user), None) => Some(Credential {
            username: user.clone(),
            auth_username: None,
            password: String::new(),
            realm: None,
        }),
        _ => None,
    };

    let mut registration = Registration::new(endpoint, credential);


    let remote_str = server_uri.to_string();

    loop {
        // Send through the outbound proxy (re-resolved each round); the
        // registrar in `dest` stays in Request-URI/From/To, unresolved.
        let result = match resolve_outbound_proxy(config.outbound_proxy.as_deref()).await {
            Ok(proxy) => {
                registration.outbound_proxy = proxy;
                do_register(&name, &mut registration, &server_uri, expires).await
            }
            Err(err) => Err(err),
        };

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
                refresh_after = if actual_expires > REFRESH_MARGIN_SECS {
                    Duration::from_secs((actual_expires - REFRESH_MARGIN_SECS) as u64)
                } else {
                    Duration::from_secs((actual_expires.max(2) / 2) as u64)
                };
                info!(
                    trunk = %name,
                    expires = actual_expires,
                    refresh_secs = refresh_after.as_secs(),
                    "trunk registration successful"
                );
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
            _ = sleep(refresh_after) => {
                debug!(trunk = %name, refresh_secs = refresh_after.as_secs(), "refreshing trunk registration");
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
    server_uri: &rsipstack::sip::Uri,
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
    statuses: &Arc<DashMap<String, TrunkRegistrationStatus>>,
    name: &str,
    registered: bool,
    expires: Option<u32>,
    error: Option<String>,
    remote_addr: Option<String>,
) {
    let mut entry = statuses
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

    #[tokio::test]
    async fn test_reconcile_restarts_changed_registration() {
        use rsipstack::transport::{TransportLayer, udp::UdpConnection};
        use rsipstack::transaction::endpoint::EndpointBuilder;
        use tokio::net::UdpSocket;

        let cancel = CancellationToken::new();
        let transports = TransportLayer::new(cancel.clone());
        let connection = UdpConnection::create_connection(
            "127.0.0.1:0".parse().unwrap(), None, Some(cancel.clone()),
        ).await.unwrap();
        transports.add_transport(connection.into());
        let endpoint = EndpointBuilder::new()
            .with_cancel_token(cancel.clone())
            .with_transport_layer(transports)
            .build();
        let _incoming = endpoint.incoming_transactions().unwrap();
        let registrar = TrunkRegistrar::new();
        registrar.set_endpoint(endpoint.inner.clone());
        let endpoint_task = tokio::spawn(async move { endpoint.serve().await });
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut addresses = Vec::new();
        let mut servers = Vec::new();
        for index in 0..2 {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            addresses.push(socket.local_addr().unwrap());
            let sent = sent.clone();
            servers.push(tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let (size, peer) = socket.recv_from(&mut buffer).await.unwrap();
                    let request = String::from_utf8(buffer[..size].to_vec()).unwrap();
                    let mut response = String::from("SIP/2.0 200 OK\r\n");
                    for line in request.lines().skip(1) {
                        let Some((name, _)) = line.split_once(':') else { continue };
                        if ["via", "from", "to", "call-id", "cseq", "contact", "expires"]
                            .contains(&name.to_ascii_lowercase().as_str())
                        {
                            response.push_str(line);
                            response.push_str("\r\n");
                        }
                    }
                    response.push_str("Content-Length: 0\r\n\r\n");
                    socket.send_to(response.as_bytes(), peer).await.unwrap();
                    if sent.send((index, request)).is_err() { break; }
                }
            }));
        }
        let mut trunks = HashMap::from([("trunk".to_string(), TrunkConfig {
            dest: addresses[0].to_string(),
            username: Some("alice".into()),
            register_enabled: Some(true),
            register_expires: Some(300),
            ..Default::default()
        })]);
        registrar.reconcile(&trunks).await;
        let (server, request) = timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
        assert_eq!(server, 0);
        assert!(request.starts_with(&format!("REGISTER sip:{} ", addresses[0])));
        // Unrelated call-routing changes must not churn registrations.
        trunks.get_mut("trunk").unwrap().max_calls = Some(10);
        registrar.reconcile(&trunks).await;
        assert!(timeout(Duration::from_millis(100), received.recv()).await.is_err());

        trunks.get_mut("trunk").unwrap().dest = addresses[1].to_string();
        registrar.reconcile(&trunks).await;
        let (server, request) = timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
        assert_eq!(server, 0);
        assert!(request.contains("Expires: 0\r\n"));
        let (server, request) = timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
        assert_eq!(server, 1);
        assert!(request.starts_with(&format!("REGISTER sip:{} ", addresses[1])));
        timeout(Duration::from_secs(2), async {
            loop {
                if registrar.get_status("trunk").is_some_and(|status| {
                    status.registered && status.remote_addr == Some(format!("sip:{}", addresses[1]))
                }) { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        // Credentials also belong to the registration task's configuration.
        trunks.get_mut("trunk").unwrap().username = Some("bob".into());
        registrar.reconcile(&trunks).await;
        let (_, request) = timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
        assert!(request.contains("Expires: 0\r\n"));
        let (_, request) = timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
        assert!(request.contains("sip:bob@"));

        trunks.get_mut("trunk").unwrap().disabled = Some(true);
        registrar.reconcile(&trunks).await;
        let (_, request) = timeout(Duration::from_secs(2), received.recv()).await.unwrap().unwrap();
        assert!(request.contains("Expires: 0\r\n"));
        assert!(registrar.get_status("trunk").is_none());
        assert!(registrar.registrations.is_empty());
        registrar.stop_all().await;
        cancel.cancel();
        endpoint_task.abort();
        for server in servers { server.abort(); }
    }

    #[tokio::test]
    async fn test_resolve_outbound_proxy() {
        assert_eq!(resolve_outbound_proxy(None).await.unwrap(), None);
        assert_eq!(resolve_outbound_proxy(Some("  ")).await.unwrap(), None);
        assert_eq!(
            resolve_outbound_proxy(Some("10.0.0.1:5080")).await.unwrap(),
            Some("10.0.0.1:5080".parse().unwrap())
        );
        assert_eq!(
            resolve_outbound_proxy(Some("sip:10.0.0.1;lr")).await.unwrap(),
            Some("10.0.0.1:5060".parse().unwrap())
        );
        let local = resolve_outbound_proxy(Some("localhost:5070")).await.unwrap().unwrap();
        assert!(local.ip().is_loopback() && local.port() == 5070);
    }

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
        let statuses = Arc::new(DashMap::new());

        update_status(
            &statuses,
            "trunk1",
            true,
            Some(300),
            None,
            Some("sip:1.2.3.4:5060".to_string()),
        );

        let s = statuses.get("trunk1").map(|r| r.value().clone()).unwrap();
        assert!(s.registered);
        assert_eq!(s.expires, Some(300));
        assert!(s.error.is_none());
        assert_eq!(s.remote_addr.as_deref(), Some("sip:1.2.3.4:5060"));
        assert!(s.last_register_at.is_some());
    }

    #[test]
    fn test_update_status_error() {
        let statuses = Arc::new(DashMap::new());

        update_status(
            &statuses,
            "trunk2",
            false,
            None,
            Some("timeout".to_string()),
            Some("sip:1.2.3.4:5060".to_string()),
        );

        let s = statuses.get("trunk2").map(|r| r.value().clone()).unwrap();
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
            assert!(registrar.registrations.is_empty());
        }
    }

    #[tokio::test]
    async fn test_reconcile_before_set_endpoint_then_after() {
        let registrar = TrunkRegistrar::new();

        let mut trunks = HashMap::new();
        trunks.insert(
            "trunk-a".to_string(),
            TrunkConfig {
                dest: "sip:192.0.2.1:5060".to_string(),
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                register_enabled: Some(true),
                register_expires: Some(60),
                ..Default::default()
            },
        );

        // First call without endpoint — must be silent no-op.
        registrar.reconcile(&trunks).await;
        assert!(
            registrar.registrations.is_empty(),
            "no token should be created before set_endpoint"
        );

        // Status should also be absent.
        assert!(registrar.get_status("trunk-a").is_none());
    }

    /// A disabled trunk must not be registered even when register_enabled=true.
    #[tokio::test]
    async fn test_reconcile_disabled_trunk_is_skipped() {
        let registrar = TrunkRegistrar::new();

        let mut trunks = HashMap::new();
        trunks.insert(
            "disabled-trunk".to_string(),
            TrunkConfig {
                dest: "sip:192.0.2.1:5060".to_string(),
                username: Some("u".to_string()),
                password: Some("p".to_string()),
                register_enabled: Some(true),
                disabled: Some(true),
                ..Default::default()
            },
        );

        registrar.reconcile(&trunks).await;
        assert!(
            registrar.registrations.is_empty(),
            "disabled trunk must not spawn a registration task"
        );
    }
}
