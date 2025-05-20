use super::{server::SipServerRef, user::SipUser, ProxyAction, ProxyModule};
use crate::{config::ProxyConfig, proxy::locator::Location};
use anyhow::Result;
use async_trait::async_trait;
use rsip::prelude::{HeadersExt, ToTypedHeader, UntypedHeader};
use rsipstack::{
    rsip_ext::extract_uri_from_contact, transaction::transaction::Transaction, transport::SipAddr,
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
                info!("failed to parse contact: {:?}", e);
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };

        let contact = extract_uri_from_contact(tx.original.contact_header()?.value())
            .map_err(|e| anyhow::anyhow!("failed to parse contact: {:?}", e))?;

        let via = tx.original.via_header()?.typed()?;

        let mut destination = SipAddr {
            r#type: via.uri.transport().cloned(),
            addr: contact.host_with_port,
        };

        via.params.iter().for_each(|param| match param {
            rsip::Param::Transport(t) => {
                destination.r#type = Some(t.clone());
            }
            rsip::Param::Received(r) => match r.value().try_into() {
                Ok(addr) => destination.addr.host = addr,
                Err(_) => {}
            },
            rsip::Param::Other(o, Some(v)) => {
                if o.value().eq_ignore_ascii_case("rport") {
                    match v.value().try_into() {
                        Ok(port) => destination.addr.port = Some(port),
                        Err(_) => {}
                    }
                }
            }
            _ => {}
        });

        let contact = rsip::typed::Contact {
            display_name: None,
            uri: rsip::Uri {
                scheme: Some(rsip::Scheme::Sip),
                auth: Some(rsip::Auth {
                    user: user.username.clone(),
                    password: None,
                }),
                host_with_port: destination.addr.clone(),
                ..Default::default()
            },
            params: vec![],
        };

        let expires = match tx.original.expires_header() {
            Some(expires) => match expires.value().parse::<u32>() {
                Ok(v) => {
                    if v == 0 {
                        // delete user
                        info!(
                            username = user.username,
                            ?contact,
                            ?destination,
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
                    self.config.registrar_expires.clone()
                }
                Err(_) => self.config.registrar_expires.clone(),
            },
            None => self.config.registrar_expires.clone(),
        };

        info!(
            username = user.username,
            ?contact,
            ?destination,
            "registered user"
        );

        let location = Location {
            aor: contact.uri.clone(),
            expires: expires.unwrap_or(60),
            destination,
            last_modified: Instant::now(),
        };

        self.server
            .locator
            .register(user.username.as_str(), user.realm.as_deref(), location)
            .await
            .ok();

        let mut headers = vec![
            contact.into(),
            rsip::Header::Expires(expires.unwrap_or(60).into()),
        ];

        if !tx.endpoint_inner.allows.is_empty() {
            headers.push(rsip::Header::Allow(
                tx.endpoint_inner
                    .allows
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<String>>()
                    .join(",")
                    .into(),
            ));
        }
        tx.reply_with(rsip::StatusCode::OK, headers, None)
            .await
            .ok();
        Ok(ProxyAction::Abort)
    }
}
