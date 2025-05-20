use super::{
    locator::{Locator, MemoryLocator},
    user::{MemoryUserBackend, UserBackend},
    FnCreateProxyModule, ProxyAction, ProxyModule,
};
use crate::config::ProxyConfig;
use anyhow::{anyhow, Result};
use rsipstack::{
    transaction::{transaction::Transaction, Endpoint, TransactionReceiver},
    transport::{udp::UdpConnection, TransportLayer},
    EndpointBuilder,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

pub struct SipServerInner {
    pub cancel_token: CancellationToken,
    pub config: Arc<ProxyConfig>,
    pub user_backend: Arc<Box<dyn UserBackend>>,
    pub locator: Arc<Box<dyn Locator>>,
}

pub type SipServerRef = Arc<SipServerInner>;

pub struct SipServer {
    inner: SipServerRef,
    endpoint: Endpoint,
    modules: Arc<Vec<Box<dyn ProxyModule>>>,
}

pub struct SipServerBuilder {
    config: Arc<ProxyConfig>,
    cancel_token: Option<CancellationToken>,
    user_backend: Box<dyn UserBackend>,
    locator: Box<dyn Locator>,
    module_fns: HashMap<String, FnCreateProxyModule>,
}

impl SipServerBuilder {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            cancel_token: None,
            user_backend: Box::new(MemoryUserBackend::new()),
            locator: Box::new(MemoryLocator::new()),
            module_fns: HashMap::new(),
        }
    }

    pub fn with_user_backend(mut self, user_backend: Box<dyn UserBackend>) -> Self {
        self.user_backend = user_backend;
        self
    }

    pub fn with_locator(mut self, registrator: Box<dyn Locator>) -> Self {
        self.locator = registrator;
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
    pub async fn build(self) -> Result<SipServer> {
        let inner = Arc::new(SipServerInner {
            config: self.config.clone(),
            cancel_token: self.cancel_token.unwrap_or_default(),
            user_backend: Arc::new(self.user_backend),
            locator: Arc::new(self.locator),
        });

        let mut allow_methods = Vec::new();
        let mut modules = Vec::new();
        if let Some(load_modules) = self.config.load_modules.as_ref() {
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

                    info!(
                        "module {} loaded in {:?}",
                        name,
                        module_start_time.elapsed()
                    );
                } else {
                    error!("module {} not found", name);
                }
            }
            allow_methods.dedup();
            info!(
                "modules loaded in {:?} modules: {:?}",
                start_time.elapsed(),
                modules.iter().map(|m| m.name()).collect::<Vec<_>>()
            );
        }

        let transport_layer = TransportLayer::new(inner.cancel_token.clone());
        let local_addr = inner
            .config
            .addr
            .parse::<IpAddr>()
            .map_err(|e| anyhow!("failed to parse local ip address: {}", e))?;

        let external_ip = match inner.config.external_ip {
            Some(ref s) => s
                .parse::<SocketAddr>()
                .map_err(|e| anyhow!("failed to parse external ip address: {}", e))
                .ok(),
            None => None,
        };

        if inner.config.udp_port.is_none()
            && inner.config.tcp_port.is_none()
            && inner.config.tls_port.is_none()
            && inner.config.ws_port.is_none()
        {
            return Err(anyhow::anyhow!(
                "No port specified, please specify at least one port: udp, tcp, tls, ws"
            ));
        }

        if let Some(udp_port) = inner.config.udp_port {
            let local_addr = SocketAddr::new(local_addr, udp_port);
            let udp_conn = UdpConnection::create_connection(local_addr, external_ip)
                .await
                .map_err(|e| anyhow!("Failed to create UDP connection: {}", e))?;
            transport_layer.add_transport(udp_conn.into());
        }

        let mut endpoint_builder = EndpointBuilder::new();
        if let Some(ref user_agent) = inner.config.useragent {
            endpoint_builder.with_user_agent(user_agent.as_str());
        }
        let endpoint_builder = endpoint_builder
            .with_cancel_token(inner.cancel_token.clone())
            .with_allows(allow_methods)
            .with_transport_layer(transport_layer);

        let endpoint = endpoint_builder.build();

        Ok(SipServer {
            inner,
            modules: Arc::new(modules),
            endpoint,
        })
    }
}

impl SipServer {
    pub async fn serve(&self) -> Result<()> {
        let incoming = self.endpoint.incoming_transactions();
        let cancel_token = self.inner.cancel_token.clone();
        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("cancelled");
            }
            _ = self.endpoint.serve() => {
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

    async fn handle_incoming(&self, mut incoming: TransactionReceiver) -> Result<()> {
        let runnings_tx = Arc::new(AtomicUsize::new(0));
        while let Some(mut tx) = incoming.recv().await {
            let key = tx.key.to_string();
            info!(key, "Received transaction");
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
            tokio::spawn(async move {
                runnings_tx.fetch_add(1, Ordering::Relaxed);
                let start_time = Instant::now();
                select! {
                    r = Self::process_transaction(token.clone(), modules, &key,  &mut tx) => {
                        let final_status = tx.last_response.as_ref().map(|r| r.status_code());
                        info!(key, ?final_status, "Transaction processed in {:?} ", start_time.elapsed());
                        runnings_tx.fetch_sub(1, Ordering::Relaxed);
                        return r;
                    }
                    _ = token.cancelled() => {
                        info!(key, "Transaction cancelled");
                        if tx.last_response.is_none() {
                            tx.reply(rsip::StatusCode::RequestTerminated).await.ok();
                        }
                    }
                };
                runnings_tx.fetch_sub(1, Ordering::Relaxed);
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
        for module in modules.iter() {
            match module.on_transaction_begin(token.clone(), tx).await {
                Ok(action) => match action {
                    ProxyAction::Continue => {}
                    ProxyAction::Abort => break,
                },
                Err(e) => {
                    error!(key, "failed to handle transaction: {}", e);
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
