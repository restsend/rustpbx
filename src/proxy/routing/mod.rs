use crate::{
    call::{
        DialDirection, DialStrategy, Location, concurrent_call_limiter::ConcurrentCallLimiter,
        cps_limiter::CpsLimiter,
    },
    config::RecordingPolicy,
};
use anyhow::{Result, anyhow};
use ipnet::IpNet;
use regex::Regex;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{StatusCode, Uri};
use rsipstack::transport::SipAddr;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::net::lookup_host;

pub mod error_catalog;
pub mod http;
pub mod http_error_catalog;
pub mod inspector_stack;
pub mod matcher;
pub mod stack;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigOrigin {
    #[default]
    Embedded,
    File(String),
}

impl ConfigOrigin {
    pub fn embedded() -> Self {
        Self::Embedded
    }

    pub fn from_file(path: impl Into<String>) -> Self {
        Self::File(path.into())
    }
}

/// Single trunk configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TrunkConfig {
    pub dest: String,
    pub backup_dest: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(
        default,
        alias = "allow_codecs",
        alias = "audio_codecs",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub codec: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls: Option<u32>,
    /// Runtime-only concurrent-call limiter built from `max_calls`.
    #[serde(skip)]
    pub concurrent_call_limiter: Option<Arc<ConcurrentCallLimiter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cps: Option<u32>,
    /// Runtime-only CPS limiter built from `max_cps`.
    #[serde(skip)]
    pub cps_limiter: Option<Arc<CpsLimiter>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<TrunkDirection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbound_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_from_user_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_to_user_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::models::policy::PolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub register_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub register_expires: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub register_extra_headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default = "default_rewrite_hostport")]
    pub rewrite_hostport: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id_mode: Option<CallIdMode>,

    // SBC Health Check
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_per_ip: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_probe_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_fallback_trunk: Option<String>,

    // SBC CAC
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cac_policy: Option<CacPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_threshold: Option<u32>,

    // SBC Header Manipulation
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_rules: Option<Vec<HeaderRule>>,

    /// Per-trunk control of whether custom headers from the original INVITE are
    /// forwarded to this trunk's outbound INVITE.
    ///
    /// - `Some(HeaderPassthrough)` — apply the rule to the original request's
    ///   custom (non-standard) headers.
    /// - `None` (default) — strict: no original custom headers are forwarded.
    ///
    /// Destinations resolved as internal (same realm / registered AOR /
    /// home-proxy) always passthrough all custom headers regardless of this
    /// setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_passthrough: Option<HeaderPassthrough>,

    // SBC Media
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_mode: Option<MediaMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_policy: Option<VideoPolicy>,
    /// Per-trunk override for the IP advertised in SDP `c=`/`o=` lines and ICE
    /// candidates. When set, this trunk's legs use this address instead of the
    /// global `rtp_config.external_ip`. Useful when some trunks terminate on an
    /// overlay network (Tailscale/WireGuard) that needs a different advertised
    /// IP than the public NAT address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ip: Option<String>,
    /// Per-trunk override for the local IP RTP sockets bind to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_ip: Option<String>,
    /// Network profile id from `[[network_profile]]` in the main config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    #[serde(skip)]
    pub origin: ConfigOrigin,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub did_numbers: Vec<String>,

    /// Per-trunk ringback/early-media audio configuration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ringback: Option<RingbackAudio>,

    /// Per-trunk max ring/setup time in seconds before a no-answer rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ring_time: Option<u32>,
}

/// Controls which original-request custom headers are forwarded to the
/// destination leg's INVITE. Standard SIP headers (Via/From/To/Call-ID/CSeq/
/// Contact/... ) are always excluded regardless of this rule.
///
/// Rule precedence:
/// - `Whitelist` — forward only the listed headers.
/// - `Blacklist` — forward all custom headers except the listed ones.
/// - `All` (default) — forward all custom headers; if `whitelist` is non-empty
///   it behaves like a whitelist, otherwise a non-empty `blacklist` is honored.
///
/// Header names are matched case-insensitively.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HeaderPassthrough {
    #[serde(default)]
    pub mode: HeaderPassthroughMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub whitelist: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blacklist: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPassthroughMode {
    #[default]
    All,
    Whitelist,
    Blacklist,
}

impl HeaderPassthrough {
    /// Passthrough every custom header — used for internal destinations.
    pub fn all() -> Self {
        Self::default()
    }

    /// Returns `true` if a header with the given (case-insensitive) name should
    /// be forwarded under this rule.
    pub fn allows(&self, name: &str) -> bool {
        match self.mode {
            HeaderPassthroughMode::Whitelist => {
                self.whitelist.iter().any(|w| w.eq_ignore_ascii_case(name))
            }
            HeaderPassthroughMode::Blacklist => {
                !self.blacklist.iter().any(|b| b.eq_ignore_ascii_case(name))
            }
            HeaderPassthroughMode::All => {
                if !self.whitelist.is_empty() {
                    self.whitelist.iter().any(|w| w.eq_ignore_ascii_case(name))
                } else if !self.blacklist.is_empty() {
                    !self.blacklist.iter().any(|b| b.eq_ignore_ascii_case(name))
                } else {
                    true
                }
            }
        }
    }
}

/// Parse a trunk destination into `(host, port)`. Handles both SIP URIs and
/// bare `host:port` strings.
pub fn trunk_dest_host_port(dest: &str) -> Option<(String, u16)> {
    if dest.trim().is_empty() {
        return None;
    }
    if let Ok(uri) = rsipstack::sip::Uri::try_from(dest) {
        let host = uri.host().to_string();
        let port = uri.host_with_port.port.map(|p| p.0).unwrap_or(5060);
        if host.is_empty() {
            return None;
        }
        return Some((host, port));
    }
    // Try as bare host:port
    let parts: Vec<&str> = dest.split(':').collect();
    let host = *parts.first()?;
    if host.is_empty() {
        return None;
    }
    let port = parts
        .get(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5060);
    Some((host.to_string(), port))
}

/// Find a trunk whose destination matches `host:port` (case-insensitive host).
pub fn find_trunk_by_dest<'a>(
    trunks: &'a HashMap<String, TrunkConfig>,
    host: &str,
    port: u16,
) -> Option<&'a TrunkConfig> {
    trunks.values().find(|trunk| {
        trunk_dest_host_port(&trunk.dest)
            .map(|(h, p)| h.eq_ignore_ascii_case(host) && p == port)
            .unwrap_or(false)
    })
}

/// Per-trunk ringback/early-media audio configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RingbackAudio {
    /// Ringback/waiting tone — played as 183 early media while callee rings
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring: Option<String>,
    /// Busy tone — played as 183 early media before sending 486
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<String>,
    /// Reject tone — played as 183 early media before sending 603
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject: Option<String>,
    /// Offline/unavailable tone — played as 183 early media before sending 480
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<String>,
    /// Not-found tone — played as 183 early media before sending 404
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notfound: Option<String>,
    /// No-answer tone — played as 183 early media before sending 408
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noanswer: Option<String>,
    /// Server-error tone — played as 183 early media before a 5xx rejection
    /// (e.g. when an IVR/app fails to start due to a missing config). Map this
    /// to a "service unavailable" announcement so a caller never
    /// hears dead air on a misconfiguration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RingbackAudio {
    /// Built-in failure-tone defaults, applied to every call unless a global
    /// `[proxy.audio_profile]` or a per-trunk `ringback` overrides them. An
    /// explicitly declared empty global profile disables them. `ring` is left
    /// `None` (custom ringback is operator-specific); every failure status gets
    /// a `tone://` beep, and 5xx (app/IVR start failures) announce the shipped
    /// English `sounds/service_unavailable_en.mp3`.
    pub fn builtin_defaults() -> Self {
        Self {
            ring: None,
            busy: Some("tone://480,3000".to_string()),
            reject: Some("tone://480,2000".to_string()),
            offline: Some("tone://480,2000".to_string()),
            notfound: Some("tone://480,1500".to_string()),
            noanswer: Some("tone://480,3000".to_string()),
            error: Some("sounds/service_unavailable_en.mp3".to_string()),
        }
    }

    /// Overlay `other` onto `self`: every field `Some` in `other` wins. Used to
    /// layer a global default with per-trunk overrides so an operator only has
    /// to configure the tones they want to change.
    pub fn merge_from(&mut self, other: Self) {
        if other.ring.is_some() {
            self.ring = other.ring;
        }
        if other.busy.is_some() {
            self.busy = other.busy;
        }
        if other.reject.is_some() {
            self.reject = other.reject;
        }
        if other.offline.is_some() {
            self.offline = other.offline;
        }
        if other.notfound.is_some() {
            self.notfound = other.notfound;
        }
        if other.noanswer.is_some() {
            self.noanswer = other.noanswer;
        }
        if other.error.is_some() {
            self.error = other.error;
        }
    }

    /// Get the audio file for a specific SIP status code.
    ///
    /// Matching is done on the numeric status code so that `StatusCode::Other(486, ..)`
    /// (as produced by the call session on a callee rejection) matches `BusyHere`
    /// exactly like the canonical `StatusCode::BusyHere` variant does.
    pub fn for_status(&self, code: &rsipstack::sip::StatusCode) -> Option<&str> {
        match u16::from(code.clone()) {
            408 | 487 => self.noanswer.as_deref(),
            480 => self.offline.as_deref(),
            404 => self.notfound.as_deref(),
            486 => self.busy.as_deref(),
            603 => self.reject.as_deref(),
            500..=599 => self.error.as_deref(),
            _ => None,
        }
    }

    /// Returns `true` if any failure tone (busy/reject/offline/notfound/noanswer/error) is configured
    pub fn has_failure_tone(&self) -> bool {
        self.busy.is_some()
            || self.reject.is_some()
            || self.offline.is_some()
            || self.notfound.is_some()
            || self.noanswer.is_some()
            || self.error.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CacPolicy {
    Lossy,
    Reject,
    Overflow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MediaMode {
    None,
    Bypass,
    Auto,
    ForceTranscode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VideoPolicy {
    PassThrough,
    Strip,
    Transcode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeaderRule {
    pub action: HeaderAction,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_caller_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_callee_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HeaderAction {
    Add,
    Remove,
    Set,
    Rename,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CallIdMode {
    Transparent,
    Rewrite,
}

fn default_rewrite_hostport() -> bool {
    true
}

impl Default for TrunkConfig {
    fn default() -> Self {
        Self {
            dest: String::new(),
            backup_dest: None,
            username: None,
            password: None,
            codec: Vec::new(),
            disabled: None,
            max_calls: None,
            concurrent_call_limiter: None,
            max_cps: None,
            cps_limiter: None,
            weight: None,
            transport: None,
            id: None,
            direction: None,
            inbound_hosts: Vec::new(),
            recording: None,
            incoming_from_user_prefix: None,
            incoming_to_user_prefix: None,
            country: None,
            policy: None,
            register_enabled: None,
            register_expires: None,
            register_extra_headers: None,
            rewrite_hostport: true,
            call_id_mode: None,
            health_check_enabled: None,
            health_check_per_ip: None,
            health_check_interval_secs: None,
            health_check_probe_count: None,
            health_check_fallback_trunk: None,
            cac_policy: None,
            overflow_threshold: None,
            header_rules: None,
            header_passthrough: None,
            media_mode: None,
            video_policy: None,
            external_ip: None,
            bind_ip: None,
            profile: None,
            did_numbers: Vec::new(),
            ringback: None,
            max_ring_time: None,
            origin: ConfigOrigin::embedded(),
        }
    }
}

impl TrunkConfig {
    pub async fn matches_inbound_source_ip(&self, addr: &IpAddr) -> bool {
        if let Some(trunk_direction) = self.direction
            && !trunk_direction.allows(&DialDirection::Inbound)
        {
            return false;
        }

        for host in &self.inbound_hosts {
            if candidate_matches(host, addr).await {
                return true;
            }
        }

        false
    }

    pub async fn matches_inbound_ip(&self, addr: &IpAddr) -> bool {
        for host in &self.inbound_hosts {
            if candidate_matches(host, addr).await {
                return true;
            }
        }

        if candidate_matches(&self.dest, addr).await {
            return true;
        }

        if let Some(backup) = &self.backup_dest
            && candidate_matches(backup, addr).await
        {
            return true;
        }

        false
    }

    pub fn matches_incoming_user_prefixes(
        &self,
        from_user: Option<&str>,
        to_user: Option<&str>,
    ) -> bool {
        if let Some(pattern) = &self.incoming_from_user_prefix
            && !pattern.trim().is_empty()
            && !matches_user_prefix(pattern, from_user.unwrap_or_default())
        {
            return false;
        }

        if let Some(pattern) = &self.incoming_to_user_prefix
            && !pattern.trim().is_empty()
            && !matches_user_prefix(pattern, to_user.unwrap_or_default())
        {
            return false;
        }

        true
    }
}

/// Build a [`SourceTrunk`] instance when direction is allowed by trunk configuration.
pub fn build_source_trunk(
    name: String,
    config: &TrunkConfig,
    direction: &DialDirection,
) -> Option<SourceTrunk> {
    if let Some(trunk_direction) = config.direction
        && !trunk_direction.allows(direction)
    {
        return None;
    }

    Some(SourceTrunk {
        name,
        id: config.id,
        direction: config.direction,
    })
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrunkDirection {
    Inbound,
    Outbound,
    Bidirectional,
}

impl TrunkDirection {
    pub fn allows(&self, direction: &DialDirection) -> bool {
        match self {
            TrunkDirection::Inbound => matches!(direction, DialDirection::Inbound),
            TrunkDirection::Outbound => matches!(direction, DialDirection::Outbound),
            TrunkDirection::Bidirectional => true,
        }
    }
}

impl From<crate::models::sip_trunk::SipTrunkDirection> for TrunkDirection {
    fn from(value: crate::models::sip_trunk::SipTrunkDirection) -> Self {
        match value {
            crate::models::sip_trunk::SipTrunkDirection::Inbound => TrunkDirection::Inbound,
            crate::models::sip_trunk::SipTrunkDirection::Outbound => TrunkDirection::Outbound,
            crate::models::sip_trunk::SipTrunkDirection::Bidirectional => {
                TrunkDirection::Bidirectional
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceTrunk {
    pub name: String,
    pub id: Option<i64>,
    pub direction: Option<TrunkDirection>,
}

/// Destination configuration (can be single or multiple trunks)
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(untagged)]
pub enum DestConfig {
    Single(String),
    Multiple(Vec<String>),
}

/// Route rule
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteRule {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_trunks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_trunk_ids: Vec<i64>,

    /// Match conditions
    #[serde(rename = "match")]
    pub match_conditions: MatchConditions,

    /// Rewrite rules
    #[serde(default)]
    pub rewrite: Option<RewriteRules>,

    /// Route action
    #[serde(flatten)]
    pub action: RouteAction,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codecs: Vec<String>,

    /// When `true`, ice_servers will not be applied for calls matching this rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_ice_servers: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::models::policy::PolicySpec>,
    /// Max ring/setup time in seconds for calls routed by this rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ring_time: Option<u32>,
    #[serde(skip)]
    pub origin: ConfigOrigin,
}

impl Default for RouteRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: None,
            priority: 0,
            source_trunks: Vec::new(),
            source_trunk_ids: Vec::new(),
            match_conditions: MatchConditions::default(),
            rewrite: None,
            action: RouteAction::default(),
            codecs: Vec::new(),
            disable_ice_servers: None,
            disabled: None,
            policy: None,
            max_ring_time: None,
            origin: ConfigOrigin::embedded(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum RouteDirection {
    #[default]
    Any,
    Inbound,
    Outbound,
}

impl RouteDirection {
    pub fn matches(&self, direction: &DialDirection) -> bool {
        match self {
            RouteDirection::Any => true,
            RouteDirection::Inbound => matches!(direction, DialDirection::Inbound),
            RouteDirection::Outbound => matches!(direction, DialDirection::Outbound),
        }
    }
}

/// Match conditions
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MatchConditions {
    /// From user part
    #[serde(rename = "from.user")]
    pub from_user: Option<String>,
    /// From host part
    #[serde(rename = "from.host")]
    pub from_host: Option<String>,
    /// To user part
    #[serde(rename = "to.user")]
    pub to_user: Option<String>,
    /// To host part
    #[serde(rename = "to.host")]
    pub to_host: Option<String>,
    /// To port
    #[serde(rename = "to.port")]
    pub to_port: Option<String>,
    /// Request URI user part
    #[serde(rename = "request_uri.user")]
    pub request_uri_user: Option<String>,
    /// Request URI host part
    #[serde(rename = "request_uri.host")]
    pub request_uri_host: Option<String>,
    /// Request URI port
    #[serde(rename = "request_uri.port")]
    pub request_uri_port: Option<String>,
    /// SIP header fields (starting with header.)
    #[serde(flatten)]
    pub headers: HashMap<String, String>,

    // Compatible simplified field names
    pub from: Option<String>,
    pub to: Option<String>,
    pub caller: Option<String>,
    pub callee: Option<String>,
}

/// Rewrite rules
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RewriteRules {
    /// Rewrite From user part
    #[serde(rename = "from.user")]
    pub from_user: Option<String>,
    /// Rewrite From host part
    #[serde(rename = "from.host")]
    pub from_host: Option<String>,
    /// Rewrite To user part
    #[serde(rename = "to.user")]
    pub to_user: Option<String>,
    /// Rewrite To host part
    #[serde(rename = "to.host")]
    pub to_host: Option<String>,
    /// Rewrite To port
    #[serde(rename = "to.port")]
    pub to_port: Option<String>,
    /// Rewrite Request URI user part
    #[serde(rename = "request_uri.user")]
    pub request_uri_user: Option<String>,
    /// Rewrite Request URI host part
    #[serde(rename = "request_uri.host")]
    pub request_uri_host: Option<String>,
    /// Rewrite Request URI port
    #[serde(rename = "request_uri.port")]
    pub request_uri_port: Option<String>,
    /// Add/modify header fields (starting with header.)
    #[serde(flatten)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteAction {
    #[serde(default)]
    pub action: Option<String>,

    #[serde(default)]
    pub dest: Option<DestConfig>,

    #[serde(default = "default_select")]
    pub select: String,

    #[serde(default)]
    pub hash_key: Option<String>,

    #[serde(default)]
    pub reject: Option<RejectConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_params: Option<serde_json::Value>,

    #[serde(default = "default_auto_answer")]
    pub auto_answer: bool,
}

fn default_auto_answer() -> bool {
    true
}

impl Default for RouteAction {
    fn default() -> Self {
        RouteAction {
            action: None,
            dest: None,
            select: default_select(),
            hash_key: None,
            reject: None,
            queue: None,
            app: None,
            app_params: None,
            auto_answer: default_auto_answer(),
        }
    }
}

impl RouteAction {
    pub fn get_action_type(&self) -> ActionType {
        match &self.action {
            Some(action) => match action.as_str() {
                "reject" => ActionType::Reject,
                "busy" => ActionType::Busy,
                "queue" => ActionType::Queue,
                "application" => ActionType::Application,
                _ => ActionType::Forward,
            },
            None => {
                if self.app.is_some() {
                    ActionType::Application
                } else if self.queue.is_some() {
                    ActionType::Queue
                } else if self.reject.is_some() {
                    ActionType::Reject
                } else {
                    ActionType::Forward
                }
            }
        }
    }
}

/// Action type enum
#[derive(Debug, Clone, PartialEq)]
pub enum ActionType {
    Forward,
    Reject,
    Busy,
    Queue,
    Application,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub accept_immediately: bool,
    #[serde(default)]
    pub passthrough_ringback: bool,
    #[serde(default)]
    pub hold: Option<RouteQueueHoldConfig>,
    #[serde(default)]
    pub fallback: Option<RouteQueueFallbackConfig>,
    #[serde(default)]
    pub strategy: RouteQueueStrategyConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_prompts: Option<crate::call::VoicePrompts>,
    #[serde(skip)]
    pub origin: ConfigOrigin,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueHoldConfig {
    pub audio_file: Option<String>,
    #[serde(default = "RouteQueueHoldConfig::default_loop")]
    pub loop_playback: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueStrategyConfig {
    #[serde(default = "QueueDialMode::default_mode")]
    pub mode: QueueDialMode,
    pub wait_timeout_secs: Option<u16>,
    #[serde(default)]
    pub targets: Vec<RouteQueueTargetConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteQueueTargetConfig {
    pub uri: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum QueueDialMode {
    #[default]
    Sequential,
    Parallel,
}

impl QueueDialMode {
    pub fn default_mode() -> Self {
        QueueDialMode::Sequential
    }
}

impl RouteQueueHoldConfig {
    fn default_loop() -> bool {
        true
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct RouteQueueFallbackConfig {
    pub redirect: Option<String>,
    pub failure_code: Option<u16>,
    pub failure_reason: Option<String>,
}

impl RouteQueueConfig {
    pub fn to_queue_plan(&self) -> Result<crate::call::QueuePlan> {
        let mut plan = crate::call::QueuePlan {
            accept_immediately: self.accept_immediately,
            passthrough_ringback: self.passthrough_ringback && self.accept_immediately,
            hold: None,
            ..Default::default()
        };
        if let Some(hold) = &self.hold {
            let mut cfg = crate::call::QueueHoldConfig::default();
            if let Some(file) = &hold.audio_file {
                cfg = cfg.with_audio_file(file.clone());
            }
            cfg = cfg.with_loop_playback(hold.loop_playback);
            plan.hold = Some(cfg);
        }
        if let Some(fallback) = &self.fallback {
            plan.fallback = Some(fallback.to_action()?);
        }
        if let Some(strategy) = self.build_dial_strategy()? {
            plan.dial_strategy = Some(strategy);
        }
        if let Some(timeout) = self.strategy.wait_timeout_secs
            && timeout > 0
        {
            plan.ring_timeout = Some(Duration::from_secs(timeout as u64));
        }
        plan.voice_prompts = self.voice_prompts.clone();
        plan.queue_name = self.name.clone().unwrap_or_default();
        Ok(plan)
    }

    fn build_dial_strategy(&self) -> Result<Option<DialStrategy>> {
        if self.strategy.targets.is_empty() {
            return Ok(None);
        }

        let mut locations = Vec::new();
        for target in &self.strategy.targets {
            let uri_text = target.uri.trim();
            if uri_text.is_empty() {
                continue;
            }

            // Handle skill-group targets (serialized as uri: "skill-group:{id}")
            if uri_text.starts_with("skill-group:") {
                let skill_group_id = uri_text
                    .strip_prefix("skill-group:")
                    .unwrap_or(uri_text)
                    .trim();
                if !skill_group_id.is_empty() {
                    // Create a special location for skill group that will be resolved at runtime
                    let location = Location {
                        aor: Uri::try_from(format!("skill-group:{}", skill_group_id)).map_err(
                            |err| anyhow!("invalid skill group uri '{}': {}", uri_text, err),
                        )?,
                        contact_raw: Some(uri_text.to_string()),
                        ..Default::default()
                    };
                    locations.push(location);
                }
                continue;
            }

            let uri = Uri::try_from(uri_text)
                .map_err(|err| anyhow!("invalid queue target uri '{}': {}", uri_text, err))?;
            let location = Location {
                aor: uri.clone(),
                contact_raw: Some(uri.to_string()),
                ..Default::default()
            };
            locations.push(location);
        }

        if locations.is_empty() {
            return Ok(None);
        }

        let strategy = match self.strategy.mode {
            QueueDialMode::Parallel => DialStrategy::Parallel(locations),
            QueueDialMode::Sequential => DialStrategy::Sequential(locations),
        };
        Ok(Some(strategy))
    }
}

impl RouteQueueFallbackConfig {
    fn to_action(&self) -> Result<crate::call::QueueFallbackAction> {
        // redirect covers all transfer targets: plain SIP URI (Redirect) or
        // ivr:/queue:/voicemail:/conference: prefixed targets (Transfer).
        if let Some(target) = self
            .redirect
            .as_ref()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            if let Some(endpoint) = crate::call::TransferEndpoint::parse(target) {
                match endpoint {
                    crate::call::TransferEndpoint::Uri(uri) => {
                        let parsed = Uri::try_from(uri.as_str())?;
                        return Ok(crate::call::QueueFallbackAction::Redirect { target: parsed });
                    }
                    other => {
                        return Ok(crate::call::QueueFallbackAction::Failure(
                            crate::call::FailureAction::Transfer(other),
                        ));
                    }
                }
            }
        }
        // Hangup with optional code/reason.
        if self.failure_code.is_some() || self.failure_reason.is_some() {
            let status = match self.failure_code {
                Some(code) => {
                    if !(100..=699).contains(&code) {
                        return Err(anyhow!("invalid failure_code {}: must be 100-699", code));
                    }
                    StatusCode::from(code)
                }
                None => StatusCode::TemporarilyUnavailable,
            };
            return Ok(crate::call::QueueFallbackAction::Failure(
                crate::call::FailureAction::Hangup {
                    code: Some(status),
                    reason: self.failure_reason.clone(),
                },
            ));
        }
        Err(anyhow!(
            "Queue fallback must specify redirect or failure action"
        ))
    }
}
/// Reject configuration
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RejectConfig {
    pub code: u16,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_select() -> String {
    "rr".to_string()
}

async fn candidate_matches(candidate: &str, addr: &IpAddr) -> bool {
    let trimmed = candidate.trim().trim_matches(|c| c == '<' || c == '>');
    if trimmed.is_empty() {
        return false;
    }

    if let Ok(network) = trimmed.parse::<IpNet>() {
        return network.contains(addr);
    }

    if let Ok(socket) = trimmed.parse::<SocketAddr>() {
        return socket.ip() == *addr;
    }

    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return ip == *addr;
    }

    if let Ok(uri) = rsipstack::sip::Uri::try_from(trimmed) {
        return host_matches(&uri.host_with_port.host.to_string(), addr).await;
    }

    if let Some((host, _)) = split_host_port(trimmed) {
        return host_matches(host, addr).await;
    }

    host_matches(trimmed, addr).await
}

/// Public helper to validate whether a candidate host definition resolves to the provided IP.
pub async fn candidate_matches_ip(candidate: &str, addr: &IpAddr) -> bool {
    candidate_matches(candidate, addr).await
}

pub fn source_addr_ip(source_addr: &SipAddr) -> Option<IpAddr> {
    let ip: IpAddr = source_addr.addr.host.clone().try_into().ok()?;
    Some(ip)
}

async fn host_matches(host: &str, addr: &IpAddr) -> bool {
    let cleaned = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();

    if cleaned.is_empty() {
        return false;
    }

    if let Ok(network) = cleaned.parse::<IpNet>() {
        return network.contains(addr);
    }

    if let Ok(socket) = cleaned.parse::<SocketAddr>() {
        return socket.ip() == *addr;
    }

    if let Ok(ip) = cleaned.parse::<IpAddr>() {
        return ip == *addr;
    }

    let lookup_target = match split_host_port(cleaned) {
        Some((host_part, _)) => host_part.to_string(),
        None => cleaned.to_string(),
    };

    match lookup_host((lookup_target.as_str(), 0)).await {
        Ok(addrs) => addrs.into_iter().any(|resolved| resolved.ip() == *addr),
        Err(_) => false,
    }
}

fn split_host_port(input: &str) -> Option<(&str, &str)> {
    if let Some(end) = input.find(']')
        && input.starts_with('[')
        && input.len() > end + 1
        && input[end + 1..].starts_with(':')
    {
        return Some((&input[1..end], &input[end + 2..]));
    }

    if let Some(idx) = input.rfind(':') {
        if input[..idx].contains(':') {
            return None;
        }
        return Some((&input[..idx], &input[idx + 1..]));
    }

    None
}

fn matches_user_prefix(pattern: &str, value: &str) -> bool {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return true;
    }

    let mut is_regex = false;
    for ch in trimmed.chars() {
        match ch {
            '^' | '$' | '.' | '*' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '\\' => {
                is_regex = true;
                break;
            }
            _ => {}
        }
    }

    if !is_regex {
        return value.starts_with(trimmed);
    }

    Regex::new(trimmed)
        .map(|regex| regex.is_match(value))
        .unwrap_or(false)
}

/// Resolve a transport enum from a lowercase string (e.g. "udp", "tcp", "tls", "ws", "wss").
/// Returns `None` for unrecognized values.
pub fn resolve_transport_from_str(s: &str) -> Option<rsipstack::sip::transport::Transport> {
    match s.to_lowercase().as_str() {
        "udp" => Some(rsipstack::sip::transport::Transport::Udp),
        "tcp" => Some(rsipstack::sip::transport::Transport::Tcp),
        "tls" => Some(rsipstack::sip::transport::Transport::Tls),
        "ws" => Some(rsipstack::sip::transport::Transport::Ws),
        "wss" => Some(rsipstack::sip::transport::Transport::Wss),
        _ => None,
    }
}

pub fn extract_via_ip(origin: &rsipstack::sip::Request) -> Option<std::net::IpAddr> {
    let via = origin.via_header().ok()?;
    let (_, target) = rsipstack::transport::SipConnection::parse_target_from_via(via).ok()?;
    target.host.try_into().ok()
}

pub fn parse_trusted_proxy(s: &str) -> Option<IpNet> {
    s.trim().parse::<IpNet>().ok().or_else(|| {
        let ip: IpAddr = s.trim().parse().ok()?;
        Some(IpNet::from(ip))
    })
}

fn ip_matches_trusted(ip: &IpAddr, trusted: &[IpNet]) -> bool {
    trusted.iter().any(|net| net.contains(ip))
}

fn split_via_values(raw: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let raw = raw.trim();
    if raw.is_empty() {
        return entries;
    }
    let mut start = 0usize;
    let mut in_quotes = false;
    let mut escaped = false;
    for (idx, ch) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                entries.push(raw[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    let last = raw[start..].trim();
    if !last.is_empty() {
        entries.push(last);
    }
    entries
}

pub fn extract_trusted_ip(
    tx: &rsipstack::transaction::transaction::Transaction,
    trusted_proxies: &[IpNet],
) -> Option<IpAddr> {
    let socket_ip = tx
        .connection
        .as_ref()
        .and_then(|conn| conn.get_remote_addr())
        .and_then(source_addr_ip)?;

    if trusted_proxies.is_empty() || !ip_matches_trusted(&socket_ip, trusted_proxies) {
        return Some(socket_ip);
    }

    // A SIP proxy prepends its own Via. Once the transport peer has been
    // verified as trusted, the next Via hop represents the source that sent
    // the request to that proxy. Via values may be comma-separated or carried
    // in separate header fields, so preserve their wire order across both
    // representations.
    let entries: Vec<&str> = tx
        .original
        .headers
        .iter()
        .filter_map(|header| match header {
            rsipstack::sip::Header::Via(via) => Some(via.value()),
            _ => None,
        })
        .flat_map(split_via_values)
        .collect();
    let Some(entry) = entries.get(1) else {
        tracing::debug!(
            %socket_ip,
            via_count = entries.len(),
            "trusted proxy request has no forwarded Via hop; using transport source"
        );
        return Some(socket_ip);
    };

    let typed = match rsipstack::sip::headers::typed::Via::parse(entry) {
        Ok(via) => via,
        Err(error) => {
            tracing::debug!(
                %socket_ip,
                %error,
                "trusted proxy forwarded Via is malformed; using transport source"
            );
            return Some(socket_ip);
        }
    };
    let received_ip = typed.received().and_then(Result::ok);
    let sent_by_ip: Option<IpAddr> = typed.sent_by().host.clone().try_into().ok();
    let Some(source_ip) = received_ip.or(sent_by_ip) else {
        tracing::debug!(
            %socket_ip,
            sent_by = %typed.sent_by(),
            "trusted proxy forwarded Via has no IP address; using transport source"
        );
        return Some(socket_ip);
    };

    tracing::debug!(
        %socket_ip,
        %source_ip,
        via_hop = 2,
        sent_by = %typed.sent_by(),
        received = ?received_ip,
        "trusted proxy source resolved from forwarded Via"
    );
    Some(source_ip)
}

pub fn extract_from_user(origin: &rsipstack::sip::Request) -> Option<String> {
    origin
        .from_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .and_then(|uri| uri.user().map(|u| u.to_string()))
}

pub fn extract_to_user(origin: &rsipstack::sip::Request) -> Option<String> {
    origin
        .to_header()
        .ok()
        .and_then(|h| h.uri().ok())
        .and_then(|uri| uri.user().map(|u| u.to_string()))
}

pub fn extract_request_user(origin: &rsipstack::sip::Request) -> Option<String> {
    origin.uri.user().map(|u| u.to_string())
}

pub fn escape_sip_quoted(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}
