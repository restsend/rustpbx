use super::UserAgent;
use anyhow::Result;
use rsip::{Response, StatusCodeKind};
use rsipstack::{
    dialog::{authenticate::Credential, registration::Registration},
    transaction::endpoint::EndpointInnerRef,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{select, sync::Mutex, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct RegisterOption {
    pub sip_server: String,
    pub credential: Option<Credential>,
}
pub struct RegistrationHandleInner {
    pub endpoint_inner: EndpointInnerRef,
    pub user: String,
    pub option: RegisterOption,
    pub cancel_token: CancellationToken,
    pub start_time: Mutex<Instant>,
    pub last_update: Mutex<Instant>,
    pub last_response: Mutex<Option<Response>>,
}
#[derive(Clone)]
pub struct RegistrationHandle {
    inner: Arc<RegistrationHandleInner>,
}

impl UserAgent {
    pub async fn register(&self, user: String, options: RegisterOption) -> Result<()> {
        let token = self.token.child_token();
        let handle = RegistrationHandle {
            inner: Arc::new(RegistrationHandleInner {
                endpoint_inner: self.endpoint.inner.clone(),
                user: user.clone(),
                option: options,
                cancel_token: token.clone(),
                start_time: Mutex::new(Instant::now()),
                last_update: Mutex::new(Instant::now()),
                last_response: Mutex::new(None),
            }),
        };
        self.registration_handles
            .lock()
            .await
            .insert(user.clone(), handle.clone());

        tokio::spawn(async move {
            select! {
                _ = handle.register_loop() => {
                    info!("registration loop for {} ended", user);
                }
                _ = token.cancelled() => {
                    info!("registration loop for {} cancelled", user);
                }
            }
            match handle.unregister().await {
                Ok(_) => {}
                Err(e) => {
                    warn!("failed to unregister user: {}", e);
                }
            }
        });
        Ok(())
    }

    pub async fn unregister(&self, user: String) -> Result<()> {
        if let Some(r) = self.registration_handles.lock().await.remove(&user) {
            info!("unregistering user: {}", user);
            r.inner.cancel_token.cancel();
        }
        Ok(())
    }
}

impl RegistrationHandle {
    async fn register_loop(&self) -> Result<()> {
        let mut start_time = self.inner.start_time.lock().await;
        *start_time = Instant::now();
        let mut last_update = self.inner.last_update.lock().await;
        *last_update = Instant::now();

        loop {
            if self.inner.cancel_token.is_cancelled() {
                break;
            }
            let mut registration = Registration::new(
                self.inner.endpoint_inner.clone(),
                self.inner.option.credential.clone(),
            );
            let resp = registration
                .register(&self.inner.option.sip_server)
                .await
                .map_err(|e| anyhow::anyhow!("Registration failed: {}", e))?;
            info!("registration response: {}", resp);

            match resp.status_code().kind() {
                StatusCodeKind::Successful => {
                    let mut last_response = self.inner.last_response.lock().await;
                    *last_response = Some(resp);
                }
                _ => {}
            }
            *last_update = Instant::now();
            let duration = Duration::from_secs(registration.expires().max(50) as u64);
            sleep(duration).await;
        }
        Ok(())
    }

    async fn unregister(&self) -> Result<()> {
        info!("unregistering user: {} not implemented", self.inner.user);
        Ok(())
    }
}
