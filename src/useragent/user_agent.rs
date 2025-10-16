use super::registration::RegistrationHandle;
use crate::call::sip::Invitation;
use crate::config::UseragentConfig;
use crate::useragent::invitation::{
    FnCreateInvitationHandler, PendingDialog, default_create_invite_handler,
};
use anyhow::{Result, anyhow};
use humantime::parse_duration;
use rsip::prelude::HeadersExt;
use rsipstack::EndpointBuilder;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::transaction::endpoint::EndpointOption;
use rsipstack::transaction::{Endpoint, TransactionReceiver};
use rsipstack::transport::{TransportLayer, udp::UdpConnection};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::select;
use tokio::sync::Mutex;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct UserAgentBuilder {
    pub config: Option<UseragentConfig>,
    pub cancel_token: Option<CancellationToken>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
}

pub struct UserAgent {
    pub config: UseragentConfig,
    pub token: CancellationToken,
    pub endpoint: Endpoint,
    pub registration_handles: Mutex<HashMap<String, RegistrationHandle>>,
    pub alive_users: Arc<RwLock<HashSet<String>>>,
    pub dialog_layer: Arc<DialogLayer>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
    pub invitation: Invitation,
    pub routing_state: Arc<crate::call::RoutingState>,
}

impl Default for UserAgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl UserAgentBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            cancel_token: None,
            create_invitation_handler: None,
        }
    }
    pub fn with_config(mut self, config: Option<UseragentConfig>) -> Self {
        self.config = config;
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_create_invitation_handler(
        mut self,
        handler: Option<FnCreateInvitationHandler>,
    ) -> Self {
        self.create_invitation_handler = handler;
        self
    }

    pub async fn build(mut self) -> Result<UserAgent> {
        let cancel_token = self.cancel_token.take().unwrap_or_default();

        let config = self.config.to_owned().unwrap_or_default();
        let local_ip = if !config.addr.is_empty() {
            IpAddr::from_str(config.addr.as_str())?
        } else {
            crate::net_tool::get_first_non_loopback_interface()?
        };
        let transport_layer = TransportLayer::new(cancel_token.clone());
        let local_addr: SocketAddr = format!("{}:{}", local_ip, config.udp_port).parse()?;

        let udp_conn =
            UdpConnection::create_connection(local_addr, None, Some(cancel_token.child_token()))
                .await
                .map_err(|e| anyhow!("Create useragent UDP connection: {} {}", local_addr, e))?;

        transport_layer.add_transport(udp_conn.into());
        info!("start useragent, addr: {}", local_addr);

        let endpoint_option = EndpointOption {
            callid_suffix: config.callid_suffix.clone(),
            ..Default::default()
        };
        let mut endpoint_builder = EndpointBuilder::new();
        if let Some(ref user_agent) = config.useragent {
            endpoint_builder.with_user_agent(user_agent.as_str());
        }
        let endpoint = endpoint_builder
            .with_cancel_token(cancel_token.child_token())
            .with_transport_layer(transport_layer)
            .with_option(endpoint_option)
            .build();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        Ok(UserAgent {
            token: cancel_token,
            config,
            endpoint,
            registration_handles: Mutex::new(HashMap::new()),
            alive_users: Arc::new(RwLock::new(HashSet::new())),
            dialog_layer: dialog_layer.clone(),
            create_invitation_handler: self.create_invitation_handler,
            invitation: Invitation::new(dialog_layer),
            routing_state: Arc::new(crate::call::RoutingState::new()),
        })
    }
}

impl UserAgent {
    async fn process_incoming_request(
        &self,
        dialog_layer: Arc<DialogLayer>,
        mut incoming: TransactionReceiver,
    ) -> Result<()> {
        while let Some(mut tx) = incoming.recv().await {
            let key: &rsipstack::transaction::key::TransactionKey = &tx.key;
            info!(?key, "received transaction");
            if tx.original.to_header()?.tag()?.as_ref().is_some() {
                match dialog_layer.match_dialog(&tx.original) {
                    Some(mut d) => {
                        tokio::spawn(async move {
                            match d.handle(&mut tx).await {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error handling transaction: {:?}", e);
                                }
                            }
                        });
                        continue;
                    }
                    None => {
                        info!("dialog not found: {}", tx.original);
                        match tx
                            .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                            .await
                        {
                            Ok(_) => (),
                            Err(e) => {
                                info!("error replying to request: {:?}", e);
                            }
                        }
                        continue;
                    }
                }
            }
            // out dialog, new server dialog
            let (state_sender, state_receiver) = unbounded_channel();
            match tx.original.method {
                rsip::Method::Invite | rsip::Method::Ack => {
                    let invitation_handler = match self.create_invitation_handler {
                        Some(ref create_invitation_handler) => {
                            create_invitation_handler(self.config.handler.as_ref()).ok()
                        }
                        _ => default_create_invite_handler(self.config.handler.as_ref()),
                    };
                    let invitation_handler = match invitation_handler {
                        Some(h) => h,
                        None => {
                            info!(?key, "no invite handler configured, rejecting INVITE");
                            match tx
                                .reply_with(
                                    rsip::StatusCode::ServiceUnavailable,
                                    vec![rsip::Header::Other(
                                        "Reason".into(),
                                        "SIP;cause=503;text=\"No invite handler configured\""
                                            .into(),
                                    )],
                                    None,
                                )
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error replying to request: {:?}", e);
                                }
                            }
                            continue;
                        }
                    };
                    let contact = dialog_layer
                        .endpoint
                        .get_addrs()
                        .first()
                        .map(|addr| rsip::Uri {
                            scheme: Some(rsip::Scheme::Sip),
                            auth: None,
                            host_with_port: addr.addr.clone(),
                            params: vec![],
                            headers: vec![],
                        });
                    let dialog = match dialog_layer.get_or_create_server_invite(
                        &tx,
                        state_sender,
                        None,
                        contact,
                    ) {
                        Ok(d) => d,
                        Err(e) => {
                            // 481 Dialog/Transaction Does Not Exist
                            info!("failed to obtain dialog: {:?}", e);
                            match tx
                                .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("error replying to request: {:?}", e);
                                }
                            }
                            continue;
                        }
                    };
                    let dialog_id_str = dialog.id().to_string();

                    let token = self.token.child_token();
                    let pending_dialog = PendingDialog {
                        token: token.clone(),
                        dialog: dialog.clone(),
                        state_receiver,
                    };
                    self.invitation
                        .pending_dialogs
                        .lock()
                        .await
                        .insert(dialog_id_str.clone(), pending_dialog);

                    let accept_timeout = self
                        .config
                        .accept_timeout
                        .as_ref()
                        .and_then(|t| parse_duration(t).ok())
                        .unwrap_or_else(|| Duration::from_secs(60));
                    let pending_dialogs = self.invitation.pending_dialogs.clone();
                    let token_ref = token.clone();
                    let dialog_id_str_clone = dialog_id_str.clone();
                    tokio::spawn(async move {
                        select! {
                            _ = token_ref.cancelled() => {}
                            _ = tokio::time::sleep(accept_timeout) => {}
                        }
                        if let Some(call) =
                            pending_dialogs.lock().await.remove(&dialog_id_str_clone)
                        {
                            warn!(dialog_id = %dialog_id_str_clone, timeout = ?accept_timeout, "accept timeout, rejecting dialog");
                            call.dialog
                                .reject(Some(rsip::StatusCode::BusyHere), None)
                                .ok();
                            token_ref.cancel();
                        }
                    });
                    let mut dialog_ref = dialog.clone();
                    let token_ref = token.clone();
                    let routing_state = self.routing_state.clone();
                    tokio::spawn(async move {
                        let invite_loop = async {
                            match invitation_handler
                                .on_invite(
                                    dialog_id_str.clone(),
                                    token,
                                    dialog.clone(),
                                    routing_state,
                                )
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!(id = dialog_id_str, "error handling invite: {:?}", e);
                                    dialog
                                        .reject(Some(rsip::StatusCode::ServerInternalError), None)
                                        .ok();
                                }
                            }
                        };
                        select! {
                            _ = token_ref.cancelled() => {}
                            _ = async {
                                let (_,_ ) = tokio::join!(dialog_ref.handle(&mut tx), invite_loop);
                             } => {}
                        }
                    });
                }
                rsip::Method::Options => {
                    if tx.endpoint_inner.option.ignore_out_of_dialog_option {
                        let to_tag = tx
                            .original
                            .to_header()
                            .and_then(|to| to.tag())
                            .ok()
                            .flatten();
                        if to_tag.is_none() {
                            info!(?key, "ignoring out-of-dialog OPTIONS request");
                            continue;
                        }
                    }
                }
                _ => {
                    info!(?key, "received request: {:?}", tx.original.method);
                    match tx.reply(rsip::StatusCode::OK).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error replying to request: {:?}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn serve(&self) -> Result<()> {
        let incoming_txs = self.endpoint.incoming_transactions()?;
        let token = self.token.child_token();
        let endpoint_inner = self.endpoint.inner.clone();
        let dialog_layer = self.dialog_layer.clone();

        match self.start_registration().await {
            Ok(count) => {
                info!("registration started, count: {}", count);
            }
            Err(e) => {
                warn!("failed to start registration: {:?}", e);
            }
        }

        tokio::select! {
            _ = token.cancelled() => {
                info!("cancelled");
            }
            result = endpoint_inner.serve() => {
                if let Err(e) = result {
                    info!("endpoint serve error: {:?}", e);
                }
            }
            result = self.process_incoming_request(dialog_layer.clone(), incoming_txs) => {
                if let Err(e) = result {
                    info!("process incoming request error: {:?}", e);
                }
            },
        }

        // Wait for registration to stop, if not stopped within 50 seconds,
        // force stop it.
        let timeout = self
            .config
            .graceful_shutdown
            .map(|_| Duration::from_secs(50));

        match self.stop_registration(timeout).await {
            Ok(_) => {
                info!("registration stopped, waiting for clear");
            }
            Err(e) => {
                warn!("failed to stop registration: {:?}", e);
            }
        }
        info!("stopping");
        Ok(())
    }

    pub fn stop(&self) {
        info!("stopping");
        self.token.cancel();
    }
}
