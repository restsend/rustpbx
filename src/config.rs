use crate::rwi::auth::RwiConfig;
use crate::{
    call::{CallRecordingConfig, DialDirection, QueuePlan, user::SipUser},
    proxy::routing::{RouteQueueConfig, RouteRule, TrunkConfig},
    storage::StorageConfig,
};
use anyhow::{Error, Result};
use clap::Parser;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip::StatusCode;
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
fn default_ami_config() -> Option<AmiConfig> {
    Some(AmiConfig::default())
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

fn default_console_api_prefix() -> String {
    "/api".to_string()
}

fn default_config_rtp_start_port() -> Option<u16> {
    Some(12000)
}

fn default_config_rtp_end_port() -> Option<u16> {
    Some(42000)
}

fn default_config_webrtc_start_port() -> Option<u16> {
    Some(30000)
}

fn default_config_webrtc_end_port() -> Option<u16> {
    Some(40000)
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
        matches!(
            (self, direction),
            (RecordingDirection::Inbound, DialDirection::Inbound)
                | (RecordingDirection::Outbound, DialDirection::Outbound)
                | (RecordingDirection::Internal, DialDirection::Internal)
        )
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
    #[serde(default)]
    pub log_rotation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_access_skip_paths: Vec<String>,
    pub proxy: ProxyConfig,

    pub external_ip: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,

    #[serde(default = "default_config_webrtc_start_port")]
    pub webrtc_port_start: Option<u16>,
    #[serde(default = "default_config_webrtc_end_port")]
    pub webrtc_port_end: Option<u16>,

    pub callrecord: Option<CallRecordConfig>,
    pub ice_servers: Option<Vec<IceServer>>,
    #[serde(default = "default_ami_config")]
    pub ami: Option<AmiConfig>,
    #[cfg(feature = "console")]
    pub console: Option<ConsoleConfig>,
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
    #[serde(default)]
    pub demo_mode: bool,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub sipflow: Option<SipFlowConfig>,
    #[cfg(feature = "commerce")]
    #[serde(default)]
    pub licenses: Option<LicenseConfig>,
    #[serde(default)]
    pub rwi: Option<RwiConfig>,
}

fn default_locale() -> String {
    "en".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LocaleInfo {
    pub name: String,
    pub native_name: String,
}

fn default_locales() -> std::collections::HashMap<String, LocaleInfo> {
    let mut m = std::collections::HashMap::new();
    m.insert(
        "en".to_string(),
        LocaleInfo {
            name: "English".to_string(),
            native_name: "English".to_string(),
        },
    );
    m.insert(
        "zh".to_string(),
        LocaleInfo {
            name: "Chinese".to_string(),
            native_name: "中文".to_string(),
        },
    );
    m
}

#[cfg(feature = "commerce")]
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LicenseConfig {
    #[serde(default)]
    pub addons: HashMap<String, String>,
    #[serde(default)]
    pub keys: HashMap<String, String>,
}

#[cfg(feature = "commerce")]
impl LicenseConfig {
    pub fn get_license_for_addon(&self, addon_id: &str) -> Option<(String, String)> {
        self.addons.get(addon_id).and_then(|key_name| {
            self.keys
                .get(key_name)
                .map(|key_value| (key_name.clone(), key_value.clone()))
        })
    }

    pub fn get_addons_for_key(&self, key_name: &str) -> Vec<&str> {
        self.addons
            .iter()
            .filter(|(_, k)| k == &key_name)
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConsoleConfig {
    #[serde(default = "default_console_session_secret")]
    pub session_secret: String,
    #[serde(default = "default_console_base_path")]
    pub base_path: String,
    /// API prefix for REST endpoints (default: "/api")
    /// All REST API endpoints will be prefixed with this path
    #[serde(default = "default_console_api_prefix")]
    pub api_prefix: String,
    #[serde(default)]
    pub allow_registration: bool,
    #[serde(default)]
    pub secure_cookie: bool,
    pub alpine_js: Option<String>,
    pub tailwind_js: Option<String>,
    pub chart_js: Option<String>,
    pub jssip_js: Option<String>,
    /// Default locale code, e.g. "en" or "zh"
    #[serde(default = "default_locale")]
    pub locale_default: String,
    /// Supported locales map: code -> LocaleInfo
    #[serde(default = "default_locales")]
    pub locales: std::collections::HashMap<String, LocaleInfo>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            session_secret: default_console_session_secret(),
            base_path: default_console_base_path(),
            api_prefix: default_console_api_prefix(),
            allow_registration: false,
            secure_cookie: false,
            alpine_js: None,
            tailwind_js: None,
            chart_js: None,
            jssip_js: None,
            locale_default: default_locale(),
            locales: default_locales(),
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
#[derive(Default)]
pub enum LocatorConfig {
    #[default]
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
    Database {
        /// Database URL for call records. If not set, uses the global database_url.
        database_url: Option<String>,
        /// Table name for call records (default: "call_records")
        #[serde(default = "default_call_record_table")]
        table_name: String,
    },
}

fn default_call_record_table() -> String {
    "call_records".to_string()
}

/// Directory structure for sipflow storage
#[derive(Debug, Deserialize, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SipFlowSubdirs {
    /// No subdirectory structure - all files in root
    None,
    /// Daily subdirectories (YYYYMMDD)
    #[default]
    Daily,
    /// Hourly subdirectories (YYYYMMDD/HH)
    Hourly,
}


/// Upload configuration for SipFlow recordings
#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum SipFlowUploadConfig {
    S3 {
        vendor: S3Vendor,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        endpoint: String,
        root: String,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
    },
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
        #[serde(default)]
        upload: Option<SipFlowUploadConfig>,
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
#[derive(Default)]
pub enum MediaProxyMode {
    /// All media goes through proxy
    All,
    /// Auto detect if media proxy is needed (webrtc to rtp)
    #[default]
    Auto,
    /// Only handle NAT (private IP addresses)
    Nat,
    /// Do not handle media proxy
    None,
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTimerMode {
    Off,
    Supported,
    Always,
}

impl SessionTimerMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn is_always(self) -> bool {
        matches!(self, Self::Always)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct RtpConfig {
    pub external_ip: Option<String>,
    pub start_port: Option<u16>,
    pub end_port: Option<u16>,
    pub webrtc_start_port: Option<u16>,
    pub webrtc_end_port: Option<u16>,
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
    pub t1_timer: Option<u64>,
    pub t1x64_timer: Option<u64>,
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
    pub session_timer_always: bool,
    #[serde(default)]
    pub session_expires: Option<u64>,
    #[serde(default)]
    pub queues: HashMap<String, RouteQueueConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues_files: Vec<String>,
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
    /// Whether to passthrough callee's failure status code to caller.
    /// When true, the caller receives the same SIP error code (e.g., 486, 603) that the callee returned.
    /// When false, a generic error code is sent instead.
    #[serde(default = "default_passthrough_failure")]
    pub passthrough_failure: bool,
}

fn default_passthrough_failure() -> bool {
    true
}

#[derive(Default, Clone)]
pub struct DialplanHints {
    pub enable_recording: Option<bool>,
    pub bypass_media: Option<bool>,
    pub max_duration: Option<std::time::Duration>,
    pub enable_sipflow: Option<bool>,
    pub allow_codecs: Option<Vec<String>>,
    pub extensions: http::Extensions,
    pub disable_ice_servers: Option<bool>,
}

impl std::fmt::Debug for DialplanHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialplanHints")
            .field("enable_recording", &self.enable_recording)
            .field("bypass_media", &self.bypass_media)
            .field("max_duration", &self.max_duration)
            .field("enable_sipflow", &self.enable_sipflow)
            .field("disable_ice_servers", &self.disable_ice_servers)
            .finish()
    }
}

#[allow(clippy::large_enum_variant)]
pub enum RouteResult {
    Forward(InviteOption, Option<DialplanHints>),
    Queue {
        option: InviteOption,
        queue: QueuePlan,
        hints: Option<DialplanHints>,
    },
    Application {
        option: InviteOption,
        app_name: String,
        app_params: Option<serde_json::Value>,
        auto_answer: bool,
    },
    NotHandled(InviteOption, Option<DialplanHints>),
    Abort(StatusCode, Option<String>),
}

#[derive(Debug, Deserialize, Serialize)]
#[derive(Default)]
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


impl ProxyConfig {
    pub fn session_timer_mode(&self) -> SessionTimerMode {
        if !self.session_timer {
            SessionTimerMode::Off
        } else if self.session_timer_always {
            SessionTimerMode::Always
        } else {
            SessionTimerMode::Supported
        }
    }

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
            if let Some(first) = realms.first()
                && !first.is_empty() {
                    return first.clone();
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
            t1_timer: None,
            t1x64_timer: None,
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
            session_timer_always: false,
            session_expires: None,
            queues: HashMap::new(),
            queues_files: Vec::new(),
            trunks: HashMap::new(),
            trunks_files: Vec::new(),
            queue_dir: None,
            recording: None,
            generated_dir: default_generated_config_dir(),
            nat_fix: true,
            sip_flow_max_items: None,
            addons: None,
            passthrough_failure: true,
        }
    }
}

impl Default for UserBackendConfig {
    fn default() -> Self {
        Self::Memory { users: None }
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
            log_rotation: String::new(),
            http_access_skip_paths: Vec::new(),
            proxy: ProxyConfig::default(),
            callrecord: None,
            ice_servers: None,
            ami: Some(AmiConfig::default()),
            external_ip: None,
            rtp_start_port: default_config_rtp_start_port(),
            rtp_end_port: default_config_rtp_end_port(),
            webrtc_port_start: default_config_webrtc_start_port(),
            webrtc_port_end: default_config_webrtc_end_port(),
            #[cfg(feature = "console")]
            console: None,
            rwi: None,
            database_url: default_database_url(),
            recording: None,
            demo_mode: false,
            storage: None,
            sipflow: None,
            #[cfg(feature = "commerce")]
            licenses: None,
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
        config.ensure_recording_defaults();
        Ok(config)
    }

    pub fn rtp_config(&self) -> RtpConfig {
        RtpConfig {
            external_ip: self.external_ip.clone(),
            start_port: self.rtp_start_port,
            end_port: self.rtp_end_port,
            webrtc_start_port: self.webrtc_port_start,
            webrtc_end_port: self.webrtc_port_end,
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

    /// Returns the wholesale bills directory.
    pub fn wholesale_bills_dir(&self) -> String {
        format!(
            "{}/wholesale_bills",
            self.recorder_path().trim_end_matches('/')
        )
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_session_timer_mode_defaults_to_supported_when_enabled() {
        #[derive(Deserialize)]
        struct SessionTimerWrapper {
            session_timer: bool,
            #[serde(default)]
            session_timer_always: bool,
        }

        let disabled: SessionTimerWrapper = toml::from_str("session_timer=false").unwrap();
        assert!(!disabled.session_timer);
        assert!(!disabled.session_timer_always);

        let enabled: SessionTimerWrapper = toml::from_str("session_timer=true").unwrap();
        assert!(enabled.session_timer);
        assert!(!enabled.session_timer_always);
    }

    #[test]
    fn test_session_timer_mode_uses_always_flag() {
        let mut config = ProxyConfig::default();

        assert_eq!(config.session_timer_mode(), SessionTimerMode::Off);

        config.session_timer = true;
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Supported);

        config.session_timer_always = true;
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Always);

        config.session_timer = false;
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Off);
    }
}
