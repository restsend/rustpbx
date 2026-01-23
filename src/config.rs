use crate::{
    call::{CallRecordingConfig, DialDirection, QueuePlan, user::SipUser},
    proxy::routing::{RouteQueueConfig, RouteRule, TrunkConfig},
    storage::StorageConfig,
};
use anyhow::{Error, Result};
use clap::Parser;
use rsip::StatusCode;
use rsipstack::dialog::invitation::InviteOption;
use rustrtc::IceServer;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};

#[derive(Parser, Debug)]
#[command(version)]
pub(crate) struct Cli {
    #[clap(long, default_value = "rustpbx.toml")]
    pub conf: Option<String>,
}

pub(crate) fn default_config_recorder_path() -> String {
    #[cfg(target_os = "windows")]
    return "./config/recorders".to_string();
    #[cfg(not(target_os = "windows"))]
    return "./config/recorders".to_string();
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

fn default_nat_fix() -> bool {
    true
}

fn default_callid_suffix() -> Option<String> {
    Some("miuda.ai".to_string())
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

    pub fn ensure_defaults(&mut self) -> bool {
        if self
            .path
            .as_ref()
            .map(|p| p.trim().is_empty())
            .unwrap_or(true)
        {
            self.path = Some(default_config_recorder_path());
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_config_http_addr")]
    pub http_addr: String,
    #[serde(default)]
    pub http_gzip: bool,
    pub https_addr: Option<String>,
    pub ssl_certificate: Option<String>,
    pub ssl_private_key: Option<String>,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_access_skip_paths: Vec<String>,
    pub proxy: ProxyConfig,

    pub external_ip: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,

    pub callrecord: Option<CallRecordConfig>,
    pub ice_servers: Option<Vec<IceServer>>,
    #[serde(default)]
    pub ami: Option<AmiConfig>,
    #[cfg(feature = "console")]
    pub console: Option<ConsoleConfig>,
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
    #[serde(default)]
    pub archive: Option<ArchiveConfig>,
    #[serde(default)]
    pub demo_mode: bool,
    #[serde(default)]
    pub addons: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub sipflow: Option<SipFlowConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchiveConfig {
    pub enabled: bool,
    pub archive_time: String,
    pub timezone: Option<String>,
    pub retention_days: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConsoleConfig {
    #[serde(default = "default_console_session_secret")]
    pub session_secret: String,
    #[serde(default = "default_console_base_path")]
    pub base_path: String,
    #[serde(default)]
    pub allow_registration: bool,
    #[serde(default)]
    pub secure_cookie: bool,
    pub alpine_js: Option<String>,
    pub tailwind_js: Option<String>,
    pub chart_js: Option<String>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            session_secret: default_console_session_secret(),
            base_path: default_console_base_path(),
            allow_registration: false,
            secure_cookie: false,
            alpine_js: None,
            tailwind_js: None,
            chart_js: None,
        }
    }
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
        sip_headers: Option<Vec<String>>,
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
        #[serde(default)]
        ttl: Option<u64>,
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

pub use crate::storage::S3Vendor;

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

/// Directory structure for sipflow storage
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SipFlowSubdirs {
    /// No subdirectory structure - all files in root
    None,
    /// Daily subdirectories (YYYYMMDD)
    Daily,
    /// Hourly subdirectories (YYYYMMDD/HH)
    Hourly,
}

impl Default for SipFlowSubdirs {
    fn default() -> Self {
        Self::Daily
    }
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SipFlowConfig {
    Local {
        root: String,
        #[serde(default)]
        subdirs: SipFlowSubdirs,
        #[serde(default = "default_sipflow_flush_count")]
        flush_count: usize,
        #[serde(default = "default_sipflow_flush_interval")]
        flush_interval_secs: u64,
        #[serde(default = "default_sipflow_id_cache_size")]
        id_cache_size: usize,
    },
    Remote {
        udp_addr: String,
        http_addr: String,
        #[serde(default = "default_sipflow_timeout")]
        timeout_secs: u64,
    },
}

fn default_sipflow_flush_count() -> usize {
    1000
}

fn default_sipflow_flush_interval() -> u64 {
    5
}

fn default_sipflow_timeout() -> u64 {
    10
}

fn default_sipflow_id_cache_size() -> usize {
    8192
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

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct HttpRouterConfig {
    pub url: String,
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub fallback_to_static: bool,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LocatorWebhookConfig {
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
    pub headers: Option<HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
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
    pub locator_webhook: Option<LocatorWebhookConfig>,
    #[serde(default)]
    pub media_proxy: MediaProxyMode,
    pub codecs: Option<Vec<String>>,
    #[serde(default)]
    pub frequency_limiter: Option<String>,
    #[serde(default)]
    pub realms: Option<Vec<String>>,
    pub ws_handler: Option<String>,
    pub http_router: Option<HttpRouterConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<RouteRule>>,
    #[serde(default)]
    pub session_timer: bool,
    #[serde(default)]
    pub session_expires: Option<u64>,
    #[serde(default)]
    pub queues: HashMap<String, RouteQueueConfig>,
    #[serde(default)]
    pub enable_latching: bool,
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
    #[serde(default = "default_nat_fix")]
    pub nat_fix: bool,
    pub sip_flow_max_items: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addons: Option<Vec<String>>,
}

#[derive(Default, Clone)]
pub struct DialplanHints {
    pub enable_recording: Option<bool>,
    pub bypass_media: Option<bool>,
    pub max_duration: Option<std::time::Duration>,
    pub enable_sipflow: Option<bool>,
    pub allow_codecs: Option<Vec<String>>,
    pub extensions: http::Extensions,
}

impl std::fmt::Debug for DialplanHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialplanHints")
            .field("enable_recording", &self.enable_recording)
            .field("bypass_media", &self.bypass_media)
            .field("max_duration", &self.max_duration)
            .field("enable_sipflow", &self.enable_sipflow)
            .finish()
    }
}

pub enum RouteResult {
    Forward(InviteOption, Option<DialplanHints>),
    Queue {
        option: InviteOption,
        queue: QueuePlan,
        hints: Option<DialplanHints>,
    },
    NotHandled(InviteOption, Option<DialplanHints>),
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
        let realm = if let Some(pos) = realm.find(':') {
            &realm[..pos]
        } else {
            realm
        };
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
            enable_latching: false,
            user_backends: default_user_backends(),
            locator: LocatorConfig::default(),
            locator_webhook: None,
            media_proxy: MediaProxyMode::default(),
            codecs: None,
            frequency_limiter: None,
            realms: Some(vec![]),
            ws_handler: None,
            http_router: None,
            routes_files: Vec::new(),
            acl_files: Vec::new(),
            routes: None,
            session_timer: false,
            session_expires: None,
            queues: HashMap::new(),
            trunks: HashMap::new(),
            trunks_files: Vec::new(),
            queue_dir: None,
            recording: None,
            generated_dir: default_generated_config_dir(),
            nat_fix: true,
            sip_flow_max_items: None,
            addons: None,
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

impl Default for CallRecordConfig {
    fn default() -> Self {
        Self::Local {
            #[cfg(target_os = "windows")]
            root: "./config/cdr".to_string(),
            #[cfg(not(target_os = "windows"))]
            root: "./config/cdr".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http_addr: default_config_http_addr(),
            http_gzip: false,
            https_addr: None,
            ssl_certificate: None,
            ssl_private_key: None,
            log_level: None,
            log_file: None,
            http_access_skip_paths: Vec::new(),
            proxy: ProxyConfig::default(),
            callrecord: None,
            ice_servers: None,
            ami: Some(AmiConfig::default()),
            external_ip: None,
            rtp_start_port: default_config_rtp_start_port(),
            rtp_end_port: default_config_rtp_end_port(),
            #[cfg(feature = "console")]
            console: None,
            database_url: default_database_url(),
            recording: None,
            archive: None,
            demo_mode: false,
            storage: None,
            addons: HashMap::new(),
            sipflow: None,
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
        if std::env::var("RUSTPBX_DEMO_MODE")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
        {
            config.demo_mode = true;
        }
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

    pub fn ensure_recording_defaults(&mut self) -> bool {
        let mut fallback = false;

        if let Some(policy) = self.recording.as_mut() {
            fallback |= policy.ensure_defaults();
        }

        fallback |= self.proxy.ensure_recording_defaults();

        fallback
    }

    pub fn config_dir(&self) -> std::path::PathBuf {
        self.proxy.generated_root_dir()
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
        config.proxy = prxconfig;
        config.ice_servers = Some(ice_servers);
        let config_str = toml::to_string(&config).unwrap();
        println!("{}", config_str);
    }

    #[test]
    fn test_normalize_realm() {
        assert_eq!(ProxyConfig::normalize_realm("localhost"), "localhost");
        assert_eq!(ProxyConfig::normalize_realm("127.0.0.1"), "localhost");
        assert_eq!(ProxyConfig::normalize_realm("::1"), "localhost");
        assert_eq!(ProxyConfig::normalize_realm(""), "localhost");
        assert_eq!(ProxyConfig::normalize_realm("*"), "localhost");
        assert_eq!(ProxyConfig::normalize_realm("example.com"), "example.com");
        assert_eq!(
            ProxyConfig::normalize_realm("example.com:5060"),
            "example.com"
        );
        assert_eq!(ProxyConfig::normalize_realm("127.0.0.1:5060"), "localhost");
    }

    #[test]
    fn test_select_realm() {
        let mut config = ProxyConfig::default();
        config.realms = Some(vec!["example.com".to_string(), "test.com".to_string()]);

        // Exact match
        assert_eq!(config.select_realm("example.com"), "example.com");
        // Match with port (should return normalized/existing realm)
        assert_eq!(config.select_realm("example.com:5060"), "example.com");
        // Match with different port
        assert_eq!(config.select_realm("test.com:8888"), "test.com");
        // No match, return first realm if configured
        assert_eq!(config.select_realm("other.com"), "example.com");
        // No match with port, return first realm if configured
        assert_eq!(config.select_realm("other.com:5060"), "example.com");
    }
}
