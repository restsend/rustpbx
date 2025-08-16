use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::DialStrategy;
use crate::call::Dialplan;
use crate::call::Location;
use crate::call::RouteInvite;
use crate::call::SipUser;
use crate::call::TransactionCookie;
use crate::call::b2bua::B2buaBuilder;
use crate::call::sip::Invitation;
use crate::config::ProxyConfig;
use crate::config::RouteResult;
use crate::proxy::routing::matcher::match_invite;
use anyhow::Error;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[async_trait]
pub trait CallRouter: Send + Sync {
    async fn resolve(
        &self,
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
    ) -> Result<Dialplan, (anyhow::Error, Option<rsip::StatusCode>)>;
}
pub struct DefaultRouteInvite {
    pub routing_state: Arc<crate::proxy::routing::RoutingState>,
    pub config: Arc<ProxyConfig>,
}

#[async_trait]
impl RouteInvite for DefaultRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsip::Request,
    ) -> Result<RouteResult> {
        match_invite(
            Some(&self.config.trunks),
            self.config.routes.as_ref(),
            self.config.default.as_ref(),
            option,
            origin,
            self.routing_state.clone(),
        )
        .await
    }
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    server: SipServerRef,
    pub invitation: Invitation,
    pub dialog_layer: Arc<DialogLayer>,
    pub routing_state: Arc<crate::proxy::routing::RoutingState>,
}

#[derive(Clone)]
pub struct CallModule {
    pub(crate) inner: Arc<CallModuleInner>,
}

impl CallModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CallModule::new(config, server);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>, server: SipServerRef) -> Self {
        let dialog_layer = Arc::new(DialogLayer::new(server.endpoint.inner.clone()));
        let invitation = Invitation::new(dialog_layer.clone());
        let inner = Arc::new(CallModuleInner {
            config,
            server,
            invitation,
            dialog_layer,
            routing_state: Arc::new(crate::proxy::routing::RoutingState::new()),
        });
        Self { inner }
    }

    async fn default_resolve(
        &self,
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
    ) -> Result<Dialplan, (Error, Option<rsip::StatusCode>)> {
        let callee_uri = original
            .to_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;
        let callee = callee_uri.user().unwrap_or_default().to_string();
        let callee_realm = callee_uri.host().to_string();

        // Check if this is an external realm
        if !self.inner.server.is_same_realm(&callee_realm).await {
            info!(callee=%callee_uri, "Creating dialplan for external realm");
            let mut location = Location {
                aor: callee_uri.clone(),
                destination: SipAddr::try_from(&callee_uri).map_err(|e| (anyhow!(e), None))?,
                ..Default::default()
            };

            if let Some(location_inspector) = self.inner.server.location_inspector.as_ref() {
                match location_inspector
                    .inspect_location(location, original)
                    .await
                {
                    Ok(r) => location = r,
                    Err(e) => {
                        warn!(callee=%callee_uri, "failed to inspect location: {:?}", e);
                        return Err(e);
                    }
                }
            }
            return Ok(Dialplan {
                targets: crate::call::DialStrategy::Sequential(vec![location]),
                route_invite: Some(route_invite),
                ..Default::default()
            });
        }

        let mut locations = self
            .inner
            .server
            .locator
            .lookup(&callee, Some(&callee_realm))
            .await
            .map_err(|e| (e, None))?;

        if locations.is_empty() {
            warn!(callee = %callee_uri, "user offline in locator");
            return Err((anyhow!("User offline"), Some(rsip::StatusCode::NotFound)));
        }

        if let Some(location_inspector) = self.inner.server.location_inspector.as_ref() {
            for loc in locations.iter_mut() {
                match location_inspector
                    .inspect_location(loc.clone(), original)
                    .await
                {
                    Ok(r) => *loc = r,
                    Err(e) => {
                        warn!(callee=%callee_uri, "failed to inspect location: {:?}", e);
                        return Err(e);
                    }
                }
            }
        }

        let targets = DialStrategy::Sequential(locations.iter().map(|loc| loc.clone()).collect());

        Ok(Dialplan {
            targets,
            route_invite: Some(route_invite),
            ..Dialplan::default()
        })
    }

    pub(crate) async fn handle_invite(
        &self,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<()> {
        let caller = cookie
            .get_user()
            .ok_or_else(|| anyhow::anyhow!("Missing caller user in transaction cookie"))?;

        let caller_contact = match caller.build_contact_from_invite(&*tx) {
            Some(contact) => contact,
            None => {
                return Err(anyhow::anyhow!("Failed to build caller contact"));
            }
        };

        let route_invite = Box::new(DefaultRouteInvite {
            routing_state: self.inner.routing_state.clone(),
            config: self.inner.config.clone(),
        }) as Box<dyn RouteInvite>;

        let r = if let Some(resolver) = self.inner.server.call_router.as_ref() {
            resolver.resolve(&tx.original, route_invite).await
        } else {
            self.default_resolve(&tx.original, route_invite).await
        };

        let dialplan = match r {
            Ok(dialplan) => dialplan,
            Err((e, code)) => {
                if let Some(code) = code {
                    tx.reply(code).await.ok();
                }
                return Err(e);
            }
        };

        let cancel_token = CancellationToken::new();
        let media_capabilities = vec![];

        let session_id = format!(
            "b2bua-{}-{}",
            rand::random::<u32>(),
            DialogId::try_from(&tx.original).map_err(|e| anyhow!("Invalid dialog ID: {}", e))?
        );

        let app_state = self.inner.server.app_state.clone();
        let b2bua = B2buaBuilder::new(app_state.clone(), cookie, session_id)
            .with_recorder(true)
            .with_cancel_token(cancel_token)
            .with_media_capabilities(media_capabilities)
            .build(&tx)
            .await?;

        match b2bua
            .serve(
                tx,
                caller_contact,
                app_state.clone(),
                self.inner.invitation.clone(),
                dialplan,
            )
            .await
        {
            Ok(()) => {
                info!(
                    session_id = b2bua.session_id,
                    "session established successfully"
                );
                Ok(())
            }
            Err(e) => {
                warn!(
                    session_id = b2bua.session_id,
                    "error establishing session: {}", e
                );
                Err(e)
            }
        }
    }

    async fn process_message(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = DialogId::try_from(&tx.original).map_err(|e| anyhow!(e))?;
        let mut dialog = match self.inner.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => dialog,
            None => {
                debug!(%dialog_id, method=%tx.original.method, "dialog not found for message");
                return Ok(());
            }
        };
        dialog.handle(tx).await.map_err(|e| anyhow!(e))
    }
}

#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Bye,
            rsip::Method::Info,
            rsip::Method::Ack,
            rsip::Method::Cancel,
            rsip::Method::Options,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        debug!("Call module with Dialog-based B2BUA started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        debug!("Call module stopped, cleaning up sessions");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        match cookie.get_user() {
            None => {
                cookie.set_user(SipUser::try_from(&*tx)?);
            }
            _ => {}
        }
        let dialog_id = DialogId::try_from(&tx.original).map_err(|e| anyhow!(e))?;
        info!(
            %dialog_id,
            method = %tx.original.method,
            uri = %tx.original.uri,
            caller = ?cookie.get_user(),
            "call transaction begin",
        );
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx, cookie).await {
                    warn!(%dialog_id, "error handling INVITE: {}", e);
                    if tx.last_response.is_none() {
                        tx.reply_with(
                            rsip::StatusCode::ServerInternalError,
                            vec![rsip::Header::ErrorInfo(e.to_string().into())],
                            None,
                        )
                        .await
                        .map_err(|e| anyhow!(e))?;
                    }
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Options
            | rsip::Method::Ack
            | rsip::Method::Cancel
            | rsip::Method::Bye => {
                if let Err(e) = self.process_message(tx).await {
                    warn!(%dialog_id, method=%tx.original.method, "error process {}\n{}", e, tx.original.to_string());
                }
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}
