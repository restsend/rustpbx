use super::UserAgent;
use anyhow::Result;
use rsip::{Response, StatusCodeKind};
use rsipstack::{
    dialog::{authenticate::Credential, registration::Registration},
    rsip_ext::RsipResponseExt,
    transaction::endpoint::EndpointInnerRef,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::Mutex,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct UserCredential {
    pub username: String,
    pub password: String,
    pub realm: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct RegisterOption {
    pub server: String,
    pub username: String,
    pub display_name: Option<String>,
    pub disabled: Option<bool>,
    pub credential: Option<UserCredential>,
}

impl Into<Credential> for UserCredential {
    fn into(self) -> Credential {
        Credential {
            username: self.username,
            password: self.password,
            realm: self.realm,
        }
    }
}

impl RegisterOption {
    pub fn aor(&self) -> String {
        format!("{}@{}", self.username, self.server)
    }
}

pub struct RegistrationHandleInner {
    pub endpoint_inner: EndpointInnerRef,
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
    pub async fn start_registration(&self) -> Result<usize> {
        let mut count = 0;
        if let Some(register_users) = &self.config.register_users {
            for option in register_users.iter() {
                match self.register(option.clone()).await {
                    Ok(_) => {
                        count += 1;
                    }
                    Err(e) => {
                        warn!("failed to register user: {:?} {:?}", e, option);
                    }
                }
            }
        }
        Ok(count)
    }

    pub async fn stop_registration(&self, wait_for_clear: Option<Duration>) -> Result<()> {
        for (_, handle) in self.registration_handles.lock().await.iter_mut() {
            handle.stop();
        }

        if let Some(duration) = wait_for_clear {
            let live_users = self.alive_users.clone();
            let check_loop = async move {
                while let Ok(users) = live_users.read() {
                    if users.is_empty() {
                        break;
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            };
            match timeout(duration, check_loop).await {
                Ok(_) => {}
                Err(e) => {
                    warn!("failed to wait for clear: {}", e);
                    return Err(anyhow::anyhow!("failed to wait for clear: {}", e));
                }
            }
        }
        Ok(())
    }

    pub async fn register(&self, option: RegisterOption) -> Result<()> {
        let user = option.aor();
        let mut server = option.server.clone();
        if !server.starts_with("sip:") && !server.starts_with("sips:") {
            server = format!("sip:{}", server);
        }
        let sip_server = match rsip::Uri::try_from(server) {
            Ok(uri) => uri,
            Err(e) => {
                warn!("failed to parse server: {} {:?}", e, option.server);
                return Err(anyhow::anyhow!("failed to parse server: {}", e));
            }
        };
        let cancel_token = self.token.child_token();
        let handle = RegistrationHandle {
            inner: Arc::new(RegistrationHandleInner {
                endpoint_inner: self.endpoint.inner.clone(),
                option,
                cancel_token,
                start_time: Mutex::new(Instant::now()),
                last_update: Mutex::new(Instant::now()),
                last_response: Mutex::new(None),
            }),
        };
        self.registration_handles
            .lock()
            .await
            .insert(user.clone(), handle.clone());
        let alive_users = self.alive_users.clone();

        tokio::spawn(async move {
            *handle.inner.start_time.lock().await = Instant::now();

            select! {
                _ = handle.inner.cancel_token.cancelled() => {
                }
                _ = async {
                    loop {
                        let user = handle.inner.option.aor();
                        alive_users.write().unwrap().remove(&user);
                        let refresh_time = match handle.do_register(&sip_server, None).await {
                            Ok(expires) => {
                                info!(
                                    user = handle.inner.option.aor(),
                                    expires = expires,
                                    alive_users = alive_users.read().unwrap().len(),
                                    "registration refreshed",
                                );
                                alive_users.write().unwrap().insert(user);
                                expires * 3 / 4 // 75% of expiration time
                            }
                            Err(e) => {
                                warn!(
                                    user = handle.inner.option.aor(),
                                    alive_users = alive_users.read().unwrap().len(),
                                    "registration failed: {:?}", e);
                                60
                            }
                        };
                        sleep(Duration::from_secs(refresh_time as u64)).await;
                    }
                } => {}
            }
            handle.do_register(&sip_server, Some(0)).await.ok();
            alive_users.write().unwrap().remove(&user);
        });
        Ok(())
    }

    pub async fn unregister(&self, option: RegisterOption) -> Result<()> {
        let user = option.aor();
        if let Some(r) = self.registration_handles.lock().await.remove(&user) {
            info!("unregistering user: {}", user);
            r.stop();
        }
        Ok(())
    }
}

impl RegistrationHandle {
    fn stop(&self) {
        self.inner.cancel_token.cancel();
    }

    async fn do_register(&self, sip_server: &rsip::Uri, expires: Option<u32>) -> Result<u32> {
        let mut registration = Registration::new(
            self.inner.endpoint_inner.clone(),
            self.inner.option.credential.clone().map(|c| c.into()),
        );
        let resp = match registration
            .register(sip_server.clone(), expires)
            .await
            .map_err(|e| anyhow::anyhow!("Registration failed: {}", e))
        {
            Ok(resp) => resp,
            Err(e) => {
                warn!("registration failed: {}", e);
                return Err(anyhow::anyhow!("Registration failed: {}", e));
            }
        };

        debug!(
            user = self.inner.option.aor(),
            "registration response: {:?}", resp
        );
        match resp.status_code().kind() {
            StatusCodeKind::Successful => {
                *self.inner.last_update.lock().await = Instant::now();
                *self.inner.last_response.lock().await = Some(resp);
                Ok(registration.expires())
            }
            _ => Err(anyhow::anyhow!("{:?}", resp.reason_phrase())),
        }
    }
}
