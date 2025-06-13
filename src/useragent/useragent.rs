use super::registration::RegistrationHandle;
use crate::config::UseragentConfig;
use crate::useragent::invitation::{InvitationHandler, UnavailableInvitationHandler};
use anyhow::{anyhow, Result};
use rsip::prelude::HeadersExt;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::DialogId;
use rsipstack::transaction::{Endpoint, TransactionReceiver};
use rsipstack::transport::{udp::UdpConnection, TransportLayer};
use rsipstack::EndpointBuilder;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct UserAgentBuilder {
    pub config: Option<UseragentConfig>,
    pub cancel_token: Option<CancellationToken>,
    pub invitation_handler: Option<Box<dyn InvitationHandler>>,
}

pub struct UserAgent {
    pub config: UseragentConfig,
    pub token: CancellationToken,
    pub endpoint: Endpoint,
    pub registration_handles: Mutex<HashMap<String, RegistrationHandle>>,
    pub alive_users: Arc<RwLock<HashSet<String>>>,
    pub dialog_layer: Arc<DialogLayer>,
    pub dialogs: Mutex<HashMap<DialogId, Dialog>>,
    pub invitation_handler: Box<dyn InvitationHandler>,
}

impl UserAgentBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            cancel_token: None,
            invitation_handler: None,
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
    pub fn with_invitation_handler(mut self, handler: Box<dyn InvitationHandler>) -> Self {
        self.invitation_handler = Some(handler);
        self
    }

    pub async fn build(mut self) -> Result<UserAgent> {
        let token = self
            .cancel_token
            .take()
            .unwrap_or_else(|| CancellationToken::new());

        let config = self.config.to_owned().unwrap_or_default();
        let local_ip = if !config.addr.is_empty() {
            IpAddr::from_str(config.addr.as_str())?
        } else {
            crate::net_tool::get_first_non_loopback_interface()?
        };
        let transport_layer = TransportLayer::new(token.clone());
        let local_addr: SocketAddr = format!("{}:{}", local_ip, config.udp_port).parse()?;

        let udp_conn = UdpConnection::create_connection(local_addr, None)
            .await
            .map_err(|e| anyhow!("Failed to create UDP connection: {}", e))?;

        transport_layer.add_transport(udp_conn.into());

        let endpoint = EndpointBuilder::new()
            .with_cancel_token(token.child_token())
            .with_transport_layer(transport_layer)
            .build();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        Ok(UserAgent {
            token,
            config,
            endpoint,
            registration_handles: Mutex::new(HashMap::new()),
            alive_users: Arc::new(RwLock::new(HashSet::new())),
            dialog_layer,
            dialogs: Mutex::new(HashMap::new()),
            invitation_handler: self
                .invitation_handler
                .take()
                .unwrap_or_else(|| Box::new(UnavailableInvitationHandler)),
        })
    }
}

impl UserAgent {
    async fn process_incoming_request(
        &self,
        dialog_layer: Arc<DialogLayer>,
        mut incoming: TransactionReceiver,
        state_sender: DialogStateSender,
    ) -> Result<()> {
        while let Some(mut tx) = incoming.recv().await {
            info!("received transaction: {:?}", tx.key);
            match tx.original.to_header()?.tag()?.as_ref() {
                Some(_) => match dialog_layer.match_dialog(&tx.original) {
                    Some(mut d) => {
                        tokio::spawn(async move {
                            match d.handle(tx).await {
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
                },
                None => {}
            }
            // out dialog, new server dialog
            match tx.original.method {
                rsip::Method::Invite | rsip::Method::Ack => {
                    let mut dialog = match dialog_layer.get_or_create_server_invite(
                        &tx,
                        state_sender.clone(),
                        None,
                        None,
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
                    tokio::spawn(async move { dialog.handle(tx).await });
                }
                _ => {
                    info!("received request: {:?}", tx.original.method);
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

    async fn process_server_dialog(
        &self,
        dialog_layer: Arc<DialogLayer>,
        state_receiver: DialogStateReceiver,
    ) -> Result<()> {
        let mut state_receiver = state_receiver;
        while let Some(state) = state_receiver.recv().await {
            let id = match &state {
                DialogState::Calling(id) => id,
                DialogState::Early(id, _) => id,
                DialogState::Terminated(id, _) => id,
                DialogState::Confirmed(id) => id,
                DialogState::Info(id, _) => id,
                DialogState::Options(id, _) => id,
                DialogState::Updated(id, _) => id,
                DialogState::Trying(id) => id,
                DialogState::WaitAck(id, _) => id,
                DialogState::Notify(id, _) => id,
            };
            let dialog = match dialog_layer.get_dialog(&id) {
                Some(Dialog::ServerInvite(d)) => d,
                _ => {
                    continue;
                }
            };
            match state {
                DialogState::Calling(id) => {
                    info!(?id, "calling dialog");
                    if let Err(_) = self.handle_server_invite(dialog.clone()).await {
                        dialog.reject().ok();
                        continue;
                    }
                    match self.invitation_handler.on_invite(dialog.clone()).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!(
                                id = ?dialog.id(),
                                "error handling server invite: {:?}", e);
                            dialog.reject().ok();
                        }
                    }
                }
                DialogState::Early(id, resp) => {
                    info!(?id, "early dialog");
                    match self
                        .invitation_handler
                        .on_early_media(dialog.clone(), resp)
                        .await
                    {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error handling early media: {:?}", e);
                        }
                    }
                }
                DialogState::Terminated(id, reason) => {
                    info!(?id, ?reason, "dialog terminated");
                    match self.invitation_handler.on_terminate(dialog, reason).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error handling terminate: {:?}", e);
                        }
                    }
                    dialog_layer.remove_dialog(&id);
                }
                DialogState::Confirmed(id) => {
                    info!(?id, "dialog confirmed");
                    match self.invitation_handler.on_confirmed(dialog).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error handling confirmed: {:?}", e);
                        }
                    }
                }
                DialogState::Info(id, req) => {
                    info!(?id, "dialog info");
                    match self.invitation_handler.on_info(dialog, req).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error handling info: {:?}", e);
                        }
                    }
                }
                DialogState::Options(id, req) => {
                    info!(?id, "dialog options");
                    match self.invitation_handler.on_options(dialog, req).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error handling options: {:?}", e);
                        }
                    }
                }
                DialogState::Updated(id, req) => {
                    info!(?id, "dialog updated");
                    match self.invitation_handler.on_update(dialog, req).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("error handling update: {:?}", e);
                        }
                    }
                }
                _ => {
                    info!("received dialog state: {}", state);
                }
            }
        }
        Ok(())
    }

    pub async fn serve(&self) -> Result<()> {
        let incoming_txs = self.endpoint.incoming_transactions();
        let token = self.token.child_token();
        let endpoint_inner = self.endpoint.inner.clone();
        let (state_sender, state_receiver) = unbounded_channel();

        let dialog_layer = self.dialog_layer.clone();
        match self.start_registration().await {
            Ok(_) => {
                info!("registration started");
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
            result = self.process_incoming_request(dialog_layer.clone(), incoming_txs, state_sender) => {
                if let Err(e) = result {
                    info!("process incoming request error: {:?}", e);
                }
            },
            result = self.process_server_dialog(dialog_layer.clone(), state_receiver) => {
                if let Err(e) = result {
                    info!("process dialog error: {:?}", e);
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
