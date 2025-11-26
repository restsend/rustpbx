use crate::{
    call::{CallRecordingConfig, DialDirection, DialplanIvrConfig, QueuePlan, user::SipUser},
    proxy::routing::{RouteQueueConfig, RouteRule, TrunkConfig},
    useragent::RegisterOption,
};
use anyhow::{Error, Result};
use clap::Parser;
use rsip::StatusCode;
use rsipstack::dialog::invitation::InviteOption;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};
use voice_engine::{IceServer, media::recorder::RecorderFormat};

#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Cli {
    #[clap(long, default_value = "rustpbx.toml")]
    pub conf: Option<String>,
}

pub(crate) fn default_config_recorder_path() -> String {
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

fn default_database_url() -> String {
    "sqlite://rustpbx.sqlite3".to_string()
}

fn default_console_session_secret() -> String {
    rsipstack::transaction::random_text(32)
}

fn default_console_base_path() -> String {
    "/console".to_string()
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

fn default_user_backends() -> Vec<UserBackendConfig> {
    vec![UserBackendConfig::default()]
}

fn default_generated_config_dir() -> String {
    "./config".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecordingDirection {
    Inbound,
    Outbound,
    Internal,
}

impl RecordingDirection {
    pub fn matches(&self, direction: &DialDirection) -> bool {
        match (self, direction) {
            (RecordingDirection::Inbound, DialDirection::Inbound) => true,
            (RecordingDirection::Outbound, DialDirection::Outbound) => true,
            (RecordingDirection::Internal, DialDirection::Internal) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct RecordingPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directions: Vec<RecordingDirection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller_allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller_deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callee_allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub callee_deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_start: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename_pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samplerate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptime: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<RecorderFormat>,
}

impl RecordingPolicy {
    pub fn new_recording_config(&self) -> CallRecordingConfig {
        crate::call::CallRecordingConfig {
            enabled: self.enabled,
            auto_start: self.auto_start.unwrap_or(true),
            option: None,
        }
    }
    pub fn recorder_path(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .unwrap_or_else(default_config_recorder_path)
    }

    pub fn recorder_format(&self) -> RecorderFormat {
        self.format.unwrap_or_default()
    }

    pub fn ensure_defaults(&mut self) -> bool {
        if self
            .path
            .as_ref()
            .map(|p| p.trim().is_empty())
            .unwrap_or(true)
        {
            self.path = Some(default_config_recorder_path());
        }

        let original = self.format.unwrap_or_default();
        let effective = original.effective();
        self.format = Some(effective);
        original != effective
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_config_http_addr")]
    pub http_addr: String,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_access_skip_paths: Vec<String>,
    pub ua: Option<UseragentConfig>,
    pub proxy: Option<ProxyConfig>,

    pub external_ip: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,

    pub callrecord: Option<CallRecordConfig>,
    #[serde(default = "default_config_media_cache_path")]
    pub media_cache_path: String,
    pub llmproxy: Option<String>,
    pub restsend_token: Option<String>,
    pub ice_servers: Option<Vec<IceServer>>,
    pub ami: Option<AmiConfig>,
    #[cfg(feature = "console")]
    pub console: Option<ConsoleConfig>,
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConsoleConfig {
    #[serde(default = "default_console_session_secret")]
    pub session_secret: String,
    #[serde(default = "default_console_base_path")]
    pub base_path: String,
    #[serde(default)]
    pub allow_registration: bool,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            session_secret: default_console_session_secret(),
            base_path: default_console_base_path(),
            allow_registration: false,
        }
    }
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
        url: Option<String>,
        urls: Option<Vec<String>>,
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
        url: Option<String>,
        table_name: Option<String>,
        id_column: Option<String>,
        username_column: Option<String>,
        password_column: Option<String>,
        enabled_column: Option<String>,
        realm_column: Option<String>,
    },
    Extension {
        #[serde(default)]
        database_url: Option<String>,
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

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct TranscriptConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub models_path: Option<String>,
    #[serde(default)]
    pub hf_endpoint: Option<String>,
    #[serde(default)]
    pub samplerate: Option<u32>,
    #[serde(default)]
    pub default_language: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
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
    pub ice_servers: Option<Vec<IceServer>>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acl_files: Vec<String>,
    pub ua_white_list: Option<Vec<String>>,
    pub ua_black_list: Option<Vec<String>>,
    pub max_concurrency: Option<usize>,
    pub registrar_expires: Option<u32>,
    pub ensure_user: Option<bool>,
    #[serde(default = "default_user_backends")]
    pub user_backends: Vec<UserBackendConfig>,
    #[serde(default)]
    pub locator: LocatorConfig,
    #[serde(default)]
    pub media_proxy: MediaProxyMode,
    #[serde(default)]
    pub frequency_limiter: Option<String>,
    #[serde(default)]
    pub realms: Option<Vec<String>>,
    pub ws_handler: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<RouteRule>>,
    #[serde(default)]
    pub queues: HashMap<String, RouteQueueConfig>,
    #[serde(default)]
    pub trunks: HashMap<String, TrunkConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trunks_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_dir: Option<String>,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
    #[serde(default = "default_generated_config_dir")]
    pub generated_dir: String,
    pub sip_flow_max_items: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addons: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub transcript: Option<TranscriptConfig>,
}

pub enum RouteResult {
    Forward(InviteOption),
    Queue {
        option: InviteOption,
        queue: QueuePlan,
    },
    Ivr {
        option: InviteOption,
        ivr: DialplanIvrConfig,
    },
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
            addr == "127.0.0.1" || addr == "::1" || addr == "localhost"
        }
    }
}

impl Default for AmiConfig {
    fn default() -> Self {
        Self { allows: None }
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

    pub fn select_realm(&self, request_host: &str) -> String {
        let requested = request_host.trim();
        let normalized = ProxyConfig::normalize_realm(requested);
        if let Some(realms) = self.realms.as_ref() {
            if let Some(existing) = realms
                .iter()
                .find(|realm| realm.as_str() == requested || realm.as_str() == normalized)
            {
                return existing.clone();
            }
            if let Some(first) = realms.first() {
                if !first.is_empty() {
                    return first.clone();
                }
            }
        }

        if requested.is_empty() {
            normalized.to_string()
        } else {
            requested.to_string()
        }
    }

    pub fn generated_root_dir(&self) -> PathBuf {
        let trimmed = self.generated_dir.trim();
        if trimmed.is_empty() {
            return PathBuf::from("./config");
        }
        PathBuf::from(trimmed)
    }

    pub fn generated_trunks_dir(&self) -> PathBuf {
        self.generated_root_dir().join("trunks")
    }

    pub fn generated_routes_dir(&self) -> PathBuf {
        self.generated_root_dir().join("routes")
    }

    pub fn generated_queue_dir(&self) -> PathBuf {
        if let Some(dir) = self
            .queue_dir
            .as_ref()
            .map(|path| path.trim())
            .filter(|path| !path.is_empty())
        {
            PathBuf::from(dir)
        } else {
            self.generated_root_dir().join("queue")
        }
    }

    pub fn generated_ivr_dir(&self) -> PathBuf {
        self.generated_root_dir().join("ivr")
    }

    pub fn generated_acl_dir(&self) -> PathBuf {
        self.generated_root_dir().join("acl")
    }

    pub fn ensure_recording_defaults(&mut self) -> bool {
        let mut fallback = false;

        if let Some(policy) = self.recording.as_mut() {
            fallback |= policy.ensure_defaults();
        }

        for trunk in self.trunks.values_mut() {
            if let Some(policy) = trunk.recording.as_mut() {
                fallback |= policy.ensure_defaults();
            }
        }
        fallback
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
            ensure_user: Some(true),
            user_backends: default_user_backends(),
            locator: LocatorConfig::default(),
            media_proxy: MediaProxyMode::default(),
            frequency_limiter: None,
            realms: Some(vec![]),
            ws_handler: None,
            routes_files: Vec::new(),
            acl_files: Vec::new(),
            routes: None,
            queues: HashMap::new(),
            trunks: HashMap::new(),
            trunks_files: Vec::new(),
            queue_dir: None,
            recording: None,
            generated_dir: default_generated_config_dir(),
            sip_flow_max_items: None,
            addons: None,
            transcript: None,
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
            http_access_skip_paths: Vec::new(),
            ua: Some(UseragentConfig::default()),
            proxy: None,
            media_cache_path: default_config_media_cache_path(),
            callrecord: None,
            llmproxy: None,
            restsend_token: None,
            ice_servers: None,
            ami: Some(AmiConfig::default()),
            external_ip: None,
            rtp_start_port: default_config_rtp_start_port(),
            rtp_end_port: default_config_rtp_end_port(),
            #[cfg(feature = "console")]
            console: None,
            database_url: default_database_url(),
            recording: None,
        }
    }
}

impl Clone for Config {
    fn clone(&self) -> Self {
        // This is a bit expensive but Config is not cloned often in hot paths
        // and implementing Clone manually for all nested structs is tedious
        let s = toml::to_string(self).unwrap();
        toml::from_str(&s).unwrap()
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Error> {
        let mut config: Self = toml::from_str(
            &std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {}", e, path))?,
        )?;
        if config.ensure_recording_defaults() {
            tracing::warn!(
                "recorder_format=ogg requires compiling with the 'opus' feature; falling back to wav"
            );
        }
        Ok(config)
    }

    pub fn rtp_config(&self) -> RtpConfig {
        RtpConfig {
            external_ip: self.external_ip.clone(),
            start_port: self.rtp_start_port.clone(),
            end_port: self.rtp_end_port.clone(),
            ice_servers: self.ice_servers.clone(),
        }
    }

    pub fn recorder_path(&self) -> String {
        self.recording
            .as_ref()
            .map(|policy| policy.recorder_path())
            .unwrap_or_else(default_config_recorder_path)
    }

    pub fn recorder_format(&self) -> RecorderFormat {
        self.recording
            .as_ref()
            .map(|policy| policy.recorder_format())
            .unwrap_or_default()
    }

    pub fn ensure_recording_defaults(&mut self) -> bool {
        let mut fallback = false;

        if let Some(policy) = self.recording.as_mut() {
            fallback |= policy.ensure_defaults();
        }

        if let Some(proxy) = self.proxy.as_mut() {
            fallback |= proxy.ensure_recording_defaults();
        }

        fallback
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
            ..Default::default()
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
            ..Default::default()
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
