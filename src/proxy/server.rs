use super::{
    locator::{Locator, MemoryLocator},
    user::{MemoryUserBackend, UserBackend},
    ProxyAction, ProxyModule,
};
use crate::config::ProxyConfig;
use anyhow::{anyhow, Result};
use rsipstack::{
    transaction::{transaction::Transaction, Endpoint, TransactionReceiver},
    transport::{udp::UdpConnection, TransportLayer},
    EndpointBuilder,
};
use std::{
    collections::HashSet,
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
    pub allow_methods: HashSet<String>,
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
    modules: Vec<Box<dyn ProxyModule>>,
}

impl SipServerBuilder {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            cancel_token: None,
            user_backend: Box::new(MemoryUserBackend::new()),
            locator: Box::new(MemoryLocator::new()),
            modules: vec![],
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

    pub fn add_module(mut self, module: Box<dyn ProxyModule>) -> Self {
        self.modules.push(module);
        self
    }
    pub async fn build(self) -> Result<SipServer> {
        let mut allow_methods = HashSet::new();
        for module in self.modules.iter() {
            for method in module.allow_methods() {
                allow_methods.insert(method.to_string());
            }
        }

        let inner = Arc::new(SipServerInner {
            config: self.config.clone(),
            cancel_token: self.cancel_token.unwrap_or_default(),
            user_backend: Arc::new(self.user_backend),
            locator: Arc::new(self.locator),
            allow_methods,
        });

        let load_modules = self.config.load_modules.as_ref();
        let mut modules = if let Some(load_modules) = load_modules {
            let mut modules = self
                .modules
                .into_iter()
                .filter(|m| load_modules.contains(&m.name().to_string()))
                .map(|m| m)
                .collect::<Vec<_>>();
            // sort modules by enable_modules order
            modules.sort_by(|a, b| {
                let a_index = load_modules.iter().position(|m| m == a.name());
                let b_index = load_modules.iter().position(|m| m == b.name());
                a_index.cmp(&b_index)
            });
            modules
        } else {
            self.modules
        };

        let start_time = Instant::now();
        for module in modules.iter_mut() {
            match module.on_start(inner.clone()).await {
                Ok(_) => {}
                Err(e) => {
                    error!("proxy: failed to start module {}: {}", module.name(), e);
                    return Err(anyhow::anyhow!(
                        "proxy: failed to start module {}: {}",
                        module.name(),
                        e
                    ));
                }
            }
        }
        info!(
            "proxy: started with modules: {:?}, elapsed: {:?}",
            modules.iter().map(|m| m.name()).collect::<Vec<_>>(),
            start_time.elapsed()
        );

        let transport_layer = TransportLayer::new(inner.cancel_token.clone());
        let local_addr = inner
            .config
            .addr
            .parse::<IpAddr>()
            .map_err(|e| anyhow!("proxy: failed to parse local ip address: {}", e))?;

        let external_ip = inner
            .config
            .external_ip
            .as_ref()
            .map(|s| s.parse::<SocketAddr>())
            .ok_or(anyhow::anyhow!(
                "proxy: failed to parse external ip address"
            ))?
            .ok();

        if inner.config.udp_port.is_none()
            && inner.config.tcp_port.is_none()
            && inner.config.tls_port.is_none()
            && inner.config.ws_port.is_none()
        {
            return Err(anyhow::anyhow!(
                "proxy: No port specified, please specify at least one port: udp, tcp, tls, ws"
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
            endpoint_builder.user_agent(user_agent.as_str());
        }
        let endpoint_builder = endpoint_builder
            .cancel_token(inner.cancel_token.clone())
            .transport_layer(transport_layer);

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
                    error!("proxy: failed to stop module {}: {}", module.name(), e);
                }
            }
        }
        info!("proxy: stopped");
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
            let method = tx.original.method().to_string();
            if !self.inner.allow_methods.contains(&method) {
                info!(key, "Method not allowed: {}", method);
                tx.reply(rsip::StatusCode::MethodNotAllowed).await.ok();
                continue;
            }

            let modules = self.modules.clone();
            //TODO: max concurrency with tokio::spawn
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
                    error!(key, "proxy: failed to handle transaction: {}", e);
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
                    error!(key, "proxy: failed to handle transaction: {}", e);
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
