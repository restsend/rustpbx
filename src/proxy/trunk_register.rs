use crate::config::ProxyConfig;
use crate::proxy::{ProxyModule, server::SipServerRef};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::dialog::{authenticate::Credential, registration::Registration};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct TrunkRegistrationModule {
    server: SipServerRef,
}

impl TrunkRegistrationModule {
    pub fn create(
        server: SipServerRef,
        _config: Arc<ProxyConfig>,
    ) -> Result<Box<dyn ProxyModule>> {
        Ok(Box::new(TrunkRegistrationModule { server }))
    }
}

#[async_trait]
impl ProxyModule for TrunkRegistrationModule {
    fn name(&self) -> &str {
        "trunk_register"
    }

    async fn on_start(&mut self) -> Result<()> {
        let trunks = self.server.data_context.trunks_snapshot();
        let endpoint_inner = self.server.endpoint.inner.clone();
        let cancel_token = self.server.cancel_token.clone();

        for (name, trunk) in trunks {
            if trunk.register != Some(true) {
                continue;
            }

            let (username, password) = match (&trunk.username, &trunk.password) {
                (Some(u), Some(p)) => (u.clone(), p.clone()),
                _ => {
                    warn!(
                        trunk = %name,
                        "trunk_register: skipping trunk without credentials"
                    );
                    continue;
                }
            };

            let dest_uri = match rsip::Uri::try_from(trunk.dest.as_str()) {
                Ok(uri) => uri,
                Err(e) => {
                    warn!(
                        trunk = %name,
                        dest = %trunk.dest,
                        error = %e,
                        "trunk_register: invalid dest URI"
                    );
                    continue;
                }
            };

            let expires = trunk.register_expires.unwrap_or(3600);
            let token = cancel_token.child_token();
            let ep = endpoint_inner.clone();
            let trunk_name = name.clone();

            info!(
                trunk = %trunk_name,
                dest = %trunk.dest,
                expires = expires,
                "trunk_register: starting registration loop"
            );

            crate::utils::spawn(async move {
                register_loop(ep, trunk_name, dest_uri, username, password, expires, token).await;
            });
        }

        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        debug!("trunk_register: module stopped");
        Ok(())
    }
}

async fn register_loop(
    endpoint: rsipstack::transaction::endpoint::EndpointInnerRef,
    trunk_name: String,
    dest_uri: rsip::Uri,
    username: String,
    password: String,
    expires: u32,
    cancel_token: CancellationToken,
) {
    let credential = Credential {
        username,
        password,
        realm: None,
    };

    let mut registration = Registration::new(endpoint, Some(credential));
    let mut retry_delay_secs: u64 = 30;
    let max_retry_delay_secs: u64 = 300;

    loop {
        if cancel_token.is_cancelled() {
            info!(trunk = %trunk_name, "trunk_register: shutting down");
            return;
        }

        match registration.register(dest_uri.clone(), Some(expires)).await {
            Ok(resp) if resp.status_code == rsip::StatusCode::OK => {
                let actual_expires = registration.expires();
                info!(
                    trunk = %trunk_name,
                    expires = actual_expires,
                    "trunk_register: registration successful"
                );
                retry_delay_secs = 30; // reset backoff on success

                // Re-register at 85% of expiry
                let sleep_secs = (actual_expires as u64) * 85 / 100;
                let sleep_secs = sleep_secs.max(30);

                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {}
                    _ = cancel_token.cancelled() => {
                        info!(trunk = %trunk_name, "trunk_register: shutting down");
                        return;
                    }
                }
            }
            Ok(resp) => {
                warn!(
                    trunk = %trunk_name,
                    status = %resp.status_code,
                    "trunk_register: registration failed"
                );

                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(retry_delay_secs)) => {}
                    _ = cancel_token.cancelled() => {
                        info!(trunk = %trunk_name, "trunk_register: shutting down");
                        return;
                    }
                }

                retry_delay_secs = (retry_delay_secs * 2).min(max_retry_delay_secs);
            }
            Err(e) => {
                warn!(
                    trunk = %trunk_name,
                    error = %e,
                    "trunk_register: registration error"
                );

                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(retry_delay_secs)) => {}
                    _ = cancel_token.cancelled() => {
                        info!(trunk = %trunk_name, "trunk_register: shutting down");
                        return;
                    }
                }

                retry_delay_secs = (retry_delay_secs * 2).min(max_retry_delay_secs);
            }
        }
    }
}
