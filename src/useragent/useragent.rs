use crate::config::UseragentConfig;

use super::registration::RegistrationHandle;
use anyhow::{anyhow, Result};
use rsip::prelude::HeadersExt;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::DialogId;
use rsipstack::transaction::{Endpoint, TransactionReceiver};
use rsipstack::transport::{udp::UdpConnection, TransportLayer};
use rsipstack::EndpointBuilder;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct UserAgentBuilder {
    pub config: Option<UseragentConfig>,
    pub cancel_token: Option<CancellationToken>,
}
pub struct UserAgent {
    pub config: UseragentConfig,
    pub token: CancellationToken,
    pub endpoint: Endpoint,
    pub registration_handles: Mutex<HashMap<String, RegistrationHandle>>,
    pub dialog_layer: Arc<DialogLayer>,
    pub dialogs: Mutex<HashMap<DialogId, Dialog>>,
}

impl UserAgentBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            cancel_token: None,
        }
    }
    pub fn with_config(mut self, config: Option<UseragentConfig>) -> Self {
        self.config = config;
        self
    }

    pub fn with_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
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
            dialog_layer,
            dialogs: Mutex::new(HashMap::new()),
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

    async fn process_dialog(
        &self,
        dialog_layer: Arc<DialogLayer>,
        state_receiver: DialogStateReceiver,
    ) -> Result<()> {
        let mut state_receiver = state_receiver;
        while let Some(state) = state_receiver.recv().await {
            match state {
                DialogState::Calling(id) => {
                    info!("calling dialog {}", id);
                    let dialog = match dialog_layer.get_dialog(&id) {
                        Some(d) => d,
                        None => {
                            info!("dialog not found {}", id);
                            continue;
                        }
                    };
                    match dialog {
                        Dialog::ServerInvite(d) => match self.handle_server_invite(d).await {
                            Ok(_) => (),
                            Err(e) => {
                                info!("error handling server invite: {:?}", e);
                            }
                        },
                        Dialog::ClientInvite(_) => {
                            info!("client invite dialog {}", id);
                        }
                    }
                }
                DialogState::Early(id, resp) => {
                    info!("early dialog {} {}", id, resp);
                }
                DialogState::Terminated(id, status_code) => {
                    info!("dialog terminated {} {:?}", id, status_code);
                    dialog_layer.remove_dialog(&id);
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
            result = self.process_dialog(dialog_layer.clone(), state_receiver) => {
                if let Err(e) = result {
                    info!("process dialog error: {:?}", e);
                }
            },
        }
        info!("stopping");
        Ok(())
    }

    pub fn stop(&self) {
        info!("stopping");
        self.token.cancel();
    }
}
