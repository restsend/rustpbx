use super::{
    FnCreateProxyModule, ProxyAction, ProxyModule,
    locator::{Locator, create_locator},
    user::{UserBackend, create_user_backend},
};
use crate::{
    call::TransactionCookie,
    callrecord::CallRecordSender,
    config::{ProxyConfig, RtpConfig},
    proxy::{
        FnCreateRouteInvite,
        auth::AuthBackend,
        call::{CallRouter, DialplanInspector, ProxyCallInspector},
    },
};
use anyhow::{Result, anyhow};
use rsip::prelude::HeadersExt;
use rsipstack::{
    EndpointBuilder,
    dialog::dialog_layer::DialogLayer,
    transaction::{
        Endpoint, TransactionReceiver,
        endpoint::{EndpointOption, MessageInspector},
        transaction::Transaction,
    },
    transport::{
        TcpListenerConnection, TransportLayer, WebSocketListenerConnection, udp::UdpConnection,
    },
};
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
    pub user_backend: Box<dyn UserBackend>,
    pub auth_backend: Option<Box<dyn AuthBackend>>,
    pub call_router: Option<Box<dyn CallRouter>>,
    pub dialplan_inspector: Option<Box<dyn DialplanInspector>>,
    pub proxycall_inspector: Option<Box<dyn ProxyCallInspector>>,
    pub locator: Box<dyn Locator>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub endpoint: Endpoint,
    pub dialog_layer: Arc<DialogLayer>,
    pub create_route_invite: Option<FnCreateRouteInvite>,
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
    auth_backend: Option<Box<dyn AuthBackend>>,
    call_router: Option<Box<dyn CallRouter>>,
    module_fns: HashMap<String, FnCreateProxyModule>,
    locator: Option<Box<dyn Locator>>,
    callrecord_sender: Option<CallRecordSender>,
    message_inspector: Option<Box<dyn MessageInspector>>,
    dialplan_inspector: Option<Box<dyn DialplanInspector>>,
    proxycall_inspector: Option<Box<dyn ProxyCallInspector>>,
    create_route_invite: Option<FnCreateRouteInvite>,
}

impl SipServerBuilder {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            rtp_config: None,
            cancel_token: None,
            user_backend: None,
            auth_backend: None,
            call_router: None,
            proxycall_inspector: None,
            module_fns: HashMap::new(),
            locator: None,
            callrecord_sender: None,
            message_inspector: None,
            dialplan_inspector: None,
            create_route_invite: None,
        }
    }

    pub fn with_user_backend(mut self, user_backend: Box<dyn UserBackend>) -> Self {
        self.user_backend = Some(user_backend);
        self
    }

    pub fn with_auth_backend(mut self, auth_backend: Box<dyn AuthBackend>) -> Self {
        self.auth_backend = Some(auth_backend);
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
        self.dialplan_inspector = Some(dialplan_inspector);
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
        self.message_inspector = Some(inspector);
        self
    }

    pub fn with_proxycall_inspector(mut self, inspector: Box<dyn ProxyCallInspector>) -> Self {
        self.proxycall_inspector = Some(inspector);
        self
    }
    pub fn with_rtp_config(mut self, config: RtpConfig) -> Self {
        self.rtp_config = Some(config);
        self
    }

    pub async fn build(self) -> Result<SipServer> {
        let user_backend = if let Some(backend) = self.user_backend {
            backend
        } else {
            match create_user_backend(&self.config.user_backend).await {
                Ok(backend) => backend,
                Err(e) => {
                    warn!(
                        "failed to create user backend: {} {:?}",
                        e, self.config.user_backend
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
        let rtp_config = self.rtp_config.unwrap_or_default();
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = self.config.clone();
        let transport_layer = TransportLayer::new(cancel_token.clone());
        let local_addr = config
            .addr
            .parse::<IpAddr>()
            .map_err(|e| anyhow!("failed to parse local ip address: {}", e))?;

        let external_ip = match rtp_config.external_ip {
            Some(ref s) => s
                .parse::<SocketAddr>()
                .map_err(|e| anyhow!("failed to parse external ip address: {}", e))
                .ok(),
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
            let udp_conn = UdpConnection::create_connection(
                local_addr,
                external_ip,
                Some(cancel_token.child_token()),
            )
            .await
            .map_err(|e| anyhow!("Failed to create proxy UDP connection {} {}", local_addr, e))?;
            transport_layer.add_transport(udp_conn.into());
            info!("start proxy, udp port: {}", local_addr);
        }

        if let Some(tcp_port) = config.tcp_port {
            let local_addr = SocketAddr::new(local_addr, tcp_port);
            let tcp_conn = TcpListenerConnection::new(local_addr.into(), external_ip)
                .await
                .map_err(|e| anyhow!("Failed to create TCP connection: {}", e))?;
            transport_layer.add_transport(tcp_conn.into());
            info!("start proxy, tcp port: {}", local_addr);
        }

        if let Some(ws_port) = config.ws_port {
            let local_addr = SocketAddr::new(local_addr, ws_port);
            let ws_conn = WebSocketListenerConnection::new(local_addr.into(), external_ip, false)
                .await
                .map_err(|e| anyhow!("Failed to create WS connection: {}", e))?;
            transport_layer.add_transport(ws_conn.into());
            info!("start proxy, ws port: {}", local_addr);
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

        if let Some(inspector) = self.message_inspector {
            endpoint_builder = endpoint_builder.with_inspector(inspector);
        }

        let endpoint = endpoint_builder.build();

        let call_router = self.call_router;
        let dialplan_inspector = self.dialplan_inspector;
        let proxycall_inspector = self.proxycall_inspector;
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        let inner = Arc::new(SipServerInner {
            rtp_config,
            proxy_config: self.config.clone(),
            cancel_token,
            user_backend: user_backend,
            auth_backend: auth_backend,
            call_router: call_router,
            proxycall_inspector: proxycall_inspector,
            locator: locator,
            callrecord_sender: self.callrecord_sender,
            endpoint,
            dialog_layer,
            dialplan_inspector: dialplan_inspector,
            create_route_invite: self.create_route_invite,
        });

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
        let runnings_tx = Arc::new(AtomicUsize::new(0));
        while let Some(mut tx) = incoming.recv().await {
            debug!(key = %tx.key, "received transaction");
            let modules = self.modules.clone();

            let token = tx
                .connection
                .as_ref()
                .map(|c| c.cancel_token())
                .flatten()
                .unwrap_or_else(|| self.inner.cancel_token.clone())
                .child_token();

            let runnings_tx = runnings_tx.clone();

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
            ) && tx.endpoint_inner.option.ignore_out_of_dialog_option
            {
                let to_tag = tx
                    .original
                    .to_header()
                    .and_then(|to| to.tag())
                    .ok()
                    .flatten();
                if to_tag.is_none() {
                    info!(key = %tx.key, "ignoring out-of-dialog OPTIONS request");
                    continue;
                }
            }
            tokio::spawn(async move {
                runnings_tx.fetch_add(1, Ordering::Relaxed);
                let start_time = Instant::now();
                let cookie = TransactionCookie::from(&tx.key);
                select! {
                    r = Self::process_transaction(token.clone(), modules, cookie.clone(),  &mut tx) => {
                        let final_status = tx.last_response.as_ref().map(|r| r.status_code());
                        match r {
                            Ok(_) => {
                                info!(key = %tx.key, ?final_status, "transaction processed in {:?}", start_time.elapsed());
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
                if !matches!(
                    tx.original.method,
                    rsip::Method::Bye | rsip::method::Method::Cancel | rsip::Method::Ack
                ) && tx.last_response.is_none()
                    && !cookie.is_spam()
                {
                    tx.reply(rsip::StatusCode::NotImplemented).await.ok();
                }
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
    pub async fn is_same_realm(&self, callee_realm: &str) -> bool {
        match callee_realm {
            "localhost" | "127.0.0.1" | "::1" => true,
            _ => {
                if let Some(external_ip) = self.rtp_config.external_ip.as_ref() {
                    if external_ip.starts_with(callee_realm) {
                        return true;
                    }
                }
                if let Some(realms) = self.proxy_config.realms.as_ref() {
                    for item in realms {
                        if item == callee_realm {
                            return true;
                        }
                    }
                }
                if self
                    .endpoint
                    .get_addrs()
                    .iter()
                    .any(|addr| addr.addr.host.to_string() == callee_realm)
                {
                    return true;
                }
                self.user_backend.is_same_realm(callee_realm).await
            }
        }
    }
}
