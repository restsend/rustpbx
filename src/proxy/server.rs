use super::{
    FnCreateProxyModule, ProxyAction, ProxyModule,
    data::ProxyDataContext,
    locator::{Locator, create_locator_with_migrate},
    user::{UserBackend, build_user_backend},
};
use crate::{
    auth::{jwt_auth_backend::JwtAuthBackend, jwt_validator::JwtValidator},
    auto_external_ip,
    call::{MediaConfig, TransactionCookie, policy::FrequencyLimiter},
    callrecord::{
        CallRecordSender,
        sipflow::{SipFlow, SipFlowBuilder},
    },
    config::{
        ClusterConfig, ClusterPeer, MediaProxyMode, ProxyConfig, RecordingPolicy, RtpConfig,
        SipFlowConfig,
    },
    proxy::{
        FnCreateRouteInvite,
        active_call_registry::ActiveProxyCallRegistry,
        auth::AuthBackend,
        call::{CallRouter, DialplanInspector},
        cluster_event::ClusterEventHub,
        locator::{
            DialogTargetLocator, LocatorEvent, LocatorEventSender, TransportInspectorLocator,
        },
        presence::PresenceManager,
    },
    sipflow::SipFlowBackend,
    sipflow::backend::create_backend,
};
use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{Auth, Param, Transport};
use rsipstack::{
    EndpointBuilder,
    dialog::dialog_layer::DialogLayer,
    sip::HostWithPort,
    transaction::{
        Endpoint, TransactionReceiver,
        endpoint::{EndpointOption, MessageInspector},
        transaction::Transaction,
    },
    transport::{
        SipAddr, SipConnection, TcpListenerConnection, TlsConfig, TlsListenerConnection,
        TransportLayer, WebSocketListenerConnection, udp::UdpConnection,
    },
};

use sea_orm::DatabaseConnection;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct SipServerInner {
    pub cancel_token: CancellationToken,
    pub rtp_config: ArcSwap<RtpConfig>,
    pub media_proxy: ArcSwap<MediaProxyMode>,
    pub proxy_config: ArcSwap<ProxyConfig>,
    pub data_context: Arc<ProxyDataContext>,
    /// Shared routing state (round-robin counters + policy guard) used by the
    /// inbound route path and by app/transfer/RWI-originated legs that opt into
    /// routing via `route_outbound_leg`. `CallModule::new` replaces the initial
    /// value with the one it constructs (which carries the policy guard).
    pub routing_state: Arc<parking_lot::RwLock<Arc<crate::call::RoutingState>>>,
    pub database: Option<DatabaseConnection>,
    pub user_backend: Box<dyn UserBackend>,
    pub auth_backend: Vec<Box<dyn AuthBackend>>,
    pub call_router: Option<Box<dyn CallRouter>>,
    pub dialplan_inspectors: Vec<Box<dyn DialplanInspector>>,
    pub locator: Arc<Box<dyn Locator>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub endpoint: Endpoint,
    pub dialog_layer: Arc<DialogLayer>,
    pub create_route_invites: Vec<FnCreateRouteInvite>,
    pub ignore_out_of_dialog_request: bool,
    pub locator_events: Option<LocatorEventSender>,
    pub sipflow_config: ArcSwap<Option<SipFlowConfig>>,
    pub recording_policy: ArcSwap<Option<RecordingPolicy>>,
    pub sip_flow: Option<SipFlow>,
    /// Lightweight SIP JSONL sidecar used when `[recording]` is on without a
    /// full `[sipflow]` backend (or with `force_file`).
    pub signaling_sidecar: Option<crate::callrecord::SignalingSidecar>,
    pub active_call_registry: Arc<ActiveProxyCallRegistry>,
    pub frequency_limiter: Option<Arc<dyn FrequencyLimiter>>,
    pub call_record_hooks: Arc<Vec<Box<dyn crate::callrecord::CallRecordHook>>>,
    pub runnings_tx: Arc<AtomicUsize>,
    pub storage: Option<crate::storage::Storage>,
    pub presence_manager: Arc<PresenceManager>,
    pub addon_registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
    pub rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
    /// IVR step trace collector (set by IVR Editor addon, accessed by StepIvrApp).
    pub ivr_trace: Option<Arc<crate::call::app::ivr::trace::IvrTraceCollector>>,
    pub tls_listener: Option<rsipstack::transport::TlsListenerConnection>,
    pub conference_manager: Arc<crate::call::runtime::ConferenceManager>,
    pub conference_server: Arc<crate::call::runtime::ConferenceServer>,
    pub agent_registry: Option<Arc<dyn crate::call::app::agent_registry::AgentRegistry>>,
    /// Optional hook for enriching resolved agent locations before dialing (e.g. injecting
    /// CC / CRM headers for screen-pop).  Registered by the cc addon via proxy_server_hook.
    pub queue_location_enricher: Option<Arc<dyn crate::proxy::call::QueueLocationEnricher>>,
    /// Subscribers for REFER NOTIFY events from SipSession.
    pub transfer_notify_subscribers:
        Arc<tokio::sync::Mutex<Vec<crate::call::domain::ReferNotifyTx>>>,
    /// Cluster event hub for local event dispatch to addon handlers.
    pub cluster_event_hub: Option<Arc<ClusterEventHub>>,
    /// SIP peer IPs for cluster auth bypass.
    pub cluster_peer_ips: Vec<IpAddr>,
    /// Cluster self address resolved from `[cluster].peers` at startup by matching
    /// the local endpoint listener against peer `addr:sip_port` entries. Used as
    /// the `home_proxy` stamped on registrations so cluster peers route INVITEs to
    /// this node's cluster-internal (reachable) address rather than a NAT/external
    /// address. `None` in single-node mode (no `[cluster]` peers), falling back to
    /// `default_contact_uri()`.
    pub cluster_self_addr: Option<SipAddr>,
    /// Cluster-wide session location registry (which node owns which call).
    /// Backend selected by `[cluster].session_registry_backend`; no-op when
    /// cluster is not configured.
    pub session_registry: crate::call::runtime::SessionRegistryRef,
    /// Keeps the owning node's sessions alive (single batch update per tick).
    pub session_registry_heartbeat: Option<crate::call::runtime::NodeHeartbeat>,
    /// Media policy for deciding when to anchor media.
    pub media_policy: Arc<dyn crate::call::MediaPolicy>,
    /// Trunk health check states (populated by trunk_health background loop).
    pub trunk_health: Option<crate::proxy::trunk_health::HealthStateMap>,
    /// Session lifecycle hooks (connected, held, unheld, ended).
    pub session_hooks: Arc<Vec<Arc<dyn crate::proxy::proxy_call::session_hooks::CallSessionHook>>>,
    /// Resolved contact username (from config or random hex).
    pub contact_username: String,
    /// Resolved CNAME for SDP ssrc attributes (from config or random hex).
    pub rtc_cname: String,
    /// Live emergency routing config, shared with `EmergencyInspector` so a
    /// hot-reload updates the numbers/trunk without restarting.
    pub emergency_config: ArcSwap<Option<crate::config::EmergencyConfig>>,
}

fn random_hex() -> String {
    format!("{:016x}", rand::random::<u64>())
}

pub type SipServerRef = Arc<SipServerInner>;

#[derive(Clone)]
pub struct SipServer {
    pub inner: SipServerRef,
    modules: Arc<Vec<Box<dyn ProxyModule>>>,
}

pub struct SipServerBuilder {
    rtp_config: Option<RtpConfig>,
    config: Arc<ProxyConfig>,
    cancel_token: Option<CancellationToken>,
    user_backend: Option<Box<dyn UserBackend>>,
    auth_backend: Vec<Box<dyn AuthBackend>>,
    call_router: Option<Box<dyn CallRouter>>,
    module_fns: HashMap<String, FnCreateProxyModule>,
    locator: Option<Box<dyn Locator>>,
    callrecord_sender: Option<CallRecordSender>,
    message_inspectors: Vec<Box<dyn MessageInspector>>,
    dialplan_inspectors: Vec<Box<dyn DialplanInspector>>,
    create_route_invites: Vec<FnCreateRouteInvite>,
    database: Option<DatabaseConnection>,
    data_context: Option<Arc<ProxyDataContext>>,
    ignore_out_of_dialog_request: bool,
    locator_events: Option<LocatorEventSender>,
    frequency_limiter: Option<Arc<dyn FrequencyLimiter>>,
    call_record_hooks: Vec<Box<dyn crate::callrecord::CallRecordHook>>,
    storage: Option<crate::storage::Storage>,
    sipflow_config: Option<SipFlowConfig>,
    /// Pre-built SipFlow backend (takes precedence over sipflow_config).
    sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
    no_bind: bool,
    /// Addon registry for accessing call applications (voicemail, ivr, etc.)
    addon_registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
    /// RWI gateway to wire into the server for call-app factory use.
    rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
    ivr_trace: Option<Arc<crate::call::app::ivr::trace::IvrTraceCollector>>,
    /// AgentRegistry for agent management and presence state.
    agent_registry: Option<Arc<dyn crate::call::app::agent_registry::AgentRegistry>>,
    queue_location_enricher: Option<Arc<dyn crate::proxy::call::QueueLocationEnricher>>,
    skip_migrate: bool,
    /// Cluster peer SocketAddrs for inter-node sync (derived from Config.cluster).
    cluster_peers: Vec<SocketAddr>,
    /// Original `[cluster]` config (peers etc.) used to resolve the self peer
    /// address at build time for `home_proxy` stamping.
    cluster_config: Option<ClusterConfig>,
    /// Media policy for deciding when to anchor media.
    media_policy: Option<Arc<dyn crate::call::MediaPolicy>>,
    /// Trunk health check states (shared map populated by background loop).
    trunk_health: Option<crate::proxy::trunk_health::HealthStateMap>,
    /// Session lifecycle hooks registered via [`SipServerBuilder::with_session_hook`].
    session_hooks: Vec<Arc<dyn crate::proxy::proxy_call::session_hooks::CallSessionHook>>,
}

impl SipServerBuilder {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            rtp_config: None,
            cancel_token: None,
            user_backend: None,
            auth_backend: Vec::new(),
            call_router: None,
            module_fns: HashMap::new(),
            locator: None,
            callrecord_sender: None,
            message_inspectors: Vec::new(),
            dialplan_inspectors: Vec::new(),
            create_route_invites: Vec::new(),
            database: None,
            data_context: None,
            ignore_out_of_dialog_request: true,
            locator_events: None,
            frequency_limiter: None,
            call_record_hooks: Vec::new(),
            storage: None,
            sipflow_config: None,
            sipflow_backend: None,
            no_bind: false,
            addon_registry: None,
            rwi_gateway: None,
            ivr_trace: None,
            agent_registry: None,
            queue_location_enricher: None,
            skip_migrate: false,
            cluster_peers: Vec::new(),
            cluster_config: None,
            media_policy: None,
            trunk_health: None,
            session_hooks: Vec::new(),
        }
    }

    pub fn with_trunk_health(mut self, health: crate::proxy::trunk_health::HealthStateMap) -> Self {
        self.trunk_health = Some(health);
        self
    }

    pub fn with_media_policy(mut self, policy: Arc<dyn crate::call::MediaPolicy>) -> Self {
        self.media_policy = Some(policy);
        self
    }

    pub fn with_cluster_peers(mut self, peers: Vec<SocketAddr>) -> Self {
        self.cluster_peers = peers;
        self
    }

    pub fn with_cluster_config(mut self, config: Option<ClusterConfig>) -> Self {
        self.cluster_config = config;
        self
    }

    pub fn with_sipflow_config(mut self, config: Option<SipFlowConfig>) -> Self {
        self.sipflow_config = config;
        self
    }

    /// Use a pre-built SipFlow backend (takes precedence over `with_sipflow_config`).
    /// This allows sharing a single backend instance with other components, e.g.
    /// `SipFlowUploadHook`, avoiding duplicate writers to the same spool directory.
    pub fn with_sipflow_backend(mut self, backend: Option<Arc<dyn SipFlowBackend>>) -> Self {
        self.sipflow_backend = backend;
        self
    }

    pub fn with_no_bind(mut self, no_bind: bool) -> Self {
        self.no_bind = no_bind;
        self
    }

    pub fn with_user_backend(mut self, user_backend: Box<dyn UserBackend>) -> Self {
        self.user_backend = Some(user_backend);
        self
    }

    pub fn with_ignore_out_of_dialog_request(mut self, ignore: bool) -> Self {
        self.ignore_out_of_dialog_request = ignore;
        self
    }

    pub fn with_auth_backend(mut self, auth_backend: Box<dyn AuthBackend>) -> Self {
        self.auth_backend.push(auth_backend);
        self
    }

    pub fn with_call_router(mut self, call_router: Box<dyn CallRouter>) -> Self {
        self.call_router = Some(call_router);
        self
    }

    pub fn with_dialplan_inspector(
        mut self,
        dialplan_inspector: Box<dyn DialplanInspector>,
    ) -> Self {
        self.dialplan_inspectors.push(dialplan_inspector);
        self
    }

    pub fn with_locator(mut self, locator: Box<dyn Locator>) -> Self {
        self.locator = Some(locator);
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_create_route_invite(mut self, f: FnCreateRouteInvite) -> Self {
        self.create_route_invites.push(f);
        self
    }

    pub fn register_module(mut self, name: &str, module_fn: FnCreateProxyModule) -> Self {
        self.module_fns.insert(name.to_lowercase(), module_fn);
        self
    }

    pub fn with_callrecord_sender(mut self, callrecord_sender: Option<CallRecordSender>) -> Self {
        self.callrecord_sender = callrecord_sender;
        self
    }

    pub fn with_message_inspector(mut self, inspector: Box<dyn MessageInspector>) -> Self {
        self.message_inspectors.push(inspector);
        self
    }

    pub fn with_rtp_config(mut self, config: RtpConfig) -> Self {
        self.rtp_config = Some(config);
        self
    }

    pub fn with_database_connection(mut self, db: DatabaseConnection) -> Self {
        self.database = Some(db);
        self
    }

    pub fn with_data_context(mut self, context: Arc<ProxyDataContext>) -> Self {
        self.data_context = Some(context);
        self
    }

    pub fn with_locator_events(mut self, locator_events: Option<LocatorEventSender>) -> Self {
        self.locator_events = locator_events;
        self
    }

    pub fn with_frequency_limiter(mut self, limiter: Arc<dyn FrequencyLimiter>) -> Self {
        self.frequency_limiter = Some(limiter);
        self
    }

    pub fn with_call_record_hooks(
        mut self,
        hooks: Vec<Box<dyn crate::callrecord::CallRecordHook>>,
    ) -> Self {
        self.call_record_hooks = hooks;
        self
    }

    pub fn with_storage(mut self, storage: crate::storage::Storage) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn with_addon_registry(
        mut self,
        registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
    ) -> Self {
        self.addon_registry = registry;
        self
    }

    pub fn with_rwi_gateway(mut self, gateway: crate::rwi::RwiGatewayRef) -> Self {
        self.rwi_gateway = Some(gateway);
        self
    }

    pub fn with_agent_registry(
        mut self,
        registry: Arc<dyn crate::call::app::agent_registry::AgentRegistry>,
    ) -> Self {
        self.agent_registry = Some(registry);
        self
    }

    pub fn with_queue_location_enricher(
        mut self,
        enricher: Arc<dyn crate::proxy::call::QueueLocationEnricher>,
    ) -> Self {
        self.queue_location_enricher = Some(enricher);
        self
    }

    /// Register a session lifecycle hook. Multiple hooks can be added; they are
    /// called in registration order.
    pub fn with_session_hook(
        mut self,
        hook: Arc<dyn crate::proxy::proxy_call::session_hooks::CallSessionHook>,
    ) -> Self {
        self.session_hooks.push(hook);
        self
    }

    pub fn with_skip_migrate(mut self, skip: bool) -> Self {
        self.skip_migrate = skip;
        self
    }

    pub async fn build(mut self) -> Result<SipServer> {
        let user_backend = if let Some(backend) = self.user_backend {
            backend
        } else {
            match build_user_backend(self.config.as_ref()).await {
                Ok(backend) => backend,
                Err(e) => {
                    warn!(
                        "failed to create user backend: {} {:?}",
                        e, &self.config.user_backends
                    );
                    return Err(e);
                }
            }
        };

        // Build JWT auth backend if configured
        let mut auth_backend = self.auth_backend;
        if let Some(ref jwt_cfg) = self.config.jwt_auth {
            if jwt_cfg.enabled {
                let validator = JwtValidator::new(jwt_cfg);
                let local_ub = if jwt_cfg.check_local_user {
                    match build_user_backend(self.config.as_ref()).await {
                        Ok(ub) => Some(ub),
                        Err(e) => {
                            warn!("failed to create user backend for JWT auth: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
                let jwt_backend =
                    JwtAuthBackend::new(validator, local_ub, jwt_cfg.sip_header_name.clone());
                info!(
                    header = %jwt_cfg.sip_header_name,
                    check_local = jwt_cfg.check_local_user,
                    "JWT auth backend enabled"
                );
                auth_backend.push(Box::new(jwt_backend));
            }
        }

        // Build HTTP token auth backend from user_backends config
        for backend_cfg in &self.config.user_backends {
            if let crate::config::UserBackendConfig::Http {
                url,
                method,
                username_field,
                realm_field,
                request_uri_field,
                headers,
                sip_headers,
                token_header: Some(token_hdr),
                http_timeout_ms,
                http_retry_count,
                http_retry_delay_ms,
                token_cache_ttl_secs,
                token_cache_size,
            } = backend_cfg
            {
                let http_backend = crate::proxy::user_http::HttpUserBackend::new(
                    url,
                    method,
                    username_field,
                    realm_field,
                    request_uri_field,
                    headers,
                    sip_headers,
                    &Some(token_hdr.clone()),
                    http_timeout_ms,
                    http_retry_count,
                    http_retry_delay_ms,
                );
                let cache_ttl = Duration::from_secs(token_cache_ttl_secs.unwrap_or(0));
                let cache_size = token_cache_size.unwrap_or(10000);
                let token_backend = crate::auth::http_token_auth_backend::HttpTokenAuthBackend::new(
                    http_backend,
                    token_hdr.clone(),
                    cache_ttl,
                    cache_size,
                );
                info!(
                    header = %token_hdr,
                    cache_ttl_secs = token_cache_ttl_secs.unwrap_or(0),
                    "HTTP token auth backend enabled"
                );
                auth_backend.push(Box::new(token_backend));
            }
        }

        let locator = if let Some(locator) = self.locator {
            locator
        } else {
            match create_locator_with_migrate(&self.config.locator, !self.skip_migrate).await {
                Ok(locator) => locator,
                Err(e) => {
                    warn!("failed to create locator: {} {:?}", e, self.config.locator);
                    return Err(e);
                }
            }
        };

        let locator = Arc::new(locator);
        let mut rtp_config = self.rtp_config.unwrap_or_default();
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = self.config.clone();
        #[cfg(unix)]
        log_rlimit_nofile().await;
        let transport_layer = TransportLayer::new(cancel_token.clone());
        if let Some(ca_path) = config.tls_ca_certificates.as_deref()
            && !ca_path.trim().is_empty()
        {
            let ca_path = ca_path.trim();
            let ca_certs = tokio::fs::read(ca_path).await.map_err(|e| {
                anyhow!(
                    "failed to read outbound SIP/TLS CA certificates {}: {}",
                    ca_path,
                    e
                )
            })?;
            transport_layer.set_tls_config(TlsConfig {
                ca_certs: Some(ca_certs),
                ..Default::default()
            });
            info!(
                path = ca_path,
                "configured outbound SIP/TLS CA certificates"
            );
        }
        // Clone of TLS listener for hot-reload support (initialized inside if !self.no_bind block)
        let mut tls_listener_clone: Option<rsipstack::transport::TlsListenerConnection> = None;

        let mut local_addrs: HashSet<SocketAddr> = HashSet::new();

        if !self.no_bind {
            let local_addr = config
                .addr
                .parse::<IpAddr>()
                .map_err(|e| anyhow!("failed to parse local ip address: {}", e))?;

            // Auto-detect external IP if not manually configured
            if rtp_config.external_ip.is_none() {
                if let Some(ref url) = rtp_config.auto_external_ip {
                    match auto_external_ip::detect_external_ip(url).await {
                        Ok(ip) => {
                            warn!(
                                "auto_external_ip: detected {} from '{}'",
                                ip,
                                if url.is_empty() {
                                    auto_external_ip::DEFAULT_AUTO_EXTERNAL_IP_URL
                                } else {
                                    url
                                }
                            );
                            rtp_config.external_ip = Some(ip.to_string());
                        }
                        Err(e) => {
                            warn!("auto_external_ip: detection failed: {}", e);
                        }
                    }
                }
            }

            let external_ip = match rtp_config.external_ip {
                Some(ref s) => Some(
                    s.parse::<IpAddr>()
                        .map_err(|e| anyhow!("failed to parse external ip address {}: {}", s, e))?,
                ),
                None => None,
            };

            if config.all_udp_ports().is_empty()
                && config.tcp_port.is_none()
                && config.tls_port.is_none()
                && config.ws_port.is_none()
            {
                return Err(anyhow::anyhow!(
                    "No port specified, please specify at least one port: udp, tcp, tls, ws"
                ));
            }

            for udp_port in config.all_udp_ports() {
                let local_addr = SocketAddr::new(local_addr, udp_port);
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(*ip, udp_port));
                let udp_conn = UdpConnection::create_connection(
                    local_addr,
                    external_addr,
                    Some(cancel_token.child_token()),
                )
                .await
                .map_err(|e| {
                    anyhow!("Failed to create proxy UDP connection {} {}", local_addr, e)
                })?;
                info!("start proxy, udp port: {}", udp_conn.get_addr());
                transport_layer.add_transport(udp_conn.into());
                local_addrs.insert(local_addr);
            }

            if let Some(tcp_port) = config.tcp_port {
                let local_addr = SocketAddr::new(local_addr, tcp_port);
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(*ip, tcp_port));
                let tcp_conn = TcpListenerConnection::new(local_addr.into(), external_addr)
                    .await
                    .map_err(|e| anyhow!("Failed to create TCP connection: {}", e))?;
                info!("start proxy, tcp port: {}", tcp_conn.get_addr());
                transport_layer.add_transport(tcp_conn.into());
                local_addrs.insert(local_addr);
            }

            if let Some(tls_port) = config.tls_port {
                let local_addr = SocketAddr::new(local_addr, tls_port);
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(*ip, tls_port));

                let cert_path = config
                    .ssl_certificate
                    .as_ref()
                    .ok_or_else(|| anyhow!("ssl_certificate is required for tls transport"))?;

                let key_path = config
                    .ssl_private_key
                    .as_ref()
                    .ok_or_else(|| anyhow!("ssl_private_key is required for tls transport"))?;

                let mut well_done = true;
                if !std::path::Path::new(cert_path).exists() {
                    well_done = false;
                    warn!("ssl_certificate file does not exist: {}", cert_path);
                }

                if !std::path::Path::new(key_path).exists() {
                    well_done = false;
                    warn!("ssl_private_key file does not exist: {}", key_path);
                }

                if well_done {
                    match async {
                        let cert = tokio::fs::read(cert_path)
                            .await
                            .map_err(|e| anyhow!("failed to read cert: {}", e))?;
                        let key = tokio::fs::read(key_path)
                            .await
                            .map_err(|e| anyhow!("failed to read key: {}", e))?;
                        Ok::<_, anyhow::Error>((cert, key))
                    }
                    .await
                    {
                        Ok((cert_data, key_data)) => {
                            let tls_config = TlsConfig {
                                cert: Some(cert_data),
                                key: Some(key_data),
                                client_cert: None,
                                client_key: None,
                                ca_certs: None,
                                sni_hostname: None,
                            };
                            match TlsListenerConnection::new(
                                local_addr.into(),
                                external_addr,
                                tls_config,
                            )
                            .await
                            {
                                Ok(conn) => {
                                    info!(
                                        "start proxy, tls port: {} cert: {}, key: {}",
                                        conn.get_addr(),
                                        cert_path,
                                        key_path
                                    );
                                    // Clone for hot-reload support
                                    tls_listener_clone = Some(conn.clone());
                                    transport_layer.add_transport(conn.into());
                                    local_addrs.insert(local_addr);
                                }
                                Err(e) => {
                                    warn!("failed to create TLS connection: {}", e);
                                }
                            };
                        }
                        Err(e) => {
                            warn!("failed to read TLS files: {}", e);
                        }
                    }
                } else {
                    warn!("skip starting TLS transport due to missing certificate or key");
                }
            }

            if let Some(ws_port) = config.ws_port {
                let local_addr = SocketAddr::new(local_addr, ws_port);
                let external_addr = external_ip.as_ref().map(|ip| SocketAddr::new(*ip, ws_port));
                let ws_conn =
                    WebSocketListenerConnection::new(local_addr.into(), external_addr, false)
                        .await
                        .map_err(|e| anyhow!("Failed to create WS connection: {}", e))?;
                info!("start proxy, ws port: {}", ws_conn.get_addr());
                transport_layer.add_transport(ws_conn.into());
                local_addrs.insert(local_addr);
            }
        }

        let mut endpoint_builder = EndpointBuilder::new();
        if let Some(ref user_agent) = config.useragent {
            endpoint_builder.with_user_agent(user_agent.as_str());
        }

        let mut endpoint_option = EndpointOption {
            callid_suffix: config.callid_suffix.clone(),
            ..Default::default()
        };

        if let Some(t1_timer) = config.t1_timer {
            endpoint_option.t1 = Duration::from_millis(t1_timer);
        }

        if let Some(t1x64_timer) = config.t1x64_timer {
            endpoint_option.t1x64 = Duration::from_millis(t1x64_timer);
        }

        let endpoint_local_addrs = transport_layer.get_addrs();
        let mut endpoint_builder = endpoint_builder
            .with_cancel_token(cancel_token.clone())
            .with_option(endpoint_option)
            .with_transport_layer(transport_layer);

        let advertised_methods = Arc::new(OnceLock::new());
        let mut inspectors: Vec<Box<dyn MessageInspector>> = self.message_inspectors;
        if self.config.nat_fix {
            inspectors.insert(0, Box::new(super::nat::NatInspector::new()));
        }
        inspectors.push(Box::new(
            super::capability_headers::CapabilityHeadersInspector::new(advertised_methods.clone()),
        ));

        let mut sip_flow = None;
        let sipflow_backend = if let Some(backend) = self.sipflow_backend.take() {
            Some(backend)
        } else if let Some(cfg) = self.sipflow_config.as_ref() {
            create_backend(cfg, cancel_token.clone())
                .await
                .ok()
                .map(|b| Arc::from(b) as Arc<dyn SipFlowBackend>)
        } else {
            None
        };
        if let Some(backend) = sipflow_backend {
            info!("Sipflow backend initialized");
            let local_addr_strs: Vec<String> = endpoint_local_addrs
                .iter()
                .map(|a| a.addr.to_string())
                .collect();
            let sflow = SipFlowBuilder::new()
                .with_backend(backend)
                .with_local_addrs(local_addr_strs)
                .build();
            sip_flow = Some(sflow.clone());
            inspectors.push(Box::new(sflow));
        }

        // Recording sidecar: sessions with recording enabled register their
        // Call-ID so SIP messages are appended to a local JSONL without
        // requiring a full `[sipflow]` backend.
        let signaling_sidecar = {
            let sc = crate::callrecord::SignalingSidecar::new();
            inspectors.push(Box::new(sc.clone()));
            Some(sc)
        };

        endpoint_builder =
            endpoint_builder.with_inspector(
                Box::new(CompositeMessageInspector { inspectors }) as Box<dyn MessageInspector>
            );

        let locator_events = self.locator_events.unwrap_or_else(|| {
            let (tx, _) = tokio::sync::broadcast::channel(12);
            tx
        });

        let locator_local_addrs = endpoint_local_addrs;
        let cluster_enabled = !self.cluster_peers.is_empty();

        endpoint_builder = endpoint_builder
            .with_target_locator(DialogTargetLocator::new(
                locator.clone(),
                locator_local_addrs,
                cluster_enabled,
            ))
            .with_transport_inspector(TransportInspectorLocator::new(
                locator.clone(),
                locator_events.clone(),
            ));

        let endpoint = endpoint_builder.build();

        // Resolve this node's cluster-internal address from `[cluster].peers` by
        // matching a peer `addr:sip_port` entry against the local endpoint
        // listeners. The matching peer entry IS this node; its address is what
        // other peers use to reach us, so it is stamped as `home_proxy` on
        // registrations instead of a possibly-NATed/external endpoint address.
        let cluster_self_addr = self.cluster_config.as_ref().and_then(|cc| {
            if cc.peers.is_empty() {
                return None;
            }
            let local_addr_strs: Vec<String> = endpoint
                .get_addrs()
                .iter()
                .map(|a| a.addr.to_string())
                .collect();
            match resolve_cluster_self_addr(&cc.peers, &local_addr_strs) {
                Some(addr) => {
                    info!(%addr, "resolved cluster self peer address for home_proxy stamping");
                    Some(addr)
                }
                None => {
                    warn!(
                        "cluster peers configured but none matched local endpoint addresses \
                         (endpoint_addrs={local_addr_strs:?}); home_proxy will fall back \
                         to default_contact_uri()"
                    );
                    None
                }
            }
        });

        let mut call_router = self.call_router;
        if call_router.is_none()
            && let Some(http_router_config) = &self.config.http_router
        {
            call_router = Some(Box::new(crate::proxy::routing::http::HttpCallRouter::new(
                http_router_config.clone(),
                ArcSwap::new(Arc::new(rtp_config.clone())),
                ArcSwap::new(Arc::new(self.config.media_proxy)),
                self.config.enable_latching,
                self.config.latching_probation_max_packets,
            )));
        }
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        let database = self.database.clone();

        let data_context = if let Some(ref context) = self.data_context {
            context.clone()
        } else {
            let dc = Arc::new(
                ProxyDataContext::new(self.config.clone(), database.clone())
                    .await
                    .map_err(|err| anyhow!("failed to initialize proxy data context: {err}"))?,
            );
            self.data_context = Some(dc.clone());
            dc
        };
        // Wire up the SIP endpoint for trunk registration, then reconcile so
        // that trunks with register_enabled=true are registered on startup
        // (previously reconcile ran before set_endpoint and was silently skipped).
        data_context
            .trunk_registrar()
            .set_endpoint(endpoint.inner.clone());
        {
            let trunks = data_context.trunks_snapshot();
            data_context.trunk_registrar().reconcile(&trunks).await;
        }

        let active_call_registry = Arc::new(ActiveProxyCallRegistry::new());
        let presence_manager = Arc::new(PresenceManager::new(database.clone()));
        presence_manager.load_from_db().await.ok();

        // Create cluster event hub for local event dispatch.
        // Always provision the hub so local locator + presence events are
        // dispatched to addon handlers (e.g. the CC registrar bridge) even on a
        // single node — AMI cluster sync is a no-op without peers.
        let cluster_peer_ips: Vec<IpAddr> = self.cluster_peers.iter().map(|p| p.ip()).collect();
        let cluster_event_hub: Arc<ClusterEventHub> = Arc::new(ClusterEventHub::new(
            locator_events.clone(),
            presence_manager.clone(),
            cancel_token.child_token(),
        ));
        cluster_event_hub.set_dialog_layer(dialog_layer.clone());
        // Start the local event-dispatch loop.
        cluster_event_hub.clone().start().await;
        let cluster_event_hub: Option<Arc<ClusterEventHub>> = Some(cluster_event_hub);

        // Background sweeper for expired SIP REGISTER bindings.
        //
        // Why we need this:
        //   Transport-layer cleanup (TransportInspectorLocator::handle on
        //   TransportEvent::Closed) only fires when the TCP/WS connection is
        //   politely closed. When a client vanishes without sending a SIP
        //   REGISTER expires=0 AND without emitting a WebSocket Close frame
        //   (e.g. browser tab closed, network loss, laptop sleep), the only
        //   signal we have is the REGISTER expiry itself. `MemoryLocator`'s
        //   opportunistic GC only runs during `lookup`/`register`, so a
        //   binding that nobody looks up would linger forever, leaving the
        //   CC agent stuck in `idle`. This task periodically sweeps expired
        //   bindings and broadcasts `LocatorEvent::Offline` so the registrar
        //   bridge can move the agent back to `offline`.
        {
            let locator_for_sweep = locator.clone();
            let locator_events_for_sweep = locator_events.clone();
            let sweep_token = cancel_token.child_token();
            tokio::spawn(async move {
                // Run roughly every quarter of the shortest typical registrar
                // expiry (the server caps Contact expiry at ~50s, see
                // registrar.rs `max_registrar_expires`). 15s gives us <1 expiry
                // interval of latency between the binding technically expiring
                // and the agent being marked offline.
                const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
                let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Skip the immediate first tick — there is nothing to sweep
                // right after boot and we don't want to log a noisy "swept 0"
                // line during startup.
                ticker.tick().await;
                loop {
                    tokio::select! {
                        biased;
                        _ = sweep_token.cancelled() => break,
                        _ = ticker.tick() => {
                            match locator_for_sweep.sweep_expired().await {
                                Ok(removed) if !removed.is_empty() => {
                                    info!(
                                        count = removed.len(),
                                        "locator swept expired registrations"
                                    );
                                    let _ = locator_events_for_sweep
                                        .send(LocatorEvent::Offline(removed));
                                }
                                Ok(_) => {}
                                Err(e) => warn!(error = %e, "locator sweep failed"),
                            }
                        }
                    }
                }
            });
        }

        // Create conference manager with in-server audio mixing
        let conference_manager = Arc::new(crate::call::runtime::ConferenceManager::new());
        let conference_server = Arc::new(crate::call::runtime::ConferenceServer::new(
            conference_manager.clone(),
        ));

        // Build the cluster-wide session registry.  Backend selection:
        //   "db"     (cluster default) — shared PostgreSQL/MySQL
        //   "memory"                  — in-process, single-node/small deploys
        //   "noop"/"disabled"         — explicitly disable even with peers set
        //   no peers                  — no-op (single-node)
        let (session_registry, session_registry_heartbeat): (
            crate::call::runtime::SessionRegistryRef,
            Option<crate::call::runtime::NodeHeartbeat>,
        ) = {
            use crate::call::runtime::{
                DbSessionRegistry, MemorySessionRegistry, NoopSessionRegistry,
            };
            let node_id = cluster_self_addr
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_else(|| "local".to_string());
            match self.cluster_config.as_ref() {
                Some(cfg) if !cfg.peers.is_empty() => {
                    let ttl = Duration::from_secs(cfg.session_registry_ttl_secs);
                    let heartbeat = Duration::from_secs(cfg.session_registry_heartbeat_secs);
                    let registry: crate::call::runtime::SessionRegistryRef =
                        match cfg.session_registry_backend.as_str() {
                            "memory" => MemorySessionRegistry::new(node_id.clone(), ttl),
                            "noop" | "disabled" => {
                                info!(
                                    backend = %cfg.session_registry_backend,
                                    "session registry disabled despite cluster peers"
                                );
                                Arc::new(NoopSessionRegistry)
                            }
                            _ => {
                                // "db" (default) requires the shared database.
                                if let Some(db) = database.clone() {
                                    DbSessionRegistry::new(db, ttl)
                                } else {
                                    warn!(
                                        "cluster session registry backend \"db\" requested but no \
                                         database configured; falling back to noop"
                                    );
                                    Arc::new(NoopSessionRegistry)
                                }
                            }
                        };
                    // Keep locally-owned sessions alive with a single batch
                    // update per tick.  Harmless for a noop registry (no-op).
                    let heartbeat_task = crate::call::runtime::NodeHeartbeat::spawn(
                        registry.clone(),
                        node_id,
                        heartbeat,
                    );
                    (registry, Some(heartbeat_task))
                }
                _ => (Arc::new(NoopSessionRegistry), None),
            }
        };

        // Create trunk health state map BEFORE inner so inner.trunk_health is populated
        // (the health loop itself is spawned after inner since it needs endpoint/cancel_token).
        let trunk_health_states: crate::proxy::trunk_health::HealthStateMap =
            Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        self.trunk_health = Some(trunk_health_states.clone());

        let inner = Arc::new(SipServerInner {
            rtp_config: ArcSwap::new(Arc::new(rtp_config)),
            media_proxy: ArcSwap::new(Arc::new(self.config.media_proxy)),
            proxy_config: ArcSwap::from_pointee(self.config.as_ref().clone()),
            cancel_token,
            data_context,
            routing_state: Arc::new(parking_lot::RwLock::new(Arc::new(
                crate::call::RoutingState::new(),
            ))),
            database: database.clone(),
            user_backend,
            auth_backend,
            call_router,
            locator: locator.clone(),
            callrecord_sender: self.callrecord_sender,
            endpoint,
            dialog_layer,
            dialplan_inspectors: self.dialplan_inspectors,
            create_route_invites: self.create_route_invites,
            ignore_out_of_dialog_request: self.ignore_out_of_dialog_request,
            locator_events: Some(locator_events),
            sipflow_config: ArcSwap::new(Arc::new(self.sipflow_config.clone())),
            recording_policy: ArcSwap::new(Arc::new(self.config.recording.clone())),
            sip_flow,
            signaling_sidecar,
            active_call_registry,
            frequency_limiter: self.frequency_limiter,
            call_record_hooks: Arc::new(self.call_record_hooks),
            runnings_tx: Arc::new(AtomicUsize::new(0)),
            storage: self.storage,
            presence_manager,
            addon_registry: self.addon_registry,
            rwi_gateway: self.rwi_gateway,
            ivr_trace: self.ivr_trace,
            tls_listener: tls_listener_clone,
            conference_manager,
            conference_server,
            agent_registry: self.agent_registry,
            queue_location_enricher: self.queue_location_enricher,
            transfer_notify_subscribers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            cluster_event_hub,
            cluster_peer_ips,
            cluster_self_addr,
            session_registry,
            session_registry_heartbeat,
            media_policy: self
                .media_policy
                .unwrap_or_else(|| Arc::new(crate::call::DefaultMediaPolicy)),
            trunk_health: self.trunk_health.clone(),
            session_hooks: Arc::new(self.session_hooks),
            contact_username: self
                .config
                .contact_username
                .clone()
                .unwrap_or_else(random_hex),
            rtc_cname: self.config.rtc_cname.clone().unwrap_or_else(random_hex),
            emergency_config: ArcSwap::from_pointee(self.config.emergency.clone()),
        });

        let inner_weak = Arc::downgrade(&inner);
        inner.locator.set_realm_checker(Arc::new(move |realm| {
            let inner = inner_weak.clone();
            let realm = realm.to_string();
            Box::pin(async move {
                if let Some(inner) = inner.upgrade() {
                    inner.is_same_realm(&realm).await
                } else {
                    false
                }
            })
        }));

        let mut allow_methods = Vec::new();
        let mut modules = Vec::new();

        if let Some(load_modules) = self.config.modules.as_ref() {
            let start_time = Instant::now();
            for name in load_modules.iter() {
                if let Some(module_fn) = self.module_fns.get(name) {
                    let module_start_time = Instant::now();
                    let mut module = match module_fn(inner.clone(), self.config.clone()) {
                        Ok(module) => module,
                        Err(e) => {
                            warn!("failed to create module {}: {}", name, e);
                            continue;
                        }
                    };
                    match module.on_start().await {
                        Ok(_) => {}
                        Err(e) => {
                            warn!("failed to start module {}: {}", name, e);
                            continue;
                        }
                    }
                    allow_methods.extend(module.allow_methods());
                    modules.push(module);

                    debug!(
                        "module {} loaded in {:?}",
                        name,
                        module_start_time.elapsed()
                    );
                } else {
                    warn!("module {} not found", name);
                }
            }
            // remove duplicate methods
            let mut i = 0;
            while i < allow_methods.len() {
                let mut j = i + 1;
                while j < allow_methods.len() {
                    if allow_methods[i] == allow_methods[j] {
                        allow_methods.remove(j);
                    } else {
                        j += 1;
                    }
                }
                i += 1;
            }

            info!(
                "modules loaded in {:?} modules: {:?} allows: {}",
                start_time.elapsed(),
                modules.iter().map(|m| m.name()).collect::<Vec<_>>(),
                allow_methods
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        // ── Trunk health check ──────────────────────────────────────
        if let Some(ref dc) = self.data_context {
            let local_sip = format!(
                "{}:{}",
                self.config.addr,
                self.config.udp_port.unwrap_or(5060),
            );
            let ep = inner.endpoint.inner.clone();
            let dc = dc.clone();
            crate::proxy::trunk_health::spawn_health_loop(
                move || dc.trunks_snapshot(),
                trunk_health_states,
                ep,
                local_sip,
                30u64,
                inner.cancel_token.clone(),
            );
        }

        advertised_methods
            .set(allow_methods.clone())
            .map_err(|_| anyhow!("advertised SIP methods already initialized"))?;
        inner.endpoint.inner.allows.lock().replace(allow_methods);
        Ok(SipServer {
            inner,
            modules: Arc::new(modules),
        })
    }
}

impl SipServer {
    /// Get a clone of the TLS listener for hot-reload support
    pub fn get_tls_listener(&self) -> Option<rsipstack::transport::TlsListenerConnection> {
        self.inner.tls_listener.clone()
    }

    pub async fn serve(&self) -> Result<()> {
        let incoming = self.inner.endpoint.incoming_transactions()?;
        let cancel_token = self.inner.cancel_token.clone();

        if let Some(webhook_config) = &self.inner.proxy_config.load().locator_webhook
            && let Some(events) = &self.inner.locator_events
        {
            let rx = events.subscribe();
            crate::utils::spawn(super::locator_webhook::handle_locator_webhook(
                webhook_config.clone(),
                rx,
            ));
        }

        // Spawn registry cleanup task
        let registry = self.inner.active_call_registry.clone();
        let cleanup_cancel = cancel_token.clone();
        crate::utils::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = cleanup_cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let removed = registry.cleanup_stale(std::time::Duration::from_secs(3600));
                        if removed > 0 {
                            tracing::warn!("Cleaned up {} stale registry entries", removed);
                        }
                    }
                }
            }
        });

        // Spawn active_calls Prometheus gauge sampling task
        let registry_for_metrics = self.inner.active_call_registry.clone();
        let metrics_cancel = cancel_token.clone();
        crate::utils::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = metrics_cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let count = registry_for_metrics.count();
                        crate::metrics::sip::set_active_dialogs(count);
                    }
                }
            }
        });

        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("cancelled");
            }
            _ = self.inner.endpoint.serve() => {
                info!("endpoint finished");
            }
            _ = self.handle_incoming(incoming) => {
                info!("incoming transactions stopped");
            }
        };

        for module in self.modules.iter() {
            match module.on_stop().await {
                Ok(_) => {}
                Err(e) => {
                    warn!("failed to stop module {}: {}", module.name(), e);
                }
            }
        }
        info!("stopped");
        Ok(())
    }
    pub fn stop(&self) {
        self.inner.cancel_token.cancel();
    }

    pub fn get_inner(&self) -> SipServerRef {
        self.inner.clone()
    }

    pub fn get_modules(&self) -> impl Iterator<Item = &Box<dyn ProxyModule>> {
        self.modules.iter()
    }

    pub fn get_cancel_token(&self) -> CancellationToken {
        self.inner.cancel_token.clone()
    }

    async fn handle_incoming(&self, mut incoming: TransactionReceiver) -> Result<()> {
        let mut tx_count: u64 = 0;
        while let Some(mut tx) = incoming.recv().await {
            crate::metrics::transaction::received();
            crate::sip_telemetry::SipTelemetry::tx_received();
            tx_count += 1;
            if tx_count % 100 == 0 {
                let stats = self.inner.endpoint.inner.get_stats();
                crate::metrics::transaction::set_endpoint_running(stats.running_transactions);
                crate::metrics::transaction::set_endpoint_finished(stats.finished_transactions);
                crate::metrics::transaction::set_endpoint_waiting_ack(stats.waiting_ack);
                crate::metrics::transaction::set_running(
                    self.inner.runnings_tx.load(Ordering::Relaxed),
                );
            }
            let modules = self.modules.clone();

            let token = tx
                .connection
                .as_ref()
                .and_then(|c| c.cancel_token())
                .unwrap_or_else(|| self.inner.cancel_token.clone())
                .child_token();

            let runnings_tx = self.inner.runnings_tx.clone();

            if let Some(max_concurrency) = self.inner.proxy_config.load().max_concurrency
                && runnings_tx.load(Ordering::Relaxed) >= max_concurrency
            {
                warn!(
                    key = %tx.key,
                    runnings = runnings_tx.load(Ordering::Relaxed),
                    "max concurrency reached, not process this transaction"
                );
                crate::metrics::transaction::rejected("max_concurrency");
                tx.reply(rsipstack::sip::StatusCode::ServiceUnavailable)
                    .await
                    .ok();
                continue;
            }
            // Spam protection for out-of-dialog requests
            if self.inner.ignore_out_of_dialog_request
                && matches!(
                    tx.original.method,
                    rsipstack::sip::Method::Options
                        | rsipstack::sip::method::Method::Info
                        | rsipstack::sip::method::Method::Refer
                        | rsipstack::sip::method::Method::Update
                )
            {
                let to_tag = tx
                    .original
                    .to_header()
                    .and_then(|to| to.tag())
                    .ok()
                    .flatten();
                if to_tag.is_none() {
                    let via_ip = crate::proxy::routing::extract_via_ip(&tx.original);
                    let via_ip_str = via_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    if tx.original.method == rsipstack::sip::Method::Options {
                        let is_trunk = if let Some(ref ip) = via_ip {
                            let inbound_trunks = self.inner.data_context.acl_inbound_trunks.load();
                            let source_network = ipnet::IpNet::from(*ip);
                            inbound_trunks
                                .cover_values(&source_network)
                                .next()
                                .is_some()
                        } else {
                            false
                        };
                        if is_trunk {
                            info!(key = %tx.key, via_ip = %via_ip_str, "responding 200 OK OPTIONS (trunk health probe)");
                            tx.reply(rsipstack::sip::StatusCode::OK).await.ok();
                            continue;
                        }
                    }
                    debug!(key = %tx.key, via_ip = %via_ip_str, "ignoring out-of-dialog {} request", tx.original.method);
                    continue;
                }
            }
            crate::utils::spawn(async move {
                runnings_tx.fetch_add(1, Ordering::Relaxed);
                let start_time = Instant::now();
                let cookie = TransactionCookie::from(&tx.key);
                let guard = token.clone().drop_guard();
                select! {
                    r = Self::process_transaction(token.clone(), modules, cookie.clone(),  &mut tx) => {
                        let final_status = tx.last_response.as_ref().map(|r| r.status_code());
                        match r {
                            Ok(_) => {
                                debug!(key = %tx.key, ?final_status, "transaction processed in {:?}", start_time.elapsed());
                            },
                            Err(e) => {
                                warn!(key = %tx.key, ?final_status, "failed to process transaction: {} in {:?}", e, start_time.elapsed());
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        info!(key = %tx.key, "transaction cancelled");
                    }
                };
                crate::metrics::transaction::latency_seconds(start_time.elapsed().as_secs_f64());
                crate::sip_telemetry::SipTelemetry::record_tx_latency(start_time.elapsed());
                runnings_tx.fetch_sub(1, Ordering::Relaxed);
                let is_mid_dialog = tx
                    .original
                    .to_header()
                    .ok()
                    .and_then(|h| h.tag().ok().flatten())
                    .is_some();

                if !matches!(
                    tx.original.method,
                    rsipstack::sip::Method::Bye
                        | rsipstack::sip::Method::Cancel
                        | rsipstack::sip::Method::Ack
                ) && !is_mid_dialog
                    && tx.last_response.is_none()
                    && !cookie.is_spam()
                {
                    tx.reply(rsipstack::sip::StatusCode::NotImplemented)
                        .await
                        .ok();
                }
                let _ = guard;
                Ok::<(), anyhow::Error>(())
            });
        }
        Ok(())
    }

    async fn process_transaction(
        token: CancellationToken,
        modules: Arc<Vec<Box<dyn ProxyModule>>>,
        cookie: TransactionCookie,
        tx: &mut Transaction,
    ) -> Result<()> {
        for module in modules.iter() {
            match module
                .on_transaction_begin(token.clone(), tx, cookie.clone())
                .await
            {
                Ok(action) => match action {
                    ProxyAction::Continue => {}
                    ProxyAction::Abort => break,
                },
                Err(e) => {
                    warn!(
                        key = %tx.key,
                        module = module.name(),
                        "failed to handle transaction: {}",
                        e
                    );
                    if tx.last_response.is_none() {
                        tx.reply(rsipstack::sip::StatusCode::ServerInternalError)
                            .await
                            .ok();
                    }
                    return Ok(());
                }
            }
        }

        for module in modules.iter() {
            match module.on_transaction_end(tx).await {
                Ok(_) => {}
                Err(e) => {
                    warn!(key = %tx.key, "failed to handle transaction: {}", e);
                }
            }
        }
        Ok(())
    }
}

impl Drop for SipServerInner {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        info!("SipServerInner dropped");
    }
}

impl SipServerInner {
    pub fn default_contact_uri(&self) -> Option<rsipstack::sip::Uri> {
        let addr = self.endpoint.get_addrs().first()?.clone();
        Some(build_contact_uri(&self.contact_username, &addr, None))
    }

    /// Build a Contact URI for a response to an inbound SIP transaction.
    ///
    /// Stream connections retain the advertised local address of the listener
    /// that accepted the request, so using the transaction connection keeps the
    /// Contact scheme, transport, host, and port on that same listener.
    pub fn contact_uri_for_transaction(&self, tx: &Transaction) -> Option<rsipstack::sip::Uri> {
        let connection = tx.connection.as_ref()?;

        // RustPBX's HTTP WebSocket adapter represents a client with a synthetic
        // ChannelConnection whose address is the remote client's address. It
        // must never be advertised as the server's Contact. Preserve the
        // existing endpoint-level fallback for that adapter.
        if matches!(connection, SipConnection::Channel(_)) {
            return None;
        }

        Some(build_contact_uri(
            &self.contact_username,
            connection.get_addr(),
            Some(connection.transport()),
        ))
    }

    pub fn default_media_config(&self) -> MediaConfig {
        let rtp = self.rtp_config.load();
        let media_proxy = **self.media_proxy.load();
        let proxy_config = self.proxy_config.load();
        MediaConfig::new()
            .with_proxy_mode(media_proxy)
            .with_external_ip(rtp.external_ip.clone())
            .with_bind_ip(rtp.bind_ip.clone())
            .with_rtp_start_port(rtp.start_port)
            .with_rtp_end_port(rtp.end_port)
            .with_webrtc_start_port(rtp.webrtc_start_port)
            .with_webrtc_end_port(rtp.webrtc_end_port)
            .with_ice_servers(rtp.ice_servers.clone())
            .with_enable_latching(proxy_config.enable_latching)
            .with_probation_max_packets(proxy_config.latching_probation_max_packets)
            .with_comfort_noise(rtp.comfort_noise, rtp.comfort_noise_level_db)
    }

    /// Hot-reload the full `[proxy]` section plus related platform settings
    /// from the on-disk configuration. The live `ProxyConfig` snapshot is
    /// swapped atomically so all per-request / per-call reads (`rtp_timeout`,
    /// dos settings, session timers, realms, codecs, latching, etc.) observe
    /// the new values on the next request/call. Existing in-flight calls keep
    /// the config they were created with. `data_context` is re-synced so trunk,
    /// route and ACL reloads also see the updated config.
    pub async fn reload_proxy_config(&self, config_path: &str) -> Result<String> {
        let config = crate::config::Config::load_async(config_path)
            .await
            .map_err(|e| anyhow!("Failed to load config: {e}"))?;

        let mut new_proxy = config.proxy.clone();
        if new_proxy.recording.is_none() {
            new_proxy.recording = config.recording.clone();
        }
        new_proxy.ensure_recording_defaults();

        let old_proxy = self.proxy_config.load();
        let new_arc = Arc::new(new_proxy.clone());

        // Atomically swap the live proxy config.
        self.proxy_config.store(new_arc.clone());

        // Keep data_context in sync so trunk/route/ACL reloads reuse the
        // updated config (generated dirs, use_db_config, etc.).
        self.data_context.update_config(new_arc.clone());

        // Propagate the platform-level ArcSwap fields that are also derived
        // from the top-level config sections.
        let mut new_rtp = config.rtp_config();
        if new_rtp.external_ip.is_none() {
            if let Some(ref url) = new_rtp.auto_external_ip {
                if let Ok(ip) = crate::auto_external_ip::detect_external_ip(url).await {
                    tracing::info!(ip = %ip, url = %url, "auto_external_ip detected on proxy reload");
                    new_rtp.external_ip = Some(ip.to_string());
                }
            }
        }
        self.rtp_config.store(Arc::new(new_rtp));
        self.media_proxy.store(Arc::new(config.proxy.media_proxy));
        self.recording_policy
            .store(Arc::new(new_proxy.recording.clone()));
        self.emergency_config
            .store(Arc::new(new_proxy.emergency.clone()));

        // Push the new emergency config into the shared inspector so existing
        // inspections observe the updated numbers/trunk immediately.
        for inspector in &self.dialplan_inspectors {
            if let Some(emg) = inspector.as_any()
                && let Some(inspector) =
                    emg.downcast_ref::<crate::proxy::emergency::EmergencyInspector>()
            {
                inspector.reload_from(&new_proxy);
            }
        }

        let mut parts: Vec<String> = Vec::new();
        let old = &old_proxy;
        if old.rtp_timeout != new_proxy.rtp_timeout {
            parts.push("rtp_timeout".to_string());
        }
        if old.realms != new_proxy.realms {
            parts.push("realms".to_string());
        }
        if old.registrar_expires != new_proxy.registrar_expires
            || old.max_registrar_expires != new_proxy.max_registrar_expires
        {
            parts.push("registrar_expires".to_string());
        }
        if old.dos_enabled != new_proxy.dos_enabled
            || old.dos_max_cps_per_ip != new_proxy.dos_max_cps_per_ip
            || old.dos_max_concurrent_per_ip != new_proxy.dos_max_concurrent_per_ip
            || old.dos_scan_probe_threshold != new_proxy.dos_scan_probe_threshold
            || old.dos_scan_block_duration_secs != new_proxy.dos_scan_block_duration_secs
        {
            parts.push("dos".to_string());
        }
        if old.session_timer != new_proxy.session_timer
            || old.session_timer_always != new_proxy.session_timer_always
            || old.session_expires != new_proxy.session_expires
        {
            parts.push("session_timer".to_string());
        }
        if old.audio_codecs != new_proxy.audio_codecs
            || old.video_codecs != new_proxy.video_codecs
            || format!("{:?}", old.audio_profile) != format!("{:?}", new_proxy.audio_profile)
        {
            parts.push("audio/video codecs".to_string());
        }
        if old.enable_latching != new_proxy.enable_latching
            || old.latching_probation_max_packets != new_proxy.latching_probation_max_packets
        {
            parts.push("latching".to_string());
        }
        if old.max_concurrency != new_proxy.max_concurrency {
            parts.push("max_concurrency".to_string());
        }
        if old.hold_music != new_proxy.hold_music {
            parts.push("hold_music".to_string());
        }
        if old.parallel_fork != new_proxy.parallel_fork
            || old.passthrough_failure != new_proxy.passthrough_failure
        {
            parts.push("routing".to_string());
        }
        if old.max_ring_time != new_proxy.max_ring_time {
            parts.push("max_ring_time".to_string());
        }
        if format!("{:?}", old.locator_webhook) != format!("{:?}", new_proxy.locator_webhook) {
            parts.push("locator_webhook".to_string());
        }
        if format!("{:?}", old.jwt_auth) != format!("{:?}", new_proxy.jwt_auth) {
            parts.push("jwt_auth".to_string());
        }
        if format!("{:?}", old.user_backends) != format!("{:?}", new_proxy.user_backends) {
            parts.push("user_backends".to_string());
        }
        if format!("{:?}", old.http_router) != format!("{:?}", new_proxy.http_router) {
            parts.push("http_router".to_string());
        }
        if old.ua_white_list != new_proxy.ua_white_list
            || old.ua_black_list != new_proxy.ua_black_list
            || old.trusted_proxies != new_proxy.trusted_proxies
            || old.uri_max_length != new_proxy.uri_max_length
            || old.uri_reject_malformed != new_proxy.uri_reject_malformed
        {
            parts.push("acl/uri".to_string());
        }
        if format!("{:?}", old.emergency) != format!("{:?}", new_proxy.emergency) {
            parts.push("emergency".to_string());
        }
        if old.blind_transfer_use_refer != new_proxy.blind_transfer_use_refer {
            parts.push("transfer".to_string());
        }
        if old.session_cmd_channel_capacity != new_proxy.session_cmd_channel_capacity
            || old.session_state_channel_capacity != new_proxy.session_state_channel_capacity
        {
            parts.push("session channel capacity".to_string());
        }

        if parts.is_empty() {
            Ok("Proxy config reloaded (no changes detected)".to_string())
        } else {
            tracing::info!(changed = %parts.join(", "), "Proxy config hot-reloaded");
            Ok(format!("Proxy config applied: {}", parts.join(", ")))
        }
    }

    /// Hot-reload recording policy from the on-disk configuration. New calls
    /// will use the updated policy immediately; existing calls are unaffected.
    pub async fn reload_recording_settings(&self, config_path: &str) -> Result<String> {
        let config = crate::config::Config::load_async(config_path)
            .await
            .map_err(|e| anyhow!("Failed to load config: {e}"))?;

        let new_policy = config.proxy.recording.or(config.recording);

        self.recording_policy.store(Arc::new(new_policy.clone()));

        if let Some(ref policy) = new_policy {
            if policy.enabled.unwrap_or(false) {
                Ok(format!(
                    "Recording policy applied (enabled, type={})",
                    policy
                        .recording_type
                        .as_ref()
                        .map(|t| format!("{t:?}"))
                        .unwrap_or_else(|| "default".to_string())
                ))
            } else {
                Ok("Recording policy applied (disabled)".to_string())
            }
        } else {
            Ok("Recording policy cleared".to_string())
        }
    }
    pub async fn reload_sipflow(&self, config_path: &str) -> Result<String> {
        let config = crate::config::Config::load_async(config_path)
            .await
            .map_err(|e| anyhow!("Failed to load config: {e}"))?;

        let old_mode = self
            .sipflow_config
            .load()
            .as_ref()
            .as_ref()
            .map(|c| match c {
                crate::config::SipFlowConfig::Local { .. } => "local",
                crate::config::SipFlowConfig::Remote { .. } => "remote",
            })
            .unwrap_or("none");

        if let Some(ref new_cfg) = config.sipflow {
            let new_backend =
                crate::sipflow::backend::create_backend(new_cfg, self.cancel_token.clone())
                    .await
                    .map_err(|e| anyhow!("Failed to create SipFlow backend: {e}"))?;
            let new_backend: Arc<dyn crate::sipflow::SipFlowBackend> = Arc::from(new_backend);

            let new_mode = match new_cfg {
                crate::config::SipFlowConfig::Local { .. } => "local",
                crate::config::SipFlowConfig::Remote { .. } => "remote",
            };

            if let Some(ref sf) = self.sip_flow {
                sf.swap_backend(new_backend);
            }
            self.sipflow_config.store(Arc::new(Some(new_cfg.clone())));

            tracing::info!(old_mode, new_mode, "SipFlow backend hot-reloaded");
            Ok(format!(
                "SipFlow backend hot-reloaded: {old_mode} → {new_mode}"
            ))
        } else {
            if let Some(ref sf) = self.sip_flow {
                sf.clear_backend();
            }
            self.sipflow_config.store(Arc::new(None));

            tracing::info!(old_mode, "SipFlow backend removed (disabled)");
            Ok(format!("SipFlow disabled (was {old_mode})"))
        }
    }

    pub async fn is_same_realm(&self, callee_realm: &str) -> bool {
        let (host, port) = if let Some(pos) = callee_realm.find(':') {
            (
                &callee_realm[..pos],
                callee_realm[pos + 1..].parse::<u16>().ok(),
            )
        } else {
            (callee_realm, None)
        };

        let proxy_config = self.proxy_config.load();
        let is_my_port = |p: u16| {
            proxy_config.udp_port == Some(p)
                || proxy_config.tcp_port == Some(p)
                || proxy_config.tls_port == Some(p)
                || proxy_config.ws_port == Some(p)
        };

        match host {
            "localhost" | "127.0.0.1" | "::1" => port.map(is_my_port).unwrap_or(true),
            _ => {
                if let Some(external_ip) = self.rtp_config.load().external_ip.as_ref()
                    && external_ip == host
                {
                    return port.map(is_my_port).unwrap_or(true);
                }
                if let Some(realms) = proxy_config.realms.as_ref() {
                    for item in realms {
                        if item == callee_realm {
                            return true;
                        }
                        if item == host {
                            return port.map(is_my_port).unwrap_or(true);
                        }
                    }
                }
                let realms_empty = proxy_config.realms.as_ref().map_or(true, |v| v.is_empty());
                if self.endpoint.get_addrs().iter().any(|addr| {
                    let addr_host = addr.addr.host.to_string();
                    if addr_host == host {
                        port.map(|p| addr.addr.port == Some(p.into()))
                            .unwrap_or(true)
                    } else if realms_empty && (addr_host == "0.0.0.0" || addr_host == "::") {
                        port.map(|p| addr.addr.port == Some(p.into()))
                            .unwrap_or(true)
                    } else {
                        false
                    }
                }) {
                    return true;
                }
                self.user_backend.is_same_realm(callee_realm).await
            }
        }
    }

    /// Resolve the original-header passthrough rule for a callee destination.
    ///
    /// Internal destinations (same realm / registered AOR / home-proxy) always
    /// passthrough every custom header. External destinations fall back to the
    /// destination trunk's `header_passthrough` config (if any); otherwise no
    /// custom headers are forwarded.
    ///
    /// `callee_uri` is used only for destination-trunk matching (host:port), so
    /// it should be the outbound callee URI (e.g. the original request's To URI).
    pub async fn header_passthrough_for(
        &self,
        target: &crate::call::Location,
        callee_is_same_realm: bool,
        callee_uri: &rsipstack::sip::Uri,
    ) -> Option<crate::proxy::routing::HeaderPassthrough> {
        use crate::proxy::routing::{HeaderPassthrough, find_trunk_by_dest};

        let internal =
            callee_is_same_realm || target.registered_aor.is_some() || target.home_proxy.is_some();
        if internal {
            return Some(HeaderPassthrough::all());
        }

        let host = callee_uri.host().to_string();
        let port = callee_uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);
        let trunks = self.data_context.trunks_snapshot();
        find_trunk_by_dest(&trunks, &host, port).and_then(|trunk| trunk.header_passthrough.clone())
    }
}

/// Resolve this node's cluster-internal address from the `[cluster].peers` list by
/// matching a peer `addr:sip_port` entry against the local endpoint listener
/// addresses (`addr:port` strings). The matching peer entry IS this node, so its
/// address is what other peers use to reach us. Returns `None` when no peer entry
/// matches (e.g. NAT where the peer address is not on a local interface).
fn resolve_cluster_self_addr(peers: &[ClusterPeer], local_addr_strs: &[String]) -> Option<SipAddr> {
    let matched = peers.iter().find(|p| {
        let peer_sip_addr = format!("{}:{}", p.addr, p.sip_port);
        local_addr_strs.iter().any(|la| la == &peer_sip_addr)
    })?;
    let peer_sip_addr = format!("{}:{}", matched.addr, matched.sip_port);
    HostWithPort::try_from(peer_sip_addr.as_str())
        .ok()
        .map(|addr| SipAddr { r#type: None, addr })
}

fn build_contact_uri(
    contact_username: &str,
    addr: &SipAddr,
    fallback_transport: Option<Transport>,
) -> rsipstack::sip::Uri {
    let transport = addr.r#type.or(fallback_transport);
    let mut params = Vec::new();
    if let Some(transport) = transport
        && !matches!(transport, Transport::Udp)
    {
        params.push(Param::Transport(transport));
    }
    rsipstack::sip::Uri {
        scheme: transport.map(|t| t.sip_scheme()),
        auth: Some(Auth {
            user: contact_username.to_string(),
            password: None,
        }),
        host_with_port: addr.addr.clone(),
        params,
        ..Default::default()
    }
}

struct CompositeMessageInspector {
    inspectors: Vec<Box<dyn MessageInspector>>,
}

impl MessageInspector for CompositeMessageInspector {
    fn before_send(
        &self,
        mut msg: rsipstack::sip::SipMessage,
        dest: Option<&rsipstack::transport::SipAddr>,
    ) -> rsipstack::sip::SipMessage {
        for inspector in &self.inspectors {
            msg = inspector.before_send(msg, dest);
        }
        msg
    }

    fn after_received(
        &self,
        mut msg: rsipstack::sip::SipMessage,
        from: Option<&rsipstack::transport::SipAddr>,
    ) -> rsipstack::sip::SipMessage {
        for inspector in &self.inspectors {
            msg = inspector.after_received(msg, from);
        }
        msg
    }
}

#[cfg(unix)]
async fn log_rlimit_nofile() {
    if let Ok(content) = tokio::fs::read_to_string("/proc/self/limits").await {
        for line in content.lines() {
            if line.contains("open files") || line.contains("Max open files") {
                info!("{line}");
                return;
            }
        }
    }
    // Fallback: check current fd count vs a reasonable estimate
    let mut count = 0;
    if let Ok(mut entries) = tokio::fs::read_dir("/proc/self/fd").await {
        while let Ok(Some(_)) = entries.next_entry().await {
            count += 1;
        }
    }
    info!("RLIMIT_NOFILE: current fd count ~{count}");
}

#[cfg(test)]
mod contact_uri_tests {
    use super::*;
    use crate::config::ClusterPeer;
    use rsipstack::sip::HostWithPort;

    fn sip_addr(value: &str, transport: Option<Transport>) -> SipAddr {
        SipAddr {
            r#type: transport,
            addr: HostWithPort::try_from(value).expect("valid SIP address"),
        }
    }

    #[test]
    fn contact_uri_uses_tls_listener_address_and_transport() {
        let addr = sip_addr("[2001:db8::20]:5061", Some(Transport::Tls));

        let uri = build_contact_uri("rustpbx", &addr, Some(Transport::Udp));

        assert_eq!(
            uri.to_string(),
            "sips:rustpbx@[2001:db8::20]:5061;transport=TLS"
        );
    }

    #[test]
    fn contact_uri_falls_back_to_connection_transport() {
        let addr = sip_addr("[2001:db8::20]:5061", None);

        let uri = build_contact_uri("rustpbx", &addr, Some(Transport::Tls));

        assert_eq!(
            uri.to_string(),
            "sips:rustpbx@[2001:db8::20]:5061;transport=TLS"
        );
    }

    #[test]
    fn udp_contact_uri_does_not_add_transport_parameter() {
        let addr = sip_addr("192.0.2.20:5060", Some(Transport::Udp));

        let uri = build_contact_uri("rustpbx", &addr, None);

        assert_eq!(uri.to_string(), "sip:rustpbx@192.0.2.20:5060");
    }

    #[test]
    fn resolve_cluster_self_addr_matches_peer_sip_port_against_local_listener() {
        let peers = vec![
            ClusterPeer {
                addr: "172.25.224.232".to_string(),
                sip_port: 15060,
                ami_port: 13080,
            },
            ClusterPeer {
                addr: "172.25.225.2".to_string(),
                sip_port: 15060,
                ami_port: 13080,
            },
        ];
        // This node is the first peer (Node B); its listener matches peer[0].
        let local_addrs = vec![
            "172.25.224.232:15060".to_string(),
            "116.62.250.247:15060".to_string(),
        ];

        let result = resolve_cluster_self_addr(&peers, &local_addrs).unwrap();

        assert_eq!(result.addr.to_string(), "172.25.224.232:15060");
        assert!(result.r#type.is_none());
    }

    #[test]
    fn resolve_cluster_self_addr_returns_none_when_no_peer_matches() {
        let peers = vec![ClusterPeer {
            addr: "10.0.0.1".to_string(),
            sip_port: 5060,
            ami_port: 5038,
        }];
        let local_addrs = vec!["172.25.224.232:15060".to_string()];

        assert!(resolve_cluster_self_addr(&peers, &local_addrs).is_none());
    }

    #[test]
    fn resolve_cluster_self_addr_returns_none_for_empty_peer_list() {
        let local_addrs = vec!["127.0.0.1:15060".to_string()];

        assert!(resolve_cluster_self_addr(&[], &local_addrs).is_none());
    }

    #[tokio::test]
    async fn reload_proxy_config_swaps_live_snapshot_and_syncs_data_context() {
        use crate::proxy::tests::common::create_test_server;
        use std::io::Write;

        let (server, _) = create_test_server().await;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rustpbx.toml");
        let mut file = std::fs::File::create(&path).expect("create config file");
        let toml = r#"
[proxy]
addr = "0.0.0.0"
rtp_timeout = 42
registrar_expires = 120
dos_enabled = true
session_timer = true
max_ring_time = 45
"#;
        file.write_all(toml.as_bytes()).expect("write config");
        drop(file);

        let msg = server
            .reload_proxy_config(path.to_str().unwrap())
            .await
            .expect("reload should succeed");

        assert!(
            msg.contains("rtp_timeout"),
            "message should mention rtp_timeout, got: {msg}"
        );
        assert_eq!(server.proxy_config.load().rtp_timeout, Some(42));
        assert_eq!(server.proxy_config.load().registrar_expires, Some(120));
        assert!(server.proxy_config.load().dos_enabled);
        assert!(server.proxy_config.load().session_timer);
        assert_eq!(server.proxy_config.load().max_ring_time, Some(45));
        assert!(
            msg.contains("max_ring_time"),
            "message should mention max_ring_time, got: {msg}"
        );
        // data_context must see the same live config.
        assert_eq!(server.data_context.config().rtp_timeout, Some(42));
    }

    #[tokio::test]
    async fn reload_proxy_config_reports_no_changes_when_identical() {
        use crate::proxy::tests::common::create_test_server;
        use std::io::Write;

        let (server, config) = create_test_server().await;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rustpbx.toml");
        let mut file = std::fs::File::create(&path).expect("create config file");
        let toml = format!(
            "\n[proxy]\naddr = \"0.0.0.0\"\nrtp_timeout = {}\n",
            config.rtp_timeout.unwrap_or(15)
        );
        file.write_all(toml.as_bytes()).expect("write config");
        drop(file);

        // First reload applies, second is a no-change.
        server
            .reload_proxy_config(path.to_str().unwrap())
            .await
            .expect("reload should succeed");
        let msg = server
            .reload_proxy_config(path.to_str().unwrap())
            .await
            .expect("reload should succeed");
        assert!(
            msg.contains("no changes detected"),
            "expected no-op message, got: {msg}"
        );
    }
}
