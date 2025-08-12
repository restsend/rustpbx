use super::{
    FnCreateProxyModule, ProxyAction, ProxyModule,
    locator::{Locator, create_locator},
    user::{UserBackend, create_user_backend},
};
use crate::{
    app::AppState,
    call::TransactionCookie,
    callrecord::CallRecordSender,
    config::ProxyConfig,
    proxy::{auth::AuthBackend, call::CallRouter},
};
use anyhow::{Result, anyhow};
use rsip::prelude::HeadersExt;
use rsipstack::{
    EndpointBuilder,
    transaction::{
        Endpoint, TransactionReceiver, endpoint::EndpointOption, transaction::Transaction,
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
use tracing::{debug, error, info, warn};

pub struct SipServerInner {
    pub app_state: AppState,
    pub cancel_token: CancellationToken,
    pub config: Arc<ProxyConfig>,
    pub user_backend: Arc<Box<dyn UserBackend>>,
    pub auth_backend: Arc<Option<Box<dyn AuthBackend>>>,
    pub call_router: Arc<Option<Box<dyn CallRouter>>>,
    pub locator: Arc<Box<dyn Locator>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub endpoint: Endpoint,
}

pub type SipServerRef = Arc<SipServerInner>;

#[derive(Clone)]
pub struct SipServer {
    pub inner: SipServerRef,
    modules: Arc<Vec<Box<dyn ProxyModule>>>,
}

pub struct SipServerBuilder {
    config: Arc<ProxyConfig>,
    cancel_token: Option<CancellationToken>,
    user_backend: Option<Box<dyn UserBackend>>,
    auth_backend: Option<Box<dyn AuthBackend>>,
    call_router: Option<Box<dyn CallRouter>>,
    module_fns: HashMap<String, FnCreateProxyModule>,
    locator: Option<Box<dyn Locator>>,
    callrecord_sender: Option<CallRecordSender>,
}

impl SipServerBuilder {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            cancel_token: None,
            user_backend: None,
            auth_backend: None,
            call_router: None,
            module_fns: HashMap::new(),
            locator: None,
            callrecord_sender: None,
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

    pub fn with_locator(mut self, locator: Box<dyn Locator>) -> Self {
        self.locator = Some(locator);
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
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

    pub async fn build(self, app_state: AppState) -> Result<SipServer> {
        let user_backend = if let Some(backend) = self.user_backend {
            backend
        } else {
            match create_user_backend(&self.config.user_backend).await {
                Ok(backend) => backend,
                Err(e) => {
                    error!(
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
                    error!("failed to create locator: {} {:?}", e, self.config.locator);
                    return Err(e);
                }
            }
        };
        let cancel_token = self.cancel_token.unwrap_or_default();
        let config = self.config.clone();
        let transport_layer = TransportLayer::new(cancel_token.clone());
        let local_addr = config
            .addr
            .parse::<IpAddr>()
            .map_err(|e| anyhow!("failed to parse local ip address: {}", e))?;

        let external_ip = match config.external_ip {
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

        let endpoint_builder = endpoint_builder
            .with_cancel_token(cancel_token.clone())
            .with_option(endpoint_option)
            .with_transport_layer(transport_layer);

        let endpoint = endpoint_builder.build();

        let call_router = self.call_router;

        let inner = Arc::new(SipServerInner {
            app_state,
            config: self.config.clone(),
            cancel_token,
            user_backend: Arc::new(user_backend),
            auth_backend: Arc::new(auth_backend),
            call_router: Arc::new(call_router),
            locator: Arc::new(locator),
            callrecord_sender: self.callrecord_sender,
            endpoint,
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
                            error!("failed to create module {}: {}", name, e);
                            continue;
                        }
                    };
                    match module.on_start().await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("failed to start module {}: {}", name, e);
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
                    error!("module {} not found", name);
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
        let incoming = self.inner.endpoint.incoming_transactions();
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
                    error!("failed to stop module {}: {}", module.name(), e);
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
            let key = tx.key.to_string();
            debug!(key, "Received transaction");
            let modules = self.modules.clone();

            let token = self.inner.cancel_token.child_token();
            let runnings_tx = runnings_tx.clone();

            if let Some(max_concurrency) = self.inner.config.max_concurrency {
                if runnings_tx.load(Ordering::Relaxed) >= max_concurrency {
                    info!(
                        key,
                        runnings = runnings_tx.load(Ordering::Relaxed),
                        "Max concurrency reached, not process this transaction"
                    );
                    tx.reply(rsip::StatusCode::ServiceUnavailable).await.ok();
                    continue;
                }
            }
            // Spam protection for OPTIONS requests
            // If the OPTIONS request is out-of-dialog and the tag is not present, ignore it
            if matches!(tx.original.method, rsip::Method::Options) {
                if tx.endpoint_inner.option.ignore_out_of_dialog_option {
                    let to_tag = tx
                        .original
                        .to_header()
                        .and_then(|to| to.tag())
                        .ok()
                        .flatten();
                    if to_tag.is_none() {
                        info!(key, "Ignoring out-of-dialog OPTIONS request");
                        continue;
                    }
                }
            }
            tokio::spawn(async move {
                runnings_tx.fetch_add(1, Ordering::Relaxed);
                let start_time = Instant::now();
                select! {
                    r = Self::process_transaction(token.clone(), modules, &key,  &mut tx) => {
                        let final_status = tx.last_response.as_ref().map(|r| r.status_code());
                        match r {
                            Ok(_) => {
                                info!(key, ?final_status, "Transaction processed in {} s ", start_time.elapsed().as_secs_f32());
                            },
                            Err(e) => {
                                warn!(key, ?final_status, "Failed to process transaction: {} in {} s", e, start_time.elapsed().as_secs_f32());
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        info!(key, "Transaction cancelled");
                    }
                };
                runnings_tx.fetch_sub(1, Ordering::Relaxed);
                if !matches!(
                    tx.original.method,
                    rsip::Method::Bye | rsip::method::Method::Cancel | rsip::Method::Ack
                ) {
                    if tx.last_response.is_none() {
                        tx.reply(rsip::StatusCode::RequestTerminated).await.ok();
                    }
                }
                return Ok::<(), anyhow::Error>(());
            });
        }
        Ok(())
    }

    async fn process_transaction(
        token: CancellationToken,
        modules: Arc<Vec<Box<dyn ProxyModule>>>,
        key: &String,
        tx: &mut Transaction,
    ) -> Result<()> {
        let cookie = TransactionCookie::from(&tx.key);
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
                    error!(
                        key,
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
                    error!(key, "failed to handle transaction: {}", e);
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
                if let Some(external_ip) = self.config.external_ip.as_ref() {
                    return external_ip.starts_with(callee_realm);
                }
                if let Some(realms) = self.config.realms.as_ref() {
                    for item in realms {
                        if item == callee_realm {
                            return true;
                        }
                    }
                }
                self.user_backend.is_same_realm(callee_realm).await
            }
        }
    }
}
