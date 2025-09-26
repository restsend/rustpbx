use crate::{
    call::user::SipUser,
    proxy::routing::{DefaultRoute, RouteRule, TrunkConfig},
    useragent::RegisterOption,
};
use anyhow::{Error, Result};
use clap::Parser;
use rsip::StatusCode;
use rsipstack::dialog::invitation::InviteOption;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Cli {
    #[clap(long, default_value = "rustpbx.toml")]
    pub conf: Option<String>,
}

fn default_config_recorder_path() -> String {
    #[cfg(target_os = "windows")]
    return "./recorder".to_string();
    #[cfg(not(target_os = "windows"))]
    return "/tmp/recorder".to_string();
}
fn default_config_media_cache_path() -> String {
    #[cfg(target_os = "windows")]
    return "./mediacache".to_string();
    #[cfg(not(target_os = "windows"))]
    return "/tmp/mediacache".to_string();
}
fn default_config_http_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_config_rtp_start_port() -> Option<u16> {
    Some(12000)
}

fn default_config_rtp_end_port() -> Option<u16> {
    Some(42000)
}

fn default_useragent() -> Option<String> {
    Some(crate::version::get_useragent())
}

fn default_callid_suffix() -> Option<String> {
    Some("rustpbx.com".to_string())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_config_http_addr")]
    pub http_addr: String,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    pub ua: Option<UseragentConfig>,
    pub proxy: Option<ProxyConfig>,

    pub external_ip: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,

    #[serde(default = "default_config_recorder_path")]
    pub recorder_path: String,
    pub callrecord: Option<CallRecordConfig>,
    #[serde(default = "default_config_media_cache_path")]
    pub media_cache_path: String,
    pub llmproxy: Option<String>,
    pub restsend_token: Option<String>,
    pub ice_servers: Option<Vec<IceServer>>,
    pub ami: Option<AmiConfig>,
}

#[derive(Default, Debug, Serialize, Deserialize, Clone)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct UseragentConfig {
    pub addr: String,
    pub udp_port: u16,
    #[serde(default = "default_useragent")]
    pub useragent: Option<String>,
    #[serde(default = "default_callid_suffix")]
    pub callid_suffix: Option<String>,
    pub register_users: Option<Vec<RegisterOption>>,
    pub graceful_shutdown: Option<bool>,
    pub handler: Option<InviteHandlerConfig>,
    pub accept_timeout: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "type")]
pub enum InviteHandlerConfig {
    Webhook {
        url: String,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum UserBackendConfig {
    Memory {
        users: Option<Vec<SipUser>>,
    },
    Http {
        url: String,
        method: Option<String>,
        username_field: Option<String>,
        realm_field: Option<String>,
        headers: Option<HashMap<String, String>>,
    },
    Plain {
        path: String,
    },
    Database {
        url: String,
        table_name: Option<String>,
        id_column: Option<String>,
        username_column: Option<String>,
        password_column: Option<String>,
        enabled_column: Option<String>,
        realm_column: Option<String>,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum LocatorConfig {
    Memory,
    Http {
        url: String,
        method: Option<String>,
        username_field: Option<String>,
        expires_field: Option<String>,
        realm_field: Option<String>,
        headers: Option<HashMap<String, String>>,
    },
    Database {
        url: String,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum S3Vendor {
    Aliyun,
    Tencent,
    Minio,
    AWS,
    GCP,
    Azure,
    DigitalOcean,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum CallRecordConfig {
    Local {
        root: String,
    },
    S3 {
        vendor: S3Vendor,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        endpoint: String,
        root: String,
        with_media: Option<bool>,
        keep_media_copy: Option<bool>,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
        with_media: Option<bool>,
        keep_media_copy: Option<bool>,
    },
}

#[derive(Debug, Deserialize, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(PartialEq)]
pub enum MediaProxyMode {
    /// All media goes through proxy
    All,
    /// Auto detect if media proxy is needed (webrtc to rtp)
    Auto,
    /// Only handle NAT (private IP addresses)
    Nat,
    /// Do not handle media proxy
    None,
}

impl Default for MediaProxyMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct RtpConfig {
    pub external_ip: Option<String>,
    pub start_port: Option<u16>,
    pub end_port: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub modules: Option<Vec<String>>,
    pub addr: String,
    #[serde(default = "default_useragent")]
    pub useragent: Option<String>,
    #[serde(default = "default_callid_suffix")]
    pub callid_suffix: Option<String>,
    pub ssl_private_key: Option<String>,
    pub ssl_certificate: Option<String>,
    pub udp_port: Option<u16>,
    pub tcp_port: Option<u16>,
    pub tls_port: Option<u16>,
    pub ws_port: Option<u16>,
    pub acl_rules: Option<Vec<String>>,
    pub ua_white_list: Option<Vec<String>>,
    pub ua_black_list: Option<Vec<String>>,
    pub max_concurrency: Option<usize>,
    pub registrar_expires: Option<u32>,
    #[serde(default)]
    pub user_backend: UserBackendConfig,
    #[serde(default)]
    pub locator: LocatorConfig,
    #[serde(default)]
    pub media_proxy: MediaProxyMode,
    #[serde(default)]
    pub realms: Option<Vec<String>>,
    pub ws_handler: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<RouteRule>>,
    #[serde(default)]
    pub trunks: HashMap<String, TrunkConfig>,
    #[serde(default)]
    pub default: Option<DefaultRoute>,
    pub rtp_config: Option<RtpConfig>,
}

pub enum RouteResult {
    Forward(InviteOption),
    NotHandled(InviteOption),
    Abort(StatusCode, Option<String>),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AmiConfig {
    pub allows: Option<Vec<String>>,
}

impl AmiConfig {
    pub fn is_allowed(&self, addr: &str) -> bool {
        if let Some(allows) = &self.allows {
            allows.iter().any(|a| a == addr || a == "*")
        } else {
            false
        }
    }
}

impl Default for AmiConfig {
    fn default() -> Self {
        Self {
            allows: Some(vec!["127.0.0.1".to_string(), "::1".to_string()]), // Default to allow localhost
        }
    }
}

impl ProxyConfig {
    pub fn normalize_realm(realm: &str) -> &str {
        if realm.is_empty() || realm == "*" || realm == "127.0.0.1" || realm == "::1" {
            "localhost"
        } else {
            realm
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            acl_rules: Some(vec!["allow all".to_string(), "deny all".to_string()]),
            ua_white_list: Some(vec![]),
            ua_black_list: Some(vec![]),
            addr: "0.0.0.0".to_string(),
            modules: Some(vec![
                "acl".to_string(),
                "auth".to_string(),
                "registrar".to_string(),
                "call".to_string(),
            ]),
            useragent: default_useragent(),
            callid_suffix: default_callid_suffix(),
            ssl_private_key: None,
            ssl_certificate: None,
            udp_port: Some(5060),
            tcp_port: None,
            tls_port: None,
            ws_port: None,
            max_concurrency: None,
            registrar_expires: Some(60),
            user_backend: UserBackendConfig::default(),
            locator: LocatorConfig::default(),
            media_proxy: MediaProxyMode::default(),
            realms: Some(vec![]),
            ws_handler: None,
            routes: None,
            trunks: HashMap::new(),
            default: None,
            rtp_config: None,
        }
    }
}

impl Default for UserBackendConfig {
    fn default() -> Self {
        Self::Memory { users: None }
    }
}

impl Default for LocatorConfig {
    fn default() -> Self {
        Self::Memory
    }
}

impl Default for UseragentConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0".to_string(),
            udp_port: 25060,
            useragent: default_useragent(),
            callid_suffix: default_callid_suffix(),
            register_users: None,
            graceful_shutdown: Some(true),
            handler: None,
            accept_timeout: Some("50s".to_string()),
        }
    }
}

impl Default for CallRecordConfig {
    fn default() -> Self {
        Self::Local {
            #[cfg(target_os = "windows")]
            root: "./cdr".to_string(),
            #[cfg(not(target_os = "windows"))]
            root: "/tmp/cdr".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: default_config_http_addr(),
            log_level: None,
            log_file: None,
            ua: Some(UseragentConfig::default()),
            proxy: None,
            recorder_path: default_config_recorder_path(),
            media_cache_path: default_config_media_cache_path(),
            callrecord: None,
            llmproxy: None,
            restsend_token: None,
            ice_servers: None,
            ami: Some(AmiConfig::default()),
            external_ip: None,
            rtp_start_port: default_config_rtp_start_port(),
            rtp_end_port: default_config_rtp_end_port(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Error> {
        let config = toml::from_str(
            &std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {}", e, path))?,
        )?;
        Ok(config)
    }

    pub fn rtp_config(&self) -> RtpConfig {
        RtpConfig {
            external_ip: self.external_ip.clone(),
            start_port: self.rtp_start_port.clone(),
            end_port: self.rtp_end_port.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dump() {
        let mut config = Config::default();
        let mut prxconfig = ProxyConfig::default();
        let mut trunks = HashMap::new();
        let mut routes = Vec::new();
        let mut ice_servers = Vec::new();
        ice_servers.push(IceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            username: Some("user".to_string()),
            ..Default::default()
        });
        ice_servers.push(IceServer {
            urls: vec![
                "stun:restsend.com:3478".to_string(),
                "turn:stun.l.google.com:1112?transport=TCP".to_string(),
            ],
            username: Some("user".to_string()),
            ..Default::default()
        });

        routes.push(crate::proxy::routing::RouteRule {
            name: "default".to_string(),
            description: None,
            priority: 1,
            match_conditions: crate::proxy::routing::MatchConditions {
                to_user: Some("xx".to_string()),
                ..Default::default()
            },
            rewrite: Some(crate::proxy::routing::RewriteRules {
                to_user: Some("xx".to_string()),
                ..Default::default()
            }),
            action: crate::proxy::routing::RouteAction::default(),
            disabled: None,
        });
        routes.push(crate::proxy::routing::RouteRule {
            name: "default3".to_string(),
            description: None,
            priority: 1,
            match_conditions: crate::proxy::routing::MatchConditions {
                to_user: Some("xx3".to_string()),
                ..Default::default()
            },
            rewrite: Some(crate::proxy::routing::RewriteRules {
                to_user: Some("xx3".to_string()),
                ..Default::default()
            }),
            action: crate::proxy::routing::RouteAction::default(),
            disabled: None,
        });
        prxconfig.routes = Some(routes);
        trunks.insert(
            "hello".to_string(),
            crate::proxy::routing::TrunkConfig {
                dest: "sip:127.0.0.1:5060".to_string(),
                ..Default::default()
            },
        );
        prxconfig.trunks = trunks;
        config.proxy = Some(prxconfig);
        config.ice_servers = Some(ice_servers);
        let config_str = toml::to_string(&config).unwrap();
        println!("{}", config_str);
    }
}
