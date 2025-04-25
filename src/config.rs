use anyhow::Error;
use clap::Parser;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Cli {
    #[clap(long)]
    pub conf: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub http_addr: String,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    pub console: Option<ConsoleConfig>,
    pub sip: Option<SipConfig>,
    pub proxy: Option<ProxyConfig>,
    pub rtp_start_port: Option<u16>,
    pub external_ip: Option<String>,
    pub stun_server: Option<String>,
    pub recorder_path: String,
    pub media_cache_path: String,
    pub llmproxy: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SipConfig {
    pub addr: String,
    pub udp_port: u16,
    pub external_ip: Option<String>,
    pub useragent: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    pub addr: String,
    pub external_ip: Option<String>,
    pub useragent: Option<String>,
    pub ssl_private_key: Option<String>,
    pub ssl_certificate: Option<String>,
    pub udp_port: Option<u16>,
    pub tcp_port: Option<u16>,
    pub tls_port: Option<u16>,
    pub ws_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct ConsoleConfig {
    pub prefix: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            prefix: "/console".to_string(),
            username: None,
            password: None,
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".to_string(),
            external_ip: None,
            useragent: None,
            ssl_private_key: None,
            ssl_certificate: None,
            udp_port: None,
            tcp_port: None,
            tls_port: None,
            ws_port: None,
        }
    }
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".to_string(),
            udp_port: 5060,
            external_ip: None,
            useragent: Some("rustpbx".to_string()),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: "0.0.0.0:8080".to_string(),
            log_level: None,
            log_file: None,
            console: Some(ConsoleConfig::default()),
            sip: Some(SipConfig::default()),
            proxy: Some(ProxyConfig::default()),
            rtp_start_port: None,
            external_ip: None,
            stun_server: None,
            #[cfg(target_os = "windows")]
            recorder_path: "./recorder".to_string(),
            #[cfg(not(target_os = "windows"))]
            recorder_path: "/tmp/recorder".to_string(),
            #[cfg(target_os = "windows")]
            media_cache_path: "./mediacache".to_string(),
            #[cfg(not(target_os = "windows"))]
            media_cache_path: "/tmp/mediacache".to_string(),
            llmproxy: None,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Error> {
        let config = toml::from_str(&std::fs::read_to_string(path)?)?;
        Ok(config)
    }
}
