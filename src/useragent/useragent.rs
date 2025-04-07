use anyhow::{anyhow, Result};
use rsip::message::HeadersExt;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::registration::Registration;
use rsipstack::transaction::Endpoint;
use rsipstack::transaction::TransactionReceiver;
use rsipstack::transport::{udp::UdpConnection, TransportLayer};
use rsipstack::EndpointBuilder;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct UserAgentBuilder {
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub rtp_start_port: u16,
}

pub struct UserAgent {
    pub token: CancellationToken,
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub rtp_start_port: u16,
    pub endpoint: Endpoint,
    pub dialog_layer: Arc<DialogLayer>,
    pub incoming_txs: Mutex<Option<TransactionReceiver>>,
    registration_tx: Mutex<Option<mpsc::Sender<RegistrationCommand>>>,
}

pub struct InviteOptions {
    pub callee: String,
    pub credential: Option<Credential>,
}

pub struct RegisterOptions {
    pub sip_server: String,
    pub credential: Option<Credential>,
}

enum RegistrationCommand {
    Unregister,
}

impl UserAgentBuilder {
    pub fn new() -> Self {
        Self {
            external_ip: None,
            stun_server: None,
            rtp_start_port: 5000,
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

    pub async fn build(&self) -> Result<UserAgent> {
        let token = CancellationToken::new();

        // Get local IP address
        let local_ip = if let Some(ip) = &self.external_ip {
            IpAddr::from_str(ip)?
        } else {
            crate::useragent::stun::get_first_non_loopback_interface()?
        };

        // Create transport layer
        let transport_layer = TransportLayer::new(token.clone());

        // Setup UDP connection
        let local_addr: SocketAddr = format!("{}:5060", local_ip).parse()?;
        let udp_conn = UdpConnection::create_connection(local_addr, None)
            .await
            .map_err(|e| anyhow!("Failed to create UDP connection: {}", e))?;

        transport_layer.add_transport(udp_conn.into());

        // Create SIP endpoint
        let endpoint = EndpointBuilder::new()
            .cancel_token(token.clone())
            .transport_layer(transport_layer)
            .build();

        let incoming_txs = endpoint.incoming_transactions();
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));

        Ok(UserAgent {
            token,
            external_ip: self.external_ip.clone(),
            stun_server: self.stun_server.clone(),
            rtp_start_port: self.rtp_start_port,
            endpoint,
            dialog_layer,
            incoming_txs: Mutex::new(Some(incoming_txs)),
            registration_tx: Mutex::new(None),
        })
    }
}

impl UserAgent {
    pub async fn register(&self, user: String, options: RegisterOptions) -> Result<()> {
        // First perform initial registration
        let mut registration =
            Registration::new(self.endpoint.inner.clone(), options.credential.clone());

        // Register and wait for response
        let resp = registration
            .register(&options.sip_server)
            .await
            .map_err(|e| anyhow!("Registration failed: {}", e))?;
        debug!("Register response: {}", resp);

        if resp.status_code != rsip::StatusCode::OK {
            return Err(anyhow!("Failed to register: {}", resp.status_code));
        }

        info!("Successfully registered user {}", user);

        // Start registration renewal task
        self.start_registration_renewal(user, options).await;

        Ok(())
    }

    // Helper function to send an unregister request (REGISTER with expires=0)
    async fn send_unregister(
        server: &str,
        endpoint_inner: Arc<rsipstack::transaction::endpoint::EndpointInner>,
        credential: Option<Credential>,
    ) -> Result<()> {
        let mut registration = Registration::new(endpoint_inner.clone(), credential);

        // Get the first address for contact
        // Using transport layer's get_addrs method via endpoint
        let first_addr = match endpoint_inner.transport_layer.get_addrs().first() {
            Some(addr) => addr.clone(),
            None => return Err(anyhow!("No local address found for unregistration")),
        };

        // Create the contact header using the URI format
        let uri = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: None,
            host_with_port: first_addr.addr.into(),
            params: vec![],
            headers: vec![],
        };

        // Create a contact with expires=0 parameter
        let contact = rsip::typed::Contact {
            display_name: None,
            uri,
            params: vec![rsip::Param::Expires(
                rsip::common::uri::param::Expires::from("0".to_string()),
            )],
        };

        // Store the contact in the registration object
        registration.contact = Some(contact);

        // Send the unregister request
        let resp = registration
            .register(&server.to_string())
            .await
            .map_err(|e| anyhow!("Unregistration failed: {}", e))?;

        if resp.status_code != rsip::StatusCode::OK {
            return Err(anyhow!("Failed to unregister: {}", resp.status_code));
        }

        Ok(())
    }

    async fn start_registration_renewal(&self, user: String, options: RegisterOptions) {
        // Create command channel
        let (tx, rx) = mpsc::channel::<RegistrationCommand>(1);

        // Store sender in UserAgent
        {
            let mut registration_tx = self.registration_tx.lock().await;
            *registration_tx = Some(tx);
        }

        // Create a token for this registration task
        let token = self.token.child_token();
        let endpoint_inner = self.endpoint.inner.clone();
        let credential = options.credential.clone();
        let sip_server = options.sip_server.clone();

        // Spawn renewal task
        tokio::spawn(async move {
            let mut registration = Registration::new(endpoint_inner.clone(), credential.clone());
            let mut rx = rx;

            // Initial registration is already done - get the expiration time
            let mut expiration_seconds = registration.expires().max(50) as u64;

            loop {
                // Calculate when to send the next registration
                // Send renewal a bit before expiration (80% of expiration time)
                let renewal_delay = Duration::from_secs((expiration_seconds * 80) / 100);

                tokio::select! {
                    _ = sleep(renewal_delay) => {
                        match registration.register(&sip_server).await {
                            Ok(resp) => {
                                if resp.status_code == rsip::StatusCode::OK {
                                    info!("Successfully renewed registration for {}", user);
                                    // Update expiration based on response
                                    expiration_seconds = registration.expires().max(50) as u64;
                                } else {
                                    warn!("Failed to renew registration: {}", resp.status_code);
                                }
                            }
                            Err(e) => {
                                warn!("Registration renewal error: {}", e);
                            }
                        }
                    }
                    _ = token.cancelled() => {
                        info!("Registration renewal task canceled");
                        break;
                    }
                    cmd = rx.recv() => {
                        match cmd {
                            Some(RegistrationCommand::Unregister) => {
                                info!("Unregistering user {}", user);
                                // To unregister in SIP, we send a REGISTER with expires=0
                                match Self::send_unregister(&sip_server, endpoint_inner.clone(),
                                    credential.clone()).await {
                                    Ok(_) => info!("Successfully unregistered user {}", user),
                                    Err(e) => warn!("Unregister failed: {}", e),
                                }
                                break;
                            }
                            None => {
                                debug!("Registration command channel closed");
                                break;
                            }
                        }
                    }
                }
            }

            info!("Registration renewal task for {} completed", user);
        });
    }

    pub async fn unregister(&self, user: String) -> Result<()> {
        info!("Requesting unregistration for user {}", user);

        let mut registration_tx = self.registration_tx.lock().await;
        if let Some(tx) = registration_tx.as_ref() {
            if let Err(e) = tx.send(RegistrationCommand::Unregister).await {
                warn!("Failed to send unregister command: {}", e);
                return Err(anyhow!("Failed to send unregister command: {}", e));
            }
            // Remove the sender to prevent further commands
            *registration_tx = None;
            Ok(())
        } else {
            warn!("No active registration found for {}", user);
            Err(anyhow!("No active registration found"))
        }
    }

    pub async fn invite(&self, callee: String, options: InviteOptions) -> Result<()> {
        // Get the first local address to use for contact
        let first_addr = self
            .endpoint
            .get_addrs()
            .first()
            .ok_or(anyhow!("No local address found"))?
            .clone();

        // Create contact URI
        let contact = rsip::Uri {
            scheme: Some(rsip::Scheme::Sip),
            auth: None, // No auth in contact
            host_with_port: first_addr.addr.into(),
            params: vec![],
            headers: vec![],
        };

        // Convert callee to URI
        let callee_copy = callee.clone();
        let callee_uri: rsip::Uri = callee
            .try_into()
            .map_err(|e| anyhow!("Invalid callee URI: {}", e))?;

        // Create invitation options
        let invite_option = InviteOption {
            callee: callee_uri,
            caller: contact.clone(),
            content_type: None,
            offer: None,
            contact: contact.clone(),
            credential: options.credential,
        };

        // Create dialogue state channel
        let (state_sender, _state_receiver) = tokio::sync::mpsc::unbounded_channel();

        // Send the INVITE
        let (_dialog, resp) = self
            .dialog_layer
            .do_invite(invite_option, state_sender)
            .await
            .map_err(|e| anyhow!("Invite failed: {}", e))?;

        // Check response
        if let Some(resp) = resp {
            debug!("Invite response: {}", resp);

            if resp.status_code != rsip::StatusCode::OK {
                return Err(anyhow!("Failed to invite: {}", resp.status_code));
            }

            info!("Call established with {}", callee_copy);
        } else {
            return Err(anyhow!("No response received"));
        }
        Ok(())
    }

    pub async fn serve(&self) -> Result<()> {
        let mut incoming = self
            .incoming_txs
            .lock()
            .await
            .take()
            .ok_or(anyhow!("TransactionReceiver already taken"))?;

        let dialog_layer = self.dialog_layer.clone();
        let token = self.token.child_token();
        let endpoint_inner = self.endpoint.inner.clone();

        tokio::select! {
            _ = token.cancelled() => {
                debug!("User agent cancelled");
            }
            result = endpoint_inner.serve() => {
                if let Err(e) = result {
                    debug!("Endpoint serve error: {:?}", e);
                }
            }
            _ = async {
                loop {
                    tokio::select! {
                        Some(mut tx) = incoming.recv() => {
                            debug!("Received transaction: {:?}", tx.key);

                            // Check if it's for an existing dialog
                            match tx.original.to_header().ok().and_then(|h| h.tag().ok()).flatten() {
                                Some(_) => match dialog_layer.match_dialog(&tx.original) {
                                    Some(mut d) => {
                                        // Clone the transaction and handle it without spawning a separate task
                                        if let Err(e) = d.handle(tx).await {
                                            debug!("Error handling dialog: {:?}", e);
                                        }
                                    }
                                    None => {
                                        debug!("No dialog found for transaction");
                                        if let Err(e) = tx.reply(rsip::StatusCode::CallTransactionDoesNotExist).await {
                                            debug!("Error replying to transaction: {:?}", e);
                                        }
                                    }
                                },
                                None => {
                                    // This is a new incoming request
                                    // Handle new incoming calls or other requests here
                                    match tx.original.method {
                                        rsip::Method::Invite => {
                                            // Handle incoming INVITE request
                                            debug!("Received new INVITE request");
                                            if let Err(e) = tx.reply(rsip::StatusCode::OK).await {
                                                debug!("Error replying to INVITE: {:?}", e);
                                            }
                                        }
                                        _ => {
                                            // Reply with method not allowed for other methods
                                            if let Err(e) = tx.reply(rsip::StatusCode::MethodNotAllowed).await {
                                                debug!("Error replying to transaction: {:?}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ = token.cancelled() => {
                            debug!("Cancellation requested, stopping transaction processor");
                            break;
                        }
                    }
                }
            } => {}
        }

        Ok(())
    }

    pub fn stop(&self) {
        info!("useragent: stopping");
        self.token.cancel();
    }
}
