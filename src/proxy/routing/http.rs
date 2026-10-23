use crate::call::cookie::SpamResult;
use crate::call::{
    DialAttempt, DialDirection, DialStrategy, Dialplan, Location, RouteInvite, SipUser, TransactionCookie,
    TrunkContext, build_sip_uri,
};
use crate::config::{
    DialplanHints, HttpRouterConfig, MediaProxyMode, RecordingAutoStartAt, RecordingPolicy,
    RtpConfig,
};
use crate::proxy::call::{CallRouter, RouteError, apply_allowed_codecs};
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
    /// Trunk registry, wired at server build (after the proxy data context is
    /// resolved) so structured targets can name a trunk explicitly.
    pub data_context: Arc<crate::proxy::data::ProxyDataContext>,
}

impl HttpCallRouter {
    pub fn new(
        config: HttpRouterConfig,
        rtp_config: ArcSwap<RtpConfig>,
        default_media_proxy_mode: ArcSwap<MediaProxyMode>,
        enable_latching: bool,
        probation_max_packets: Option<u8>,
        data_context: Arc<crate::proxy::data::ProxyDataContext>,
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

    /// Resolve a named trunk for a structured target the way the RWI
    /// originate path does (`resolve_originate_trunk` in rwi/processor.rs):
    /// unknown or disabled trunks are config errors, fail-closed.
    fn resolve_router_trunk(&self, name: &str) -> Result<TrunkConfig, String> {
        let trunk = self
            .data_context
            .get_trunk(name)
            .ok_or_else(|| format!("unknown trunk '{name}'"))?;
        if trunk.disabled.unwrap_or(false) {
            return Err(format!("trunk '{name}' is disabled"));
        }
        Ok(trunk)
    }

    /// String/config targets stay bare Locations (the original schema, kept
    /// verbatim so existing decision services are unaffected). Ok(None):
    /// malformed target URI — preserved legacy behavior, skip it.
    fn build_uri_attempt(
        uri: String,
        retry_policy: Option<RetryPolicy>,
        headers: Option<Vec<rsipstack::sip::Header>>,
    ) -> Result<Option<DialAttempt>, RouteError> {
        if let Some(policy) = &retry_policy {
            policy.validate().map_err(|error| RouteError {
                error,
                status: Some(rsipstack::sip::StatusCode::ServerInternalError),
                extensions: None,
            }.with_code(&crate::proxy::routing::http_error_catalog::PARSE_FAILED))?;
        }
        // Preserve legacy HTTP routing: skip malformed target URIs.
        let Ok(uri) = rsipstack::sip::Uri::try_from(uri) else {
            return Ok(None);
        };
        Ok(Some(DialAttempt {
            retry_policy,
            ..Location {
                aor: uri,
                headers,
                ..Default::default()
            }.into()
        }))
    }

    /// Build a dial attempt from one structured target. A target naming a
    /// `trunk` is resolved against the trunk registry and stamped the same
    /// way the static matcher stamps its selected trunk: `apply_trunk_config`
    /// (destination next-hop, transport, credentials, host rewrite,
    /// P-Asserted-Identity), `merge_trunk_media_hints` (media), and outbound
    /// trunk metadata (CDR attribution) — plus the trunk's CPS /
    /// concurrent-call gates.
    ///
    /// A bare-user callee qualifies against the trunk's destination host when
    /// a trunk names the route, else the original request's host — so
    /// `{"callee": "6001", "trunk": "gw"}` dials `sip:6001@<trunk dest>`.
    ///
    /// Ok(None): the target trunk's call budget is exhausted — transient;
    /// routing advances to the next target (use_next_gw semantics). Err:
    /// unknown/disabled trunk or an unparsable target — a decision-response /
    /// config error, fail-closed.
    #[allow(clippy::too_many_arguments)]
    fn build_trunk_attempt(
        &self,
        callee: Option<String>,
        trunk: Option<String>,
        caller: Option<String>,
        retry_policy: Option<RetryPolicy>,
        target_headers: Option<HashMap<String, String>>,
        route_headers: Option<Vec<rsipstack::sip::Header>>,
        callee_realm: &str,
        caller_uri: rsipstack::sip::Uri,
        call_id: &str,
    ) -> Result<Option<DialAttempt>, RouteError> {
        let parse_error = |message: String| {
            RouteError {
                error: anyhow!("{message}"),
                status: Some(rsipstack::sip::StatusCode::ServerInternalError),
                extensions: None,
            }
            .with_code(&crate::proxy::routing::http_error_catalog::PARSE_FAILED)
        };
        let clean = |value: Option<String>| -> Option<String> {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };

        // Per-target headers merge into (and override) the route-level ones.
        let mut headers = route_headers.unwrap_or_default();
        if let Some(target_headers) = &target_headers {
            headers.extend(target_headers.iter().map(|(k, v)| {
                rsipstack::sip::Header::Other(k.clone(), v.clone())
            }));
        }

        let (callee, trunk_name, caller_override) =
            (clean(callee), clean(trunk), clean(caller));

        // Resolve the named trunk first — its destination host qualifies a
        // bare-user callee.
        let trunk = match trunk_name.as_deref() {
            Some(name) => Some(
                self.resolve_router_trunk(name)
                    .map_err(|e| parse_error(format!("HTTP router target trunk: {e}")))?,
            ),
            None => None,
        };
        let callee = callee.ok_or_else(|| {
            parse_error("HTTP router structured target is missing a callee".to_string())
        })?;

        let qualify_realm = match &trunk {
            Some(trunk) => {
                let dest_uri: rsipstack::sip::Uri = trunk.dest.as_str().try_into().map_err(|e| {
                    parse_error(format!(
                        "trunk '{}' has invalid destination '{}': {:?}",
                        trunk_name.as_deref().unwrap_or_default(),
                        trunk.dest,
                        e
                    ))
                })?;
                dest_uri.host_with_port.to_string()
            }
            None => callee_realm.to_string(),
        };
        let callee_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(build_sip_uri(&callee, &qualify_realm))
                .map_err(|e| parse_error(format!("invalid callee '{callee}': {e}")))?;

        let mut option = rsipstack::dialog::invitation::InviteOption {
            callee: callee_uri,
            caller: caller_uri,
            headers: Some(headers),
            ..Default::default()
        };

        let mut hints: Option<DialplanHints> = None;
        if let Some(trunk) = &trunk {
            let name = trunk_name.as_deref().unwrap_or_default();
            // Destination-trunk CPS / concurrent-call gates — the same gates
            // the static matcher enforces before stamping (matcher.rs). On
            // reject the target is skipped: transient overload, routing
            // advances (use_next_gw semantics).
            if let Some(limiter) = &trunk.cps_limiter {
                if let Err(reason) = limiter.try_acquire() {
                    info!(
                        call_id = %call_id,
                        trunk = %name,
                        reason = %reason,
                        "target trunk CPS limit exceeded, skipping target"
                    );
                    return Ok(None);
                }
            }
            if let Some(limiter) = &trunk.concurrent_call_limiter {
                match limiter.try_acquire() {
                    Ok(permit) => {
                        hints
                            .get_or_insert_with(DialplanHints::default)
                            .concurrent_call_lease
                            .push(permit);
                    }
                    Err(reason) => {
                        info!(
                            call_id = %call_id,
                            trunk = %name,
                            reason = %reason,
                            "target trunk concurrent-call limit exceeded, skipping target"
                        );
                        return Ok(None);
                    }
                }
            }
            crate::proxy::routing::matcher::apply_trunk_config(&mut option, trunk)
                .map_err(|e| parse_error(format!("trunk '{name}' config failed: {e}")))?;
            crate::proxy::routing::matcher::merge_trunk_media_hints(&mut hints, trunk);
            crate::proxy::routing::matcher::attach_outbound_trunk_metadata(
                &mut hints,
                name,
                trunk,
            );
            info!(
                call_id = %call_id,
                trunk = %name,
                dest = ?option.destination,
                "target trunk resolved for http-routed call"
            );
        }

        let mut attempt = DialAttempt {
            location: Location {
                aor: option.callee,
                destination: option.destination,
                credential: option.credential,
                headers: option.headers,
                ..Default::default()
            },
            caller: None,
            hints: hints.map(Arc::new),
            retry_policy,
        };
        // A trunk fixes the leg the same way the static path does: the
        // (possibly rewritten) caller becomes the per-attempt override
        // (plan_for_attempt prefers attempt.caller over the shared one).
        if trunk.is_some() {
            attempt.caller = Some(option.caller);
        }
        if let Some(caller_override) = caller_override {
            let uri = rsipstack::sip::Uri::try_from(build_sip_uri(&caller_override, callee_realm))
                .map_err(|e| {
                    parse_error(format!("invalid caller override '{caller_override}': {e}"))
                })?;
            attempt.caller = Some(uri);
        }
        Ok(Some(attempt))
    }
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
    /// Trunk the call arrived on (inbound only), resolved by the proxy from
    /// its own trunk registry — the decision service classifies by source
    /// trunk name instead of guessing from source IPs.
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

/// One route-decision target. URI-string targets remain compatible with
/// existing routing services; the structured object form adds an explicit
/// trunk name plus per-target callee/caller/header/retry overrides. Untagged,
/// so both forms deserialize and existing decision services keep working.
#[derive(Deserialize)]
#[serde(untagged)]
enum HttpRouteTarget {
    Uri(String),
    Config {
        uri: String,
        retry_policy: Option<RetryPolicy>,
    },
    Structured {
        callee: Option<String>,
        trunk: Option<String>,
        caller: Option<String>,
        retry_policy: Option<RetryPolicy>,
        headers: Option<HashMap<String, String>>,
    },
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
    /// When set, overrides the original-header passthrough for this route.
    /// `true` forwards the original INVITE's custom headers to the callee leg
    /// (equivalent to `header_passthrough.mode = "all"`); `false` forwards none.
    /// When omitted, the internal-vs-trunk resolution applies (internal
    /// destinations forward everything, external trunks forward per their
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
    /// Network profile id to apply for this call. Overrides trunk.profile
    /// when the route has no resolved trunk (e.g. passive gateways returning
    /// pure-URI targets).
    pub network_profile_id: Option<String>,
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
        // One trunk-registry lookup drives both the direction flag and the
        // `source_trunk` payload field — inbound calls carry the trunk name
        // the call arrived on.
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

        // RFC 7989 唯一入向确立点：整条 resolve 内（决策请求、Dialplan，以及经
        // Dialplan 继承的 RWI 事件）共用这一份 session。WebRTC 的 JsSIP Call-ID
        // （uuid@host）非 UUID 形且无 Session-ID 头，若两处各自 resolve_incoming
        // 都会 new_v4() 生成不同随机值，导致决策与拨号分裂成两张卡——这里一次
        // 确立后全局继承，不再重复生成。
        let session_id = crate::call::session_id::resolve_incoming(&original, &call_id);
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
            source_trunk,
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
                let custom_headers = result.headers.map(|h| {
                    h.iter()
                        .map(|(k, v)| rsipstack::sip::Header::Other(k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                });

                // Bare-user callees qualify against the original request's host;
                // structured targets naming a trunk qualify against the trunk dest.
                let callee_realm = original.uri.host().to_string();
                let caller_uri = caller.from.clone().unwrap_or_default();
                let mut attempts: Vec<DialAttempt> = Vec::new();
                if let Some(targets) = result.targets {
                    for target in targets {
                        let attempt = match target {
                            HttpRouteTarget::Uri(uri) => {
                                Self::build_uri_attempt(uri, None, custom_headers.clone())?
                            }
                            HttpRouteTarget::Config { uri, retry_policy } => {
                                Self::build_uri_attempt(uri, retry_policy, custom_headers.clone())?
                            }
                            HttpRouteTarget::Structured {
                                callee,
                                trunk,
                                caller,
                                retry_policy,
                                headers,
                            } => self.build_trunk_attempt(
                                callee,
                                trunk,
                                caller,
                                retry_policy,
                                headers,
                                custom_headers.clone(),
                                &callee_realm,
                                caller_uri.clone(),
                                &call_id,
                            )?,
                        };
                        if let Some(attempt) = attempt {
                            attempts.push(attempt);
                        }
                    }
                }

                if let Some(profile_id) = result.network_profile_id.as_deref().filter(|s| !s.is_empty()) {
                    for attempt in &mut attempts {
                        let mut hints: DialplanHints = match attempt.hints.take() {
                            Some(h) => Arc::try_unwrap(h).unwrap_or_else(|_| DialplanHints::default()),
                            None => DialplanHints::default(),
                        };
                        if hints.network_profile_id.is_none() {
                            hints.network_profile_id = Some(profile_id.to_string());
                        }
                        attempt.hints = Some(Arc::new(hints));
                    }
                }

                let strategy = match result.strategy {
                    Some(HttpRouteStrategy::Parallel) => {
                        // DialStrategy::Parallel carries bare Locations (no
                        // per-attempt hints channel yet): trunk stamps on the
                        // Location (destination/transport/credential) still
                        // apply, media hints / CDR attribution / CPS permits
                        // are sequential-only for now.
                        DialStrategy::Parallel(
                            attempts.into_iter().map(|attempt| attempt.location).collect(),
                        )
                    }
                    _ => DialStrategy::Sequential(attempts),
                };

                // Global session id (RFC 7989): 复用函数开头唯一确立的那一份，不再
                // 重新 resolve_incoming，确保 Dialplan（进而 RWI 事件）与决策请求
                // 的 session 完全一致。
                let session_id = session_id.clone();
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
                if let Some(config) = result.rtp_config {
                    if let Some(ip) = config.bind_ip {
                        dialplan.media.bind_ip = Some(ip.to_string());
                    }
                    if let Some(ip) = config.external_ip {
                        dialplan.media.external_ip = Some(ip.to_string());
                    }
                }

                if let Some(from) = caller.from.as_ref() {
                    dialplan = dialplan.with_caller(from.clone());
                }

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
                    match rsipstack::sip::Uri::try_from(build_sip_uri(caller_override, &realm)) {
                        Ok(uri) => dialplan = dialplan.with_caller(uri),
                        Err(e) => {
                            return Err(RouteError {
                                error: anyhow!(
                                    "HTTP router returned invalid caller override {:?}: {}",
                                    caller_override,
                                    e
                                ),
                                status: Some(rsipstack::sip::StatusCode::ServerInternalError),
                                extensions: None,
                            }
                            .with_code(&crate::proxy::routing::http_error_catalog::PARSE_FAILED));
                        }
                    }
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
                    dialplan.header_passthrough = if with_orig {
                        Some(crate::proxy::routing::HeaderPassthrough::all())
                    } else {
                        None
                    };
                }

                if let Some(exts) = result.extensions {
                    dialplan = dialplan.with_extension(exts);
                }

                if let Some(timeout) = result.timeout {
                    dialplan.max_call_duration = Some(Duration::from_secs(timeout as u64));
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