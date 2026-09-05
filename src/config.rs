use crate::rwi::auth::RwiConfig;
use crate::{
    call::{CallRecordingConfig, DialDirection, QueuePlan, user::SipUser},
    proxy::routing::{MatchConditions, RouteQueueConfig, RouteRule, TrunkConfig},
    storage::StorageConfig,
};
use anyhow::{Error, Result};
use clap::Parser;
use ipnet::IpNet;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip::StatusCode;
use rustpbx_models::DatabasePoolConfig;
use rustrtc::IceServer;
use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::HashMap, net::IpAddr, path::PathBuf};

/// Default AMI HTTP endpoint path.
pub const DEFAULT_AMI_PATH: &str = "/ami/v1";
/// Default SIP-over-WebSocket handler path.
pub const DEFAULT_WS_PATH: &str = "/ws";
/// Default ICE servers endpoint path.
pub const DEFAULT_ICE_SERVERS_PATH: &str = "/iceservers";
/// Default IVR editor mode.
pub const DEFAULT_IVR_MODE: &str = "tree";
/// Product brand name.
pub const BRAND_NAME: &str = "RustPBX";

/// An ICE server with optional settings for issuing temporary browser credentials.
#[derive(Clone, Deserialize, Serialize)]
pub struct IceServerConfig {
    #[serde(flatten)]
    pub server: IceServer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrete: Option<String>,
    /// Temporary credential lifetime in seconds; defaults to one hour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<u64>,
}

impl std::fmt::Debug for IceServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceServerConfig")
            .field("server", &self.server)
            .field("secrete", &self.secrete.as_ref().map(|_| "[redacted]"))
            .field("lifetime", &self.lifetime)
            .finish()
    }
}

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

fn default_enable_latching() -> bool {
    true
}

fn default_latching_probation_max_packets() -> Option<u8> {
    Some(6)
}

fn default_rtp_timeout() -> Option<u64> {
    Some(60)
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

#[derive(Debug, Clone, Deserialize, Serialize, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecordingType {
    /// Local WAV file under `[recording].path`, archived by RecordingUploadHook.
    #[default]
    Local,
    /// WAV file uploaded via HTTP.
    Http,
    /// WAV file uploaded to S3-compatible storage.
    S3,
    /// RTP captured by SipflowRecorder into the `[sipflow]` backend.
    /// Media upload (if any) is handled by `[sipflow.upload].media`.
    Sipflow,
}

impl RecordingType {
    /// `local` / `http` / `s3` own file media; `sipflow` does not.
    pub fn is_file_media(self) -> bool {
        matches!(self, Self::Local | Self::Http | Self::S3)
    }
}

/// Signaling point at which `auto_start` installs the selected recorder.
#[derive(Debug, Clone, Deserialize, Serialize, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingAutoStartAt {
    /// Start as soon as the caller media connection is set up, whether that
    /// connection is exposed through a provisional or final response.
    #[default]
    Media,
    /// Wait until caller media is ready for the final 200 response.
    Answer,
}

/// `[proxy.transcript]` section. Only the `remote` sub-table is typed; the
/// offline sensevoice fields (`command`, `models_path`, ...) live in the raw
/// TOML and are parsed ad-hoc by the transcript addon.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TranscriptSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<crate::call::transcription::remote::RemoteTranscriptConfig>,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default, rename_all = "snake_case")]
pub struct RecordingPolicy {
    pub enabled: Option<bool>,
    #[serde(rename = "type")]
    pub recording_type: Option<RecordingType>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub directions: Vec<RecordingDirection>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caller_allow: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caller_deny: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callee_allow: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callee_deny: Vec<String>,
    pub auto_start: Option<bool>,
    pub auto_start_at: Option<RecordingAutoStartAt>,
    pub filename_pattern: Option<String>,
    pub samplerate: Option<u32>,
    pub ptime: Option<u32>,
    pub path: Option<String>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub vendor: Option<crate::storage::S3Vendor>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub endpoint: Option<String>,
    pub root: Option<String>,
    /// Deprecated: ignored. Use `type = "local"|"http"|"s3"` for WAV media, or
    /// `type = "sipflow"` for SipFlow RTP capture. Kept for config compatibility.
    #[serde(default)]
    pub force_file: Option<bool>,
    /// Deprecated: the signaling JSONL sidecar has been removed. SIP signalling
    /// is captured only when `[sipflow]` is configured. Kept for compatibility.
    #[serde(default)]
    pub signaling: Option<bool>,
    /// Swap stereo channels in recording: callee→left, caller→right.
    #[serde(default)]
    pub stereo_swap: Option<bool>,
    /// Local archival layout when `type = "local"`: `daily` (default) or
    /// `hourly`. Files are moved under `{path}/YYYYMMDD[/HH]/` after the call.
    #[serde(default)]
    pub subdir: Option<String>,
    /// Lifetime in seconds of presigned download URLs generated on demand for
    /// S3-uploaded recordings. Defaults to 86400 (24h); the SigV4 signature
    /// used by S3/S3-compatible services (AWS, Aliyun OSS, Tencent COS…)
    /// caps validity at 7 days (604800), so larger values are clamped.
    #[serde(default)]
    pub signed_url_expiry_secs: Option<u64>,
}

impl RecordingPolicy {
    /// Effective media destination, applying deprecated `force_file` as a
    /// migration hint (force file media when set true with `type = sipflow`).
    pub fn effective_recording_type(&self) -> RecordingType {
        let mut ty = self.recording_type.unwrap_or_default();
        if self.force_file == Some(true) && ty == RecordingType::Sipflow {
            ty = RecordingType::Local;
        }
        ty
    }

    pub fn new_recording_config(&self) -> CallRecordingConfig {
        crate::call::CallRecordingConfig {
            enabled: self.enabled.unwrap_or(false),
            auto_start: self.auto_start.unwrap_or(true),
            auto_start_at: self.auto_start_at.unwrap_or_default(),
            recording_type: self.effective_recording_type(),
            stereo_swap: self.stereo_swap.unwrap_or(false),
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

    /// True when the `[recording]` upload path should handle WAV artifacts.
    pub fn uploads_recording(&self) -> bool {
        self.enabled.unwrap_or(false) && self.effective_recording_type().is_file_media()
    }

    /// Lifetime of on-demand presigned recording URLs, clamped to the SigV4
    /// 7-day maximum (see [`crate::storage::MAX_PRESIGN_EXPIRY_SECS`]).
    pub fn effective_signed_url_expiry_secs(&self) -> u64 {
        const DEFAULT_SIGNED_URL_EXPIRY_SECS: u64 = 86_400;
        self.signed_url_expiry_secs
            .unwrap_or(DEFAULT_SIGNED_URL_EXPIRY_SECS)
            .clamp(1, crate::storage::MAX_PRESIGN_EXPIRY_SECS)
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// Optional local stats log path. When set, one JSON line is appended to
    /// this file every `stats_interval` seconds with system + PBX summary
    /// metrics (load, registrations, message volume, media loss, DB/tokio
    /// pressure) for local analysis without Prometheus. Empty/unset disables.
    pub stats_log: Option<String>,
    /// Local stats log refresh interval in seconds. Default 5.
    #[serde(default = "default_stats_interval")]
    pub stats_interval: u64,
    #[serde(default)]
    pub log_rotation: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_access_skip_paths: Vec<String>,
    pub proxy: ProxyConfig,

    pub external_ip: Option<String>,
    pub auto_external_ip: Option<String>,
    /// Public IP advertised in SIP Contact / dialog signaling. When unset,
    /// WAN destinations use `external_ip` (RTP). LAN destinations (see
    /// `local_networks`) always use `proxy.addr` when `contact_lan_use_bind`
    /// is true (default).
    pub sip_external_ip: Option<String>,
    /// Auto-detect `sip_external_ip` via HTTP (mutually exclusive with manual).
    pub auto_sip_external_ip: Option<String>,
    /// CIDR ranges treated as "local" for Contact host selection. Defaults to
    /// RFC1918 + loopback when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_networks: Vec<String>,
    /// When true (default), peers whose IP falls in `local_networks` receive a
    /// Contact host of `proxy.addr` instead of the public SIP/RTP IP.
    #[serde(default = "default_contact_lan_use_bind")]
    pub contact_lan_use_bind: bool,
    /// When true, SIP Contact always uses `proxy.addr` (never RTP/public IP).
    #[serde(default)]
    pub sip_contact_always_bind: bool,
    /// Named network profiles (public WAN, overlay, etc.). When empty, a
    /// synthetic `default` profile is derived from the top-level fields above.
    #[serde(default, rename = "network_profile", alias = "network_profiles")]
    pub network_profiles: Vec<NetworkProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_network_profile: Option<String>,
    #[serde(default = "default_config_rtp_start_port")]
    pub rtp_start_port: Option<u16>,
    #[serde(default = "default_config_rtp_end_port")]
    pub rtp_end_port: Option<u16>,

    #[serde(default = "default_config_webrtc_start_port")]
    pub webrtc_port_start: Option<u16>,
    #[serde(default = "default_config_webrtc_end_port")]
    pub webrtc_port_end: Option<u16>,

    pub callrecord: Option<CallRecordConfig>,
    pub ice_servers: Option<Vec<IceServerConfig>>,
    /// Media server settings (comfort noise etc.).
    #[serde(default)]
    pub media: Option<MediaSection>,
    #[serde(default = "default_ami_config")]
    pub ami: Option<AmiConfig>,
    #[cfg(feature = "console")]
    pub console: Option<ConsoleConfig>,
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default)]
    pub database_pool: DatabasePoolConfig,
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
    /// SSO login broker (commerce builds only). Handlers are mounted only
    /// when `enabled = true` and the provider section validates.
    #[cfg(feature = "commerce")]
    #[serde(default)]
    pub sso: Option<SsoConfig>,
    #[serde(default)]
    pub rwi: Option<RwiConfig>,
    #[serde(default)]
    pub rwi_webhook: Option<LocatorWebhookConfig>,
    #[serde(default)]
    pub cluster: Option<ClusterConfig>,
    #[serde(default)]
    pub outbound: Option<OutboundConfig>,
    /// Graceful shutdown (drain) tuning — see `GracefulShutdownConfig`.
    #[serde(default)]
    pub graceful_shutdown: Option<GracefulShutdownConfig>,
    /// Maximum size (in bytes) for audio files downloaded over HTTP from the
    /// console (queue prompts, voicemail prompts, ...). Defaults to 20 MB.
    #[serde(default = "default_max_audio_download_bytes")]
    pub max_audio_download_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClusterPeer {
    pub addr: String,
    pub sip_port: u16,
    pub ami_port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClusterConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<ClusterPeer>,
    /// Session registry backend: "db" (cluster default), "memory", or
    /// "noop"/"disabled" to explicitly disable it even with cluster peers set.
    #[serde(default = "default_session_registry_backend")]
    pub session_registry_backend: String,
    /// TTL for session records.  A crashed node's sessions are reclaimed after
    /// this duration by the SWEA sweeper.  Default 3600s (1 hour).
    #[serde(default = "default_session_registry_ttl")]
    pub session_registry_ttl_secs: u64,
    /// Interval at which the per-node heartbeat refreshes owned sessions.
    /// Default 30s.
    #[serde(default = "default_session_registry_heartbeat")]
    pub session_registry_heartbeat_secs: u64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            peers: Vec::new(),
            session_registry_backend: default_session_registry_backend(),
            session_registry_ttl_secs: default_session_registry_ttl(),
            session_registry_heartbeat_secs: default_session_registry_heartbeat(),
        }
    }
}

fn default_session_registry_backend() -> String {
    "db".to_string()
}

fn default_session_registry_ttl() -> u64 {
    3600
}

fn default_session_registry_heartbeat() -> u64 {
    30
}

fn default_max_audio_download_bytes() -> u64 {
    crate::utils::MAX_AUDIO_DOWNLOAD_BYTES
}

fn default_stats_interval() -> u64 {
    5
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
    /// Static files HTTP path prefix (default: "/static")
    pub static_path: Option<String>,
    /// Static API tokens for authenticating to /api endpoints.
    /// Each token has an optional list of scopes (e.g. ["call.control", "recording"]).
    #[serde(default)]
    pub api_tokens: Vec<ApiTokenConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ApiTokenConfig {
    pub token: String,
    #[serde(default)]
    pub scopes: Vec<String>,
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
            static_path: None,
            api_tokens: Vec::new(),
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
        request_uri_field: Option<String>,
        headers: Option<HashMap<String, String>>,
        sip_headers: Option<Vec<String>>,
        /// If set, enables one-shot token auth: when the SIP request carries
        /// this header, the token is forwarded to the HTTP service for
        /// immediate validation (skipping the 401/407 Digest challenge).
        token_header: Option<String>,
        /// HTTP request timeout in milliseconds (applies to both token auth
        /// and Digest password lookup). Default: 5000.
        http_timeout_ms: Option<u64>,
        /// Number of retries on HTTP failure. Default: 1.
        http_retry_count: Option<u32>,
        /// Delay between retries in milliseconds. Default: 500.
        http_retry_delay_ms: Option<u64>,
        /// Token cache TTL in seconds. 0 = disabled. Default: 0.
        token_cache_ttl_secs: Option<u64>,
        /// Maximum token cache entries (LRU eviction). Default: 10000.
        token_cache_size: Option<usize>,
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

pub const DEFAULT_CALL_RECORD_CHANNEL_CAPACITY: usize = 2048;
pub const DEFAULT_CALL_RECORD_BATCH_SIZE: usize = 16;

fn default_call_record_channel_capacity() -> usize {
    DEFAULT_CALL_RECORD_CHANNEL_CAPACITY
}

fn default_call_record_batch_size() -> usize {
    DEFAULT_CALL_RECORD_BATCH_SIZE
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct CallRecordConfig {
    /// Bounded channel capacity used between call producers and the manager.
    #[serde(default = "default_call_record_channel_capacity")]
    pub channel_capacity: usize,
    /// Maximum number of records passed to hooks and the saver at once.
    #[serde(default = "default_call_record_batch_size")]
    pub batch_size: usize,
    #[serde(flatten)]
    pub storage: CallRecordStorageConfig,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum CallRecordStorageConfig {
    Local {
        #[serde(default = "default_call_record_local_root")]
        root: String,
    },
    S3 {
        vendor: S3Vendor,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        #[serde(default)]
        endpoint: Option<String>,
        #[serde(default = "default_call_record_root")]
        root: String,
        /// Deprecated and unused. Recording media upload is configured by `[recording]`.
        with_media: Option<bool>,
        /// Deprecated with `with_media`; accepted for config compatibility.
        keep_media_copy: Option<bool>,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
        /// Deprecated and unused. Recording media upload is configured by `[recording]`.
        with_media: Option<bool>,
        /// Deprecated with `with_media`; accepted for config compatibility.
        keep_media_copy: Option<bool>,
    },
    Database {
        /// Database URL for call records.
        database_url: Option<String>,
        /// Table name for call records (default: "rustpbx_call_records").
        #[serde(default = "default_call_record_table")]
        table_name: String,
        /// When true, don't CREATE TABLE or run migrations on the table.
        #[serde(default)]
        skip_create_table: bool,
        /// Daily rotation mode. For SQLite: daily files ({path}-YYYYMMDD.db).
        /// For other databases: daily tables ({table}_YYYYMMDD).
        #[serde(default)]
        rotate: RotationMode,
    },
}

#[derive(Debug, Deserialize, Clone, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RotationMode {
    #[default]
    None,
    Daily,
}

fn default_call_record_table() -> String {
    "rustpbx_call_records".to_string()
}

fn default_call_record_root() -> String {
    "cdr".to_string()
}

fn default_call_record_local_root() -> String {
    "./config/cdr".to_string()
}

pub use rustpbx_sipflow::config::{
    SipFlowClusterNode, SipFlowConfig, SipFlowSubdirs, SipFlowUploadConfig,
};

#[derive(Debug, Deserialize, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(PartialEq, Default)]
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
    /// Bypass: rewrite SDP but let RTP flow directly between endpoints
    Bypass,
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

fn default_contact_lan_use_bind() -> bool {
    true
}

/// RFC1918, loopback, and link-local ranges used when `local_networks` is unset.
pub fn default_local_networks() -> Vec<IpNet> {
    [
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "::1/128",
        "fe80::/10",
    ]
    .iter()
    .filter_map(|s| s.parse().ok())
    .collect()
}

pub fn parse_local_networks(raw: &[String]) -> Vec<IpNet> {
    if raw.is_empty() {
        return default_local_networks();
    }
    raw.iter().filter_map(|s| s.trim().parse().ok()).collect()
}

#[derive(Clone, Debug, Default)]
pub struct SipContactConfig {
    pub sip_external_ip: Option<String>,
    pub auto_sip_external_ip: Option<String>,
    pub local_networks: Vec<IpNet>,
    pub contact_lan_use_bind: bool,
    pub sip_contact_always_bind: bool,
}

/// Network egress profile (FreeSWITCH-style). Groups RTP/SDP and SIP Contact
/// settings for a logical network path (public WAN, overlay VPN, etc.).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NetworkProfile {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub external_ip: Option<String>,
    pub auto_external_ip: Option<String>,
    pub sip_external_ip: Option<String>,
    pub auto_sip_external_ip: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_networks: Vec<String>,
    #[serde(default = "default_contact_lan_use_bind")]
    pub contact_lan_use_bind: bool,
    #[serde(default)]
    pub sip_contact_always_bind: bool,
    pub bind_ip: Option<String>,
    pub rtp_start_port: Option<u16>,
    pub rtp_end_port: Option<u16>,
}

impl NetworkProfile {
    pub fn sip_contact_config(&self) -> SipContactConfig {
        SipContactConfig {
            sip_external_ip: self.sip_external_ip.clone(),
            auto_sip_external_ip: self.auto_sip_external_ip.clone(),
            local_networks: parse_local_networks(&self.local_networks),
            contact_lan_use_bind: self.contact_lan_use_bind,
            sip_contact_always_bind: self.sip_contact_always_bind,
        }
    }

    pub fn effective_bind_ip<'a>(
        &'a self,
        proxy_bind: &'a str,
        rtp_bind: Option<&'a str>,
    ) -> &'a str {
        self.bind_ip
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| rtp_bind.filter(|s| !s.is_empty()))
            .unwrap_or(proxy_bind)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct RtpConfig {
    pub external_ip: Option<String>,
    pub auto_external_ip: Option<String>,
    pub bind_ip: Option<String>,
    pub start_port: Option<u16>,
    pub end_port: Option<u16>,
    pub webrtc_start_port: Option<u16>,
    pub webrtc_end_port: Option<u16>,
    pub ice_servers: Option<Vec<IceServer>>,
    /// Emit comfort noise instead of digital silence when a leg's egress has
    /// no source (defaults to true).
    #[serde(default = "default_comfort_noise")]
    pub comfort_noise: bool,
    /// Comfort-noise level in dBFS (default -35.0).
    #[serde(default = "default_comfort_noise_level_db")]
    pub comfort_noise_level_db: f32,
}

fn default_comfort_noise() -> bool {
    true
}

fn default_comfort_noise_level_db() -> f32 {
    -35.0
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MediaSection {
    /// Emit comfort noise instead of digital silence when a leg's egress has
    /// no source. Defaults to true.
    #[serde(default = "default_comfort_noise")]
    pub comfort_noise: bool,
    /// Comfort-noise level in dBFS (default -35.0).
    #[serde(default = "default_comfort_noise_level_db")]
    pub comfort_noise_level_db: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// Global recovery for Step IVR when the external provider cannot continue.
///
/// Rules are evaluated in descending `priority` order using the same
/// [`MatchConditions`] semantics as dialplan routes (`from.user`, `to.user`,
/// `header.X-Foo`, …). The first full match wins; otherwise `default` is used.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct IvrFallbackConfig {
    /// IVR name (resolved via `resolve_ivr_file`) when no rule matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<IvrFallbackRule>,
}

impl IvrFallbackConfig {
    /// True when at least one recovery target is configured.
    pub fn is_configured(&self) -> bool {
        self.default.as_ref().is_some_and(|s| !s.is_empty()) || !self.rules.is_empty()
    }
}

/// One match → target IVR entry for [`IvrFallbackConfig`].
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct IvrFallbackRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, rename = "match")]
    pub match_conditions: MatchConditions,
    /// Built-in IVR name to jump to (`toivr:{target}`).
    pub target: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JwtAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    pub secret: String,
    #[serde(default = "default_jwt_user_id_claim")]
    pub user_id_claim: String,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default = "default_jwt_sip_header")]
    pub sip_header_name: String,
    #[serde(default)]
    pub check_local_user: bool,
    #[serde(default = "default_jwt_ws_token_param")]
    pub ws_token_param: String,
    /// Enable dev-console JWT/PhoneAuth token mint endpoints
    /// (`POST /cc/dev/jwt-preview`, `POST /cc/dev/phone-token`).
    /// Defaults to false — production should keep this off.
    #[serde(default)]
    pub dev_mint_enabled: bool,
}

fn default_jwt_user_id_claim() -> String {
    "userId".to_string()
}
fn default_jwt_sip_header() -> String {
    "X-Auth-Token".to_string()
}
fn default_jwt_ws_token_param() -> String {
    "token".to_string()
}

/// SSO login broker — brokers an enterprise SSO login into a native-app
/// deep link. Handlers only mount when `enabled = true` (commerce builds).
#[cfg(feature = "commerce")]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SsoConfig {
    #[serde(default)]
    pub enabled: bool,
    /// URL prefix for the SSO endpoints (`/authorize`, `/callback`, `/token`).
    #[serde(default = "default_sso_base_path")]
    pub base_path: String,
    /// Active upstream provider kind. Phase 1 supports `"jwt"`
    /// (third-party HS256 JWT handoff).
    #[serde(default = "default_sso_provider")]
    pub provider: String,
    /// Full deep-link URL handed back to the native app after login,
    /// e.g. `myapp://callback` or `corp://auth/sso`. The one-time code and
    /// client state are appended as query parameters.
    #[serde(default)]
    pub redirect_url: Option<String>,
    /// JIT-create a local console user on first SSO login (no roles).
    #[serde(default)]
    pub auto_provision: bool,
    /// Authorization-code lifetime. Default 60s.
    #[serde(default = "default_sso_code_ttl")]
    pub code_ttl_secs: u64,
    /// authorize → upstream-login → callback flow lifetime. Default 600s.
    #[serde(default = "default_sso_flow_ttl")]
    pub flow_ttl_secs: u64,
    /// Provider-specific settings; required when provider is "jwt".
    #[serde(default)]
    pub jwt: Option<SsoJwtConfig>,
}

#[cfg(feature = "commerce")]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SsoJwtConfig {
    pub secret: String,
    /// `passthrough` (default) returns the upstream JWT verbatim as
    /// access_token; `minted` issues a rustpbx-signed JWT instead.
    #[serde(default = "default_sso_token_mode")]
    pub token_mode: String,
    /// Claim carrying the enterprise user id (userId / mis_id / sub ...).
    #[serde(default = "default_jwt_user_id_claim")]
    pub user_id_claim: String,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    /// Enterprise login page; PBX appends `state=<..>` and the upstream must
    /// 302 back to `{base}/callback?token=<jwt>&state=<same value>`.
    pub upstream_login_url: String,
    /// access_token TTL for minted mode. Default 3600s.
    #[serde(default = "default_sso_token_ttl")]
    pub token_ttl_secs: u64,
    /// refresh_token TTL for minted mode; 0 disables the refresh grant.
    /// Default 86400s. Ignored in passthrough mode.
    #[serde(default = "default_sso_refresh_ttl")]
    pub refresh_token_ttl_secs: u64,
}

#[cfg(feature = "commerce")]
fn default_sso_base_path() -> String {
    "/sso".to_string()
}
#[cfg(feature = "commerce")]
fn default_sso_provider() -> String {
    "jwt".to_string()
}
#[cfg(feature = "commerce")]
fn default_sso_code_ttl() -> u64 {
    60
}
#[cfg(feature = "commerce")]
fn default_sso_flow_ttl() -> u64 {
    600
}
#[cfg(feature = "commerce")]
fn default_sso_token_mode() -> String {
    "passthrough".to_string()
}
#[cfg(feature = "commerce")]
fn default_sso_token_ttl() -> u64 {
    3600
}
#[cfg(feature = "commerce")]
fn default_sso_refresh_ttl() -> u64 {
    86400
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
    pub tls_ca_certificates: Option<String>,
    pub udp_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_ports: Option<Vec<u16>>,
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
    pub max_registrar_expires: Option<u32>,
    pub ensure_user: Option<bool>,
    #[serde(default = "default_user_backends")]
    pub user_backends: Vec<UserBackendConfig>,
    #[serde(default)]
    pub locator: LocatorConfig,
    pub locator_webhook: Option<LocatorWebhookConfig>,
    #[serde(default)]
    pub media_proxy: MediaProxyMode,
    /// Global failure-tone profile (`[proxy.audio_profile]`): the base
    /// `RingbackAudio` applied to every call. Per-trunk `ringback` overrides
    /// individual fields. When unset, built-in defaults are used. An explicitly
    /// declared but empty table disables the built-in failure audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_profile: Option<crate::proxy::routing::RingbackAudio>,
    pub audio_codecs: Option<Vec<String>>,
    #[serde(default)]
    pub frequency_limiter: Option<String>,
    #[serde(default)]
    pub realms: Option<Vec<String>>,
    #[serde(default = "default_sip_worker_threads")]
    pub sip_worker_threads: usize,
    #[serde(default = "default_media_worker_threads")]
    pub media_worker_threads: usize,
    pub ws_handler: Option<String>,
    pub ami_path: Option<String>,
    pub rwi_path: Option<String>,
    pub ice_servers_path: Option<String>,
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
    #[serde(default = "default_rtp_timeout")]
    pub rtp_timeout: Option<u64>,
    #[serde(default = "default_session_cmd_channel_capacity")]
    pub session_cmd_channel_capacity: usize,
    #[serde(default = "default_session_state_channel_capacity")]
    pub session_state_channel_capacity: usize,
    #[serde(default)]
    pub queues: HashMap<String, RouteQueueConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues_files: Vec<String>,
    #[serde(default = "default_enable_latching")]
    pub enable_latching: bool,
    #[serde(default = "default_latching_probation_max_packets")]
    pub latching_probation_max_packets: Option<u8>,
    #[serde(default)]
    pub trunks: HashMap<String, TrunkConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trunks_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ivr_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ivr_files: Vec<String>,
    /// Global Step-IVR recovery: when `/step` or `/fail` cannot continue the
    /// current provider session, match `from`/`to`/`headers` rules and jump to
    /// a built-in IVR; if no rule matches, use `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ivr_fallback: Option<IvrFallbackConfig>,
    #[serde(default)]
    pub recording: Option<RecordingPolicy>,
    /// Transcription settings (`[proxy.transcript]`). Only the `remote`
    /// sub-table is typed here (live streaming ASR); the offline sensevoice
    /// fields are read ad-hoc from the raw TOML by the transcript addon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<TranscriptSection>,
    #[serde(default = "default_generated_config_dir")]
    pub generated_dir: String,
    #[serde(default)]
    pub generated_db: bool,
    #[serde(default = "default_nat_fix")]
    pub nat_fix: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addons: Option<Vec<String>>,
    #[serde(default = "default_passthrough_failure")]
    pub passthrough_failure: bool,
    /// Video codecs allowed for pass-through relay. Only H264 and VP8 are
    /// accepted; both are enabled when this setting is omitted.
    #[serde(
        default = "default_video_codecs",
        deserialize_with = "deserialize_video_codecs"
    )]
    pub video_codecs: Vec<String>,
    #[serde(default = "default_dialog_auth_cache")]
    pub dialog_auth_cache: Option<AuthCacheConfig>,
    #[serde(default)]
    pub blind_transfer_use_refer: bool,
    /// When enabled, app/transfer/RWI-originated calls whose target is not a
    /// registered internal contact are routed through the route table
    /// (match/rewrite/trunk selection) just like inbound calls. Default off —
    /// legacy direct-dial behavior is preserved unless explicitly enabled.
    #[serde(default)]
    pub route_originated_calls: bool,

    /// When enabled, direct extension-to-extension (P2P) calls to a callee
    /// with multiple registered devices ring ALL of them in parallel (first to
    /// answer wins; the remaining forks are cancelled). When disabled, only the
    /// most recently registered device is rung. Default: enabled.
    #[serde(default = "default_parallel_fork")]
    pub parallel_fork: bool,

    /// Global default max ring/setup time (seconds) before a no-answer call is
    /// rejected with 408. `0` or unset disables the ring timeout entirely — the
    /// call rings until answered or the caller cancels. Per-trunk and per-route
    /// `max_ring_time` override this value. Hot-reloadable (new calls only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ring_time: Option<u64>,

    #[serde(default)]
    pub dos_enabled: bool,
    #[serde(default = "default_dos_max_cps")]
    pub dos_max_cps_per_ip: u32,
    #[serde(default = "default_dos_max_concurrent")]
    pub dos_max_concurrent_per_ip: u32,
    #[serde(default = "default_dos_scan_threshold")]
    pub dos_scan_probe_threshold: u32,
    #[serde(default = "default_dos_scan_block_secs")]
    pub dos_scan_block_duration_secs: u64,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<String>,

    #[serde(default = "default_uri_max_length")]
    pub uri_max_length: usize,
    #[serde(default)]
    pub uri_reject_malformed: bool,
    #[serde(default)]
    pub emergency: Option<EmergencyConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtc_cname: Option<String>,
    #[serde(default)]
    pub jwt_auth: Option<JwtAuthConfig>,
    #[serde(default)]
    pub hold_music: Option<String>,
}

/// Emergency number routing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EmergencyConfig {
    pub enabled: bool,
    #[serde(default = "default_emergency_numbers")]
    pub numbers: Vec<String>,
    pub emergency_trunk: String,
}

fn default_emergency_numbers() -> Vec<String> {
    vec![
        "110".to_string(),
        "119".to_string(),
        "120".to_string(),
        "122".to_string(),
        "911".to_string(),
        "999".to_string(),
    ]
}

fn default_dos_max_cps() -> u32 {
    100
}
fn default_dos_max_concurrent() -> u32 {
    500
}
fn default_dos_scan_threshold() -> u32 {
    50
}
fn default_dos_scan_block_secs() -> u64 {
    600
}
fn default_uri_max_length() -> usize {
    256
}
fn default_parallel_fork() -> bool {
    true
}
fn default_session_cmd_channel_capacity() -> usize {
    256
}
fn default_session_state_channel_capacity() -> usize {
    256
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn default_sip_worker_threads() -> usize {
    let n = available_parallelism();
    // Half the cores, capped at 12. The old fixed 4 starved under load: with
    // recording enabled the SIP runtime accumulated a 90k-task backlog at
    // >3.3k concurrent calls while all 4 workers sat idle in futex waits
    // (lock/queue convoy), surfacing as 408 INVITE timeouts and multi-second
    // DB-pool queueing. SIP and media runtimes never peak simultaneously, so
    // oversubscribing both against the core count is safe.
    (n / 2).clamp(2, 12)
}

fn default_media_worker_threads() -> usize {
    let n = available_parallelism();
    let sip = default_sip_worker_threads();
    if n > sip { n - sip } else { 1 }
}

fn default_auth_cache_size() -> usize {
    10000
}

fn default_auth_cache_ttl_seconds() -> u64 {
    3600 // 1 hour
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthCacheConfig {
    /// Whether to enable in-dialog authentication caching. Default: true.
    #[serde(default = "default_auth_cache_enabled")]
    pub enabled: bool,
    /// Maximum number of cached authenticated dialogs (LRU cache size). Default: 10000.
    #[serde(default = "default_auth_cache_size")]
    pub cache_size: usize,
    /// TTL (time-to-live) in seconds for cached entries. Default: 3600.
    #[serde(default = "default_auth_cache_ttl_seconds")]
    pub ttl_seconds: u64,
}

fn default_auth_cache_enabled() -> bool {
    true
}

fn default_dialog_auth_cache() -> Option<AuthCacheConfig> {
    Some(AuthCacheConfig::default())
}

impl Default for AuthCacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_auth_cache_enabled(),
            cache_size: default_auth_cache_size(),
            ttl_seconds: default_auth_cache_ttl_seconds(),
        }
    }
}

fn default_passthrough_failure() -> bool {
    true
}

pub(crate) fn default_video_codecs() -> Vec<String> {
    vec!["H264".to_string(), "VP8".to_string()]
}

fn deserialize_video_codecs<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let codecs = Vec::<String>::deserialize(deserializer)?;
    if let Some(codec) = codecs
        .iter()
        .find(|codec| !codec.eq_ignore_ascii_case("H264") && !codec.eq_ignore_ascii_case("VP8"))
    {
        return Err(serde::de::Error::custom(format!(
            "unsupported video codec `{codec}`; supported codecs are H264 and VP8"
        )));
    }
    Ok(codecs)
}

#[derive(Default)]
pub struct DialplanHints {
    pub enable_recording: Option<bool>,
    pub recording: Option<RecordingPolicy>,
    pub bypass_media: Option<bool>,
    pub max_duration: Option<std::time::Duration>,
    pub max_ring_time: Option<std::time::Duration>,
    pub enable_sipflow: Option<bool>,
    pub allow_codecs: Option<Vec<String>>,
    pub extensions: http::Extensions,
    pub disable_ice_servers: Option<bool>,
    /// Media mode override from trunk config
    pub media_mode: Option<MediaProxyMode>,
    /// Video policy from trunk config
    pub video_policy: Option<crate::proxy::routing::VideoPolicy>,
    /// Per-trunk override for the advertised external IP (SDP c=/o= and ICE).
    pub external_ip: Option<String>,
    /// Per-trunk override for the local bind IP.
    pub bind_ip: Option<String>,
    /// Resolved network profile id stamped during routing (from trunk.profile).
    pub network_profile_id: Option<String>,
    /// Per-trunk ringback/early-media audio configuration
    pub ringback: Option<crate::proxy::routing::RingbackAudio>,
    /// Concurrency slots acquired during routing policy enforcement. The
    /// session releases them on hangup to avoid leaking the concurrency budget.
    pub concurrency_holds: Vec<crate::call::policy::ConcurrencyHold>,
    /// All concurrent-call permits acquired during routing.
    /// Tenant, carrier, and trunk permits share this call-lifetime lease.
    pub concurrent_call_lease: crate::call::concurrent_call_limiter::ConcurrentCallLease,
}

impl std::fmt::Debug for DialplanHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialplanHints")
            .field("enable_recording", &self.enable_recording)
            .field("recording", &self.recording)
            .field("bypass_media", &self.bypass_media)
            .field("max_duration", &self.max_duration)
            .field("max_ring_time", &self.max_ring_time)
            .field("enable_sipflow", &self.enable_sipflow)
            .field("disable_ice_servers", &self.disable_ice_servers)
            .field("media_mode", &self.media_mode)
            .field("video_policy", &self.video_policy)
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
        hints: Option<DialplanHints>,
    },
    NotHandled(InviteOption, Option<DialplanHints>),
    Abort(StatusCode, Option<String>),
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AmiConfig {
    pub allows: Option<Vec<String>>,
}

/// `[graceful_shutdown]` — drain-then-exit behaviour shared by the AMI
/// `/shutdown` endpoint and SIGTERM/SIGINT (docker stop / systemctl stop).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GracefulShutdownConfig {
    /// Fallback max seconds to wait for active calls to finish before
    /// force exiting, when the caller did not give an explicit timeout
    /// (signal-triggered drains, AMI `/shutdown` without `timeout_secs`).
    #[serde(default = "default_graceful_shutdown_drain_timeout")]
    pub drain_timeout_secs: u64,
    /// Start the process already draining (maintenance hold: replies
    /// 503/500 to everything out-of-dialog, never takes traffic until a
    /// restart without the flag). Default `false`.
    #[serde(default)]
    pub enabled_at_startup: bool,
}

impl Default for GracefulShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_secs: default_graceful_shutdown_drain_timeout(),
            enabled_at_startup: false,
        }
    }
}

fn default_graceful_shutdown_drain_timeout() -> u64 {
    300
}

impl GracefulShutdownConfig {
    /// Effective drain timeout: `None` (wait indefinitely) only when the
    /// user explicitly configured zero.
    pub fn effective_timeout(&self) -> Option<std::time::Duration> {
        (self.drain_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(self.drain_timeout_secs))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboundConfig {
    #[serde(default = "default_outbound_enabled")]
    pub enabled: bool,
    #[serde(default = "default_outbound_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_outbound_ring_timeout")]
    pub default_ring_timeout: u64,
    #[serde(default = "default_outbound_answer_timeout")]
    pub default_answer_timeout: u64,
    #[serde(default = "default_outbound_webhook_timeout")]
    pub default_webhook_timeout: u64,
}

impl Default for OutboundConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: 100,
            default_ring_timeout: 30,
            default_answer_timeout: 60,
            default_webhook_timeout: 5,
        }
    }
}

fn default_outbound_enabled() -> bool {
    true
}
fn default_outbound_max_concurrent() -> usize {
    100
}
fn default_outbound_ring_timeout() -> u64 {
    30
}
fn default_outbound_answer_timeout() -> u64 {
    60
}
fn default_outbound_webhook_timeout() -> u64 {
    5
}

impl AmiConfig {
    pub fn is_allowed(&self, addr: &str) -> bool {
        if let Some(allows) = &self.allows {
            let ip = addr.parse::<IpAddr>().ok();

            allows.iter().any(|allow| {
                let allow = allow.trim();

                allow == addr
                    || allow == "*"
                    || ip.is_some_and(|ip| {
                        allow
                            .parse::<IpNet>()
                            .is_ok_and(|network| network.contains(&ip))
                    })
            })
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

    /// The SIP realm for constructed URIs: first configured realm, else the
    /// proxy's advertised address.
    pub fn first_realm(&self) -> String {
        self.realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| self.addr.clone())
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
                && !first.is_empty()
            {
                return first.clone();
            }
        }

        if requested.is_empty() {
            normalized.to_string()
        } else {
            requested.to_string()
        }
    }

    pub fn use_db_config(&self) -> bool {
        self.generated_db
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
        if let Some(dir) = self
            .ivr_dir
            .as_ref()
            .map(|path| path.trim())
            .filter(|path| !path.is_empty())
        {
            PathBuf::from(dir)
        } else {
            self.generated_root_dir().join("ivr")
        }
    }

    pub fn generated_acl_dir(&self) -> PathBuf {
        self.generated_root_dir().join("acl")
    }

    pub fn generated_cc_dir(&self) -> PathBuf {
        self.generated_root_dir().join("cc")
    }

    pub fn all_udp_ports(&self) -> Vec<u16> {
        let mut ports = self.udp_port.into_iter().collect::<Vec<_>>();
        if let Some(extra) = &self.udp_ports {
            for p in extra {
                if !ports.contains(p) {
                    ports.push(*p);
                }
            }
        }
        ports
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
                "presence".to_string(),
            ]),
            useragent: default_useragent(),
            callid_suffix: default_callid_suffix(),
            t1_timer: None,
            t1x64_timer: None,
            ssl_private_key: None,
            ssl_certificate: None,
            tls_ca_certificates: None,
            udp_port: Some(5060),
            udp_ports: None,
            tcp_port: None,
            tls_port: None,
            ws_port: None,
            max_concurrency: None,
            registrar_expires: Some(30),
            max_registrar_expires: Some(50),
            ensure_user: Some(true),
            enable_latching: true,
            latching_probation_max_packets: default_latching_probation_max_packets(),
            user_backends: default_user_backends(),
            locator: LocatorConfig::default(),
            locator_webhook: None,
            media_proxy: MediaProxyMode::default(),
            audio_profile: None,
            audio_codecs: None,
            frequency_limiter: None,
            realms: Some(vec![]),
            ws_handler: None,
            ami_path: None,
            rwi_path: None,
            ice_servers_path: None,
            http_router: None,
            routes_files: Vec::new(),
            acl_files: Vec::new(),
            routes: None,
            session_timer: true,
            session_timer_always: false,
            session_expires: Some(600),
            rtp_timeout: default_rtp_timeout(),
            session_cmd_channel_capacity: default_session_cmd_channel_capacity(),
            session_state_channel_capacity: default_session_state_channel_capacity(),
            queues: HashMap::new(),
            queues_files: Vec::new(),
            trunks: HashMap::new(),
            trunks_files: Vec::new(),
            queue_dir: None,
            ivr_dir: None,
            ivr_files: Vec::new(),
            ivr_fallback: None,
            recording: None,
            transcript: None,
            generated_dir: default_generated_config_dir(),
            generated_db: false,
            nat_fix: true,
            addons: None,
            passthrough_failure: true,
            video_codecs: default_video_codecs(),
            dialog_auth_cache: default_dialog_auth_cache(),
            blind_transfer_use_refer: false,
            route_originated_calls: false,
            parallel_fork: default_parallel_fork(),
            max_ring_time: None,
            dos_enabled: false,
            dos_max_cps_per_ip: default_dos_max_cps(),
            dos_max_concurrent_per_ip: default_dos_max_concurrent(),
            dos_scan_probe_threshold: default_dos_scan_threshold(),
            dos_scan_block_duration_secs: default_dos_scan_block_secs(),
            trusted_proxies: Vec::new(),
            uri_max_length: default_uri_max_length(),
            uri_reject_malformed: false,
            emergency: None,
            contact_username: None,
            rtc_cname: None,
            jwt_auth: None,
            hold_music: None,
            sip_worker_threads: default_sip_worker_threads(),
            media_worker_threads: default_media_worker_threads(),
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
        Self {
            channel_capacity: default_call_record_channel_capacity(),
            batch_size: default_call_record_batch_size(),
            storage: CallRecordStorageConfig::Local {
                #[cfg(target_os = "windows")]
                root: "./config/cdr".to_string(),
                #[cfg(not(target_os = "windows"))]
                root: "./config/cdr".to_string(),
            },
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
            stats_log: None,
            stats_interval: default_stats_interval(),
            log_rotation: String::new(),
            http_access_skip_paths: Vec::new(),
            proxy: ProxyConfig::default(),
            callrecord: None,
            ice_servers: None,
            media: None,
            ami: Some(AmiConfig::default()),
            graceful_shutdown: None,
            external_ip: None,
            auto_external_ip: None,
            sip_external_ip: None,
            auto_sip_external_ip: None,
            local_networks: Vec::new(),
            contact_lan_use_bind: default_contact_lan_use_bind(),
            sip_contact_always_bind: false,
            network_profiles: Vec::new(),
            default_network_profile: None,
            rtp_start_port: default_config_rtp_start_port(),
            rtp_end_port: default_config_rtp_end_port(),
            webrtc_port_start: default_config_webrtc_start_port(),
            webrtc_port_end: default_config_webrtc_end_port(),
            #[cfg(feature = "console")]
            console: None,
            rwi: None,
            database_url: default_database_url(),
            database_pool: DatabasePoolConfig::default(),
            recording: None,
            demo_mode: false,
            storage: None,
            sipflow: None,
            #[cfg(feature = "commerce")]
            licenses: None,
            #[cfg(feature = "commerce")]
            sso: None,
            rwi_webhook: None,
            cluster: None,
            outbound: None,
            max_audio_download_bytes: default_max_audio_download_bytes(),
        }
    }
}

impl Config {
    /// Drain timeout used by signal-triggered graceful shutdowns and by
    /// AMI `/shutdown` calls that omit `timeout_secs`. Section absent →
    /// default 300s; `drain_timeout_secs = 0` → `None` (wait forever).
    pub fn graceful_shutdown_timeout(&self) -> Option<std::time::Duration> {
        match &self.graceful_shutdown {
            Some(c) => c.effective_timeout(),
            None => Some(std::time::Duration::from_secs(300)),
        }
    }

    /// Whether the process should boot already draining (maintenance
    /// hold). See `GracefulShutdownConfig::enabled_at_startup`.
    pub fn start_draining_at_startup(&self) -> bool {
        self.graceful_shutdown
            .as_ref()
            .is_some_and(|c| c.enabled_at_startup)
    }

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

    pub async fn load_async(path: &str) -> Result<Self, Error> {
        let contents = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("{}: {}", e, path))?;
        let mut config: Self = toml::from_str(&contents)?;
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
        let media = self.media.as_ref();
        RtpConfig {
            external_ip: self.external_ip.clone(),
            auto_external_ip: self.auto_external_ip.clone(),
            bind_ip: Some(self.proxy.addr.clone()),
            start_port: self.rtp_start_port,
            end_port: self.rtp_end_port,
            webrtc_start_port: self.webrtc_port_start,
            webrtc_end_port: self.webrtc_port_end,
            // Shared-secret entries are issued on demand for browsers. Avoid
            // caching expiring credentials in the PBX's long-lived RTP config.
            ice_servers: self.ice_servers.as_ref().map(|servers| {
                servers
                    .iter()
                    .filter(|entry| entry.secrete.is_none())
                    .map(|entry| entry.server.clone())
                    .collect()
            }),
            comfort_noise: media
                .map(|m| m.comfort_noise)
                .unwrap_or_else(default_comfort_noise),
            comfort_noise_level_db: media
                .map(|m| m.comfort_noise_level_db)
                .unwrap_or_else(default_comfort_noise_level_db),
        }
    }

    /// Resolve browser ICE configuration at the current Unix time (seconds).
    /// Static entries are preserved; shared-secret TURN credentials are minted
    /// per request and are not cached in the server-side RTP configuration.
    pub fn browser_ice_servers(&self, now: u64) -> Result<Vec<IceServer>> {
        let mut servers = Vec::new();
        for entry in self.ice_servers.iter().flatten() {
            let Some(secret) = &entry.secrete else {
                servers.push(entry.server.clone());
                continue;
            };
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            use hmac::{Hmac, KeyInit, Mac};

            let turn = &entry.server;
            let user = turn.username.as_deref().unwrap_or("rustpbx");
            let lifetime = entry.lifetime.unwrap_or(3600);
            anyhow::ensure!(!secret.is_empty(), "ice_servers.secrete must not be empty");
            anyhow::ensure!(
                turn.credential.is_none(),
                "ice_servers cannot combine secrete and credential"
            );
            anyhow::ensure!(
                turn.credential_type == rustrtc::IceCredentialType::Password,
                "ice_servers.secrete requires password credentials"
            );
            anyhow::ensure!(
                !turn.urls.is_empty()
                    && turn.urls.iter().all(|url| {
                        url.strip_prefix("turn:")
                            .or_else(|| url.strip_prefix("turns:"))
                            .is_some_and(|host| !host.is_empty())
                    }),
                "ice_servers with secrete must contain TURN URLs"
            );
            anyhow::ensure!(
                !user.is_empty() && !user.contains(':'),
                "ice_servers with secrete require a nonempty username suffix without a colon"
            );
            anyhow::ensure!(lifetime > 0, "ice_servers.lifetime must be positive");
            let expires = now
                .checked_add(lifetime)
                .ok_or_else(|| anyhow::anyhow!("ice_servers.lifetime overflows expiry"))?;
            let username = format!("{}:{}", expires, user);
            let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes())?;
            mac.update(username.as_bytes());
            // TURN REST passwords use standard padded Base64, as in coturn.
            let credential = STANDARD.encode(mac.finalize().into_bytes());
            servers.push(turn.clone().with_credential(username, credential));
        }
        Ok(servers)
    }

    pub fn sip_contact_config(&self) -> SipContactConfig {
        SipContactConfig {
            sip_external_ip: self.sip_external_ip.clone(),
            auto_sip_external_ip: self.auto_sip_external_ip.clone(),
            local_networks: parse_local_networks(&self.local_networks),
            contact_lan_use_bind: self.contact_lan_use_bind,
            sip_contact_always_bind: self.sip_contact_always_bind,
        }
    }

    pub fn synthetic_default_network_profile(&self) -> NetworkProfile {
        NetworkProfile {
            id: "default".to_string(),
            label: Some("Default".to_string()),
            description: Some("Derived from top-level RTP/SIP Contact settings".to_string()),
            external_ip: self.external_ip.clone(),
            auto_external_ip: self.auto_external_ip.clone(),
            sip_external_ip: self.sip_external_ip.clone(),
            auto_sip_external_ip: self.auto_sip_external_ip.clone(),
            local_networks: self.local_networks.clone(),
            contact_lan_use_bind: self.contact_lan_use_bind,
            sip_contact_always_bind: self.sip_contact_always_bind,
            bind_ip: None,
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
        }
    }

    pub fn effective_network_profiles(&self) -> Vec<NetworkProfile> {
        if self.network_profiles.is_empty() {
            return vec![self.synthetic_default_network_profile()];
        }
        self.network_profiles.clone()
    }

    pub fn default_network_profile_id(&self) -> String {
        self.default_network_profile
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| {
                self.network_profiles
                    .first()
                    .map(|p| p.id.clone())
                    .unwrap_or_else(|| "default".to_string())
            })
    }

    pub fn network_profile(&self, id: &str) -> Option<NetworkProfile> {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(found) = self
            .network_profiles
            .iter()
            .find(|p| p.id == trimmed)
            .cloned()
        {
            return Some(found);
        }
        if trimmed == "default" && self.network_profiles.is_empty() {
            return Some(self.synthetic_default_network_profile());
        }
        None
    }

    pub fn resolve_trunk_network_profile(
        &self,
        trunk: &crate::proxy::routing::TrunkConfig,
    ) -> NetworkProfile {
        if let Some(ref id) = trunk.profile {
            if let Some(profile) = self.network_profile(id) {
                return profile;
            }
        }
        self.network_profile(&self.default_network_profile_id())
            .unwrap_or_else(|| self.synthetic_default_network_profile())
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

    /// Returns the configured static files HTTP path prefix.
    /// Defaults to "/static" when not configured.
    #[cfg(feature = "console")]
    pub fn static_path(&self) -> String {
        self.console
            .as_ref()
            .and_then(|c| c.static_path.clone())
            .unwrap_or_else(|| "/static".to_string())
    }

    /// Returns the configured static files HTTP path prefix.
    /// Defaults to "/static" when the console feature is not compiled.
    #[cfg(not(feature = "console"))]
    pub fn static_path(&self) -> String {
        "/static".to_string()
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_turn_rest_ice_servers_match_miuturn() {
        let config: Config = toml::from_str(
            r#"
            [[ice_servers]]
            urls = ["stun:stun.example.com:3478"]
            [[ice_servers]]
            urls = ["turn:turn.example.com:3478", "turns:turn.example.com:5349"]
            secrete = "interop-secret"
            [[ice_servers]]
            urls = ["turn:static.example.com:3478"]
            username = "static-user"
            credential = "static-password"
            [[ice_servers]]
            urls = ["turn:second.example.com:3478"]
            secrete = "second-secret"
            username = "agent"
            lifetime = 120
            [proxy]
            addr = "127.0.0.1"
        "#,
        )
        .unwrap();
        let servers = config.browser_ice_servers(4_102_441_200).unwrap();
        assert_eq!(servers.len(), 4);
        assert_eq!(
            servers[0],
            IceServer::new(vec!["stun:stun.example.com:3478".to_string()])
        );
        assert_eq!(
            servers[2],
            IceServer::new(vec!["turn:static.example.com:3478".to_string()])
                .with_credential("static-user", "static-password")
        );
        assert_eq!(
            servers[1].urls,
            vec!["turn:turn.example.com:3478", "turns:turn.example.com:5349"]
        );
        assert_eq!(servers[1].username.as_deref(), Some("4102444800:rustpbx"));
        // Independent HMAC-SHA1 test vector, also checked by miuturn's auth test.
        assert_eq!(
            servers[1].credential.as_deref(),
            Some("+dOIp7n9DHzIT8uNfVz8tNFZoLs=")
        );
        assert_eq!(servers[3].urls, vec!["turn:second.example.com:3478"]);
        assert_eq!(servers[3].username.as_deref(), Some("4102441320:agent"));
        assert_eq!(
            servers[3].credential.as_deref(),
            Some("N0aMpuJsuGx1vnor9b2IygKfbeE=")
        );
        assert_eq!(
            config.rtp_config().ice_servers,
            Some(vec![servers[0].clone(), servers[2].clone()])
        );
        let refreshed = config.browser_ice_servers(4_102_441_201).unwrap();
        assert_eq!(refreshed[1].username.as_deref(), Some("4102444801:rustpbx"));
        assert_ne!(refreshed[1].credential, servers[1].credential);
        assert!(!format!("{:?}", config.ice_servers).contains("interop-secret"));
        assert!(!format!("{:?}", config.ice_servers).contains("second-secret"));

        let saved = toml::to_string(&config).unwrap();
        assert!(!saved.contains("turn_rest"));
        let restored: Config = toml::from_str(&saved).unwrap();
        assert_eq!(
            restored.browser_ice_servers(4_102_441_200).unwrap(),
            servers
        );
    }

    #[test]
    fn test_turn_rest_static_ice_servers_unchanged() {
        let config = Config::default();
        assert!(config.browser_ice_servers(0).unwrap().is_empty());
        assert!(config.rtp_config().ice_servers.is_none());
        let config: Config = toml::from_str(
            r#"
            [[ice_servers]]
            urls = ["stun:stun.example.com:3478"]
            [[ice_servers]]
            urls = ["turn:static.example.com:3478"]
            username = "user"
            credential = "password"
            credential_type = "oauth"
            [proxy]
            addr = "127.0.0.1"
        "#,
        )
        .unwrap();
        let expected = vec![
            IceServer::new(vec!["stun:stun.example.com:3478".to_string()]),
            IceServer::new(vec!["turn:static.example.com:3478".to_string()])
                .with_credential("user", "password")
                .credential_type(rustrtc::IceCredentialType::Oauth),
        ];
        assert_eq!(config.browser_ice_servers(0).unwrap(), expected);
        assert_eq!(config.rtp_config().ice_servers, Some(expected));
    }

    #[test]
    fn test_turn_rest_custom_lifetime_and_invalid_config() {
        let settings = r#"
            urls = ["turn:turn.example.com:3478"]
            secrete = "private-secret"
            username = "agent"
            lifetime = 120
        "#;
        let turn: IceServerConfig = toml::from_str(settings).unwrap();
        let mut config = Config {
            ice_servers: Some(vec![turn]),
            ..Config::default()
        };
        assert_eq!(
            config.browser_ice_servers(1000).unwrap()[0]
                .username
                .as_deref(),
            Some("1120:agent")
        );
        assert_eq!(config.rtp_config().ice_servers, Some(vec![]));
        assert!(config.browser_ice_servers(u64::MAX).is_err());
        for invalid_setting in [
            "secrete = ''",
            "urls = []",
            "urls = ['stun:example.com']",
            "urls = ['turn:']",
            "username = ''",
            "username = 'agent:extra'",
            "lifetime = 0",
            "credential = 'static-password'",
            "credential_type = 'oauth'",
        ] {
            let mut fields: toml::Table = toml::from_str(settings).unwrap();
            fields.extend(toml::from_str::<toml::Table>(invalid_setting).unwrap());
            config.ice_servers = Some(vec![fields.try_into().unwrap()]);
            let error = config.browser_ice_servers(1000).unwrap_err().to_string();
            assert!(!error.contains("private-secret"));
        }
    }

    #[test]
    fn test_video_codecs_default_and_override() {
        let default_config: ProxyConfig = toml::from_str("addr = \"::\"").unwrap();
        assert_eq!(default_config.video_codecs, vec!["H264", "VP8"]);

        let configured: ProxyConfig = toml::from_str(
            r#"
            addr = "::"
            video_codecs = ["H264", "VP8"]
            "#,
        )
        .unwrap();
        assert_eq!(configured.video_codecs, vec!["H264", "VP8"]);

        let unsupported = toml::from_str::<ProxyConfig>(
            r#"
            addr = "::"
            video_codecs = ["H265"]
            "#,
        )
        .expect_err("unsupported video codec must fail configuration parsing");
        assert!(
            unsupported
                .to_string()
                .contains("supported codecs are H264 and VP8")
        );
    }

    #[test]
    fn test_all_udp_ports_preserves_default_primary_port() {
        let config = ProxyConfig::default();

        assert_eq!(config.all_udp_ports(), vec![5060]);
    }

    #[test]
    fn test_recording_type_sipflow_and_file_media_helpers() {
        assert!(crate::config::RecordingType::Local.is_file_media());
        assert!(crate::config::RecordingType::Http.is_file_media());
        assert!(crate::config::RecordingType::S3.is_file_media());
        assert!(!crate::config::RecordingType::Sipflow.is_file_media());

        let policy: RecordingPolicy =
            toml::from_str("enabled = true\ntype = \"sipflow\"\n").unwrap();
        assert_eq!(
            policy.effective_recording_type(),
            crate::config::RecordingType::Sipflow
        );
        assert!(!policy.uploads_recording());

        let policy: RecordingPolicy = toml::from_str("enabled = true\ntype = \"local\"\n").unwrap();
        assert!(policy.uploads_recording());
        assert_eq!(
            policy.new_recording_config().recording_type,
            crate::config::RecordingType::Local
        );
    }

    #[test]
    fn test_all_udp_ports_is_empty_when_udp_configuration_is_omitted() {
        let config: ProxyConfig = toml::from_str(
            r#"
            addr = "::"
            tls_port = 5061
            "#,
        )
        .unwrap();

        assert_eq!(config.udp_port, None);
        assert_eq!(config.udp_ports, None);
        assert!(config.all_udp_ports().is_empty());
    }

    #[test]
    fn test_all_udp_ports_uses_explicit_additional_ports_without_primary() {
        let mut config = ProxyConfig::default();
        config.udp_port = None;
        config.udp_ports = Some(vec![5062, 5064, 5062]);

        assert_eq!(config.all_udp_ports(), vec![5062, 5064]);
    }

    #[test]
    fn test_all_udp_ports_combines_primary_and_unique_additional_ports() {
        let mut config = ProxyConfig::default();
        config.udp_port = Some(5060);
        config.udp_ports = Some(vec![5060, 5062, 5064, 5062]);

        assert_eq!(config.all_udp_ports(), vec![5060, 5062, 5064]);
    }

    #[test]
    fn test_proxy_tls_ca_certificates_path_is_parsed() {
        let config: ProxyConfig = toml::from_str(
            r#"
            addr = "0.0.0.0"
            tls_port = 5061
            tls_ca_certificates = "/etc/ssl/certs/ca-certificates.crt"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.tls_ca_certificates.as_deref(),
            Some("/etc/ssl/certs/ca-certificates.crt")
        );
    }

    #[test]
    fn test_callrecord_batch_config() {
        let callrecord: CallRecordConfig = toml::from_str(
            r#"
            type = "http"
            url = "https://example.com/cdr"
            channel_capacity = 4096
            batch_size = 128
            "#,
        )
        .unwrap();
        assert_eq!(callrecord.channel_capacity, 4096);
        assert_eq!(callrecord.batch_size, 128);
        assert!(matches!(
            callrecord.storage,
            CallRecordStorageConfig::Http { .. }
        ));

        let defaulted: CallRecordConfig = toml::from_str(
            r#"
            type = "local"
            root = "./cdr"
            "#,
        )
        .unwrap();
        assert_eq!(
            defaulted.channel_capacity,
            DEFAULT_CALL_RECORD_CHANNEL_CAPACITY
        );
        assert_eq!(defaulted.batch_size, DEFAULT_CALL_RECORD_BATCH_SIZE);
    }

    #[test]
    fn test_full_config_parses_callrecord_s3() {
        let toml_str = r#"
            http_addr = "0.0.0.0:8080"

            [proxy]
            addr = "0.0.0.0"

            [callrecord]
            type = "s3"
            vendor = "aliyun"
            bucket = "my-bucket"
            region = "oss-cn-hangzhou"
            access_key = "ak"
            secret_key = "sk"
        "#;
        let config: Config = toml::from_str(toml_str).expect("Config should parse");
        assert!(
            config.callrecord.is_some(),
            "callrecord should be Some when [callrecord] section is present"
        );
        assert!(matches!(
            config.callrecord.as_ref().unwrap().storage,
            CallRecordStorageConfig::S3 { .. }
        ));
    }

    #[test]
    fn test_full_config_without_callrecord() {
        let toml_str = r#"
            http_addr = "0.0.0.0:8080"

            [proxy]
            addr = "0.0.0.0"
        "#;
        let config: Config = toml::from_str(toml_str).expect("Config should parse");
        assert!(
            config.callrecord.is_none(),
            "callrecord should be None when [callrecord] section is absent"
        );
    }

    #[test]
    fn test_recording_signed_url_expiry_parsing() {
        // Explicit value parses and survives round-trip.
        let toml_str = r#"
            [proxy]
            addr = "0.0.0.0"

            [recording]
            enabled = true
            type = "s3"
            bucket = "my-bucket"
            region = "oss-cn-hangzhou"
            access_key = "ak"
            secret_key = "sk"
            endpoint = "https://oss-cn-hangzhou.aliyuncs.com"
            signed_url_expiry_secs = 3600
        "#;
        let config: Config = toml::from_str(toml_str).expect("Config should parse");
        let policy = config.recording.expect("recording policy should parse");
        assert_eq!(policy.signed_url_expiry_secs, Some(3600));
        assert_eq!(policy.effective_signed_url_expiry_secs(), 3600);

        // Absent value falls back to the 24h default.
        let config: Config =
            toml::from_str("[proxy]\naddr = \"0.0.0.0\"\n[recording]\nenabled = true\n")
                .expect("Config should parse");
        let policy = config.recording.expect("recording policy should parse");
        assert_eq!(policy.signed_url_expiry_secs, None);
        assert_eq!(policy.effective_signed_url_expiry_secs(), 86_400);

        // Values beyond the SigV4 7-day limit are clamped (90 days -> 7 days).
        let config: Config = toml::from_str(
            "[proxy]\naddr = \"0.0.0.0\"\n[recording]\nsigned_url_expiry_secs = 7776000\n", // 90 days
        )
        .expect("Config should parse");
        let policy = config.recording.expect("recording policy should parse");
        assert_eq!(
            policy.effective_signed_url_expiry_secs(),
            crate::storage::MAX_PRESIGN_EXPIRY_SECS
        );
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
    fn test_session_timer_mode_defaults_to_supported() {
        // RFC 4028 session timer is ON by default (Supported mode): a silently
        // disconnected peer (e.g. WebRTC over WS closed without BYE) is detected
        // via the refresh/expiry cycle instead of leaking the session forever.
        let config = ProxyConfig::default();
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Supported);
        assert_eq!(config.session_expires, Some(600));
    }

    #[test]
    fn test_session_timer_mode_uses_always_flag() {
        let mut config = ProxyConfig::default();

        // Default: timer on in Supported mode (off only when explicitly set).
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Supported);

        config.session_timer_always = true;
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Always);

        config.session_timer = false;
        assert_eq!(config.session_timer_mode(), SessionTimerMode::Off);
    }

    #[test]
    fn test_rtp_config_uses_proxy_addr_for_bind_ip() {
        let mut config = Config::default();
        config.proxy.addr = "120.228.209.243".to_string();
        config.external_ip = Some("203.0.113.10".to_string());

        let rtp_config = config.rtp_config();

        assert_eq!(rtp_config.bind_ip.as_deref(), Some("120.228.209.243"));
        assert_eq!(rtp_config.external_ip.as_deref(), Some("203.0.113.10"));
    }

    #[test]
    fn test_network_profile_toml_roundtrip() {
        let raw = r#"
            proxy = { addr = "127.0.0.1" }
            default_network_profile = "wan"

            [[network_profile]]
            id = "wan"
            label = "Public WAN"
            external_ip = "203.0.113.10"
            sip_external_ip = "203.0.113.11"
            local_networks = ["10.0.0.0/8"]
            rtp_start_port = 12000
            rtp_end_port = 12010

            [[network_profile]]
            id = "overlay"
            external_ip = "100.64.0.5"
            bind_ip = "100.64.0.5"
        "#;
        let config: Config = toml::from_str(raw).unwrap();
        assert_eq!(config.network_profiles.len(), 2);
        assert_eq!(config.default_network_profile_id(), "wan");
        let wan = config.network_profile("wan").unwrap();
        assert_eq!(wan.external_ip.as_deref(), Some("203.0.113.10"));
        assert_eq!(
            wan.sip_contact_config().sip_external_ip.as_deref(),
            Some("203.0.113.11")
        );
        let overlay = config.network_profile("overlay").unwrap();
        assert_eq!(
            overlay.effective_bind_ip("0.0.0.0", Some("192.168.1.5")),
            "100.64.0.5"
        );
        let no_bind = NetworkProfile {
            id: "x".to_string(),
            label: None,
            description: None,
            external_ip: None,
            auto_external_ip: None,
            sip_external_ip: None,
            auto_sip_external_ip: None,
            local_networks: vec![],
            contact_lan_use_bind: true,
            sip_contact_always_bind: false,
            bind_ip: None,
            rtp_start_port: None,
            rtp_end_port: None,
        };
        assert_eq!(
            no_bind.effective_bind_ip("10.0.0.1", Some("192.168.1.5")),
            "192.168.1.5"
        );
        assert_eq!(no_bind.effective_bind_ip("10.0.0.1", None), "10.0.0.1");
    }

    #[test]
    fn test_effective_network_profiles_synthetic_when_empty() {
        let config = Config::default();
        let profiles = config.effective_network_profiles();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "default");
    }

    #[test]
    fn test_sip_contact_config_parses_local_networks() {
        let raw = r#"
            proxy = { addr = "127.0.0.1" }
            local_networks = ["192.168.50.0/24"]
            sip_external_ip = "203.0.113.20"
            contact_lan_use_bind = true
            sip_contact_always_bind = false
        "#;
        let config: Config = toml::from_str(raw).unwrap();
        let sip = config.sip_contact_config();
        assert_eq!(sip.sip_external_ip.as_deref(), Some("203.0.113.20"));
        assert!(sip.contact_lan_use_bind);
        assert!(!sip.sip_contact_always_bind);
        assert_eq!(sip.local_networks.len(), 1);
        assert!(sip.local_networks[0].contains(&"192.168.50.10".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_sip_contact_config_default_local_networks_when_empty() {
        let config = Config::default();
        let sip = config.sip_contact_config();
        assert!(!sip.local_networks.is_empty());
        assert!(
            sip.local_networks
                .iter()
                .any(|n| n.contains(&"10.1.2.3".parse::<IpAddr>().unwrap()))
        );
    }

    #[cfg(feature = "commerce")]
    #[test]
    fn test_cluster_config_default_is_none() {
        let config = Config::default();
        assert!(config.cluster.is_none());
    }

    #[cfg(feature = "commerce")]
    #[test]
    fn test_cluster_peer_roundtrip() {
        let peer = ClusterPeer {
            addr: "10.0.0.2".to_string(),
            sip_port: 5060,
            ami_port: 8080,
        };
        let toml_str = toml::to_string(&peer).unwrap();
        let parsed: ClusterPeer = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.addr, "10.0.0.2");
        assert_eq!(parsed.sip_port, 5060);
        assert_eq!(parsed.ami_port, 8080);
    }

    #[cfg(feature = "commerce")]
    #[test]
    fn test_cluster_config_toml_roundtrip() {
        let config = ClusterConfig {
            peers: vec![
                ClusterPeer {
                    addr: "10.0.0.2".to_string(),
                    sip_port: 5060,
                    ami_port: 8080,
                },
                ClusterPeer {
                    addr: "10.0.0.3".to_string(),
                    sip_port: 5061,
                    ami_port: 8081,
                },
            ],
            session_registry_backend: "db".to_string(),
            session_registry_ttl_secs: 3600,
            session_registry_heartbeat_secs: 30,
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: ClusterConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.peers.len(), 2);
        assert_eq!(parsed.peers[0].addr, "10.0.0.2");
        assert_eq!(parsed.peers[1].addr, "10.0.0.3");
    }

    #[cfg(feature = "commerce")]
    #[test]
    fn test_cluster_config_empty_peers() {
        let config = ClusterConfig::default();
        assert!(config.peers.is_empty());
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: ClusterConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.peers.is_empty());
    }

    #[test]
    fn test_cluster_config_session_registry_defaults() {
        // Defaults: backend "db", TTL 3600, heartbeat 30 — and they survive
        // a TOML round-trip (fields are always available, not commerce-gated).
        let cfg = ClusterConfig::default();
        assert_eq!(cfg.session_registry_backend, "db");
        assert_eq!(cfg.session_registry_ttl_secs, 3600);
        assert_eq!(cfg.session_registry_heartbeat_secs, 30);

        let toml_str = toml::to_string(&cfg).unwrap();
        let parsed: ClusterConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.session_registry_backend, "db");
        assert_eq!(parsed.session_registry_ttl_secs, 3600);
        assert_eq!(parsed.session_registry_heartbeat_secs, 30);
    }

    #[test]
    fn test_cluster_config_session_registry_overrides() {
        let toml_str = r#"
            peers = []
            session_registry_backend = "memory"
            session_registry_ttl_secs = 120
            session_registry_heartbeat_secs = 10
        "#;
        let parsed: ClusterConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.session_registry_backend, "memory");
        assert_eq!(parsed.session_registry_ttl_secs, 120);
        assert_eq!(parsed.session_registry_heartbeat_secs, 10);
    }

    #[test]
    fn test_cluster_config_session_registry_disabled() {
        let toml_str = r#"
            peers = [{ addr = "10.0.0.2", sip_port = 5060, ami_port = 8080 }]
            session_registry_backend = "disabled"
        "#;
        let parsed: ClusterConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.session_registry_backend, "disabled");
        assert_eq!(parsed.peers.len(), 1);
    }

    /// Regression: `Config::clone` used to round-trip through TOML which is
    /// expensive (called on every server bootstrap and config reload). The
    /// derive(d Clone implementation must produce a deeply-equal copy while
    /// avoiding the serialization hop. We assert equality on a representative
    /// field set covering primitives, Vec, nested config and Option types.
    #[test]
    fn test_config_clone_preserves_all_fields() {
        let mut original = Config::default();
        original.http_addr = "127.0.0.1:8080".to_string();
        original.http_gzip = true;
        original.http_access_skip_paths = vec!["/health".to_string(), "/metrics".to_string()];
        original.proxy.addr = "127.0.0.1:5060".to_string();
        original.proxy.useragent = Some("TestPBX/1.0".to_string());
        original.database_url = "sqlite://test.db".to_string();
        original.demo_mode = true;
        original.ami = Some(AmiConfig {
            allows: Some(vec!["10.0.0.0/8".to_string()]),
        });
        original.recording = Some(RecordingPolicy::default());

        let cloned = original.clone();

        // Equality is intentionally checked via TOML round-trip serialization
        // (not PartialEq, which is not derived) so we exercise the same
        // surface the previous implementation relied on, but at clone time we
        // no longer pay that cost.
        let original_toml = toml::to_string(&original).unwrap();
        let cloned_toml = toml::to_string(&cloned).unwrap();
        assert_eq!(original_toml, cloned_toml, "Config::clone lost data");

        // Mutating the clone must not bleed into the original (deep copy).
        let mut cloned2 = original.clone();
        cloned2.http_addr = "0.0.0.0:9999".to_string();
        assert_ne!(original.http_addr, cloned2.http_addr);
        assert_eq!(original.http_addr, "127.0.0.1:8080");
    }
}
