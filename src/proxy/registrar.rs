use super::{server::SipServerRef, user::SipUser, ProxyAction, ProxyModule};
use crate::{config::ProxyConfig, proxy::locator::Location};
use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::{HeadersExt, UntypedHeader};
use rsipstack::{
    transaction::transaction::Transaction,
    transport::{SipAddr, SipConnection},
};
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::info;

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
        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        let method = tx.original.method();
        if !matches!(method, rsip::Method::Register) {
            return Ok(ProxyAction::Continue);
        }

        let user = match SipUser::try_from(&tx.original) {
            Ok(u) => u,
            Err(e) => {
                info!("failed to parse user: {:?}", e);
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };

        // Use rsipstack's via_received functionality to get destination
        let via_header = tx.original.via_header()?;
        let destination_addr = SipConnection::parse_target_from_via(via_header)
            .map_err(|e| anyhow::anyhow!("failed to parse via header: {:?}", e))?;

        let destination = SipAddr {
            r#type: via_header.trasnport().ok(),
            addr: destination_addr,
        };
        let addr = match tx.endpoint_inner.get_addrs().first() {
            Some(addr) => addr.clone(),
            None => return Err(anyhow::anyhow!("endpoint not available addr")),
        };

        let expires = match tx.original.expires_header() {
            Some(expires) => match expires.value().parse::<u32>() {
                Ok(v) => Some(v),
                Err(_) => self.config.registrar_expires.clone(),
            },
            _ => self.config.registrar_expires.clone(),
        }
        .unwrap_or(60);

        let contact_params = destination
            .r#type
            .map(|t| {
                vec![
                    rsip::Param::Transport(t),
                    rsip::Param::Expires(expires.to_string().into()),
                ]
            })
            .unwrap_or_default();

        let contact = rsip::typed::Contact {
            display_name: None,
            uri: rsip::Uri {
                scheme: addr.r#type.map(|t| t.sip_scheme()),
                auth: Some(rsip::Auth {
                    user: user.get_contact_username(),
                    password: None,
                }),
                host_with_port: addr.addr,
                ..Default::default()
            },
            params: contact_params,
        };

        let supports_webrtc = destination
            .r#type
            .is_some_and(|t| t == rsip::transport::Transport::Wss);

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
            supports_webrtc,
            "registered user"
        );

        let location = Location {
            aor: contact.uri.clone(),
            expires,
            destination,
            last_modified: Instant::now(),
            supports_webrtc,
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
        match tx.endpoint_inner.allows.lock().unwrap().as_ref() {
            Some(allows) => {
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
            None => {}
        }
        tx.reply_with(rsip::StatusCode::OK, headers, None)
            .await
            .ok();
        Ok(ProxyAction::Abort)
    }
}
