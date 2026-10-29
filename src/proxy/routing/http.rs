use crate::call::cookie::SpamResult;
use crate::call::{
    DialAttempt, DialDirection, DialStrategy, Dialplan, Location, RouteInvite, SipUser, TransactionCookie,
    TrunkContext, build_sip_uri,
};
use crate::config::{
    DialplanHints, HttpRouterConfig, MediaProxyMode, RecordingAutoStartAt, RecordingPolicy, RtpConfig,
};
use crate::proxy::call::{CallRouter, RouteError, apply_allowed_codecs};
use crate::proxy::data::ProxyDataContext;
use crate::proxy::routing::TrunkConfig;
use super::RetryPolicy;
use anyhow::{Result, anyhow};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use rsipstack::sip::prelude::*;
use rsipstack::transport::SipConnection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info};

pub struct HttpCallRouter {
    pub config: HttpRouterConfig,
    pub rtp_config: ArcSwap<RtpConfig>,
    pub default_media_proxy_mode: ArcSwap<MediaProxyMode>,
    pub enable_latching: bool,
    pub probation_max_packets: Option<u8>,
    pub client: reqwest::Client,
    /// Trunk registry, so a target can name a trunk to dial through.
    pub data_context: Arc<ProxyDataContext>,
}

impl HttpCallRouter {
    pub fn new(
        config: HttpRouterConfig,
        rtp_config: ArcSwap<RtpConfig>,
        default_media_proxy_mode: ArcSwap<MediaProxyMode>,
        enable_latching: bool,
        probation_max_packets: Option<u8>,
        data_context: Arc<ProxyDataContext>,
    ) -> Self {
        let mut builder = reqwest::Client::builder();
        builder = builder
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8);
        if let Some(timeout) = config.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout));
        } else {
            builder = builder.timeout(Duration::from_secs(5));
        }
        Self {
            config,
            rtp_config,
            default_media_proxy_mode,
            enable_latching,
            probation_max_packets,
            client: builder.build().unwrap_or_default(),
            data_context,
        }
    }

    /// Dial attempt for a target that names a trunk. The trunk is stamped
    /// exactly as a statically routed one (`matcher::stamp_trunk`), so its
    /// destination, credentials, rewrites, media hints, ring time and CDR
    /// attribution all come from the trunk config. The target's retry policy
    /// falls back to the trunk's; with neither, the attempt never advances,
    /// like any other routed trunk target. A bare-user callee (default: the
    /// dialed user) is qualified with the trunk's destination host. Unknown or
    /// disabled trunks fail the call. `hints` (the arrival trunk's) are the
    /// base the trunk is stamped over, so the target trunk's settings win.
    #[allow(clippy::too_many_arguments)]
    fn trunk_attempt(
        &self,
        hints: Option<DialplanHints>,
        trunk_name: &str,
        callee: Option<&str>,
        retry_policy: Option<RetryPolicy>,
        original: &rsipstack::sip::Request,
        caller: rsipstack::sip::Uri,
        headers: Option<Vec<rsipstack::sip::Header>>,
    ) -> Result<DialAttempt> {
        let trunk = self
            .data_context
            .get_trunk(trunk_name)
            .ok_or_else(|| anyhow!("unknown trunk '{trunk_name}'"))?;
        if trunk.disabled.unwrap_or(false) {
            return Err(anyhow!("trunk '{trunk_name}' is disabled"));
        }
        let dest: rsipstack::sip::Uri = trunk
            .dest
            .as_str()
            .try_into()
            .map_err(|e| anyhow!("trunk '{trunk_name}' has invalid destination: {e:?}"))?;
        let callee = match callee.map(str::trim).filter(|value| !value.is_empty()) {
            Some(callee) => callee.to_string(),
            None => original
                .uri
                .user()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("request URI has no user to dial through trunk '{trunk_name}'"))?,
        };
        let callee = rsipstack::sip::Uri::try_from(build_sip_uri(&callee, &dest.host_with_port.to_string()))
            .map_err(|e| anyhow!("invalid callee {callee:?}: {e}"))?;

        let mut option = rsipstack::dialog::invitation::InviteOption {
            callee,
            caller,
            headers,
            ..Default::default()
        };
        let mut hints = hints;
        crate::proxy::routing::matcher::stamp_trunk(&mut option, &mut hints, trunk_name, &trunk)?;
        Ok(crate::config::RouteTarget {
            option,
            hints,
            retry_policy: retry_policy.or(trunk.retry_policy),
        }
        .into())
    }
}

/// Network path of the trunk the call arrived on (profile, media IPs,
/// ICE-lite), as each attempt's base hints. The static matcher applies the
/// arrival trunk the same way, under the destination trunk's settings.
fn arrival_hints(trunk: Option<&TrunkConfig>) -> Option<DialplanHints> {
    let trunk = trunk?;
    let mut hints = None;
    crate::proxy::routing::matcher::merge_trunk_media_hints(&mut hints, trunk);
    let all = hints?;
    let hints = DialplanHints {
        network_profile_id: all.network_profile_id,
        external_ip: all.external_ip,
        bind_ip: all.bind_ip,
        ice_lite: all.ice_lite,
        ..Default::default()
    };
    (hints.network_profile_id.is_some()
        || hints.external_ip.is_some()
        || hints.bind_ip.is_some()
        || hints.ice_lite.is_some())
    .then_some(hints)
}

#[derive(Serialize)]
struct HttpRequestPayload {
    pub call_id: String,
    /// Global session id (RFC 7989) resolved for this call, same value that
    /// RustPBX uses in the dialplan / RWI events. Letting the HTTP router
    /// know it lets it correlate route-time artifacts (e.g. X-cid) with
    /// RWI call events by exact session id instead of guessing.
    pub session_id: String,
    pub from: String,
    pub to: String,
    pub source_addr: Option<String>,
    /// Trunk the call arrived on (inbound calls only).
    pub source_trunk: Option<String>,
    pub direction: String,
    pub method: String,
    pub uri: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HttpRouteAction {
    Forward,
    Reject,
    Abort,
    NotHandled,
    Spam,
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HttpRouteStrategy {
    Sequential,
    Parallel,
}

/// String targets remain compatible with existing routing services.
#[derive(Deserialize)]
#[serde(untagged)]
enum HttpRouteTarget {
    Uri(String),
    Trunk(HttpTrunkTarget),
    Config {
        uri: String,
        retry_policy: Option<RetryPolicy>,
        /// Only here to reject a target that names both a URI and a trunk.
        trunk: Option<serde::de::IgnoredAny>,
    },
}

/// Dial through a configured trunk, which supplies everything else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpTrunkTarget {
    trunk: String,
    callee: Option<String>,
    retry_policy: Option<RetryPolicy>,
}

/// Partial call-level overrides; omitted values inherit the server configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpRtpConfig {
    bind_ip: Option<IpAddr>,
    external_ip: Option<IpAddr>,
}

#[derive(Deserialize)]
struct HttpResponsePayload {
    pub action: HttpRouteAction,
    pub targets: Option<Vec<HttpRouteTarget>>,
    pub rtp_config: Option<HttpRtpConfig>,
    pub strategy: Option<HttpRouteStrategy>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub record: Option<bool>,
    /// Per-call override for when automatic recording starts. When omitted,
    /// the global recording policy is inherited.
    pub record_start_at: Option<RecordingAutoStartAt>,
    /// Maximum call duration (in seconds)
    pub timeout: Option<u32>,
    /// Max ring time for call setup/ringback phase (in seconds)
    pub max_ring_time: Option<u32>,
    /// RTP timeout per direction in seconds — if no audio is received for this
    /// duration on either direction, the call is terminated. Overrides proxy-level
    /// `rtp_timeout`. Set to 0 to explicitly disable.
    pub rtp_timeout: Option<u32>,
    pub media_proxy: Option<MediaProxyMode>,
    pub headers: Option<HashMap<String, String>>,
    /// When set, overrides the custom-header passthrough for this route, in
    /// both directions (caller INVITE -> callee INVITE, callee 18x/200 ->
    /// caller). `true` copies all custom headers (equivalent to
    /// `header_passthrough.mode = "all"`); `false` copies none.
    /// When omitted, the internal-vs-trunk resolution applies (internal
    /// destinations copy everything, external trunks per their
    /// `header_passthrough` config, default none).
    pub with_original_headers: Option<bool>,
    pub extensions: Option<HashMap<String, String>>,
    /// Allowed audio codecs. If set, restricts the audio codecs used for this call.
    /// Values are codec names like "pcma", "pcmu", "g722", "opus", "g729".
    pub allow_codecs: Option<Vec<String>>,
    /// Caller override for the callee leg (From header). Accepts a full SIP
    /// URI ("sip:88888888@pbx.example.com"), a "user@host" shorthand, or a
    /// bare user which is qualified with the caller's realm. When omitted the
    /// authenticated caller is kept. An unparseable value fails the call
    /// immediately with 500 Server Internal Error.
    pub caller: Option<String>,
}

#[async_trait]
impl CallRouter for HttpCallRouter {
    async fn resolve(
        &self,
        original: &rsipstack::sip::Request,
        _route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, RouteError> {
        let source_trunk = cookie.get_extension::<TrunkContext>().map(|ctx| ctx.name.clone());
        let direction = if source_trunk.is_some() {
            DialDirection::Inbound
        } else {
            DialDirection::Internal
        };

        let call_id = original
            .call_id_header()
            .map(|h| h.value().to_string())
            .unwrap_or_default();
        // Global session id: HTTP-routed calls always participate — an
        // upstream Session-ID, else the id-shaped Call-ID (honouring
        // `session_id_inherit_callid`), else a fresh id. Resolved once so the
        // decision request and the dialplan carry the same id; resolving twice
        // would generate two for a Call-ID that is not id-shaped.
        let session_id = crate::call::session_id::extract_inherited(original)
            .unwrap_or_else(|| crate::call::session_id::derive_from_call_id(&call_id));
        let from = original
            .from_header()
            .map(|h| h.value().to_string())
            .unwrap_or_default();
        let to = original
            .to_header()
            .map(|h| h.value().to_string())
            .unwrap_or_default();

        let mut headers = HashMap::new();
        for header in original.headers.iter() {
            let h_str = header.to_string();
            if let Some((name, value)) = h_str.split_once(':') {
                headers.insert(name.trim().to_string(), value.trim().to_string());
            }
        }

        let body = String::from_utf8_lossy(&original.body).to_string();
        let source_addr = if let Ok(via) = original.via_header() {
            if let Ok((_, target)) = SipConnection::parse_target_from_via(via) {
                Some(target.to_string())
            } else {
                None
            }
        } else {
            None
        };

        let payload = HttpRequestPayload {
            call_id: call_id.clone(),
            session_id: session_id.clone(),
            from,
            to,
            source_addr,
            source_trunk: source_trunk.clone(),
            direction: direction.to_string(),
            method: original.method.to_string(),
            uri: original.uri.to_string(),
            headers,
            body,
        };

        let req = self.client.post(&self.config.url).json(&payload);
        let config_headers = self.config.headers.clone().unwrap_or_default();

        let start = Instant::now();
        let response = match crate::http_util::execute_request(req, &config_headers, None).await {
            Ok(resp) => resp,
            Err(e) => {
                let elapsed = start.elapsed();
                let err_str = e.to_string();
                let is_status_error = err_str.contains("HTTP returned");
                error!(
                    %call_id,
                    from = %payload.from,
                    to = %payload.to,
                    elapsed_ms = elapsed.as_millis(),
                    "HTTP router error: {}",
                    err_str
                );
                if is_status_error && self.config.fallback_to_static {
                    return Err(RouteError {
                        error: anyhow!("HTTP router returned error"),
                        status: None,
                        extensions: None,
                    }
                    .with_code(&crate::proxy::routing::http_error_catalog::UPSTREAM_ERROR));
                }
                return Err(RouteError {
                    error: anyhow!("HTTP router failed: {}", err_str),
                    status: Some(rsipstack::sip::StatusCode::ServiceUnavailable),
                    extensions: None,
                }
                .with_code(&crate::proxy::routing::http_error_catalog::UPSTREAM_FAILED));
            }
        };

        let elapsed = start.elapsed();
        let result: HttpResponsePayload = response.json().await.map_err(|e| {
            RouteError {
                error: anyhow!("Failed to parse HTTP router response: {}", e),
                status: Some(rsipstack::sip::StatusCode::ServerInternalError),
                extensions: None,
            }
            .with_code(&crate::proxy::routing::http_error_catalog::PARSE_FAILED)
        })?;

        info!(
            %call_id,
            from = %payload.from,
            to = %payload.to,
            elapsed_ms = elapsed.as_millis(),
            action = ?result.action,
            "HTTP router resolved route"
        );

        match result.action {
            HttpRouteAction::Spam => {
                cookie.mark_as_spam(SpamResult::Spam);
                return Err(RouteError {
                    error: anyhow!(
                        result
                            .reason
                            .unwrap_or_else(|| "marked as spam by HTTP router".to_string())
                    ),
                    status: Some(rsipstack::sip::StatusCode::Forbidden),
                    extensions: result.extensions,
                }
                .with_code(&crate::proxy::routing::http_error_catalog::SPAM));
            }
            HttpRouteAction::Reject | HttpRouteAction::Abort => {
                let status = result
                    .status
                    .map(rsipstack::sip::StatusCode::from)
                    .unwrap_or(rsipstack::sip::StatusCode::Forbidden);
                return Err(RouteError {
                    error: anyhow!(
                        result
                            .reason
                            .unwrap_or_else(|| "rejected by HTTP router".to_string())
                    ),
                    status: Some(status),
                    extensions: result.extensions,
                }
                .with_code(&crate::proxy::routing::http_error_catalog::REJECTED));
            }
            HttpRouteAction::NotHandled => {
                return Err(RouteError {
                    error: anyhow!("not handled by HTTP router"),
                    status: None,
                    extensions: result.extensions,
                }
                .with_code(&crate::proxy::routing::http_error_catalog::NOT_HANDLED));
            }
            HttpRouteAction::Forward => {
                let parse_error = |error: anyhow::Error| {
                    RouteError {
                        error,
                        status: Some(rsipstack::sip::StatusCode::ServerInternalError),
                        extensions: None,
                    }
                    .with_code(&crate::proxy::routing::http_error_catalog::PARSE_FAILED)
                };
                let custom_headers = result.headers.map(|h| {
                    h.iter()
                        .map(|(k, v)| rsipstack::sip::Header::Other(k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                });

                // Resolved before the targets: a trunk target starts its own
                // caller from it, as the static matcher does.
                let mut dialplan_caller = caller.from.clone();
                if let Some(caller_override) = result
                    .caller
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    let realm = caller.realm.clone().unwrap_or_else(|| {
                        caller
                            .from
                            .as_ref()
                            .map(|from| from.host().to_string())
                            .unwrap_or_default()
                    });
                    let uri = rsipstack::sip::Uri::try_from(build_sip_uri(caller_override, &realm))
                        .map_err(|e| {
                            parse_error(anyhow!(
                                "HTTP router returned invalid caller override {:?}: {}",
                                caller_override,
                                e
                            ))
                        })?;
                    dialplan_caller = Some(uri);
                }

                let arrival_trunk = source_trunk
                    .as_deref()
                    .and_then(|name| self.data_context.get_trunk(name));
                let rtp_bind_ip = result.rtp_config.as_ref().and_then(|c| c.bind_ip).map(|ip| ip.to_string());
                let rtp_external_ip =
                    result.rtp_config.as_ref().and_then(|c| c.external_ip).map(|ip| ip.to_string());
                let mut attempts = Vec::new();
                if let Some(targets) = result.targets {
                    for target in targets {
                        let (uri, retry_policy) = match target {
                            HttpRouteTarget::Uri(uri) => (uri, None),
                            HttpRouteTarget::Config { trunk: Some(_), .. } => {
                                return Err(parse_error(anyhow!(
                                    "HTTP router target names both a uri and a trunk"
                                )));
                            }
                            HttpRouteTarget::Config { uri, retry_policy, .. } =>
                                (uri, retry_policy),
                            HttpRouteTarget::Trunk(HttpTrunkTarget { trunk, callee, retry_policy }) => {
                                if let Some(policy) = &retry_policy {
                                    policy.validate().map_err(parse_error)?;
                                }
                                let attempt = self
                                    .trunk_attempt(
                                        arrival_hints(arrival_trunk.as_ref()),
                                        &trunk,
                                        callee.as_deref(),
                                        retry_policy,
                                        original,
                                        dialplan_caller.clone().unwrap_or_default(),
                                        custom_headers.clone(),
                                    )
                                    .map_err(|e| parse_error(e.context("HTTP router trunk target")))?;
                                attempts.push(attempt);
                                continue;
                            }
                        };
                        if let Some(policy) = &retry_policy {
                            policy.validate().map_err(parse_error)?;
                        }
                        // Preserve legacy HTTP routing: skip malformed target URIs.
                        let Ok(uri) = rsipstack::sip::Uri::try_from(uri) else {
                            continue;
                        };
                        attempts.push(DialAttempt {
                            retry_policy,
                            hints: arrival_hints(arrival_trunk.as_ref()).map(Arc::new),
                            ..Location {
                                aor: uri,
                                headers: custom_headers.clone(),
                                ..Default::default()
                            }.into()
                        });
                    }
                }

                // Each attempt's hints layer the arrival trunk, then the target
                // trunk, then the response's RTP addresses: explicit hint IPs
                // apply after a hint's network profile, so `rtp_config` wins.
                if rtp_bind_ip.is_some() || rtp_external_ip.is_some() {
                    for attempt in &mut attempts {
                        let hints = attempt.hints.get_or_insert_with(Default::default);
                        // Freshly built here, so never shared yet.
                        if let Some(hints) = Arc::get_mut(hints) {
                            if rtp_bind_ip.is_some() {
                                hints.bind_ip = rtp_bind_ip.clone();
                            }
                            if rtp_external_ip.is_some() {
                                hints.external_ip = rtp_external_ip.clone();
                            }
                        }
                    }
                }

                // The sequential dialer times a trunk's attempt only within a
                // sequence; a lone trunk target's ring time is the call's.
                let single_ring_time = match attempts.as_slice() {
                    [only] => only.hints.as_ref().and_then(|hints| hints.max_ring_time),
                    _ => None,
                };
                let strategy = match result.strategy {
                    Some(HttpRouteStrategy::Parallel) => DialStrategy::Parallel(
                        attempts.into_iter().map(|attempt| attempt.location).collect(),
                    ),
                    _ => DialStrategy::Sequential(attempts),
                };

                let mut dialplan = Dialplan::new(session_id, original.clone(), direction)
                    .with_session_id_enabled(true);

                // Start from server defaults, then let HTTP router override individual fields.
                let rtp_cfg = self.rtp_config.load();
                dialplan.media.proxy_mode = **self.default_media_proxy_mode.load();
                dialplan.media.external_ip = rtp_cfg.external_ip.clone();
                dialplan.media.bind_ip = rtp_cfg.bind_ip.clone();
                dialplan.media.rtp_start_port = rtp_cfg.start_port;
                dialplan.media.rtp_end_port = rtp_cfg.end_port;
                dialplan.media.webrtc_port_start = rtp_cfg.webrtc_start_port;
                dialplan.media.webrtc_port_end = rtp_cfg.webrtc_end_port;
                dialplan.media.ice_servers = rtp_cfg.ice_servers.clone();
                dialplan.media.enable_latching = self.enable_latching;
                dialplan.media.probation_max_packets = self.probation_max_packets;
                dialplan.media.ice_lite = rtp_cfg.ice_lite;
                // Call-wide too: parallel dialing carries no per-attempt hints.
                if rtp_bind_ip.is_some() {
                    dialplan.media.bind_ip = rtp_bind_ip;
                }
                if rtp_external_ip.is_some() {
                    dialplan.media.external_ip = rtp_external_ip;
                }

                if let Some(uri) = dialplan_caller {
                    dialplan = dialplan.with_caller(uri);
                }

                dialplan = dialplan.with_targets(strategy);

                if result.record.is_some() {
                    if let Some(record) = result.record {
                        dialplan.recording.enabled = record;
                    }
                    dialplan.recording_policy = Some(RecordingPolicy {
                        enabled: result.record,
                        auto_start_at: result.record_start_at,
                        ..Default::default()
                    });
                }

                if let Some(mode) = result.media_proxy {
                    dialplan.media.proxy_mode = mode;
                }

                // `with_original_headers` is an explicit per-route override of
                // the original-header passthrough rule. When omitted, the
                // internal-vs-trunk resolution applies at INVITE build time.
                if let Some(with_orig) = result.with_original_headers {
                    dialplan.header_passthrough = Some(if with_orig {
                        crate::proxy::routing::HeaderPassthrough::all()
                    } else {
                        crate::proxy::routing::HeaderPassthrough::none()
                    });
                }

                if let Some(exts) = result.extensions {
                    dialplan = dialplan.with_extension(exts);
                }

                if let Some(timeout) = result.timeout {
                    dialplan.max_call_duration = Some(Duration::from_secs(timeout as u64));
                }

                if single_ring_time.is_some() {
                    dialplan.max_ring_time = single_ring_time;
                }
                if let Some(max_ring_time) = result.max_ring_time {
                    dialplan.max_ring_time =
                        (max_ring_time > 0).then(|| Duration::from_secs(max_ring_time as u64));
                }

                if let Some(rtp_timeout) = result.rtp_timeout {
                    if rtp_timeout > 0 {
                        dialplan.rtp_timeout = Some(Duration::from_secs(rtp_timeout as u64));
                    } else {
                        dialplan.rtp_timeout = None;
                    }
                }

                apply_allowed_codecs(&mut dialplan, result.allow_codecs.as_deref(), None);

                Ok(dialplan)
            }
        }
    }
}