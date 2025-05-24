use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::net_tool::sdp_contains_private_ip;
use anyhow::Result;
use async_trait::async_trait;
use rsip::headers::{ContentLength, UntypedHeader};
use rsip::prelude::{HasHeaders, HeadersExt};
use rsipstack::transaction::transaction::Transaction;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};
use tokio::net::UdpSocket;
use tokio::{select, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct MediaSession {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: Option<String>,
    pub start_time: SystemTime,
    pub sdp_offer: Option<String>,
    pub sdp_answer: Option<String>,
    pub recording_enabled: bool,
    pub recording_path: Option<PathBuf>,
    pub caller_rtp_address: Option<SocketAddr>,
    pub callee_rtp_address: Option<SocketAddr>,
    pub proxy_caller_port: Option<u16>,
    pub proxy_callee_port: Option<u16>,
    pub proxy_task: Option<JoinHandle<()>>,
}

impl MediaSession {
    pub fn new(call_id: &str, from_tag: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            from_tag: from_tag.to_string(),
            to_tag: None,
            start_time: SystemTime::now(),
            sdp_offer: None,
            sdp_answer: None,
            recording_enabled: false,
            recording_path: None,
            caller_rtp_address: None,
            callee_rtp_address: None,
            proxy_caller_port: None,
            proxy_callee_port: None,
            proxy_task: None,
        }
    }
}

pub struct RtpPortManager {
    start_port: u16,
    end_port: u16,
    allocated_ports: HashMap<u16, String>, // port -> session_id
}

impl RtpPortManager {
    pub fn new(start_port: u16, end_port: u16) -> Self {
        Self {
            start_port,
            end_port,
            allocated_ports: HashMap::new(),
        }
    }

    pub fn allocate_port(&mut self, session_id: &str) -> Option<u16> {
        for port in (self.start_port..=self.end_port).step_by(2) {
            if !self.allocated_ports.contains_key(&port)
                && !self.allocated_ports.contains_key(&(port + 1))
            {
                self.allocated_ports.insert(port, session_id.to_string());
                self.allocated_ports
                    .insert(port + 1, session_id.to_string());
                return Some(port);
            }
        }
        None
    }

    pub fn deallocate_port(&mut self, port: u16) {
        self.allocated_ports.remove(&port);
        self.allocated_ports.remove(&(port + 1));
    }
}

pub struct RtpProxy {
    local_port: u16,
    remote_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    cancel_token: CancellationToken,
}

impl RtpProxy {
    pub async fn new(
        local_port: u16,
        remote_addr: SocketAddr,
        cancel_token: CancellationToken,
    ) -> Result<Self> {
        let local_addr = SocketAddr::new(IpAddr::from([0, 0, 0, 0]), local_port);
        let socket = UdpSocket::bind(local_addr).await?;

        Ok(Self {
            local_port,
            remote_addr,
            socket: Arc::new(socket),
            cancel_token,
        })
    }

    pub async fn start_forwarding(&self) -> Result<()> {
        let mut buffer = [0u8; 1500];

        loop {
            select! {
                _ = self.cancel_token.cancelled() => {
                    break;
                }
                result = self.socket.recv_from(&mut buffer) => {
                    match result {
                        Ok((len, from_addr)) => {
                            // Forward the packet
                            if let Err(e) = self.socket.send_to(&buffer[..len], self.remote_addr).await {
                                warn!("Failed to forward RTP packet: {}", e);
                            } else {
                                debug!("Forwarded RTP packet from {} to {} ({} bytes)",
                                      from_addr, self.remote_addr, len);
                            }
                        }
                        Err(e) => {
                            error!("Failed to receive RTP packet: {}", e);
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct MediaProxyModule {
    config: Arc<ProxyConfig>,
    sessions: Arc<Mutex<HashMap<String, MediaSession>>>,
    port_manager: Arc<Mutex<RtpPortManager>>,
}

impl MediaProxyModule {
    pub fn create(_server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = MediaProxyModule::new(config);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>) -> Self {
        let media_config = &config.media_proxy;
        let start_port = media_config.rtp_start_port.unwrap_or(20000);
        let end_port = media_config.rtp_end_port.unwrap_or(30000);

        Self {
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            port_manager: Arc::new(Mutex::new(RtpPortManager::new(start_port, end_port))),
        }
    }

    fn should_proxy_media(&self, sdp_offer: Option<&str>, sdp_answer: Option<&str>) -> bool {
        let media_config = &self.config.media_proxy;

        match media_config.mode {
            MediaProxyMode::None => false,
            MediaProxyMode::All => true,
            MediaProxyMode::NatOnly => {
                // Check if either SDP contains private IP addresses
                let offer_has_private = sdp_offer
                    .map(|sdp| sdp_contains_private_ip(sdp).unwrap_or(false))
                    .unwrap_or(false);

                let answer_has_private = sdp_answer
                    .map(|sdp| sdp_contains_private_ip(sdp).unwrap_or(false))
                    .unwrap_or(false);

                offer_has_private || answer_has_private
            }
        }
    }

    /// Replace RTP addresses in SDP with proxy addresses
    fn modify_sdp_for_proxy(&self, sdp: &str, proxy_ip: IpAddr, proxy_port: u16) -> Result<String> {
        let mut modified_sdp = String::new();
        let proxy_port_str = proxy_port.to_string();

        for line in sdp.lines() {
            if line.starts_with("c=") {
                // Replace connection information
                let parts: Vec<&str> = line.splitn(4, ' ').collect();
                if parts.len() >= 3 {
                    modified_sdp.push_str(&format!(
                        "c={} {} {}\r\n",
                        parts[0][2..].to_string(),
                        parts[1],
                        proxy_ip
                    ));
                } else {
                    modified_sdp.push_str(line);
                    modified_sdp.push_str("\r\n");
                }
            } else if line.starts_with("m=") {
                // Replace media port
                let parts: Vec<&str> = line.split(' ').collect();
                if parts.len() >= 2 {
                    let mut new_parts = parts.clone();
                    new_parts[1] = &proxy_port_str;
                    modified_sdp.push_str(&new_parts.join(" "));
                    modified_sdp.push_str("\r\n");
                } else {
                    modified_sdp.push_str(line);
                    modified_sdp.push_str("\r\n");
                }
            } else {
                modified_sdp.push_str(line);
                modified_sdp.push_str("\r\n");
            }
        }

        Ok(modified_sdp)
    }

    /// Extract RTP address and port from SDP
    fn extract_rtp_endpoint_from_sdp(&self, sdp: &str) -> Option<SocketAddr> {
        let mut ip_addr = None;
        let mut port = None;

        for line in sdp.lines() {
            if line.starts_with("c=") {
                let parts: Vec<&str> = line.splitn(4, ' ').collect();
                if parts.len() >= 3 {
                    if let Ok(addr) = parts[2].parse::<IpAddr>() {
                        ip_addr = Some(addr);
                    }
                }
            } else if line.starts_with("m=audio") {
                let parts: Vec<&str> = line.split(' ').collect();
                if parts.len() >= 2 {
                    if let Ok(p) = parts[1].parse::<u16>() {
                        port = Some(p);
                    }
                }
            }
        }

        if let (Some(ip), Some(p)) = (ip_addr, port) {
            Some(SocketAddr::new(ip, p))
        } else {
            None
        }
    }

    async fn start_media_proxy(&self, session_id: &str) -> Result<()> {
        let session_data = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(session_id)
                .map(|s| (s.caller_rtp_address, s.callee_rtp_address))
        };

        if let Some((Some(caller_addr), Some(callee_addr))) = session_data {
            // Allocate ports for this session
            let (caller_port, callee_port) = {
                let mut port_manager = self.port_manager.lock().unwrap();
                let caller_port = port_manager.allocate_port(session_id);
                let callee_port = port_manager.allocate_port(session_id);
                (caller_port, callee_port)
            };

            if let (Some(caller_port), Some(callee_port)) = (caller_port, callee_port) {
                // Update session with allocated ports
                {
                    let mut sessions = self.sessions.lock().unwrap();
                    if let Some(session) = sessions.get_mut(session_id) {
                        session.proxy_caller_port = Some(caller_port);
                        session.proxy_callee_port = Some(callee_port);
                    }
                }

                // Create RTP proxies
                let cancel_token = CancellationToken::new();

                let caller_proxy =
                    RtpProxy::new(caller_port, callee_addr, cancel_token.clone()).await?;

                let callee_proxy =
                    RtpProxy::new(callee_port, caller_addr, cancel_token.clone()).await?;

                // Start forwarding tasks
                let caller_task = tokio::spawn(async move {
                    if let Err(e) = caller_proxy.start_forwarding().await {
                        error!("Caller RTP proxy error: {}", e);
                    }
                });

                let _callee_task = tokio::spawn(async move {
                    if let Err(e) = callee_proxy.start_forwarding().await {
                        error!("Callee RTP proxy error: {}", e);
                    }
                });

                // Store the task handle
                {
                    let mut sessions = self.sessions.lock().unwrap();
                    if let Some(session) = sessions.get_mut(session_id) {
                        session.proxy_task = Some(caller_task);
                    }
                }

                info!(
                    "Started media proxy for session {}: caller_port={}, callee_port={}",
                    session_id, caller_port, callee_port
                );
            } else {
                warn!("Failed to allocate RTP ports for session {}", session_id);
            }
        }
        Ok(())
    }

    fn extract_call_id_and_tags(
        &self,
        tx: &Transaction,
    ) -> Option<(String, String, Option<String>)> {
        let call_id = tx.original.call_id_header().ok()?.value().to_string();
        let from_tag = tx.original.from_header().ok()?.tag().ok()??;
        let to_tag = tx
            .original
            .to_header()
            .ok()
            .and_then(|h| h.tag().ok().flatten())
            .map(|t| t.to_string());

        Some((call_id, from_tag.to_string(), to_tag))
    }

    fn extract_sdp_from_transaction(&self, tx: &Transaction) -> Option<String> {
        if !tx.original.body.is_empty() {
            if let Ok(sdp) = String::from_utf8(tx.original.body.clone()) {
                Some(sdp)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn extract_sdp_from_response(&self, response: &rsip::Response) -> Option<String> {
        if !response.body.is_empty() {
            if let Ok(sdp) = String::from_utf8(response.body.clone()) {
                Some(sdp)
            } else {
                None
            }
        } else {
            None
        }
    }

    async fn handle_invite(&self, tx: &Transaction) -> Result<()> {
        if let Some((call_id, from_tag, to_tag)) = self.extract_call_id_and_tags(tx) {
            let session_key = format!("{}:{}", call_id, from_tag);
            let sdp_offer = self.extract_sdp_from_transaction(tx);

            // Extract caller's RTP endpoint from SDP
            let caller_rtp_address = sdp_offer
                .as_ref()
                .and_then(|sdp| self.extract_rtp_endpoint_from_sdp(sdp));

            let mut sessions = self.sessions.lock().unwrap();
            let session = sessions.entry(session_key.clone()).or_insert_with(|| {
                let mut session = MediaSession::new(&call_id, &from_tag);
                session.to_tag = to_tag;
                session
            });

            session.sdp_offer = sdp_offer;
            session.caller_rtp_address = caller_rtp_address;

            info!(
                "MediaProxy: Processed INVITE for session {} with caller RTP: {:?}",
                session_key, caller_rtp_address
            );
        }

        Ok(())
    }

    async fn handle_response(&self, tx: &Transaction) -> Result<()> {
        // Note: In a real implementation, we'd need to intercept SIP responses
        // This is a simplified version for demonstration
        if let Some((call_id, from_tag, _to_tag)) = self.extract_call_id_and_tags(tx) {
            let session_key = format!("{}:{}", call_id, from_tag);
            let sdp_answer = self.extract_sdp_from_transaction(tx);

            // Extract callee's RTP endpoint from SDP
            let callee_rtp_address = sdp_answer
                .as_ref()
                .and_then(|sdp| self.extract_rtp_endpoint_from_sdp(sdp));

            let should_proxy = {
                let mut sessions = self.sessions.lock().unwrap();
                if let Some(session) = sessions.get_mut(&session_key) {
                    session.sdp_answer = sdp_answer.clone();
                    session.callee_rtp_address = callee_rtp_address;
                    self.should_proxy_media(session.sdp_offer.as_deref(), sdp_answer.as_deref())
                } else {
                    false
                }
            };

            if should_proxy {
                info!(
                    "MediaProxy: Starting media proxy for session {} with callee RTP: {:?}",
                    session_key, callee_rtp_address
                );
                self.start_media_proxy(&session_key).await?;
            }
        }

        Ok(())
    }

    async fn handle_bye(&self, tx: &Transaction) -> Result<()> {
        if let Some((call_id, from_tag, _)) = self.extract_call_id_and_tags(tx) {
            let session_key = format!("{}:{}", call_id, from_tag);

            let mut sessions = self.sessions.lock().unwrap();
            if let Some(session) = sessions.remove(&session_key) {
                // Stop the proxy task
                if let Some(task) = session.proxy_task {
                    task.abort();
                }

                // Deallocate ports
                if let Some(port) = session.proxy_caller_port {
                    self.port_manager.lock().unwrap().deallocate_port(port);
                }
                if let Some(port) = session.proxy_callee_port {
                    self.port_manager.lock().unwrap().deallocate_port(port);
                }

                info!("MediaProxy: Cleaned up session {}", session_key);
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ProxyModule for MediaProxyModule {
    fn name(&self) -> &str {
        "mediaproxy"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Ack,
            rsip::Method::Bye,
            rsip::Method::Cancel,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        info!(
            "MediaProxyModule started with mode: {:?}",
            self.config.media_proxy.mode
        );
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        // Clean up all sessions
        let mut sessions = self.sessions.lock().unwrap();
        for (session_id, session) in sessions.drain() {
            if let Some(task) = session.proxy_task {
                task.abort();
            }
            info!("MediaProxy: Cleaned up session {} on shutdown", session_id);
        }

        info!("MediaProxyModule stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx).await {
                    error!("MediaProxy: Error handling INVITE: {}", e);
                }

                // Check if we need to modify SDP in the INVITE
                if let Some(sdp) = self.extract_sdp_from_transaction(tx) {
                    if let Some((call_id, from_tag, _)) = self.extract_call_id_and_tags(tx) {
                        let session_key = format!("{}:{}", call_id, from_tag);

                        if self.should_proxy_media(Some(&sdp), None) {
                            // Allocate a port for the caller
                            let caller_port = {
                                let mut port_manager = self.port_manager.lock().unwrap();
                                port_manager.allocate_port(&session_key)
                            };

                            if let Some(caller_port) = caller_port {
                                // Get external IP for proxy
                                let external_ip = self
                                    .config
                                    .external_ip
                                    .as_ref()
                                    .and_then(|ip| ip.parse().ok())
                                    .unwrap_or_else(|| {
                                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
                                    });

                                // Modify SDP to use proxy address
                                if let Ok(modified_sdp) =
                                    self.modify_sdp_for_proxy(&sdp, external_ip, caller_port)
                                {
                                    tx.original.body = modified_sdp.into_bytes();

                                    // Update Content-Length header
                                    let content_length = tx.original.body.len() as u32;
                                    tx.original.headers_mut().unique_push(
                                        rsip::Header::ContentLength(ContentLength::from(
                                            content_length,
                                        )),
                                    );

                                    info!("MediaProxy: Modified INVITE SDP for session {} with proxy port {}", 
                                          session_key, caller_port);
                                }

                                // Update session with allocated port
                                {
                                    let mut sessions = self.sessions.lock().unwrap();
                                    if let Some(session) = sessions.get_mut(&session_key) {
                                        session.proxy_caller_port = Some(caller_port);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            rsip::Method::Bye => {
                if let Err(e) = self.handle_bye(tx).await {
                    error!("MediaProxy: Error handling BYE: {}", e);
                }
            }
            _ => {}
        }

        Ok(ProxyAction::Continue)
    }

    async fn on_transaction_end(&self, tx: &mut Transaction) -> Result<()> {
        // Handle responses, especially 200 OK responses to INVITE
        if let Some(response) = &tx.last_response {
            if *response.status_code() == rsip::StatusCode::OK
                && tx.original.method == rsip::Method::Invite
            {
                if let Some(sdp) = self.extract_sdp_from_response(response) {
                    if let Some((call_id, from_tag, _)) = self.extract_call_id_and_tags(tx) {
                        let session_key = format!("{}:{}", call_id, from_tag);

                        let should_proxy = {
                            let sessions = self.sessions.lock().unwrap();
                            if let Some(session) = sessions.get(&session_key) {
                                self.should_proxy_media(session.sdp_offer.as_deref(), Some(&sdp))
                            } else {
                                false
                            }
                        };

                        if should_proxy {
                            // Allocate a port for the callee
                            let callee_port = {
                                let mut port_manager = self.port_manager.lock().unwrap();
                                port_manager.allocate_port(&session_key)
                            };

                            if let Some(callee_port) = callee_port {
                                // Note: We cannot modify the response here as it's immutable
                                // In a real implementation, this would need to be handled differently
                                // For now, we'll log what would be done
                                info!("MediaProxy: Would modify 200 OK SDP for session {} with proxy port {}", 
                                      session_key, callee_port);

                                // Update session with allocated port and start proxy
                                {
                                    let mut sessions = self.sessions.lock().unwrap();
                                    if let Some(session) = sessions.get_mut(&session_key) {
                                        session.proxy_callee_port = Some(callee_port);
                                        session.callee_rtp_address =
                                            self.extract_rtp_endpoint_from_sdp(&sdp);
                                        session.sdp_answer = Some(sdp);
                                    }
                                }

                                // Start the media proxy
                                if let Err(e) = self.start_media_proxy(&session_key).await {
                                    error!("MediaProxy: Failed to start media proxy: {}", e);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MediaProxyConfig, MediaProxyMode};
    use std::net::{IpAddr, Ipv4Addr};

    fn create_test_config(mode: MediaProxyMode) -> Arc<ProxyConfig> {
        let mut config = ProxyConfig::default();
        config.media_proxy = MediaProxyConfig {
            mode,
            rtp_start_port: Some(20000),
            rtp_end_port: Some(30000),
            external_ip: Some("192.168.1.1".to_string()),
            force_proxy: None,
        };
        Arc::new(config)
    }

    #[test]
    fn test_should_proxy_media_none_mode() {
        let config = create_test_config(MediaProxyMode::None);
        let module = MediaProxyModule::new(config);

        let private_sdp =
            "v=0\r\no=- 1234567890 1234567890 IN IP4 192.168.1.100\r\nc=IN IP4 192.168.1.100\r\n";
        let public_sdp = "v=0\r\no=- 1234567890 1234567890 IN IP4 8.8.8.8\r\nc=IN IP4 8.8.8.8\r\n";

        assert!(!module.should_proxy_media(Some(private_sdp), Some(public_sdp)));
        assert!(!module.should_proxy_media(Some(public_sdp), Some(private_sdp)));
    }

    #[test]
    fn test_should_proxy_media_all_mode() {
        let config = create_test_config(MediaProxyMode::All);
        let module = MediaProxyModule::new(config);

        let private_sdp =
            "v=0\r\no=- 1234567890 1234567890 IN IP4 192.168.1.100\r\nc=IN IP4 192.168.1.100\r\n";
        let public_sdp = "v=0\r\no=- 1234567890 1234567890 IN IP4 8.8.8.8\r\nc=IN IP4 8.8.8.8\r\n";

        assert!(module.should_proxy_media(Some(private_sdp), Some(public_sdp)));
        assert!(module.should_proxy_media(Some(public_sdp), Some(private_sdp)));
        assert!(module.should_proxy_media(Some(public_sdp), Some(public_sdp)));
    }

    #[test]
    fn test_should_proxy_media_nat_only_mode() {
        let config = create_test_config(MediaProxyMode::NatOnly);
        let module = MediaProxyModule::new(config);

        let private_sdp =
            "v=0\r\no=- 1234567890 1234567890 IN IP4 192.168.1.100\r\nc=IN IP4 192.168.1.100\r\n";
        let public_sdp = "v=0\r\no=- 1234567890 1234567890 IN IP4 8.8.8.8\r\nc=IN IP4 8.8.8.8\r\n";

        assert!(module.should_proxy_media(Some(private_sdp), Some(public_sdp)));
        assert!(module.should_proxy_media(Some(public_sdp), Some(private_sdp)));
        assert!(!module.should_proxy_media(Some(public_sdp), Some(public_sdp)));
    }

    #[test]
    fn test_extract_rtp_endpoint_from_sdp() {
        let config = create_test_config(MediaProxyMode::None);
        let module = MediaProxyModule::new(config);

        let sdp = "v=0\r\n\
                   o=- 1234567890 1234567890 IN IP4 192.168.1.100\r\n\
                   s=Call\r\n\
                   c=IN IP4 192.168.1.100\r\n\
                   t=0 0\r\n\
                   m=audio 49170 RTP/AVP 0 8 97\r\n";

        let endpoint = module.extract_rtp_endpoint_from_sdp(sdp);
        assert!(endpoint.is_some());

        let endpoint = endpoint.unwrap();
        assert_eq!(endpoint.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)));
        assert_eq!(endpoint.port(), 49170);
    }

    #[test]
    fn test_modify_sdp_for_proxy() {
        let config = create_test_config(MediaProxyMode::All);
        let module = MediaProxyModule::new(config);

        let original_sdp = "v=0\r\n\
                           o=- 1234567890 1234567890 IN IP4 192.168.1.100\r\n\
                           s=Call\r\n\
                           c=IN IP4 192.168.1.100\r\n\
                           t=0 0\r\n\
                           m=audio 49170 RTP/AVP 0 8 97\r\n";

        let proxy_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let proxy_port = 20000;

        let modified_sdp = module.modify_sdp_for_proxy(original_sdp, proxy_ip, proxy_port);
        assert!(modified_sdp.is_ok());

        let modified_sdp = modified_sdp.unwrap();
        assert!(modified_sdp.contains("c=IN IP4 10.0.0.1"));
        assert!(modified_sdp.contains("m=audio 20000 RTP/AVP 0 8 97"));
    }

    #[test]
    fn test_port_manager() {
        let mut manager = RtpPortManager::new(20000, 20010);

        // Allocate first port pair
        let port1 = manager.allocate_port("session1");
        assert_eq!(port1, Some(20000));

        // Allocate second port pair
        let port2 = manager.allocate_port("session2");
        assert_eq!(port2, Some(20002));

        // Deallocate first port pair
        manager.deallocate_port(20000);

        // Should be able to allocate first port pair again
        let port3 = manager.allocate_port("session3");
        assert_eq!(port3, Some(20000));
    }
}
