use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::user::SipUser;
use crate::call::{Location, TransactionCookie};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::{HeadersExt, UntypedHeader};
use rsipstack::transaction::transaction::Transaction;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Clone)]
pub struct RegistrarModule {
    server: SipServerRef,
    config: Arc<ProxyConfig>,
}

impl RegistrarModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = RegistrarModule::new(server, config);
        Ok(Box::new(module))
    }
    pub fn new(server: SipServerRef, config: Arc<ProxyConfig>) -> Self {
        Self { server, config }
    }
}

#[async_trait]
impl ProxyModule for RegistrarModule {
    fn name(&self) -> &str {
        "registrar"
    }
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![rsip::Method::Register]
    }
    async fn on_start(&mut self) -> Result<()> {
        debug!("Registrar module started");
        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        debug!("Registrar module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        _cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        let method = tx.original.method();
        if !matches!(method, rsip::Method::Register) {
            return Ok(ProxyAction::Continue);
        }

        let user = match SipUser::try_from(&*tx) {
            Ok(u) => u,
            Err(e) => {
                info!("failed to parse user: {:?}", e);
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };

        let expires = match tx.original.expires_header() {
            Some(expires) => match expires.value().parse::<u32>() {
                Ok(v) => Some(v),
                Err(_) => self.config.registrar_expires,
            },
            _ => self.config.registrar_expires,
        }
        .unwrap_or(60);

        let destination = match user.destination.as_ref() {
            Some(d) => d,
            None => {
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };
        let mut contact_params = vec![rsip::Param::Expires(expires.to_string().into())];
        match destination.r#type {
            Some(rsip::Transport::Udp) | None => {}
            Some(t) => {
                contact_params.push(rsip::Param::Transport(t));
            }
        }
        let contact = rsip::typed::Contact {
            display_name: None,
            uri: rsip::Uri {
                scheme: destination.r#type.map(|t| t.sip_scheme()),
                auth: Some(rsip::Auth {
                    user: user.get_contact_username(),
                    password: None,
                }),
                host_with_port: destination.addr.clone(),
                ..Default::default()
            },
            params: contact_params,
        };

        if expires == 0 {
            // delete user
            info!(
                username = user.username,
                contact = contact.to_string(),
                destination = destination.to_string(),
                realm = user.realm,
                "unregistered user"
            );
            self.server
                .locator
                .unregister(user.username.as_str(), user.realm.as_deref())
                .await
                .ok();
            tx.reply(rsip::StatusCode::OK).await.ok();
            return Ok(ProxyAction::Abort);
        }

        info!(
            username = user.username,
            contact = contact.to_string(),
            destination = destination.to_string(),
            realm = user.realm,
            supports_webrtc = user.is_support_webrtc,
            "registered user"
        );

        let location = Location {
            aor: contact.uri.clone(),
            expires,
            destination: destination.clone(),
            last_modified: Some(Instant::now()),
            supports_webrtc: user.is_support_webrtc,
            ..Default::default()
        };

        match self
            .server
            .locator
            .register(user.username.as_str(), user.realm.as_deref(), location)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                info!("failed to register user: {:?}", e);
                tx.reply(rsip::StatusCode::ServiceUnavailable).await.ok();
                return Ok(ProxyAction::Abort);
            }
        }

        let mut headers = vec![contact.into(), rsip::Header::Expires(expires.into())];
        if let Some(allows) = tx.endpoint_inner.allows.lock().unwrap().as_ref() {
            if !allows.is_empty() {
                headers.push(rsip::Header::Allow(
                    allows
                        .iter()
                        .map(|m| m.to_string())
                        .collect::<Vec<String>>()
                        .join(",")
                        .into(),
                ));
            }
        }
        tx.reply_with(rsip::StatusCode::OK, headers, None)
            .await
            .ok();
        Ok(ProxyAction::Abort)
    }
}
