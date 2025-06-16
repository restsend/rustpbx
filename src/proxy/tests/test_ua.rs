use anyhow::{anyhow, Result};
use rsip::prelude::HeadersExt;
use rsip::typed::MediaType;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::registration::Registration;
use rsipstack::dialog::DialogId;
use rsipstack::transaction::{EndpointBuilder, TransactionReceiver};
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::TransportLayer;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// Extension trait for converting rsipstack::Error to anyhow::Error
trait RsipErrorExt {
    fn into_anyhow(self) -> anyhow::Error;
}

impl RsipErrorExt for rsipstack::Error {
    fn into_anyhow(self) -> anyhow::Error {
        anyhow!("rsipstack error: {:?}", self)
    }
}

#[derive(Debug, Clone)]
pub struct TestUaConfig {
    pub username: String,
    pub password: String,
    pub realm: String,
    pub local_port: u16,
    pub proxy_addr: SocketAddr,
}

pub struct TestUa {
    config: TestUaConfig,
    cancel_token: CancellationToken,
    dialog_layer: Option<Arc<DialogLayer>>,
    state_sender: Option<DialogStateSender>,
    state_receiver: Option<DialogStateReceiver>,
    contact_uri: Option<rsip::Uri>,
}

#[derive(Debug, Clone)]
#[allow(unused)]
pub enum TestUaEvent {
    Registered,
    RegistrationFailed(String),
    IncomingCall(DialogId),
    CallEstablished(DialogId),
    CallTerminated(DialogId),
    CallFailed(String),
}

impl TestUa {
    pub fn new(config: TestUaConfig) -> Self {
        Self {
            config,
            cancel_token: CancellationToken::new(),
            dialog_layer: None,
            state_sender: None,
            state_receiver: None,
            contact_uri: None,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let transport_layer = TransportLayer::new(self.cancel_token.clone());

        // Bind to local port
        let local_addr = format!("127.0.0.1:{}", self.config.local_port).parse::<SocketAddr>()?;

        let connection = UdpConnection::create_connection(local_addr, None)
            .await
            .map_err(|e| e.into_anyhow())?;
        transport_layer.add_transport(connection.into());

        let endpoint = EndpointBuilder::new()
            .with_cancel_token(self.cancel_token.clone())
            .with_transport_layer(transport_layer)
            .build();

        let incoming = endpoint.incoming_transactions();
        self.dialog_layer = Some(Arc::new(DialogLayer::new(endpoint.inner.clone())));

        let (state_sender, state_receiver) = unbounded_channel();
        self.state_sender = Some(state_sender.clone());
        self.state_receiver = Some(state_receiver);

        // Create Contact URI
        self.contact_uri = Some(rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: Some(rsip::Auth {
                user: self.config.username.clone(),
                password: None,
            }),
            host_with_port: local_addr.into(),
            params: vec![],
            headers: vec![],
        });

        // Start endpoint service
        let cancel_token = self.cancel_token.clone();
        tokio::spawn(async move {
            select! {
                _ = endpoint.serve() => {
                    info!("Endpoint serve finished");
                }
                _ = cancel_token.cancelled() => {
                    info!("Endpoint cancelled");
                }
            }
        });

        // Process incoming transactions
        if let Some(dialog_layer) = &self.dialog_layer {
            let dialog_layer_clone = dialog_layer.clone();
            let state_sender_clone = state_sender.clone();
            let contact_clone = self.contact_uri.clone().unwrap();
            let cancel_token = self.cancel_token.clone();

            tokio::spawn(async move {
                Self::process_incoming_request(
                    dialog_layer_clone,
                    incoming,
                    state_sender_clone,
                    contact_clone,
                    cancel_token,
                )
                .await
            });
        }

        Ok(())
    }

    pub async fn register(&self) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let credential = Credential {
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            realm: Some(self.config.realm.clone()),
        };

        let sip_server = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: None,
            host_with_port: self.config.proxy_addr.into(),
            params: vec![],
            headers: vec![],
        };

        let mut registration = Registration::new(dialog_layer.endpoint.clone(), Some(credential));

        let resp = registration
            .register(sip_server, None)
            .await
            .map_err(|e| e.into_anyhow())?;
        debug!("Register response: {}", resp.to_string());

        if resp.status_code == rsip::StatusCode::OK {
            info!("Registration successful for {}", self.config.username);
            Ok(())
        } else {
            Err(anyhow!("Registration failed: {}", resp.status_code))
        }
    }

    pub async fn make_call(&self, callee: &str) -> Result<DialogId> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        let state_sender = self
            .state_sender
            .as_ref()
            .ok_or_else(|| anyhow!("State sender not available"))?;

        let contact = self
            .contact_uri
            .as_ref()
            .ok_or_else(|| anyhow!("Contact URI not available"))?;

        let credential = Credential {
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            realm: Some(self.config.realm.clone()),
        };

        let callee_uri = format!(
            "sip:{}@{}:{}",
            callee,
            self.config.proxy_addr.ip(),
            self.config.proxy_addr.port()
        )
        .try_into()
        .map_err(|e| anyhow!("Invalid callee URI: {:?}", e))?;

        // Add Route header to force routing through proxy
        let route_header = rsip::Header::Route(
            rsip::typed::Route(rsip::UriWithParamsList(vec![rsip::UriWithParams {
                uri: format!(
                    "sip:{}:{}",
                    self.config.proxy_addr.ip(),
                    self.config.proxy_addr.port()
                )
                .try_into()
                .map_err(|e| anyhow!("Invalid proxy URI: {:?}", e))?,
                params: vec![rsip::Param::Other("lr".into(), None)].into(),
            }]))
            .into(),
        );

        let invite_option = InviteOption {
            callee: callee_uri,
            caller: contact.clone(),
            content_type: None,
            offer: None,
            contact: contact.clone(),
            credential: Some(credential),
            headers: Some(vec![route_header]),
        };

        let (dialog, resp) = dialog_layer
            .do_invite(invite_option, state_sender.clone())
            .await
            .map_err(|e| e.into_anyhow())?;

        let resp = resp.ok_or_else(|| anyhow!("No response"))?;

        if resp.status_code == rsip::StatusCode::OK {
            info!("Call established to {}", callee);
            Ok(dialog.id())
        } else {
            Err(anyhow!("Call failed: {}", resp.status_code))
        }
    }

    pub async fn hangup(&self, dialog_id: &DialogId) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::ClientInvite(d) => {
                    d.bye().await.map_err(|e| e.into_anyhow())?;
                    info!("Call hangup sent for dialog {}", dialog_id);
                    Ok(())
                }
                Dialog::ServerInvite(d) => {
                    d.bye().await.map_err(|e| e.into_anyhow())?;
                    info!("Call hangup sent for dialog {}", dialog_id);
                    Ok(())
                }
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    pub async fn answer_call(&self, dialog_id: &DialogId) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::ServerInvite(d) => {
                    let headers = vec![rsip::typed::ContentType(MediaType::Sdp(vec![])).into()];
                    d.accept(Some(headers), None).map_err(|e| e.into_anyhow())?;
                    info!("Call answered for dialog {}", dialog_id);
                    Ok(())
                }
                _ => Err(anyhow!("Invalid dialog type for answering")),
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    pub async fn reject_call(&self, dialog_id: &DialogId) -> Result<()> {
        let dialog_layer = self
            .dialog_layer
            .as_ref()
            .ok_or_else(|| anyhow!("TestUa not started"))?;

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::ServerInvite(d) => {
                    d.reject().map_err(|e| e.into_anyhow())?;
                    info!("Call rejected for dialog {}", dialog_id);
                    Ok(())
                }
                _ => Err(anyhow!("Invalid dialog type for rejecting")),
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    pub async fn process_dialog_events(&mut self) -> Result<Vec<TestUaEvent>> {
        let mut events = Vec::new();

        if let Some(state_receiver) = &mut self.state_receiver {
            while let Ok(state) = state_receiver.try_recv() {
                match state {
                    DialogState::Calling(id) => {
                        info!("Incoming call dialog {}", id);
                        events.push(TestUaEvent::IncomingCall(id));
                    }
                    DialogState::Early(id, resp) => {
                        info!("Early dialog {} {}", id, resp);
                    }
                    DialogState::Confirmed(id) => {
                        info!("Call established {}", id);
                        events.push(TestUaEvent::CallEstablished(id));
                    }
                    DialogState::Terminated(id, reason) => {
                        info!("Dialog terminated {} {:?}", id, reason);
                        events.push(TestUaEvent::CallTerminated(id.clone()));
                        if let Some(dialog_layer) = &self.dialog_layer {
                            dialog_layer.remove_dialog(&id);
                        }
                    }
                    _ => {
                        info!("Received dialog state: {}", state);
                    }
                }
            }
        }

        Ok(events)
    }

    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    async fn process_incoming_request(
        dialog_layer: Arc<DialogLayer>,
        mut incoming: TransactionReceiver,
        state_sender: DialogStateSender,
        contact: rsip::Uri,
        cancel_token: CancellationToken,
    ) -> Result<()> {
        loop {
            select! {
                tx_opt = incoming.recv() => {
                    if let Some(mut tx) = tx_opt {
                        info!("Received transaction: {:?}", tx.key);

                        match tx.original.to_header()?.tag()?.as_ref() {
                            Some(_) => match dialog_layer.match_dialog(&tx.original) {
                                Some(mut d) => {
                                    tokio::spawn(async move {
                                        if let Err(e) = d.handle(tx).await {
                                            warn!("Error handling dialog transaction: {:?}", e);
                                        }
                                    });
                                    continue;
                                }
                                None => {
                                    info!("Dialog not found: {}", tx.original);
                                    if let Err(e) = tx.reply(rsip::StatusCode::CallTransactionDoesNotExist).await {
                                        warn!("Error replying to transaction: {:?}", e);
                                    }
                                    continue;
                                }
                            },
                            None => {}
                        }

                        // Process new dialog
                        match tx.original.method {
                            rsip::Method::Invite | rsip::Method::Ack => {
                                let mut dialog = match dialog_layer.get_or_create_server_invite(
                                    &tx,
                                    state_sender.clone(),
                                    None,
                                    Some(contact.clone()),
                                ) {
                                    Ok(d) => d,
                                    Err(e) => {
                                        info!("Failed to obtain dialog: {:?}", e);
                                        if let Err(e) = tx.reply(rsip::StatusCode::CallTransactionDoesNotExist).await {
                                            warn!("Error replying to transaction: {:?}", e);
                                        }
                                        continue;
                                    }
                                };
                                tokio::spawn(async move {
                                    if let Err(e) = dialog.handle(tx).await {
                                        warn!("Error handling invite transaction: {:?}", e);
                                    }
                                });
                            }
                            _ => {
                                info!("Received request: {:?}", tx.original.method);
                                if let Err(e) = tx.reply(rsip::StatusCode::OK).await {
                                    warn!("Error replying to transaction: {:?}", e);
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
                _ = cancel_token.cancelled() => {
                    info!("Incoming request processing cancelled");
                    break;
                }
            }
        }
        Ok(())
    }
}
