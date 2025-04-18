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
    pub sip_addr: Option<String>,
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub rtp_start_port: u16,
    pub sip_port: u16,
    pub token: Option<CancellationToken>,
}
pub struct UserAgent {
    pub token: CancellationToken,
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub rtp_start_port: u16,
    pub endpoint: Endpoint,
    pub registration_handles: Mutex<HashMap<String, RegistrationHandle>>,
    pub dialog_layer: Arc<DialogLayer>,
    pub dialogs: Mutex<HashMap<DialogId, Dialog>>,
}

impl UserAgentBuilder {
    pub fn new() -> Self {
        Self {
            sip_addr: None,
            token: None,
            sip_port: 5060,
            external_ip: None,
            stun_server: None,
            rtp_start_port: 12000,
        }
    }

    pub fn external_ip(mut self, external_ip: Option<String>) -> Self {
        self.external_ip = external_ip;
        self
    }

    pub fn stun_server(mut self, stun_server: Option<String>) -> Self {
        self.stun_server = stun_server;
        self
    }

    pub fn rtp_start_port(mut self, port: u16) -> Self {
        self.rtp_start_port = port;
        self
    }

    pub fn sip_port(mut self, port: u16) -> Self {
        self.sip_port = port;
        self
    }

    pub fn token(mut self, token: Option<CancellationToken>) -> Self {
        self.token = token;
        self
    }

    pub fn sip_addr(mut self, sip_addr: String) -> Self {
        self.sip_addr = Some(sip_addr);
        self
    }

    pub async fn build(mut self) -> Result<UserAgent> {
        let token = self
            .token
            .take()
            .unwrap_or_else(|| CancellationToken::new());

        let local_ip = if let Some(ip) = &self.sip_addr {
            IpAddr::from_str(ip)?
        } else {
            crate::net_tool::get_first_non_loopback_interface()?
        };

        let transport_layer = TransportLayer::new(token.clone());
        let local_addr: SocketAddr = format!("{}:{}", local_ip, self.sip_port).parse()?;

        let udp_conn = UdpConnection::create_connection(local_addr, None)
            .await
            .map_err(|e| anyhow!("Failed to create UDP connection: {}", e))?;

        transport_layer.add_transport(udp_conn.into());

        let endpoint = EndpointBuilder::new()
            .cancel_token(token.child_token())
            .transport_layer(transport_layer)
            .build();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        Ok(UserAgent {
            token,
            external_ip: self.external_ip.clone(),
            stun_server: self.stun_server.clone(),
            rtp_start_port: self.rtp_start_port,
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
            info!("useragent: received transaction: {:?}", tx.key);
            match tx.original.to_header()?.tag()?.as_ref() {
                Some(_) => match dialog_layer.match_dialog(&tx.original) {
                    Some(mut d) => {
                        tokio::spawn(async move {
                            match d.handle(tx).await {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("useragent: error handling transaction: {:?}", e);
                                }
                            }
                        });
                        continue;
                    }
                    None => {
                        info!("useragent: dialog not found: {}", tx.original);
                        match tx
                            .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                            .await
                        {
                            Ok(_) => (),
                            Err(e) => {
                                info!("useragent: error replying to request: {:?}", e);
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
                            info!("useragent: failed to obtain dialog: {:?}", e);
                            match tx
                                .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                                .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    info!("useragent: error replying to request: {:?}", e);
                                }
                            }
                            continue;
                        }
                    };
                    tokio::spawn(async move { dialog.handle(tx).await });
                }
                _ => {
                    info!("useragent: received request: {:?}", tx.original.method);
                    match tx.reply(rsip::StatusCode::OK).await {
                        Ok(_) => (),
                        Err(e) => {
                            info!("useragent: error replying to request: {:?}", e);
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
                    info!("useragent: calling dialog {}", id);
                    let dialog = match dialog_layer.get_dialog(&id) {
                        Some(d) => d,
                        None => {
                            info!("useragent: dialog not found {}", id);
                            continue;
                        }
                    };
                    match dialog {
                        Dialog::ServerInvite(d) => match self.handle_server_invite(d).await {
                            Ok(_) => (),
                            Err(e) => {
                                info!("useragent: error handling server invite: {:?}", e);
                            }
                        },
                        Dialog::ClientInvite(_) => {
                            info!("useragent: client invite dialog {}", id);
                        }
                    }
                }
                DialogState::Early(id, resp) => {
                    info!("useragent: early dialog {} {}", id, resp);
                }
                DialogState::Terminated(id, status_code) => {
                    info!("useragent: dialog terminated {} {:?}", id, status_code);
                    dialog_layer.remove_dialog(&id);
                }
                _ => {
                    info!("useragent: received dialog state: {}", state);
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
                info!("useragent: cancelled");
            }
            result = endpoint_inner.serve() => {
                if let Err(e) = result {
                    info!("useragent: endpoint serve error: {:?}", e);
                }
            }
            result = self.process_incoming_request(dialog_layer.clone(), incoming_txs, state_sender) => {
                if let Err(e) = result {
                    info!("useragent: process incoming request error: {:?}", e);
                }
            },
            result = self.process_dialog(dialog_layer.clone(), state_receiver) => {
                if let Err(e) = result {
                    info!("useragent: process dialog error: {:?}", e);
                }
            },
        }
        info!("useragent: stopping");
        Ok(())
    }

    pub fn stop(&self) {
        info!("useragent: stopping");
        self.token.cancel();
    }
}
