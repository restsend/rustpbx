use super::{
    FnCreateProxyModule, ProxyAction, ProxyModule,
    data::ProxyDataContext,
    locator::{Locator, create_locator},
    user::{UserBackend, build_user_backend},
};
use crate::{
    call::{TransactionCookie, policy::FrequencyLimiter},
    callrecord::{
        CallRecordSender,
        sipflow::{SipFlow, SipFlowBuilder},
    },
    config::{ProxyConfig, RtpConfig, SipFlowConfig},
    proxy::{
        FnCreateRouteInvite,
        active_call_registry::ActiveProxyCallRegistry,
        auth::AuthBackend,
        call::{CallRouter, DialplanInspector},
        locator::{DialogTargetLocator, LocatorEventSender, TransportInspectorLocator},
        presence::PresenceManager,
    },
    sipflow::backend::create_backend,
};
use anyhow::{Result, anyhow};
use rsip::prelude::{HeadersExt, UntypedHeader};
use rsip::{Auth, Param, Transport};
use rsipstack::{
    EndpointBuilder,
    dialog::dialog_layer::DialogLayer,
    transaction::{
        Endpoint, TransactionReceiver,
        endpoint::{EndpointOption, MessageInspector},
        transaction::Transaction,
    },
    transport::{
        TcpListenerConnection, TlsConfig, TlsListenerConnection, TransportLayer,
        WebSocketListenerConnection, udp::UdpConnection,
    },
};
use sea_orm::DatabaseConnection;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct SipServerInner {
    pub cancel_token: CancellationToken,
    pub rtp_config: RtpConfig,
    pub proxy_config: Arc<ProxyConfig>,
    pub data_context: Arc<ProxyDataContext>,
    pub database: Option<DatabaseConnection>,
    pub user_backend: Box<dyn UserBackend>,
    pub auth_backend: Vec<Box<dyn AuthBackend>>,
    pub call_router: Option<Box<dyn CallRouter>>,
    pub dialplan_inspectors: Vec<Box<dyn DialplanInspector>>,
    pub locator: Arc<Box<dyn Locator>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub endpoint: Endpoint,
    pub dialog_layer: Arc<DialogLayer>,
    pub create_route_invite: Option<FnCreateRouteInvite>,
    pub ignore_out_of_dialog_request: bool,
    pub locator_events: Option<LocatorEventSender>,
    pub sip_flow: Option<SipFlow>,
    pub active_call_registry: Arc<ActiveProxyCallRegistry>,
    pub frequency_limiter: Option<Arc<dyn FrequencyLimiter>>,
    pub call_record_hooks: Arc<Vec<Box<dyn crate::callrecord::CallRecordHook>>>,
    pub runnings_tx: Arc<AtomicUsize>,
    pub storage: Option<crate::storage::Storage>,
    pub presence_manager: Arc<PresenceManager>,
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
    create_route_invite: Option<FnCreateRouteInvite>,
    database: Option<DatabaseConnection>,
    data_context: Option<Arc<ProxyDataContext>>,
    ignore_out_of_dialog_request: bool,
    locator_events: Option<LocatorEventSender>,
    frequency_limiter: Option<Arc<dyn FrequencyLimiter>>,
    call_record_hooks: Vec<Box<dyn crate::callrecord::CallRecordHook>>,
    storage: Option<crate::storage::Storage>,
    sipflow_config: Option<SipFlowConfig>,
    no_bind: bool,
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
            create_route_invite: None,
            database: None,
            data_context: None,
            ignore_out_of_dialog_request: true,
            locator_events: None,
            frequency_limiter: None,
            call_record_hooks: Vec::new(),
            storage: None,
            sipflow_config: None,
            no_bind: false,
        }
    }

    pub fn with_sipflow_config(mut self, config: Option<SipFlowConfig>) -> Self {
        self.sipflow_config = config;
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
        self.create_route_invite = Some(f);
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

    pub async fn build(self) -> Result<SipServer> {
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
        let auth_backend = self.auth_backend;
        let locator = if let Some(locator) = self.locator {
            locator
        } else {
            match create_locator(&self.config.locator).await {
                Ok(locator) => locator,
                Err(e) => {
                    warn!("failed to create locator: {} {:?}", e, self.config.locator);
                    return Err(e);
                }
            }
        };

        let locator = Arc::new(locator);
        let rtp_config = self.rtp_config.unwrap_or_default();
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = self.config.clone();
        let transport_layer = TransportLayer::new(cancel_token.clone());

        if !self.no_bind {
            let local_addr = config
                .addr
                .parse::<IpAddr>()
                .map_err(|e| anyhow!("failed to parse local ip address: {}", e))?;

            let external_ip = match rtp_config.external_ip {
                Some(ref s) => Some(
                    s.parse::<IpAddr>()
                        .map_err(|e| anyhow!("failed to parse external ip address {}: {}", s, e))?,
                ),
                None => None,
            };

            if config.udp_port.is_none()
                && config.tcp_port.is_none()
                && config.tls_port.is_none()
                && config.ws_port.is_none()
            {
                return Err(anyhow::anyhow!(
                    "No port specified, please specify at least one port: udp, tcp, tls, ws"
                ));
            }

            if let Some(udp_port) = config.udp_port {
                let local_addr = SocketAddr::new(local_addr, udp_port);
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(ip.clone(), udp_port));
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
            }

            if let Some(tcp_port) = config.tcp_port {
                let local_addr = SocketAddr::new(local_addr, tcp_port);
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(ip.clone(), tcp_port));
                let tcp_conn = TcpListenerConnection::new(local_addr.into(), external_addr)
                    .await
                    .map_err(|e| anyhow!("Failed to create TCP connection: {}", e))?;
                info!("start proxy, tcp port: {}", tcp_conn.get_addr());
                transport_layer.add_transport(tcp_conn.into());
            }

            if let Some(tls_port) = config.tls_port {
                let local_addr = SocketAddr::new(local_addr, tls_port);
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(ip.clone(), tls_port));

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
                                    transport_layer.add_transport(conn.into());
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
                let external_addr = external_ip
                    .as_ref()
                    .map(|ip| SocketAddr::new(ip.clone(), ws_port));
                let ws_conn =
                    WebSocketListenerConnection::new(local_addr.into(), external_addr, false)
                        .await
                        .map_err(|e| anyhow!("Failed to create WS connection: {}", e))?;
                info!("start proxy, ws port: {}", ws_conn.get_addr());
                transport_layer.add_transport(ws_conn.into());
            }
        }

        let mut endpoint_builder = EndpointBuilder::new();
        if let Some(ref user_agent) = config.useragent {
            endpoint_builder.with_user_agent(user_agent.as_str());
        }

        let endpoint_option = EndpointOption {
            callid_suffix: config.callid_suffix.clone(),
            ..Default::default()
        };

        let mut endpoint_builder = endpoint_builder
            .with_cancel_token(cancel_token.clone())
            .with_option(endpoint_option)
            .with_transport_layer(transport_layer);

        let mut inspectors: Vec<Box<dyn MessageInspector>> = self.message_inspectors;
        if self.config.nat_fix {
            inspectors.insert(0, Box::new(super::nat::NatInspector::new()));
        }

        let mut sip_flow = None;
        if let Some(sipflow_config) = &self.sipflow_config {
            if let Ok(backend) = create_backend(sipflow_config) {
                info!("Sipflow backend initialized: {:?}", sipflow_config);
                let sflow = SipFlowBuilder::new()
                    .with_backend(Arc::from(backend))
                    .build();
                sip_flow = Some(sflow.clone());
                inspectors.push(Box::new(sflow));
            } else {
                warn!("Failed to create sipflow backend");
            }
        }

        endpoint_builder = endpoint_builder.with_inspector(Box::new(CompositeMessageInspector {
            inspectors,
        }) as Box<dyn MessageInspector>);

        let locator_events = self.locator_events.unwrap_or_else(|| {
            let (tx, _) = tokio::sync::broadcast::channel(12);
            tx
        });

        endpoint_builder = endpoint_builder
            .with_target_locator(DialogTargetLocator::new(locator.clone()))
            .with_transport_inspector(TransportInspectorLocator::new(
                locator.clone(),
                locator_events.clone(),
            ));

        let endpoint = endpoint_builder.build();

        let mut call_router = self.call_router;
        if call_router.is_none() {
            if let Some(http_router_config) = &self.config.http_router {
                call_router = Some(Box::new(crate::proxy::routing::http::HttpCallRouter::new(
                    http_router_config.clone(),
                )));
            }
        }
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        let database = self.database.clone();

        let data_context = if let Some(context) = self.data_context {
            context
        } else {
            Arc::new(
                ProxyDataContext::new(self.config.clone(), database.clone())
                    .await
                    .map_err(|err| anyhow!("failed to initialize proxy data context: {err}"))?,
            )
        };

        let active_call_registry = Arc::new(ActiveProxyCallRegistry::new());
        let presence_manager = Arc::new(PresenceManager::new(database.clone()));
        presence_manager.load_from_db().await.ok();

        let inner = Arc::new(SipServerInner {
            rtp_config,
            proxy_config: self.config.clone(),
            cancel_token,
            data_context,
            database: database.clone(),
            user_backend: user_backend,
            auth_backend: auth_backend,
            call_router: call_router,
            locator: locator.clone(),
            callrecord_sender: self.callrecord_sender,
            endpoint,
            dialog_layer,
            dialplan_inspectors: self.dialplan_inspectors,
            create_route_invite: self.create_route_invite,
            ignore_out_of_dialog_request: self.ignore_out_of_dialog_request,
            locator_events: Some(locator_events),
            sip_flow,
            active_call_registry,
            frequency_limiter: self.frequency_limiter,
            call_record_hooks: Arc::new(self.call_record_hooks),
            runnings_tx: Arc::new(AtomicUsize::new(0)),
            storage: self.storage,
            presence_manager,
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
        inner
            .endpoint
            .inner
            .allows
            .lock()
            .unwrap()
            .replace(allow_methods);
        Ok(SipServer {
            inner,
            modules: Arc::new(modules),
        })
    }
}

impl SipServer {
    pub async fn serve(&self) -> Result<()> {
        let incoming = self.inner.endpoint.incoming_transactions()?;
        let cancel_token = self.inner.cancel_token.clone();

        if let Some(webhook_config) = &self.inner.proxy_config.locator_webhook {
            if let Some(events) = &self.inner.locator_events {
                let rx = events.subscribe();
                crate::utils::spawn(super::locator_webhook::handle_locator_webhook(
                    webhook_config.clone(),
                    rx,
                ));
            }
        }

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
        while let Some(mut tx) = incoming.recv().await {
            let modules = self.modules.clone();

            let token = tx
                .connection
                .as_ref()
                .map(|c| c.cancel_token())
                .flatten()
                .unwrap_or_else(|| self.inner.cancel_token.clone())
                .child_token();

            let runnings_tx = self.inner.runnings_tx.clone();

            if let Some(max_concurrency) = self.inner.proxy_config.max_concurrency {
                if runnings_tx.load(Ordering::Relaxed) >= max_concurrency {
                    info!(
                        key = %tx.key,
                        runnings = runnings_tx.load(Ordering::Relaxed),
                        "max concurrency reached, not process this transaction"
                    );
                    tx.reply(rsip::StatusCode::ServiceUnavailable).await.ok();
                    continue;
                }
            }
            // Spam protection for OPTIONS requests
            // If the OPTIONS request is out-of-dialog and the tag is not present, ignore it
            if matches!(
                tx.original.method,
                rsip::Method::Options
                    | rsip::method::Method::Info
                    | rsip::method::Method::Refer
                    | rsip::method::Method::Update
            ) && self.inner.ignore_out_of_dialog_request
            {
                let to_tag = tx
                    .original
                    .to_header()
                    .and_then(|to| to.tag())
                    .ok()
                    .flatten();
                let via = tx.original.via_header()?.value();
                if to_tag.is_none() {
                    info!(key = %tx.key, via, "ignoring out-of-dialog {} request", tx.original.method);
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
                runnings_tx.fetch_sub(1, Ordering::Relaxed);
                let is_mid_dialog = tx.original.to_header().ok()
                    .and_then(|h| h.tag().ok().flatten())
                    .is_some();

                if !matches!(
                    tx.original.method,
                    rsip::Method::Bye | rsip::Method::Cancel | rsip::Method::Ack
                ) && !is_mid_dialog && tx.last_response.is_none()
                    && !cookie.is_spam()
                {
                    tx.reply(rsip::StatusCode::NotImplemented).await.ok();
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
                        tx.reply(rsip::StatusCode::ServerInternalError).await.ok();
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
    pub fn default_contact_uri(&self) -> Option<rsip::Uri> {
        let addr = self.endpoint.get_addrs().first()?.clone();
        let mut params = Vec::new();
        if let Some(transport) = addr.r#type {
            if !matches!(transport, Transport::Udp) {
                params.push(Param::Transport(transport));
            }
        }
        Some(rsip::Uri {
            scheme: addr.r#type.map(|t| t.sip_scheme()),
            auth: Some(Auth {
                user: "rustpbx".to_string(),
                password: None,
            }),
            host_with_port: addr.addr,
            params,
            ..Default::default()
        })
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

        let is_my_port = |p: u16| {
            self.proxy_config.udp_port == Some(p)
                || self.proxy_config.tcp_port == Some(p)
                || self.proxy_config.tls_port == Some(p)
                || self.proxy_config.ws_port == Some(p)
        };

        match host {
            "localhost" | "127.0.0.1" | "::1" => {
                return port.map(is_my_port).unwrap_or(true);
            }
            _ => {
                if let Some(external_ip) = self.rtp_config.external_ip.as_ref() {
                    if external_ip == host {
                        return port.map(is_my_port).unwrap_or(true);
                    }
                }
                if let Some(realms) = self.proxy_config.realms.as_ref() {
                    for item in realms {
                        if item == callee_realm {
                            return true;
                        }
                        if item == host {
                            return port.map(is_my_port).unwrap_or(true);
                        }
                    }
                }
                if self.endpoint.get_addrs().iter().any(|addr| {
                    if addr.addr.host.to_string() == host {
                        return port
                            .map(|p| addr.addr.port == Some(p.into()))
                            .unwrap_or(true);
                    }
                    false
                }) {
                    return true;
                }
                self.user_backend.is_same_realm(callee_realm).await
            }
        }
    }
}

struct CompositeMessageInspector {
    inspectors: Vec<Box<dyn MessageInspector>>,
}

impl MessageInspector for CompositeMessageInspector {
    fn before_send(
        &self,
        mut msg: rsip::SipMessage,
        dest: Option<&rsipstack::transport::SipAddr>,
    ) -> rsip::SipMessage {
        for inspector in &self.inspectors {
            msg = inspector.before_send(msg, dest);
        }
        msg
    }

    fn after_received(
        &self,
        mut msg: rsip::SipMessage,
        from: &rsipstack::transport::SipAddr,
    ) -> rsip::SipMessage {
        for inspector in &self.inspectors {
            msg = inspector.after_received(msg, from);
        }
        msg
    }
}
