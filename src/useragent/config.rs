use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// SIP UserAgent Configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipConfig {
    /// Local SIP address, example: sip:user@domain.com
    pub local_uri: String,
    /// Local SIP port, default is 5060
    pub local_port: u16,
    /// Local IP address, default is 127.0.0.1
    pub local_ip: String,
    /// Transport protocol, default is UDP
    pub transport: String,
    /// Registrar server address
    pub registrar: Option<String>,
    /// Proxy server address
    pub proxy: Option<String>,
    /// Username
    pub username: Option<String>,
    /// Password
    pub password: Option<String>,
    /// Display name
    pub display_name: Option<String>,
    /// User agent
    pub user_agent: Option<String>,
    /// Registration expiration (seconds)
    pub register_expires: Option<u32>,
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            local_uri: "sip:user@127.0.0.1".to_string(),
            local_port: 5060,
            local_ip: "127.0.0.1".to_string(),
            transport: "UDP".to_string(),
            registrar: None,
            proxy: None,
            username: None,
            password: None,
            display_name: None,
            user_agent: Some("RustPBX/0.1.0".to_string()),
            register_expires: Some(3600),
        }
    }
}

impl SipConfig {
    /// Get local address
    pub fn local_addr(&self) -> SocketAddr {
        format!("{}:{}", self.local_ip, self.local_port)
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:5060".parse().unwrap())
    }
}
