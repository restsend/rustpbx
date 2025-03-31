use anyhow::{anyhow, Result};
use rsip::{
    self,
    headers::{self, Header},
    message::{Request, SipMessage},
    Uri,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time;
use tracing::{debug, error};
use uuid::Uuid;

use crate::media::stream::{MediaStream, MediaStreamBuilder};
use crate::useragent::config::SipConfig;
use crate::useragent::dialog::{DialogManager, SipDialog};

/// SIP client events
#[derive(Debug, Clone)]
pub enum SipClientEvent {
    /// Successfully connected
    Connected,
    /// Connection disconnected
    Disconnected,
    /// Successfully registered
    Registered,
    /// Registration failed
    RegisterFailed(String),
    /// Call in progress
    CallProgress(String),
    /// Call established
    CallEstablished(String),
    /// Call terminated
    CallTerminated(String),
    /// Incoming call
    IncomingCall(String, String),
    /// Message received
    MessageReceived(String, String),
    /// Error occurred
    Error(String),
}

/// SIP client
pub struct SipClient {
    /// Configuration
    config: SipConfig,
    /// Local address
    local_addr: SocketAddr,
    /// Dialog manager
    dialog_manager: Arc<DialogManager>,
    /// Media stream
    media_stream: Arc<MediaStream>,
    /// UDP socket
    socket: Option<Arc<UdpSocket>>,
    /// Event sender
    event_sender: mpsc::Sender<SipClientEvent>,
    /// Message sender
    message_sender: mpsc::Sender<SipMessage>,
    /// Running state
    running: Arc<Mutex<bool>>,
}

impl SipClient {
    /// Create a new SIP client
    pub fn new(config: SipConfig) -> (Self, mpsc::Receiver<SipClientEvent>) {
        let (event_sender, event_receiver) = mpsc::channel(100);
        let (message_sender, _) = mpsc::channel(100);

        // Create media stream
        let stream_id = format!("sip-{}", Uuid::new_v4());
        let cancel_token = tokio_util::sync::CancellationToken::new();

        let media_stream = Arc::new(
            MediaStreamBuilder::new()
                .with_id(stream_id)
                .cancel_token(cancel_token)
                .build(),
        );

        let client = Self {
            config: config.clone(),
            local_addr: config.local_addr(),
            dialog_manager: Arc::new(DialogManager::new()),
            media_stream,
            socket: None,
            event_sender,
            message_sender,
            running: Arc::new(Mutex::new(false)),
        };

        (client, event_receiver)
    }

    /// Start the SIP client
    pub async fn start(&mut self) -> Result<()> {
        let socket = UdpSocket::bind(self.local_addr).await?;
        self.socket = Some(Arc::new(socket));

        // Start message receiver loop
        self.start_message_receiver().await?;

        // Start media stream
        let media_stream = self.media_stream.clone();
        tokio::spawn(async move {
            if let Err(e) = media_stream.serve().await {
                error!("Media stream error: {}", e);
            }
        });

        let mut running = self.running.lock().await;
        *running = true;

        // Send connection successful event
        self.event_sender.send(SipClientEvent::Connected).await?;

        Ok(())
    }

    /// Stop the SIP client
    pub async fn stop(&self) -> Result<()> {
        let mut running = self.running.lock().await;
        if *running {
            *running = false;

            // Stop media stream
            self.media_stream.stop();

            // Send disconnect event
            self.event_sender.send(SipClientEvent::Disconnected).await?;
        }

        Ok(())
    }

    /// Send a message
    pub async fn send_message(&self, message: SipMessage) -> Result<()> {
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| anyhow!("Socket not initialized"))?;

        let bytes = message.to_string().into_bytes();
        debug!("Sending message: {}", String::from_utf8_lossy(&bytes));

        // Get target address
        let target_addr = if let Some(proxy) = &self.config.proxy {
            // If proxy configured, send to proxy
            proxy.parse::<SocketAddr>()?
        } else {
            // Otherwise send directly to target
            match &message {
                SipMessage::Request(req) => {
                    let uri = req.uri.clone();
                    // Extract host and port from URI manually
                    let uri_str = uri.to_string();

                    // Parse URI string to extract host and port
                    if let Some(host_part) = uri_str.split('@').nth(1) {
                        let host_str = host_part.split(':').next().unwrap_or(host_part);
                        let host_str = host_str.split(';').next().unwrap_or(host_str);
                        let host_str = host_str.split('>').next().unwrap_or(host_str);

                        // Try to find port
                        let port = if let Some(port_part) = host_part.split(':').nth(1) {
                            port_part
                                .split(';')
                                .next()
                                .unwrap_or(port_part)
                                .split('>')
                                .next()
                                .unwrap_or(port_part)
                                .parse::<u16>()
                                .unwrap_or(5060)
                        } else {
                            5060
                        };

                        format!("{}:{}", host_str, port).parse::<SocketAddr>()?
                    } else {
                        return Err(anyhow!("No host in URI"));
                    }
                }
                SipMessage::Response(_) => {
                    return Err(anyhow!("Cannot determine destination for response"));
                }
            }
        };

        socket.send_to(&bytes, target_addr).await?;

        Ok(())
    }

    /// Initiate a SIP call
    pub async fn make_call(&self, target_uri: &str, sdp: Option<&str>) -> Result<String> {
        let dialog = self
            .dialog_manager
            .create_dialog(&self.config.local_uri, target_uri)
            .await?;

        let dialog_id = {
            let mut d = dialog.lock().await;
            let invite = d.create_invite(sdp)?;
            self.send_message(invite).await?;
            d.id.clone()
        };

        // Send call progress event
        self.event_sender
            .send(SipClientEvent::CallProgress(dialog_id.clone()))
            .await?;

        Ok(dialog_id)
    }

    /// Terminate a SIP call
    pub async fn terminate_call(&self, dialog_id: &str) -> Result<()> {
        if let Some(dialog) = self.dialog_manager.find_by_id(dialog_id).await {
            let bye = {
                let mut d = dialog.lock().await;
                d.create_bye()?
            };

            self.send_message(bye).await?;

            // Send call terminated event
            self.event_sender
                .send(SipClientEvent::CallTerminated(dialog_id.to_string()))
                .await?;
        }

        Ok(())
    }

    /// Handle SIP responses
    async fn handle_response(&self, response: &SipMessage) -> Result<()> {
        if let SipMessage::Response(resp) = response {
            // Find CallId from headers
            let call_id = resp.headers.iter().find_map(|h| {
                if let Header::CallId(call_id) = h {
                    Some(call_id.to_string())
                } else {
                    None
                }
            });

            if let Some(call_id) = call_id {
                if let Some(dialog) = self.dialog_manager.find_by_call_id(&call_id).await {
                    let mut d = dialog.lock().await;
                    d.handle_response(response)?;

                    let dialog_id = d.id.clone();
                    match d.state {
                        crate::useragent::dialog::DialogState::Early => {
                            self.event_sender
                                .send(SipClientEvent::CallProgress(dialog_id))
                                .await?;
                        }
                        crate::useragent::dialog::DialogState::Confirmed => {
                            // Send ACK confirmation
                            let ack = d.create_ack()?;
                            drop(d); // Release lock before sending message

                            self.send_message(ack).await?;

                            // Send call established event
                            self.event_sender
                                .send(SipClientEvent::CallEstablished(dialog_id))
                                .await?;
                        }
                        crate::useragent::dialog::DialogState::Terminated => {
                            self.event_sender
                                .send(SipClientEvent::CallTerminated(dialog_id))
                                .await?;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    /// Start the message receiver loop
    async fn start_message_receiver(&self) -> Result<()> {
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| anyhow!("Socket not initialized"))?
            .clone();

        let dialog_manager = self.dialog_manager.clone();
        let event_sender = self.event_sender.clone();
        let running = self.running.clone();
        let message_sender = self.message_sender.clone();

        tokio::spawn(async move {
            let mut buf = vec![0; 4096];
            while *running.lock().await {
                let res = time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await;

                match res {
                    Ok(Ok((len, src))) => {
                        let data = &buf[..len];
                        let msg_str = String::from_utf8_lossy(data);
                        debug!("Received message from {}: {}", src, msg_str);

                        // Parse SIP message
                        match rsip::SipMessage::try_from(msg_str.as_ref()) {
                            Ok(msg) => {
                                // Forward message to handler
                                match msg {
                                    SipMessage::Request(_) => {
                                        // Forward to request handler
                                        if let Err(e) = message_sender.send(msg.clone()).await {
                                            error!("Failed to forward request: {}", e);
                                        }
                                    }
                                    SipMessage::Response(_) => {
                                        // Handle response directly
                                        if let Err(e) = Self::handle_response_static(
                                            &dialog_manager,
                                            &event_sender,
                                            &msg,
                                            &socket,
                                        )
                                        .await
                                        {
                                            error!("Failed to handle response: {}", e);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to parse SIP message: {}", e);
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        error!("Failed to receive: {}", e);
                    }
                    Err(_) => {
                        // Timeout, just continue
                    }
                }
            }
        });

        Ok(())
    }

    /// Static response handler
    async fn handle_response_static(
        dialog_manager: &DialogManager,
        event_sender: &mpsc::Sender<SipClientEvent>,
        response: &SipMessage,
        socket: &UdpSocket,
    ) -> Result<()> {
        if let SipMessage::Response(resp) = response {
            // Find CallId from headers
            let call_id = resp.headers.iter().find_map(|h| {
                if let Header::CallId(call_id) = h {
                    Some(call_id.to_string())
                } else {
                    None
                }
            });

            if let Some(call_id) = call_id {
                if let Some(dialog) = dialog_manager.find_by_call_id(&call_id).await {
                    let mut d = dialog.lock().await;
                    d.handle_response(response)?;

                    let dialog_id = d.id.clone();

                    match d.state {
                        crate::useragent::dialog::DialogState::Early => {
                            event_sender
                                .send(SipClientEvent::CallProgress(dialog_id))
                                .await?;
                        }
                        crate::useragent::dialog::DialogState::Confirmed => {
                            // Send ACK confirmation
                            let ack = d.create_ack()?;
                            drop(d); // Release lock before sending message

                            // Send ACK
                            let bytes = ack.to_string().into_bytes();
                            debug!("Sending ACK: {}", String::from_utf8_lossy(&bytes));

                            // Extract target from response
                            let to_addr = if let Some(via) = resp.headers.iter().find_map(|h| {
                                if let Header::Via(via) = h {
                                    Some(via.to_string())
                                } else {
                                    None
                                }
                            }) {
                                // Parse received/rport from Via header
                                if let Some(received_idx) = via.find("received=") {
                                    let received_start = received_idx + 9;
                                    let received_end = via[received_start..]
                                        .find(&[';', ' '][..])
                                        .map(|idx| received_start + idx)
                                        .unwrap_or_else(|| via.len());

                                    let received = &via[received_start..received_end];

                                    // Find rport
                                    let port = if let Some(rport_idx) = via.find("rport=") {
                                        let rport_start = rport_idx + 6;
                                        let rport_end = via[rport_start..]
                                            .find(&[';', ' '][..])
                                            .map(|idx| rport_start + idx)
                                            .unwrap_or_else(|| via.len());

                                        via[rport_start..rport_end].parse::<u16>().unwrap_or(5060)
                                    } else {
                                        5060
                                    };

                                    format!("{}:{}", received, port)
                                        .parse()
                                        .unwrap_or_else(|_| {
                                            SocketAddr::from(([127, 0, 0, 1], 5060))
                                        })
                                } else {
                                    SocketAddr::from(([127, 0, 0, 1], 5060))
                                }
                            } else {
                                SocketAddr::from(([127, 0, 0, 1], 5060))
                            };

                            if let Err(e) = socket.send_to(&bytes, to_addr).await {
                                error!("Failed to send ACK: {}", e);
                            }

                            // Send call established event
                            event_sender
                                .send(SipClientEvent::CallEstablished(dialog_id))
                                .await?;
                        }
                        crate::useragent::dialog::DialogState::Terminated => {
                            event_sender
                                .send(SipClientEvent::CallTerminated(dialog_id))
                                .await?;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    /// Get the media stream
    pub fn media_stream(&self) -> Arc<MediaStream> {
        self.media_stream.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::useragent::config::SipConfig;
    use std::time::Duration;
    use tokio::time;

    /// Simple SDP generation function
    fn generate_test_sdp() -> String {
        let sdp = r#"v=0
o=user1 53655765 2353687637 IN IP4 127.0.0.1
s=SIP Call
c=IN IP4 127.0.0.1
t=0 0
m=audio 49170 RTP/AVP 0
a=rtpmap:0 PCMU/8000
"#;
        sdp.to_string()
    }

    /// Test SIP client creation and startup
    #[tokio::test]
    async fn test_sip_client_create_and_start() {
        // Create default configuration
        let config = SipConfig::default();

        // Create SIP client
        let (mut client, mut event_receiver) = SipClient::new(config);

        // Start client
        let result = client.start().await;
        assert!(result.is_ok());

        // Wait for connection successful event
        if let Ok(Some(event)) = time::timeout(Duration::from_secs(1), event_receiver.recv()).await
        {
            match event {
                SipClientEvent::Connected => {
                    // Success
                }
                _ => panic!("Unexpected event: {:?}", event),
            }
        } else {
            panic!("No event received");
        }

        // Stop client
        let result = client.stop().await;
        assert!(result.is_ok());

        // Wait for disconnection event
        if let Ok(Some(event)) = time::timeout(Duration::from_secs(1), event_receiver.recv()).await
        {
            match event {
                SipClientEvent::Disconnected => {
                    // Success
                }
                _ => panic!("Unexpected event: {:?}", event),
            }
        } else {
            panic!("No event received");
        }
    }

    /// Test media stream creation
    #[tokio::test]
    async fn test_media_stream_creation() {
        // Create default configuration
        let config = SipConfig::default();

        // Create SIP client
        let (client, _) = SipClient::new(config);

        // Get media stream
        let media_stream = client.media_stream();

        // Verify media stream is created
        let _ = media_stream.subscribe();
        assert!(true);
    }

    /// Test dialog creation
    #[tokio::test]
    async fn test_dialog_creation() {
        // Create dialog manager
        let dialog_manager = DialogManager::new();

        // Create dialog
        let result = dialog_manager
            .create_dialog("sip:alice@example.com", "sip:bob@example.com")
            .await;

        assert!(result.is_ok());

        let dialog = result.unwrap();
        let dialog_id = dialog.lock().await.id.clone();

        // Find dialog by ID
        let found = dialog_manager.find_by_id(&dialog_id).await;
        assert!(found.is_some());

        // Close dialog
        let result = dialog_manager.close_dialog(&dialog_id).await;
        assert!(result.is_ok());

        // Verify dialog was deleted
        let found = dialog_manager.find_by_id(&dialog_id).await;
        assert!(found.is_none());
    }
}
