use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::DialDirection;
use crate::call::DialStrategy;
use crate::call::Dialplan;
use crate::call::Location;
use crate::call::MediaConfig;
use crate::call::RouteInvite;
use crate::call::SipUser;
use crate::call::TransactionCookie;
use crate::config::ProxyConfig;
use crate::config::RouteResult;
use crate::proxy::proxy_call::ProxyCall;
use crate::proxy::proxy_call::ProxyCallBuilder;
use crate::proxy::routing::matcher::match_invite;
use anyhow::Error;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::transaction::transaction::Transaction;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[async_trait]
pub trait CallRouter: Send + Sync {
    async fn resolve(
        &self,
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
        contact: rsip::typed::Contact,
    ) -> Result<Dialplan, (anyhow::Error, Option<rsip::StatusCode>)>;
}

#[async_trait]
pub trait DialplanInspector: Send + Sync {
    async fn inspect_dialplan(&self, dialplan: Dialplan, _original: &rsip::Request) -> Dialplan {
        dialplan
    }
}

#[async_trait]
pub trait ProxyCallInspector: Send + Sync {
    async fn on_start(&self, call: ProxyCall) -> Result<ProxyCall, (rsip::StatusCode, String)>;
    async fn on_end(&self, call: &ProxyCall);
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
        direction: &DialDirection,
    ) -> Result<RouteResult> {
        match_invite(
            Some(&self.config.trunks),
            self.config.routes.as_ref(),
            self.config.default.as_ref(),
            option,
            origin,
            self.routing_state.clone(),
            direction,
        )
        .await
    }
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    server: SipServerRef,
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
        let inner = Arc::new(CallModuleInner {
            config,
            server,
            dialog_layer,
            routing_state: Arc::new(crate::proxy::routing::RoutingState::new()),
        });
        Self { inner }
    }

    async fn default_resolve(
        &self,
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller_contact: rsip::typed::Contact,
    ) -> Result<Dialplan, (Error, Option<rsip::StatusCode>)> {
        let callee_uri = original
            .to_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;
        let callee_realm = callee_uri.host().to_string();
        let dialog_id = DialogId::try_from(original).map_err(|e| (anyhow!(e), None))?;
        let session_id = format!("{}/{}", dialog_id, rand::random::<u32>());

        let caller = original
            .from_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;

        let media_config = MediaConfig::new()
            .with_external_ip(self.inner.server.app_state.config.external_ip.clone())
            .with_rtp_start_port(self.inner.server.app_state.config.rtp_start_port.clone())
            .with_rtp_end_port(self.inner.server.app_state.config.rtp_end_port.clone());

        let direction = if self
            .inner
            .server
            .is_same_realm(&caller.host().to_string())
            .await
        {
            DialDirection::Outbound
        } else {
            DialDirection::Inbound
        };

        let mut dialplan = Dialplan::new(session_id, original.clone(), direction)
            .with_caller_contact(caller_contact)
            .with_caller(caller)
            .with_media(media_config)
            .with_route_invite(route_invite);

        // Check if this is an external realm
        let mut targets = if !self.inner.server.is_same_realm(&callee_realm).await {
            info!(callee=%callee_uri, callee_realm, "Creating dialplan for external realm");
            DialStrategy::Sequential(vec![Location {
                aor: callee_uri.clone(),
                ..Default::default()
            }])
        } else {
            let mut locations = self
                .inner
                .server
                .locator
                .lookup(&callee_uri)
                .await
                .map_err(|e| (e, Some(rsip::StatusCode::TemporarilyUnavailable)))?;

            if locations.is_empty() {
                info!(%dialog_id, callee = %callee_uri, "user offline in locator");
                locations.push(Location {
                    aor: callee_uri.clone(),
                    abort_on_route_invite_missing: Some(rsip::StatusCode::TemporarilyUnavailable), // abort if route invite is missing
                    ..Default::default()
                });
            }
            DialStrategy::Sequential(locations)
        };

        if let Some(location_inspector) = self.inner.server.location_inspector.as_ref() {
            match location_inspector
                .inspect_locations(targets, original)
                .await
            {
                Ok(t) => targets = t,
                Err(e) => {
                    warn!(callee=%callee_uri, "failed to inspect location: {:?}", e);
                    return Err(e);
                }
            }
        }

        dialplan = dialplan.with_targets(targets);
        Ok(dialplan)
    }

    pub(crate) async fn handle_invite(
        &self,
        cancel_token: CancellationToken,
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

        let route_invite = match self.inner.server.create_route_invite.as_ref() {
            Some(f) => f(self.inner.server.clone(), self.inner.config.clone())?,
            None => Box::new(DefaultRouteInvite {
                routing_state: self.inner.routing_state.clone(),
                config: self.inner.config.clone(),
            }) as Box<dyn RouteInvite>,
        };

        let r = if let Some(resolver) = self.inner.server.call_router.as_ref() {
            resolver
                .resolve(&tx.original, route_invite, caller_contact)
                .await
        } else {
            self.default_resolve(&tx.original, route_invite, caller_contact)
                .await
        };

        let dialplan = match r {
            Ok(dialplan) => dialplan,
            Err((e, code)) => {
                let code = code.unwrap_or(rsip::StatusCode::ServerInternalError);
                let reason_phrase = rsip::Header::Other("Reason".into(), e.to_string());
                warn!(%code, key = %tx.key,"failed to resolve dialplan: {}", reason_phrase);
                tx.reply_with(code, vec![reason_phrase], None)
                    .await
                    .map_err(|e| anyhow!("Failed to send reply: {}", e))?;
                return Err(e);
            }
        };

        let dialplan = if let Some(inspector) = self.inner.server.dialplan_inspector.as_ref() {
            inspector.inspect_dialplan(dialplan, &tx.original).await
        } else {
            dialplan
        };

        // Create event sender for media stream events
        let builder = ProxyCallBuilder::new(cookie, dialplan)
            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
            .with_cancel_token(cancel_token);

        let proxy_call = builder.build(self.inner.dialog_layer.clone());
        let proxy_call = if let Some(inspector) = self.inner.server.proxycall_inspector.as_ref() {
            match inspector.on_start(proxy_call).await {
                Ok(call) => call,
                Err((code, reason_phrase)) => {
                    warn!(%code, key = %tx.key,"failed to proxy call {}", reason_phrase);
                    let reason_phrase = rsip::Header::Other("Reason".into(), reason_phrase);
                    tx.reply_with(code, vec![reason_phrase], None)
                        .await
                        .map_err(|e| anyhow!("failed to proxy call: {}", e))?;
                    return Err(anyhow::anyhow!("failed toproxy call"));
                }
            }
        } else {
            proxy_call
        };

        let r = proxy_call.process(tx).await;
        if let Some(inspector) = self.inner.server.proxycall_inspector.as_ref() {
            inspector.on_end(&proxy_call).await;
        }

        match r {
            Ok(()) => {
                info!(session_id=proxy_call.id(), elapsed = ?proxy_call.elapsed(), "session successfully");
                Ok(())
            }
            Err(e) => {
                warn!(session_id=proxy_call.id(), elapsed = ?proxy_call.elapsed(), "error establishing session: {}", e);
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
        token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        if cookie.get_user().is_none() {
            cookie.set_user(SipUser::try_from(&*tx)?);
        }
        let dialog_id = DialogId::try_from(&tx.original).map_err(|e| anyhow!(e))?;
        info!(
            %dialog_id,
            method = %tx.original.method,
            uri = %tx.original.uri,
            caller = %cookie.get_user().as_ref().map(|u|u.to_string()).unwrap_or_default(),
            "call transaction begin",
        );
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(token, tx, cookie).await {
                    if tx.last_response.is_none() {
                        tx.reply_with(
                            rsip::StatusCode::ServerInternalError,
                            vec![rsip::Header::Other(
                                "Reason".into(),
                                format!(
                                    "SIP;cause=500;text=\"{}\"",
                                    urlencoding::encode(e.to_string().as_str())
                                ),
                            )],
                            None,
                        )
                        .await
                        .map_err(|e| anyhow!(e))?;
                    }
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Options
            | rsip::Method::Info
            | rsip::Method::Ack
            | rsip::Method::Update
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
