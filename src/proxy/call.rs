use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::runtime::SessionId;
use crate::call::{
    CalleeDisplayName, DialDirection, DialStrategy, Dialplan, Location, MediaConfig, RouteInvite,
    RoutingState, SipUser, TransactionCookie, TrunkContext,
};
use crate::config::{ProxyConfig, RouteResult};
use crate::media::{Track, recorder::RecorderOption};
use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
use crate::proxy::data::ProxyDataContext;
use crate::proxy::proxy_call::CallSessionBuilder;
use crate::proxy::proxy_call::sip_session::SipSession;
use crate::proxy::routing::{
    RouteRule, SourceTrunk, TrunkConfig, build_source_trunk,
    matcher::{RouteResourceLookup, match_invite},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use chrono::Utc;
use futures::FutureExt;
use glob::Pattern;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::Dialog;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::transaction::key::TransactionRole;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipConnection;
use std::{collections::HashMap, net::IpAddr, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Error type returned by [`CallRouter::resolve`] on failure.
#[derive(Debug)]
pub struct RouteError {
    pub error: anyhow::Error,
    pub status: Option<rsipstack::sip::StatusCode>,
    /// Optional metadata extensions carried from the router response (e.g. HTTP router abort/reject).
    pub extensions: Option<HashMap<String, String>>,
}

impl<E: Into<anyhow::Error>> From<(E, Option<rsipstack::sip::StatusCode>)> for RouteError {
    fn from((error, status): (E, Option<rsipstack::sip::StatusCode>)) -> Self {
        Self {
            error: error.into(),
            status,
            extensions: None,
        }
    }
}

#[async_trait]
pub trait CallRouter: Send + Sync {
    async fn resolve(
        &self,
        original: &rsipstack::sip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, RouteError>;
}

fn q850_cause_from_status(code: &rsipstack::sip::StatusCode) -> u16 {
    match u16::from(code.clone()) {
        400 | 401 | 402 | 403 | 405 | 406 | 407 | 421 | 603 => 21, // call rejected / not allowed
        404 | 484 | 604 => 1,                                      // unallocated number
        410 => 22,                                                 // number changed
        413 | 414 | 416 | 420 => 127, // interworking / feature not supported
        480 | 408 => 18,              // no user responding / timeout
        486 | 600 => 17,              // user busy
        487 => 31,                    // request terminated / normal unspecified
        488 | 606 => 79,              // service or option not available
        502 | 503 => 38,              // network out of order
        500 | 580 => 41,              // temporary failure
        504 => 34,                    // no circuit / channel available
        500..=599 => 41,
        _ => 16,
    }
}

fn escape_reason_text(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Decide what to do when routing returned NotHandled (no explicit forward/queue).
/// Returns `Ok(targets)` to proceed with the given locations, or `Err` to reject.
fn resolve_unhandled_targets(
    callee_is_same_realm: bool,
    internal_lookup_empty: bool,
    locs: Vec<Location>,
) -> Result<DialStrategy, RouteError> {
    if callee_is_same_realm && internal_lookup_empty {
        return Err(RouteError::from((
            anyhow!("target user is offline"),
            Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
        )));
    }
    if !callee_is_same_realm {
        return Err(RouteError::from((
            anyhow!("no route found for external destination"),
            Some(rsipstack::sip::StatusCode::NotFound),
        )));
    }
    Ok(DialStrategy::Sequential(locs))
}

pub fn q850_reason_value(code: &rsipstack::sip::StatusCode, detail: Option<&str>) -> String {
    let fallback = format!("SIP {}", u16::from(code.clone()));
    let text = detail
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or(fallback);
    format!(
        "Q.850;cause={};text=\"{}\"",
        q850_cause_from_status(code),
        escape_reason_text(&text)
    )
}

#[async_trait]
pub trait DialplanInspector: Send + Sync {
    async fn inspect_dialplan(
        &self,
        dialplan: Dialplan,
        cookie: &TransactionCookie,
        original: &rsipstack::sip::Request,
    ) -> Result<Dialplan, RouteError>;
}

pub struct DefaultRouteInvite {
    pub routing_state: Arc<RoutingState>,
    pub data_context: Arc<ProxyDataContext>,
    pub source_trunk_hint: Option<String>,
}

#[async_trait]
impl RouteInvite for DefaultRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let (trunks_snapshot, routes_snapshot, source_trunk) =
            self.build_context(origin, direction).await;
        if matches!(direction, DialDirection::Inbound)
            && let Some(source) = source_trunk.as_ref()
            && let Some(trunk_cfg) = trunks_snapshot.get(&source.name)
        {
            let from_user = extract_from_user(origin);
            let to_user = extract_to_user(origin);
            match trunk_cfg.matches_incoming_user_prefixes(from_user.as_deref(), to_user.as_deref())
            {
                Ok(true) => {}
                Ok(false) => {
                    let detail = format!(
                        "caller='{}', callee='{}' rejected by prefix policy",
                        from_user.as_deref().unwrap_or(""),
                        to_user.as_deref().unwrap_or("")
                    );
                    let reason =
                        q850_reason_value(&rsipstack::sip::StatusCode::Forbidden, Some(&detail));
                    warn!(
                        trunk = %source.name,
                        from = from_user.as_deref().unwrap_or(""),
                        to = to_user.as_deref().unwrap_or(""),
                        reason = %reason,
                        "dropping inbound INVITE due to SIP trunk user prefix mismatch",
                    );
                    return Ok(RouteResult::Abort(
                        rsipstack::sip::StatusCode::Forbidden,
                        Some(reason),
                    ));
                }
                Err(mismatch) => {
                    let reason = q850_reason_value(
                        &rsipstack::sip::StatusCode::Forbidden,
                        Some(&mismatch.to_string()),
                    );
                    warn!(
                        trunk = %source.name,
                        from = from_user.as_deref().unwrap_or(""),
                        to = to_user.as_deref().unwrap_or(""),
                        reason = %reason,
                        "dropping inbound INVITE due to SIP trunk user prefix mismatch",
                    );
                    return Ok(RouteResult::Abort(
                        rsipstack::sip::StatusCode::Forbidden,
                        Some(reason),
                    ));
                }
            }
        }
        let resource_lookup = self.data_context.as_ref() as &dyn RouteResourceLookup;
        match_invite(
            if trunks_snapshot.is_empty() {
                None
            } else {
                Some(&trunks_snapshot)
            },
            if routes_snapshot.is_empty() {
                None
            } else {
                Some(&routes_snapshot)
            },
            Some(resource_lookup),
            option,
            origin,
            source_trunk.as_ref(),
            self.routing_state.clone(),
            direction,
        )
        .await
    }

    async fn preview_route(
        &self,
        option: InviteOption,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let (trunks_snapshot, routes_snapshot, source_trunk) =
            self.build_context(origin, direction).await;

        let resource_lookup = self.data_context.as_ref() as &dyn RouteResourceLookup;
        match_invite(
            if trunks_snapshot.is_empty() {
                None
            } else {
                Some(&trunks_snapshot)
            },
            if routes_snapshot.is_empty() {
                None
            } else {
                Some(&routes_snapshot)
            },
            Some(resource_lookup),
            option,
            origin,
            source_trunk.as_ref(),
            self.routing_state.clone(),
            direction,
        )
        .await
    }
}

impl DefaultRouteInvite {
    async fn build_context(
        &self,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
    ) -> (
        std::collections::HashMap<String, TrunkConfig>,
        Vec<RouteRule>,
        Option<SourceTrunk>,
    ) {
        let trunks_snapshot = self.data_context.trunks_snapshot();
        let routes_snapshot = self.data_context.routes_snapshot();
        let source_trunk = self
            .resolve_source_trunk(&trunks_snapshot, origin, direction)
            .await;
        (trunks_snapshot, routes_snapshot, source_trunk)
    }

    async fn resolve_source_trunk(
        &self,
        trunks: &HashMap<String, TrunkConfig>,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
    ) -> Option<SourceTrunk> {
        if !matches!(direction, DialDirection::Inbound) {
            return None;
        }

        if let Some(name) = self.source_trunk_hint.as_ref()
            && let Some(config) = trunks.get(name)
        {
            return build_source_trunk(name.clone(), config, direction);
        }

        let via = origin.via_header().ok()?;
        let (_, target) = SipConnection::parse_target_from_via(via).ok()?;
        let ip: IpAddr = target.host.try_into().ok()?;
        let name = self.data_context.find_trunk_by_ip(&ip).await?;
        let config = trunks.get(&name)?;
        build_source_trunk(name, config, direction)
    }
}

fn extract_from_user(origin: &rsipstack::sip::Request) -> Option<String> {
    origin
        .from_header()
        .ok()
        .and_then(|header| header.uri().ok())
        .and_then(|uri| uri.user().map(|u| u.to_string()))
}

fn extract_to_user(origin: &rsipstack::sip::Request) -> Option<String> {
    origin
        .to_header()
        .ok()
        .and_then(|header| header.uri().ok())
        .and_then(|uri| uri.user().map(|u| u.to_string()))
}

fn resolve_callee_uri(origin: &rsipstack::sip::Request) -> Result<rsipstack::sip::Uri> {
    if origin
        .uri
        .user()
        .map(|user| !user.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok(origin.uri.clone());
    }

    origin
        .to_header()
        .map_err(anyhow::Error::from)?
        .uri()
        .map_err(anyhow::Error::from)
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    server: SipServerRef,
    pub dialog_layer: Arc<DialogLayer>,
    pub routing_state: Arc<RoutingState>,
}

#[derive(Clone)]
pub struct CallModule {
    pub(crate) inner: Arc<CallModuleInner>,
}

impl CallModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CallModule::new(config, server);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>, server: SipServerRef) -> Self {
        let dialog_layer = server.dialog_layer.clone();
        let mut routing_state = RoutingState::new();
        let limiter = server
            .frequency_limiter
            .clone()
            .or_else(|| match config.frequency_limiter.as_deref() {
            Some("db") => {
                if let Some(db) = server.database.clone() {
                    let l = crate::call::policy::DbFrequencyLimiter::new(db);
                    let l_clone = l.clone();
                    let token = server.cancel_token.child_token();
                    crate::utils::spawn(async move {
                        l_clone.run_cleanup_loop(token).await;
                    });
                    Some(l)
                } else {
                    warn!("Frequency limiter configured as 'db' but no database connection available. Falling back to in-memory.");
                    let l = crate::call::policy::InMemoryFrequencyLimiter::new();
                    let l_clone = l.clone();
                    let token = server.cancel_token.child_token();
                    crate::utils::spawn(async move {
                        l_clone.run_cleanup_loop(token).await;
                    });
                    Some(l)
                }
            }
            Some(_) => {
                let l = crate::call::policy::InMemoryFrequencyLimiter::new();
                let l_clone = l.clone();
                let token = server.cancel_token.child_token();
                crate::utils::spawn(async move {
                    l_clone.run_cleanup_loop(token).await;
                });
                Some(l)
            }
            None => None,
        });

        if let Some(limiter) = limiter {
            routing_state.policy_guard =
                Some(Arc::new(crate::call::policy::PolicyGuard::new(limiter)));
        }

        let inner = Arc::new(CallModuleInner {
            config,
            server,
            dialog_layer,
            routing_state: Arc::new(routing_state),
        });
        Self { inner }
    }

    async fn default_resolve(
        &self,
        original: &rsipstack::sip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, RouteError> {
        let callee_uri = resolve_callee_uri(original).map_err(|e| RouteError::from((e, None)))?;
        let callee_realm = callee_uri.host().to_string();

        let dialog_id = original
            .call_id_header()
            .map_err(|e| RouteError::from((anyhow::anyhow!(e), None)))?
            .value();
        let session_id = dialog_id.to_string();

        let media_config = MediaConfig::new()
            .with_proxy_mode(self.inner.config.media_proxy)
            .with_external_ip(self.inner.server.rtp_config.external_ip.clone())
            .with_rtp_start_port(self.inner.server.rtp_config.start_port)
            .with_rtp_end_port(self.inner.server.rtp_config.end_port)
            .with_webrtc_start_port(self.inner.server.rtp_config.webrtc_start_port)
            .with_webrtc_end_port(self.inner.server.rtp_config.webrtc_end_port)
            .with_ice_servers(self.inner.server.rtp_config.ice_servers.clone());

        let caller_is_same_realm = self
            .inner
            .server
            .is_same_realm(caller.realm.as_deref().unwrap_or(""))
            .await;
        let callee_is_same_realm = self.inner.server.is_same_realm(&callee_realm).await;

        let is_from_trunk = cookie.get_extension::<TrunkContext>().is_some();

        let direction = if caller_is_same_realm && callee_is_same_realm && !is_from_trunk {
            match self
                .inner
                .server
                .user_backend
                .get_user(
                    callee_uri.user().unwrap_or_default(),
                    Some(&callee_realm),
                    Some(original),
                )
                .await
            {
                Ok(None) => {
                    // User not found locally — check if callee is registered on a cluster peer
                    match self.inner.server.locator.lookup(&callee_uri).await {
                        Ok(locs) if !locs.is_empty() => {
                            info!(dialog_id, callee = %callee_uri, "Callee not in local user backend but found in shared locator; treating as internal cluster call");
                            DialDirection::Internal
                        }
                        _ => DialDirection::Outbound,
                    }
                }
                res => {
                    if let Some(display_name) =
                        res.ok().flatten().and_then(|user| user.display_name)
                    {
                        cookie.insert_extension(CalleeDisplayName(display_name))
                    }
                    DialDirection::Internal
                }
            }
        } else if caller_is_same_realm && callee_is_same_realm {
            DialDirection::Outbound
        } else if caller_is_same_realm && !callee_is_same_realm {
            DialDirection::Outbound
        } else if !caller_is_same_realm && callee_is_same_realm {
            DialDirection::Inbound
        } else {
            if is_from_trunk {
                // If the call comes from a trunk, we can allow it to reach an internal destination even if the callee realm doesn't match, as long as the caller realm also doesn't match (to prevent external-to-external calls).
                // This allows for more flexible routing from trusted trunks.
                DialDirection::Inbound
            } else {
                warn!(dialog_id, caller_realm = ?caller.realm, callee_realm, "Both caller and callee are external realm, reject");
                return Err(RouteError::from((
                    anyhow::anyhow!("Both caller and callee are external realm"),
                    Some(rsipstack::sip::StatusCode::Forbidden),
                )));
            }
        };

        let mut locs = vec![Location {
            aor: callee_uri.clone(),
            ..Default::default()
        }];
        let mut internal_lookup_empty = false;

        let callee_forwarding = self
            .resolve_callee_user(original)
            .await
            .ok()
            .flatten()
            .and_then(|callee| callee.forwarding_config());

        let always_forwarding = matches!(
            callee_forwarding.as_ref().map(|cfg| &cfg.mode),
            Some(crate::call::CallForwardingMode::Always)
        );

        let mut forced_preview_forward: Option<InviteOption> = None;
        let mut forced_pending_queue: Option<crate::call::QueuePlan> = None;
        let mut forced_pending_app: Option<(String, Option<serde_json::Value>, bool)> = None;

        if let Some(config) = callee_forwarding.as_ref()
            && matches!(config.mode, crate::call::CallForwardingMode::Always)
        {
            match &config.endpoint {
                crate::call::TransferEndpoint::Uri(uri) => {
                    let forwarded_uri =
                        rsipstack::sip::Uri::try_from(uri.as_str()).map_err(|e| {
                            RouteError::from((
                                anyhow!("invalid always-forwarding target '{}': {}", uri, e),
                                Some(rsipstack::sip::StatusCode::ServerInternalError),
                            ))
                        })?;
                    forced_preview_forward = Some(InviteOption {
                        callee: forwarded_uri,
                        ..Default::default()
                    });
                }
                crate::call::TransferEndpoint::Queue(queue_ref) => {
                    let reference = queue_ref.trim();
                    if reference.is_empty() {
                        return Err(RouteError::from((
                            anyhow!("always-forwarding queue reference is empty"),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        )));
                    }
                    let lookup_ref = if reference.chars().all(|c| c.is_ascii_digit()) {
                        format!("db-{}", reference)
                    } else {
                        reference.to_string()
                    };
                    let queue_cfg = self
                        .inner
                        .server
                        .data_context
                        .resolve_queue_config(lookup_ref.as_str())
                        .map_err(|e| {
                            RouteError::from((
                                anyhow!(
                                    "failed to resolve always-forwarding queue '{}': {}",
                                    reference,
                                    e
                                ),
                                Some(rsipstack::sip::StatusCode::ServerInternalError),
                            ))
                        })?
                        .ok_or_else(|| {
                            RouteError::from((
                                anyhow!("always-forwarding queue '{}' not found", reference),
                                Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                            ))
                        })?;

                    let mut queue_plan = queue_cfg.to_queue_plan().map_err(|e| {
                        RouteError::from((
                            anyhow!(
                                "failed to build always-forwarding queue plan '{}': {}",
                                reference,
                                e
                            ),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        ))
                    })?;
                    if queue_plan.label.is_none() {
                        queue_plan.label = Some(reference.to_string());
                    }
                    forced_pending_queue = Some(queue_plan);
                }
                crate::call::TransferEndpoint::Ivr(ivr_name) => {
                    let name = ivr_name.trim();
                    if name.is_empty() {
                        return Err(RouteError::from((
                            anyhow!("always-forwarding IVR name is empty"),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        )));
                    }
                    let ivr_file = format!("config/ivr/{}.toml", name);
                    forced_pending_app = Some((
                        "ivr".to_string(),
                        Some(serde_json::json!({ "file": ivr_file })),
                        true,
                    ));
                }
            }
        }

        if callee_is_same_realm
            && !always_forwarding
            && let Ok(results) = self.inner.server.locator.lookup(&callee_uri).await
        {
            internal_lookup_empty = results.is_empty();
            if internal_lookup_empty {
                warn!(
                    callee_uri = %callee_uri,
                    callee_realm = %callee_realm,
                    caller_realm = ?caller.realm,
                    "locator lookup returned empty results for same-realm callee"
                );
            } else if !results.is_empty() {
                // Keep locator-provided target metadata (destination/home_proxy/path/etc.)
                // so SipSession can route cross-node calls via remote home_proxy.
                locs = results;
            }
        }

        let caller_uri = match caller.from.as_ref() {
            Some(uri) => uri.clone(),
            None => original
                .from_header()
                .map_err(|e| RouteError::from((anyhow::anyhow!(e), None)))?
                .uri()
                .map_err(|e| RouteError::from((anyhow::anyhow!(e), None)))?,
        };

        let preview_option = InviteOption {
            callee: callee_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri.clone(),
            ..Default::default()
        };

        let (preview_forward, pending_queue, pending_app, dialplan_hints) = if always_forwarding {
            (
                forced_preview_forward,
                forced_pending_queue,
                forced_pending_app,
                None,
            )
        } else {
            let preview_outcome = route_invite
                .preview_route(preview_option, original, &direction, cookie)
                .await
                .map_err(|e| {
                    RouteError::from((
                        anyhow::anyhow!(e),
                        Some(rsipstack::sip::StatusCode::ServerInternalError),
                    ))
                })?;

            match preview_outcome {
                RouteResult::Queue { queue, hints, .. } => (None, Some(queue), None, hints),
                RouteResult::Forward(option, hints) => (Some(option), None, None, hints),
                RouteResult::NotHandled(_, hints) => (None, None, None, hints),
                RouteResult::Abort(code, reason) => {
                    let err = anyhow::anyhow!(
                        reason.unwrap_or_else(|| "route aborted during preview".to_string())
                    );
                    return Err(RouteError::from((err, Some(code))));
                }
                RouteResult::Application {
                    option: _,
                    app_name,
                    app_params,
                    auto_answer,
                    ..
                } => (None, None, Some((app_name, app_params, auto_answer)), None),
            }
        };

        let queue_targets = pending_queue
            .as_ref()
            .and_then(|plan| plan.dial_strategy.clone());
        let targets = if pending_app.is_some() {
            DialStrategy::Sequential(vec![])
        } else if let Some(queue_targets) = queue_targets {
            queue_targets
        } else if let Some(option) = preview_forward.as_ref() {
            let target = Location {
                aor: option.callee.clone(),
                destination: option.destination.clone(),
                credential: option.credential.clone(),
                headers: option.headers.clone(),
                contact_raw: Some(option.callee.to_string()),
                ..Default::default()
            };
            DialStrategy::Sequential(vec![target])
        } else {
            resolve_unhandled_targets(callee_is_same_realm, internal_lookup_empty, locs)?
        };
        let recording = self
            .inner
            .config
            .recording
            .as_ref()
            .map(|r| r.new_recording_config())
            .unwrap_or_default();

        let mut dialplan = Dialplan::new(session_id, original.clone(), direction)
            .with_caller(caller_uri)
            .with_media(media_config)
            .with_recording(recording)
            .with_route_invite(route_invite)
            .with_passthrough_failure(self.inner.config.passthrough_failure);

        if let Some((app_name, app_params, auto_answer)) = pending_app {
            dialplan = dialplan.with_application(app_name, app_params, auto_answer);
        } else if let Some(queue) = pending_queue {
            dialplan = dialplan.with_queue(queue);
        } else {
            dialplan = dialplan.with_targets(targets);
        }

        if let Some(mut hints) = dialplan_hints {
            if let Some(enabled) = hints.enable_recording {
                dialplan.recording.enabled = enabled;
            }
            if let Some(bypass) = hints.bypass_media
                && bypass
            {
                dialplan.media.proxy_mode = crate::config::MediaProxyMode::None;
            }
            if let Some(max_duration) = hints.max_duration {
                dialplan.max_call_duration = Some(max_duration);
            }
            if let Some(enable_sipflow) = hints.enable_sipflow {
                dialplan.enable_sipflow = enable_sipflow;
            }
            if hints.disable_ice_servers == Some(true) {
                dialplan.media.ice_servers = None;
            }
            if let Some(codecs) = hints.allow_codecs {
                let mut allow_codecs = Vec::new();
                for codec_name in codecs {
                    if let Ok(codec) = CodecType::try_from(codec_name.as_str()) {
                        allow_codecs.push(codec);
                    }
                }
                if !allow_codecs.is_empty() {
                    dialplan.allow_codecs = allow_codecs;
                }
            } else if let Some(codecs) = &self.inner.config.codecs {
                let mut allow_codecs = Vec::new();
                for codec_name in codecs {
                    if let Ok(codec) = CodecType::try_from(codec_name.as_str()) {
                        allow_codecs.push(codec);
                    }
                }
                if !allow_codecs.is_empty() {
                    dialplan.allow_codecs = allow_codecs;
                }
            }
            dialplan.extensions = std::mem::take(&mut hints.extensions);
        } else if let Some(codecs) = &self.inner.config.codecs {
            let mut allow_codecs = Vec::new();
            for codec_name in codecs {
                if let Ok(codec) = CodecType::try_from(codec_name.as_str()) {
                    allow_codecs.push(codec);
                }
            }
            if !allow_codecs.is_empty() {
                dialplan.allow_codecs = allow_codecs;
            }
        }

        Ok(dialplan)
    }

    fn apply_recording_policy(&self, mut dialplan: Dialplan, caller: &SipUser) -> Dialplan {
        let policy = match self.inner.config.recording.as_ref() {
            Some(policy) if policy.enabled => policy,
            _ => return dialplan,
        };

        if dialplan.recording.enabled && dialplan.recording.option.is_some() {
            return dialplan;
        }

        if !dialplan.recording.enabled {
            if !policy.directions.is_empty()
                && !policy
                    .directions
                    .iter()
                    .any(|direction| direction.matches(&dialplan.direction))
            {
                return dialplan;
            }

            let caller_identity = Self::caller_identity(caller);
            if Self::matches_any_pattern(&caller_identity, &policy.caller_deny) {
                return dialplan;
            }
            if !policy.caller_allow.is_empty()
                && !Self::matches_any_pattern(&caller_identity, &policy.caller_allow)
            {
                return dialplan;
            }

            let callee_identity = Self::callee_identity(&dialplan).unwrap_or_default();
            if Self::matches_any_pattern(&callee_identity, &policy.callee_deny) {
                return dialplan;
            }
            if !policy.callee_allow.is_empty()
                && !Self::matches_any_pattern(&callee_identity, &policy.callee_allow)
            {
                return dialplan;
            }
        }

        let caller_identity = Self::caller_identity(caller);
        let callee_identity = Self::callee_identity(&dialplan).unwrap_or_default();

        let recorder_option =
            match self.build_recorder_option(&dialplan, policy, &caller_identity, &callee_identity)
            {
                Some(option) => option,
                None => return dialplan,
            };

        debug!(
            session_id = dialplan.session_id.as_deref(),
            caller = %caller_identity,
            callee = %callee_identity,
            "recording policy enabled for dialplan"
        );

        dialplan.recording.enabled = true;
        dialplan.recording.auto_start = policy.auto_start.unwrap_or(true);

        if let Some(existing) = dialplan.recording.option.as_mut() {
            if existing.recorder_file.is_empty() {
                existing.recorder_file = recorder_option.recorder_file.clone();
            }
            if let Some(rate) = policy.samplerate {
                existing.samplerate = rate;
            }
            if let Some(ptime) = policy.ptime {
                existing.ptime = ptime;
            }
        } else {
            dialplan.recording.option = Some(recorder_option);
        }

        dialplan
    }

    /// Resolve the callee's [`SipUser`] from the user backend.
    ///
    /// Returns `None` when the callee realm doesn't belong to this server or
    /// the user is not found.  The result is LRU-cached by the backend so
    /// repeated lookups within the same call leg are cheap.
    async fn resolve_callee_user(
        &self,
        request: &rsipstack::sip::Request,
    ) -> Result<Option<SipUser>> {
        let callee_uri = resolve_callee_uri(request)?;
        let callee_realm = callee_uri.host().to_string();
        if !self.inner.server.is_same_realm(&callee_realm).await {
            return Ok(None);
        }

        let username = callee_uri
            .user()
            .map(|u| u.to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        if username.is_empty() {
            return Ok(None);
        }

        self.inner
            .server
            .user_backend
            .get_user(username.as_str(), Some(&callee_realm), Some(request))
            .await
            .map_err(Into::into)
    }

    fn matches_any_pattern(value: &str, patterns: &[String]) -> bool {
        patterns
            .iter()
            .any(|pattern| Self::match_pattern(pattern, value))
    }

    fn match_pattern(pattern: &str, value: &str) -> bool {
        if pattern == "*" {
            return true;
        }
        Pattern::new(pattern)
            .map(|compiled| compiled.matches(value))
            .unwrap_or_else(|_| pattern.eq_ignore_ascii_case(value))
    }

    fn caller_identity(caller: &SipUser) -> String {
        caller.to_string()
    }

    fn callee_identity(dialplan: &Dialplan) -> Option<String> {
        dialplan
            .original
            .to_header()
            .ok()
            .and_then(|header| header.uri().ok())
            .map(Self::identity_from_uri)
    }

    fn identity_from_uri(uri: rsipstack::sip::Uri) -> String {
        let user = uri.user().unwrap_or_default().to_string();
        let host = uri.host().to_string();
        if user.is_empty() {
            host
        } else {
            format!("{}@{}", user, host)
        }
    }

    fn build_recorder_option(
        &self,
        dialplan: &Dialplan,
        policy: &crate::config::RecordingPolicy,
        caller: &str,
        callee: &str,
    ) -> Option<RecorderOption> {
        let session_id = dialplan
            .session_id
            .as_ref()
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let root = policy.recorder_path();
        let pattern = policy.filename_pattern.as_deref().unwrap_or("{session_id}");
        let direction = dialplan.direction.to_string();
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let rendered =
            Self::render_filename(pattern, &session_id, caller, callee, &direction, &timestamp);
        let sanitized = Self::sanitize_filename_component(&rendered, &session_id);
        let mut path = PathBuf::from(root);
        if sanitized.is_empty() {
            return None;
        }
        path.push(sanitized);
        path.set_extension("wav");

        let mut option = RecorderOption::new(path.to_string_lossy().to_string());
        if let Some(rate) = policy.samplerate {
            option.samplerate = rate;
        }
        if let Some(ptime) = policy.ptime {
            option.ptime = ptime;
        }
        Some(option)
    }

    fn render_filename(
        pattern: &str,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: &str,
        timestamp: &str,
    ) -> String {
        let mut rendered = pattern.to_string();
        for (token, value) in [
            ("{session_id}", session_id),
            ("{caller}", caller),
            ("{callee}", callee),
            ("{direction}", direction),
            ("{timestamp}", timestamp),
        ] {
            rendered = rendered.replace(token, value);
        }
        rendered
    }

    fn sanitize_filename_component(value: &str, fallback: &str) -> String {
        let mut sanitized = String::with_capacity(value.len());
        let mut last_was_sep = false;
        for ch in value.chars() {
            let allowed = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.');
            if allowed {
                sanitized.push(ch);
                last_was_sep = false;
            } else if !last_was_sep {
                sanitized.push('_');
                last_was_sep = true;
            }
            if sanitized.len() >= 120 {
                break;
            }
        }
        let trimmed = sanitized.trim_matches('_').trim_matches('.');
        if trimmed.is_empty() {
            fallback.to_string()
        } else {
            trimmed.to_string()
        }
    }

    async fn build_dialplan(
        &self,
        tx: &mut Transaction,
        cookie: TransactionCookie,
        caller: &SipUser,
    ) -> Result<Dialplan, RouteError> {
        let trunk_context = cookie.get_extension::<TrunkContext>();
        let source_trunk_hint = trunk_context.as_ref().map(|c| c.name.clone());

        let route_invite: Box<dyn RouteInvite> =
            if let Some(f) = self.inner.server.create_route_invite.as_ref() {
                f(
                    self.inner.server.clone(),
                    self.inner.config.clone(),
                    self.inner.routing_state.clone(),
                )
                .map_err(|e| RouteError {
                    error: e,
                    status: None,
                    extensions: None,
                })?
            } else {
                Box::new(DefaultRouteInvite {
                    routing_state: self.inner.routing_state.clone(),
                    data_context: self.inner.server.data_context.clone(),
                    source_trunk_hint,
                })
            };

        let dialplan = if let Some(resolver) = self.inner.server.call_router.as_ref() {
            resolver
                .resolve(&tx.original, route_invite, caller, &cookie)
                .await
        } else {
            self.default_resolve(&tx.original, route_invite, caller, &cookie)
                .await
        }?;

        let mut dialplan = dialplan;
        if dialplan.caller_contact.is_none()
            && let Some(contact_uri) = self.inner.server.default_contact_uri()
        {
            let contact = rsipstack::sip::typed::Contact {
                display_name: None,
                uri: contact_uri,
                params: vec![],
            };
            dialplan = dialplan.with_caller_contact(contact);
        }
        for inspector in &self.inner.server.dialplan_inspectors {
            dialplan = inspector
                .inspect_dialplan(dialplan, &cookie, &tx.original)
                .await?
        }

        // Optimization: skip callee lookup for wholesale (trunk-originated) calls.
        let is_wholesale = cookie
            .get_extension::<TrunkContext>()
            .map(|ctx| ctx.tenant_id.is_some())
            .unwrap_or(false);

        if !is_wholesale {
            match self.resolve_callee_user(&tx.original).await {
                Ok(Some(callee)) => {
                    // Apply call-forwarding only when no custom resolver already set it.
                    if dialplan.call_forwarding.is_none()
                        && let Some(config) = callee.forwarding_config()
                    {
                        dialplan = dialplan.with_call_forwarding(Some(config));
                    }
                    // Propagate voicemail eligibility into the dialplan so that
                    // the call session can decide whether to chain to voicemail
                    // on no-answer / busy without having to re-query the DB.
                    dialplan.voicemail_enabled = !callee.voicemail_disabled;
                }
                Ok(None) => {}
                Err(err) => {
                    warn!(error = %err, "failed to resolve callee user for forwarding/voicemail");
                }
            }
        }

        let dialplan = self.apply_recording_policy(dialplan, caller);
        Ok(dialplan)
    }

    fn report_failure(
        &self,
        tx: &mut Transaction,
        cookie: &TransactionCookie,
        code: rsipstack::sip::StatusCode,
        reason: Option<String>,
        extensions: Option<HashMap<String, String>>,
    ) {
        let direction = if cookie.get_extension::<TrunkContext>().is_some() {
            DialDirection::Inbound
        } else {
            DialDirection::Internal
        };
        let session_id = tx.original.call_id_header().map_or_else(
            |_| uuid::Uuid::new_v4().to_string(),
            |h| h.value().to_string(),
        );
        let mut dialplan = Dialplan::new(session_id, tx.original.clone(), direction);
        if let Some(exts) = extensions {
            dialplan = dialplan.with_extension(exts);
        }
        let proxy_call = CallSessionBuilder::new(cookie.clone(), dialplan, 70)
            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
            .with_addon_registry(self.inner.server.addon_registry.clone());
        let _ = proxy_call.report_failure(self.inner.server.clone(), code, reason);
    }

    pub(crate) async fn handle_invite(
        &self,
        _cancel_token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<()> {
        let caller = cookie
            .get_user()
            .ok_or_else(|| anyhow::anyhow!("Missing caller user in transaction cookie"))?;

        // Check for incoming INVITE with Replaces header (seat replacement scenario)
        if let Some((replaces_call_id, replaces_to_tag, replaces_from_tag)) =
            Self::parse_replaces_header(&tx.original)
        {
            info!(
                call_id = %replaces_call_id,
                to_tag = %replaces_to_tag,
                from_tag = %replaces_from_tag,
                "Incoming INVITE contains Replaces header"
            );

            // Look up the dialog being replaced in the dialog layer
            let _dialog_layer = self.inner.dialog_layer.clone();
            let registry = self.inner.server.active_call_registry.clone();
            let conference_manager = self.inner.server.conference_manager.clone();

            // Find the old session by searching dialogs with matching call-id and tags
            let old_handle = {
                let dialog_id = rsipstack::dialog::DialogId {
                    call_id: replaces_call_id.clone(),
                    local_tag: replaces_to_tag.clone(),
                    remote_tag: replaces_from_tag.clone(),
                };
                registry.get_handle_by_dialog(&dialog_id.to_string())
            };

            if let Some(ref old_handle) = old_handle {
                let old_session_id = old_handle.session_id().to_string();
                let old_handle_clone = old_handle.clone();
                info!(%old_session_id, "Found session to replace via Replaces header");

                // Check if the old session is in a conference
                let old_leg = crate::call::domain::LegId::new(&old_session_id);
                let conf_id = conference_manager.get_conference_id_for_leg(&old_leg).await;

                if conf_id.is_some() {
                    info!(%old_session_id, "Replaces target is in a conference; proceeding with seat replacement");
                    // Proceed with normal call creation, then spawn background task to do seat replacement
                    let dialplan = match self.build_dialplan(tx, cookie.clone(), &caller).await {
                        Ok(d) => d,
                        Err(route_err) => {
                            if cookie.is_spam() {
                                return Ok(());
                            }
                            let code = route_err
                                .status
                                .unwrap_or(rsipstack::sip::StatusCode::ServerInternalError);
                            let reason_text = route_err.error.to_string();
                            let reason_value = if reason_text.contains(";cause=") {
                                reason_text.clone()
                            } else {
                                q850_reason_value(&code, Some(reason_text.as_str()))
                            };
                            self.report_failure(
                                tx,
                                &cookie,
                                code.clone(),
                                Some(reason_text),
                                route_err.extensions,
                            );
                            tx.reply_with(
                                code.clone(),
                                vec![rsipstack::sip::Header::Other("Reason".into(), reason_value)],
                                None,
                            )
                            .await
                            .map_err(|e| anyhow!("Failed to send reply: {}", e))?;
                            return Err(route_err.error);
                        }
                    };

                    let max_forwards = if let Ok(header) = tx.original.max_forwards_header() {
                        header.value().parse::<u32>().unwrap_or(70)
                    } else {
                        70
                    };

                    if max_forwards == 0 {
                        tx.reply(rsipstack::sip::StatusCode::TooManyHops).await?;
                        return Ok(());
                    }

                    let builder =
                        CallSessionBuilder::new(cookie.clone(), dialplan, max_forwards - 1)
                            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
                            .with_cancel_token(self.inner.server.cancel_token.child_token());

                    let server = self.inner.server.clone();
                    let result = builder.build_and_serve(server.clone(), tx).await;

                    // Spawn background task to perform seat replacement once new call answers
                    let new_session_id = tx
                        .original
                        .call_id_header()
                        .map(|h| h.value().to_string())
                        .unwrap_or_default();
                    if !new_session_id.is_empty() {
                        tokio::spawn(async move {
                            // Poll registry until new call is answered (Talking)
                            for _ in 0..300 {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                if let Some(entry) = registry.get(&new_session_id)
                                    && matches!(entry.status, crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking) {
                                        info!(%new_session_id, "New Replaces call answered; executing conference seat replacement");

                                        if let Some(ref conf) = conf_id {
                                            let _ = conference_manager.remove_participant(conf, &old_leg).await;
                                            let new_leg = crate::call::domain::LegId::new(&new_session_id);
                                            let _ = conference_manager.add_participant(conf, new_leg).await;
                                            info!(%old_session_id, %new_session_id, "Conference seat replacement completed for Replaces");
                                        }

                                        // Hang up old session
                                        let _ = old_handle_clone.send_command(crate::call::domain::CallCommand::Hangup(
                                            crate::call::domain::HangupCommand::local(
                                                "replaced_by_replaces",
                                                Some(crate::callrecord::CallRecordHangupReason::BySystem),
                                                Some(200),
                                            ),
                                        ));

                                        // Send RWI events if gateway is available
                                        if let Some(ref gw) = server.rwi_gateway {
                                            let event = crate::rwi::proto::RwiEvent::ConferenceSeatReplaceSucceeded {
                                                conf_id: conf_id.map(|c| c.0).unwrap_or_default(),
                                                old_call_id: old_session_id.clone(),
                                                new_call_id: new_session_id.clone(),
                                            };
                                            let g = gw.read().await;
                                            g.broadcast_event(&event);
                                        }
                                        break;
                                    }
                            }
                        });
                    }

                    return result;
                } else {
                    info!(%old_session_id, "Replaces target is not in a conference; creating conference for attended transfer");
                    // Standard attended transfer: C sends INVITE with Replaces to replace B
                    // We create a conference on the fly and merge both sessions
                    let dialplan = match self.build_dialplan(tx, cookie.clone(), &caller).await {
                        Ok(d) => d,
                        Err(route_err) => {
                            if cookie.is_spam() {
                                return Ok(());
                            }
                            let code = route_err
                                .status
                                .unwrap_or(rsipstack::sip::StatusCode::ServerInternalError);
                            let reason_text = route_err.error.to_string();
                            let reason_value = if reason_text.contains(";cause=") {
                                reason_text.clone()
                            } else {
                                q850_reason_value(&code, Some(reason_text.as_str()))
                            };
                            self.report_failure(
                                tx,
                                &cookie,
                                code.clone(),
                                Some(reason_text),
                                route_err.extensions,
                            );
                            tx.reply_with(
                                code.clone(),
                                vec![rsipstack::sip::Header::Other("Reason".into(), reason_value)],
                                None,
                            )
                            .await
                            .map_err(|e| anyhow!("Failed to send reply: {}", e))?;
                            return Err(route_err.error);
                        }
                    };

                    let max_forwards = if let Ok(header) = tx.original.max_forwards_header() {
                        header.value().parse::<u32>().unwrap_or(70)
                    } else {
                        70
                    };

                    if max_forwards == 0 {
                        tx.reply(rsipstack::sip::StatusCode::TooManyHops).await?;
                        return Ok(());
                    }

                    let builder =
                        CallSessionBuilder::new(cookie.clone(), dialplan, max_forwards - 1)
                            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
                            .with_cancel_token(self.inner.server.cancel_token.child_token());

                    let server = self.inner.server.clone();
                    let result = builder.build_and_serve(server.clone(), tx).await;

                    // Spawn background task to perform conference merge once new call answers
                    let new_session_id = tx
                        .original
                        .call_id_header()
                        .map(|h| h.value().to_string())
                        .unwrap_or_default();
                    if !new_session_id.is_empty() {
                        tokio::spawn(async move {
                            // Poll registry until new call is answered (Talking)
                            for _ in 0..300 {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                if let Some(entry) = registry.get(&new_session_id)
                                    && matches!(entry.status, crate::proxy::active_call_registry::ActiveProxyCallStatus::Talking) {
                                        info!(%new_session_id, "New Replaces call answered; creating conference for attended transfer");

                                        // Create a new conference for this transfer
                                        let conf_id = crate::call::runtime::ConferenceId::from(format!("conf-replaces-{}", new_session_id).as_str());
                                        let _ = conference_manager.create_conference(conf_id.clone(), None).await;

                                        // Add old session (A-B) to conference
                                        let old_leg = crate::call::domain::LegId::new(&old_session_id);
                                        let _ = conference_manager.add_participant(&conf_id, old_leg.clone()).await;

                                        // Add new session (C) to conference
                                        let new_leg = crate::call::domain::LegId::new(&new_session_id);
                                        let _ = conference_manager.add_participant(&conf_id, new_leg.clone()).await;

                                        info!(%old_session_id, %new_session_id, "Conference created for attended transfer");

                                        // Hang up old session's callee side (B) only
                                        // Use AllExcept caller to keep A connected
                                        let _ = old_handle_clone.send_command(crate::call::domain::CallCommand::Hangup(
                                            crate::call::domain::HangupCommand::local(
                                                "replaced_by_replaces",
                                                Some(crate::callrecord::CallRecordHangupReason::BySystem),
                                                Some(200),
                                            ).with_cascade(crate::call::domain::HangupCascade::AllExcept(vec![
                                                crate::call::domain::LegId::from("caller")
                                            ])),
                                        ));

                                        // Send RWI events if gateway is available
                                        if let Some(ref gw) = server.rwi_gateway {
                                            let event = crate::rwi::proto::RwiEvent::CallTransferred {
                                                call_id: old_session_id.clone(),
                                            };
                                            let g = gw.read().await;
                                            g.broadcast_event(&event);
                                        }
                                        break;
                                    }
                            }
                        });
                    }

                    return result;
                }
            } else {
                warn!(%replaces_call_id, "Replaces header refers to unknown dialog; returning 481");
                tx.reply(rsipstack::sip::StatusCode::CallTransactionDoesNotExist)
                    .await?;
                return Ok(());
            }
        }

        let dialplan = match self.build_dialplan(tx, cookie.clone(), &caller).await {
            Ok(d) => d,
            Err(route_err) => {
                if cookie.is_spam() {
                    return Ok(());
                }
                let code = route_err
                    .status
                    .unwrap_or(rsipstack::sip::StatusCode::ServerInternalError);
                let reason_text = route_err.error.to_string();
                // If error already contains ;cause= (e.g. "invite;cause=1234;text=\"xxx\""),
                // treat it as pre-formatted Q850 and use directly.
                let reason_value = if reason_text.contains(";cause=") {
                    reason_text.clone()
                } else {
                    q850_reason_value(&code, Some(reason_text.as_str()))
                };
                warn!(%code, key = %tx.key, reason = %reason_value, "failed to build dialplan");
                self.report_failure(
                    tx,
                    &cookie,
                    code.clone(),
                    Some(reason_text),
                    route_err.extensions,
                );
                tx.reply_with(
                    code.clone(),
                    vec![rsipstack::sip::Header::Other("Reason".into(), reason_value)],
                    None,
                )
                .await
                .map_err(|e| anyhow!("Failed to send reply: {}", e))?;
                return Err(route_err.error);
            }
        };

        // Create event sender for media stream events
        let max_forwards = if let Ok(header) = tx.original.max_forwards_header() {
            header.value().parse::<u32>().unwrap_or(70)
        } else {
            70
        };

        if max_forwards == 0 {
            info!(key = %tx.key, "Max-Forwards exceeded");
            self.report_failure(
                tx,
                &cookie,
                rsipstack::sip::StatusCode::TooManyHops,
                None,
                None,
            );
            tx.reply(rsipstack::sip::StatusCode::TooManyHops).await?;
            return Ok(());
        }

        let builder = CallSessionBuilder::new(cookie.clone(), dialplan, max_forwards - 1)
            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
            .with_cancel_token(self.inner.server.cancel_token.child_token());

        builder.build_and_serve(self.inner.server.clone(), tx).await
    }

    async fn process_message(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id =
            DialogId::try_from((&tx.original, TransactionRole::Server)).map_err(|e| anyhow!(e))?;
        let mut dialog = match self.inner.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => dialog,
            None => {
                debug!(%dialog_id, method=%tx.original.method, "dialog not found for message");
                return Ok(());
            }
        };

        dialog.handle(tx).await.map_err(|e| anyhow!(e))
    }

    /// Handle inbound REFER request (transfer target scenario)
    ///
    /// When PBX receives a REFER request, it means someone wants to transfer
    /// a call to us. We need to:
    /// 1. Parse the Refer-To header to get the transfer target
    /// 2. Send 202 Accepted response
    /// 3. Send NOTIFY with 100 Trying
    /// 4. Initiate a new call to the transfer target (with Replaces if present)
    /// 5. Bridge the transferred call with the original call
    /// 6. Send NOTIFY with final result (200 OK or error)
    async fn handle_inbound_refer(
        &self,
        tx: &mut Transaction,
        cookie: &TransactionCookie,
    ) -> Result<()> {
        info!("Handling inbound REFER request");

        // Extract Refer-To header (handle both typed and untyped header forms)
        let refer_to = tx.original.headers.iter().find_map(|h| match h {
            rsipstack::sip::Header::ReferTo(refer_to) => Some(refer_to.value().to_string()),
            rsipstack::sip::Header::Other(name, value) if name.eq_ignore_ascii_case("Refer-To") => {
                Some(value.to_string())
            }
            _ => None,
        });

        let refer_to = match refer_to {
            Some(uri) => {
                // Parse Refer-To URI (may be in angle brackets)
                let uri = uri.trim();
                let uri = uri.strip_prefix('<').unwrap_or(uri);
                let uri = uri.strip_suffix('>').unwrap_or(uri);
                uri.to_string()
            }
            None => {
                warn!("Missing Refer-To header in REFER request");
                tx.reply_with(rsipstack::sip::StatusCode::BadRequest, vec![], None)
                    .await
                    .map_err(|e| anyhow!(e))?;
                return Err(anyhow!("Missing Refer-To header"));
            }
        };

        info!(refer_to = %refer_to, "Inbound REFER received");

        // Check Referred-By header (optional)
        let referred_by = tx.original.headers.iter().find_map(|h| match h {
            rsipstack::sip::Header::ReferredBy(referred_by) => {
                Some(referred_by.value().to_string())
            }
            rsipstack::sip::Header::Other(name, value)
                if name.eq_ignore_ascii_case("Referred-By") =>
            {
                Some(value.to_string())
            }
            _ => None,
        });

        if let Some(by) = &referred_by {
            info!(referred_by = %by, "Transfer initiated by");
        }

        // Get dialog ID for this REFER
        let dialog_id = DialogId::try_from((&tx.original, TransactionRole::Server))
            .map_err(|e| anyhow!("Failed to get dialog ID: {}", e))?;

        // Find the original SipSession associated with this dialog
        let original_handle = self
            .inner
            .server
            .active_call_registry
            .get_handle_by_dialog(&dialog_id.to_string());

        if original_handle.is_none() {
            warn!(dialog_id = %dialog_id, "No active session found for REFER dialog");
            tx.reply_with(
                rsipstack::sip::StatusCode::CallTransactionDoesNotExist,
                vec![],
                None,
            )
            .await
            .map_err(|e| anyhow!(e))?;
            return Err(anyhow!("No active session for REFER dialog"));
        }

        // Send 202 Accepted response
        tx.reply_with(rsipstack::sip::StatusCode::Accepted, vec![], None)
            .await
            .map_err(|e| anyhow!(e))?;

        info!("Sent 202 Accepted for REFER");

        // Spawn async task to handle the transfer and send NOTIFYs
        let dialog_layer = self.inner.dialog_layer.clone();
        let refer_to_clone = refer_to.clone();
        let server = self.inner.server.clone();
        let original_handle = original_handle.unwrap();
        let original_session_id = original_handle.session_id().to_string();
        let user = cookie.get_user().clone();

        // Track transfer via CC addon event system
        let transfer_id = format!("refer-{}", dialog_id.call_id);
        let _transfer_id_clone = transfer_id.clone();

        tokio::spawn(async move {
            info!("Spawned inbound REFER background task");

            // Small delay to ensure 202 response is sent
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Send NOTIFY with 100 Trying
            info!("Sending NOTIFY 100 Trying for REFER");
            match Self::send_refer_notify(&dialog_layer, &dialog_id, 100, "Trying", &refer_to_clone)
                .await
            {
                Ok(_) => info!("Sent NOTIFY 100 Trying for REFER"),
                Err(e) => {
                    warn!(error = %e, "Failed to send NOTIFY 100 Trying");
                    return;
                }
            }

            // Determine if this REFER includes a Replaces parameter
            let (target_uri, replaces_header) = Self::parse_refer_to(&refer_to_clone);

            // Originate new call to transfer target
            let result = Self::execute_inbound_refer_transfer(
                &server,
                &original_handle,
                &original_session_id,
                &target_uri,
                replaces_header.as_deref(),
            )
            .await;

            // Send final NOTIFY based on result
            let (notify_status, notify_reason) = match result {
                Ok(_) => (200, "OK"),
                Err((status, ref reason)) => {
                    warn!(status = status, reason = %reason, "Inbound REFER transfer failed");
                    (status, reason.as_str())
                }
            };

            if let Err(e) = Self::send_refer_notify(
                &dialog_layer,
                &dialog_id,
                notify_status,
                notify_reason,
                &refer_to_clone,
            )
            .await
            {
                warn!(error = %e, "Failed to send final NOTIFY for REFER");
            } else {
                info!(status = notify_status, "Sent final NOTIFY for REFER");
            }

            // Emit transfer event to RWI if applicable
            if let Some(_user) = user
                && let Some(ref gw) = server.rwi_gateway
            {
                let event = crate::rwi::proto::RwiEvent::CallTransferred {
                    call_id: original_session_id.clone(),
                };
                let g = gw.read().await;
                g.send_event_to_call_owner(&original_session_id, &event);
            }
        });

        Ok(())
    }

    /// Parse Replaces header from incoming request headers.
    /// Returns (call_id, to_tag, from_tag) if present and well-formed.
    fn parse_replaces_header(
        request: &rsipstack::sip::Request,
    ) -> Option<(String, String, String)> {
        let replaces_value = request.headers.iter().find_map(|h| match h {
            rsipstack::sip::Header::Other(name, value) if name.eq_ignore_ascii_case("Replaces") => {
                Some(value.to_string())
            }
            _ => None,
        })?;

        let replaces = replaces_value.trim();
        // Format: call-id;to-tag=xxx;from-tag=yyy
        let mut call_id = None;
        let mut to_tag = None;
        let mut from_tag = None;

        for (idx, part) in replaces.split(';').enumerate() {
            if idx == 0 {
                call_id = Some(part.trim().to_string());
            } else if let Some(val) = part.strip_prefix("to-tag=") {
                to_tag = Some(val.trim().to_string());
            } else if let Some(val) = part.strip_prefix("from-tag=") {
                from_tag = Some(val.trim().to_string());
            }
        }

        Some((call_id?, to_tag?, from_tag?))
    }

    /// Parse Refer-To URI, extracting the base target and optional Replaces header.
    fn parse_refer_to(refer_to: &str) -> (String, Option<String>) {
        if let Some(pos) = refer_to.find("?Replaces=") {
            let base = &refer_to[..pos];
            let encoded = &refer_to[pos + 10..];
            let decoded = urlencoding::decode(encoded).unwrap_or_else(|_| encoded.into());
            (base.to_string(), Some(decoded.into_owned()))
        } else if let Some(pos) = refer_to.find("&Replaces=") {
            let base = &refer_to[..pos];
            let encoded = &refer_to[pos + 10..];
            let decoded = urlencoding::decode(encoded).unwrap_or_else(|_| encoded.into());
            (base.to_string(), Some(decoded.into_owned()))
        } else {
            (refer_to.to_string(), None)
        }
    }

    /// Execute the actual transfer for an inbound REFER.
    ///
    /// This originates a new call to the target and bridges it with the original session.
    /// Returns Ok(()) on success, or Err((sip_status, reason)) on failure so the caller
    /// can send an accurate NOTIFY sipfrag.
    async fn execute_inbound_refer_transfer(
        server: &SipServerRef,
        original_handle: &crate::proxy::proxy_call::sip_session::SipSessionHandle,
        original_session_id: &str,
        target_uri: &str,
        replaces_header: Option<&str>,
    ) -> Result<(), (u16, String)> {
        info!(target_uri, "Starting inbound REFER transfer execution");

        // Parse destination URI
        let destination_uri: rsipstack::sip::Uri = rsipstack::sip::Uri::try_from(target_uri)
            .map_err(|e| (400, format!("Invalid transfer target URI: {:?}", e)))?;

        // Build caller URI (use server realm)
        let realm = server
            .proxy_config
            .realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| server.proxy_config.addr.clone());
        let caller_uri_str = format!("sip:transfer@{}", realm);
        let caller_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(caller_uri_str.as_str())
                .map_err(|e| (500, format!("Invalid caller URI: {:?}", e)))?;

        // Build headers
        let mut headers = vec![rsipstack::sip::Header::Other(
            "Max-Forwards".into(),
            "70".into(),
        )];
        if let Some(replaces) = replaces_header {
            headers.push(rsipstack::sip::Header::Other(
                "Replaces".into(),
                replaces.into(),
            ));
        }

        // Get external IP for SDP
        let external_ip = server
            .rtp_config
            .external_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());

        // Create media track and SDP offer
        let new_call_id = uuid::Uuid::new_v4().to_string();
        let media_track =
            crate::media::RtpTrackBuilder::new(format!("inbound-refer-{}", new_call_id))
                .with_cancel_token(tokio_util::sync::CancellationToken::new())
                .with_external_ip(external_ip);
        let media_track = if let Some(bind_ip) = server.rtp_config.bind_ip.clone() {
            media_track.with_bind_ip(bind_ip)
        } else {
            media_track
        }
        .build();

        let sdp_offer = media_track
            .local_description()
            .await
            .map_err(|e| (500, format!("Failed to generate SDP: {}", e)))?;

        // Build invite options
        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri,
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.into_bytes()),
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(new_call_id.clone()),
            ..Default::default()
        };

        info!(%new_call_id, callee = %destination_uri, "Sending INVITE for inbound REFER transfer");

        let dialog_layer = server.dialog_layer.clone();
        let registry = server.active_call_registry.clone();
        let original_session_id = original_session_id.to_string();
        let target_for_log = target_uri.to_string();

        // Do the originate
        let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

        // Create session and register
        let id = SessionId::from(new_call_id.clone());
        let (new_handle, mut _cmd_rx) = SipSession::with_handle(id);

        let entry = ActiveProxyCallEntry {
            session_id: new_call_id.clone(),
            caller: Some("transfer".to_string()),
            callee: Some(target_for_log),
            direction: "outbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        };
        registry.upsert(entry, new_handle.clone());

        // Wait for invitation result with timeout
        let timeout_secs = 60u64;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            async {
                loop {
                    tokio::select! {
                        res = &mut invitation => break res,
                        state = state_rx.recv() => {
                            if let Some(ref state) = state {
                                let state_str = match state {
                                    rsipstack::dialog::dialog::DialogState::Calling(_) => "Calling",
                                    rsipstack::dialog::dialog::DialogState::Early(_, _) => "Early",
                                    rsipstack::dialog::dialog::DialogState::Confirmed(_, _) => "Confirmed",
                                    rsipstack::dialog::dialog::DialogState::Terminated(_, _) => "Terminated",
                                    rsipstack::dialog::dialog::DialogState::Updated(_, _, _) => "Updated",
                                    rsipstack::dialog::dialog::DialogState::Refer(_, _, _) => "Refer",
                                    _ => "Other",
                                };
                                info!(state = state_str, "Inbound REFER transfer invitation state update");
                            }
                        }
                    }
                }
            },
        )
        .await;

        match result {
            Ok(Ok((_, Some(resp))))
                if resp.status_code().kind()
                    == rsipstack::sip::status_code::StatusCodeKind::Successful =>
            {
                info!(%new_call_id, "Inbound REFER transfer target answered");

                registry.update(&new_call_id, |entry| {
                    entry.answered_at = Some(chrono::Utc::now());
                    entry.status = ActiveProxyCallStatus::Talking;
                });

                // Bridge original call with new call
                let leg_a = crate::call::domain::LegId::new(&original_session_id);
                let leg_b = crate::call::domain::LegId::new(&new_call_id);

                original_handle
                    .send_command(crate::call::domain::CallCommand::Bridge {
                        leg_a,
                        leg_b,
                        mode: crate::call::domain::P2PMode::Audio,
                    })
                    .map_err(|e| (500, format!("Failed to bridge calls: {}", e)))?;

                info!(%original_session_id, %new_call_id, "Bridged original and transfer target calls");
                Ok(())
            }
            Ok(Ok((_, Some(resp)))) => {
                let code = resp.status_code().code();
                warn!(%new_call_id, status = %code, "Inbound REFER transfer target rejected");
                registry.remove(&new_call_id);
                Err((code, format!("Transfer target rejected with {}", code)))
            }
            Ok(Err(e)) => {
                warn!(%new_call_id, error = %e, "Inbound REFER transfer error");
                registry.remove(&new_call_id);
                Err((500, format!("Invite failed: {}", e)))
            }
            Err(_) => {
                warn!(%new_call_id, "Inbound REFER transfer timeout");
                registry.remove(&new_call_id);
                Err((408, "Transfer target timeout".to_string()))
            }
            _ => {
                registry.remove(&new_call_id);
                Err((500, "Unexpected invite result".to_string()))
            }
        }
    }

    /// Send NOTIFY for REFER subscription
    ///
    /// Uses `ServerInviteDialog::notify_refer` which follows RFC 3515 and
    /// automatically builds the correct `message/sipfrag` body and
    /// `Subscription-State` header.
    async fn send_refer_notify(
        dialog_layer: &Arc<DialogLayer>,
        dialog_id: &DialogId,
        status_code: u16,
        _reason_phrase: &str,
        _refer_to: &str,
    ) -> Result<()> {
        let status = rsipstack::sip::StatusCode::from(status_code);
        let sub_state = if status_code >= 200 {
            "terminated;reason=noresource"
        } else {
            "active"
        };

        if let Some(dialog) = dialog_layer.get_dialog(dialog_id) {
            match dialog {
                Dialog::ServerInvite(d) => match d.notify_refer(status, sub_state).await {
                    Ok(Some(response)) => {
                        info!(
                            status = %response.status_code(),
                            "NOTIFY sent successfully"
                        );
                        Ok(())
                    }
                    Ok(None) => {
                        warn!("No response received for NOTIFY");
                        Ok(())
                    }
                    Err(e) => Err(anyhow!("Failed to send NOTIFY: {}", e)),
                },
                _ => {
                    warn!("Dialog is not a server invite dialog, cannot send NOTIFY");
                    Ok(())
                }
            }
        } else {
            Err(anyhow!("Dialog not found: {}", dialog_id))
        }
    }

    /// Handle incoming SIP MESSAGE request from CC Phone.
    /// Supports consult transfer commands via JSON body.
    async fn handle_message(
        &self,
        tx: &mut Transaction,
        _cookie: &TransactionCookie,
    ) -> Result<()> {
        let body = String::from_utf8_lossy(tx.original.body());
        let content_type = tx.original.headers.iter().find_map(|h| match h {
            rsipstack::sip::Header::ContentType(ct) => Some(ct.value().to_string()),
            rsipstack::sip::Header::Other(name, value)
                if name.eq_ignore_ascii_case("Content-Type") =>
            {
                Some(value.to_string())
            }
            _ => None,
        });

        info!(content_type = ?content_type, body = %body, "Received SIP MESSAGE");

        // Parse JSON body
        if content_type.as_deref() == Some("application/json")
            && let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&body)
        {
            let cmd_type = cmd.get("cmd").and_then(|v| v.as_str()).unwrap_or_default();
            let call_id = cmd
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let agent_id = cmd
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let target = cmd
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let transfer_id = cmd
                .get("transfer_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            match cmd_type {
                "consult.initiate" => {
                    info!(call_id = %call_id, agent_id = %agent_id, target = %target, "SIP MESSAGE: consult.initiate");

                    // Hold the original call (A)
                    let registry = self.inner.server.active_call_registry.clone();
                    if let Some(handle) = registry.get_handle(call_id) {
                        let _ = handle.send_command(crate::call::domain::CallCommand::Hold {
                            leg_id: crate::call::domain::LegId::new(call_id),
                            music: None,
                        });
                    }

                    // Send 200 OK with transfer_id
                    let response_body = serde_json::json!({
                        "status": "ok",
                        "transfer_id": transfer_id,
                        "cmd": "consult.initiate"
                    })
                    .to_string();
                    tx.reply_with(
                        rsipstack::sip::StatusCode::OK,
                        vec![rsipstack::sip::Header::ContentType(
                            rsipstack::sip::headers::untyped::ContentType::new("application/json"),
                        )],
                        Some(response_body.into_bytes()),
                    )
                    .await?;
                    return Ok(());
                }
                "consult.merge" => {
                    info!(transfer_id = %transfer_id, "SIP MESSAGE: consult.merge");

                    tx.reply_with(rsipstack::sip::StatusCode::OK, vec![], None)
                        .await?;
                    return Ok(());
                }
                "consult.complete" => {
                    info!(transfer_id = %transfer_id, "SIP MESSAGE: consult.complete");

                    tx.reply_with(rsipstack::sip::StatusCode::OK, vec![], None)
                        .await?;
                    return Ok(());
                }
                "consult.cancel" => {
                    info!(transfer_id = %transfer_id, "SIP MESSAGE: consult.cancel");

                    // Unhold the original call
                    let registry = self.inner.server.active_call_registry.clone();
                    if let Some(handle) = registry.get_handle(call_id) {
                        let _ = handle.send_command(crate::call::domain::CallCommand::Unhold {
                            leg_id: crate::call::domain::LegId::new(call_id),
                        });
                    }

                    tx.reply_with(rsipstack::sip::StatusCode::OK, vec![], None)
                        .await?;
                    return Ok(());
                }
                _ => {}
            }
        }

        // Default: accept but do nothing
        tx.reply_with(rsipstack::sip::StatusCode::OK, vec![], None)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }

    fn allow_methods(&self) -> Vec<rsipstack::sip::Method> {
        vec![
            rsipstack::sip::Method::Invite,
            rsipstack::sip::Method::Bye,
            rsipstack::sip::Method::Info,
            rsipstack::sip::Method::Update,
            rsipstack::sip::Method::Ack,
            rsipstack::sip::Method::Cancel,
            rsipstack::sip::Method::Options,
            rsipstack::sip::Method::Refer,
            rsipstack::sip::Method::Notify,
            rsipstack::sip::Method::Message,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        debug!("Call module with Dialog-based B2BUA started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        debug!("Call module stopped, cleaning up sessions");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        if cookie.get_user().is_none() {
            cookie.set_user(SipUser::try_from(&*tx)?);
        }
        let dialog_id =
            DialogId::try_from((&tx.original, TransactionRole::Server)).map_err(|e| anyhow!(e))?;
        info!(
            %dialog_id,
            tx = %tx.key,
            uri = %tx.original.uri,
            caller = %cookie.get_user().as_ref().map(|u|u.to_string()).unwrap_or_default(),
            "call transaction begin",
        );
        match tx.original.method {
            rsipstack::sip::Method::Invite => {
                // Check for Re-invite (INVITE within an existing dialog)
                // For server-side dialog, local_tag corresponds to To header tag
                // A Re-INVITE has both From and To tags present
                if !dialog_id.local_tag.is_empty() {
                    debug!(%dialog_id, "Detected Re-invite, processing via dialog layer");
                    if let Err(e) = self.process_message(tx).await {
                        warn!(%dialog_id, "Failed to process Re-invite message: {}", e);
                    }
                    return Ok(ProxyAction::Abort);
                }

                if let Err(e) = self.handle_invite(token, tx, cookie).await
                    && tx.last_response.is_none()
                {
                    let code = rsipstack::sip::StatusCode::ServerInternalError;
                    let reason_text = e.to_string();
                    tx.reply_with(
                        code.clone(),
                        vec![rsipstack::sip::Header::Other(
                            "Reason".into(),
                            q850_reason_value(&code, Some(reason_text.as_str())),
                        )],
                        None,
                    )
                    .await
                    .map_err(|e| anyhow!(e))?;
                }
                Ok(ProxyAction::Abort)
            }
            rsipstack::sip::Method::Options
            | rsipstack::sip::Method::Info
            | rsipstack::sip::Method::Ack
            | rsipstack::sip::Method::Update
            | rsipstack::sip::Method::Cancel
            | rsipstack::sip::Method::Bye => {
                if let Err(e) = self.process_message(tx).await {
                    warn!(%dialog_id, method=%tx.original.method, "error process {}\n{}", e, tx.original.to_string());
                }
                Ok(ProxyAction::Abort)
            }
            rsipstack::sip::Method::Refer => {
                // Handle inbound REFER request (transfer target scenario)
                if let Err(e) = self.handle_inbound_refer(tx, &cookie).await {
                    warn!(%dialog_id, "Failed to handle inbound REFER: {}", e);
                    // Send appropriate error response
                    let code = rsipstack::sip::StatusCode::ServerInternalError;
                    let _ = tx.reply_with(code, vec![], None).await;
                }
                Ok(ProxyAction::Abort)
            }
            rsipstack::sip::Method::Notify => {
                // Handle NOTIFY request (typically from REFER subscription)
                if let Err(e) = self.process_message(tx).await {
                    warn!(%dialog_id, "Failed to process NOTIFY: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsipstack::sip::Method::Message => {
                if let Err(e) = self.handle_message(tx, &cookie).await {
                    warn!(%dialog_id, "Failed to handle MESSAGE: {}", e);
                    let _ = tx
                        .reply_with(
                            rsipstack::sip::StatusCode::ServerInternalError,
                            vec![],
                            None,
                        )
                        .await;
                }
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::Location;
    use crate::call::{DialDirection, RouteInvite, SipUser, TransactionCookie};
    use crate::config::RouteResult;
    use crate::proxy::tests::common::create_test_server;
    use async_trait::async_trait;
    use rsipstack::dialog::invitation::InviteOption;

    fn make_loc() -> Vec<Location> {
        vec![Location {
            aor: rsipstack::sip::Uri {
                scheme: Some(rsipstack::sip::Scheme::Sip),
                auth: Some(rsipstack::sip::Auth {
                    user: "test".to_string(),
                    password: None,
                }),
                host_with_port: rsipstack::sip::HostWithPort {
                    host: rsipstack::sip::Host::Domain("example.com".to_string().into()),
                    port: None,
                },
                params: vec![],
                headers: vec![],
            },
            ..Default::default()
        }]
    }

    struct NotHandledRouteInvite;

    #[async_trait]
    impl RouteInvite for NotHandledRouteInvite {
        async fn route_invite(
            &self,
            option: InviteOption,
            _origin: &rsipstack::sip::Request,
            _direction: &DialDirection,
            _cookie: &TransactionCookie,
        ) -> Result<RouteResult> {
            Ok(RouteResult::NotHandled(option, None))
        }
    }

    fn replace_to_header(request: &mut rsipstack::sip::Request, to_uri: rsipstack::sip::Uri) {
        request
            .headers
            .retain(|header| !matches!(header, rsipstack::sip::Header::To(_)));
        request.headers.push(
            rsipstack::sip::typed::To {
                display_name: None,
                uri: to_uri,
                params: vec![],
            }
            .into(),
        );
    }

    #[test]
    fn loop_guard_same_realm_online_user_passes() {
        let result = resolve_unhandled_targets(true, false, make_loc());
        assert!(
            result.is_ok(),
            "same-realm online user should not be rejected"
        );
    }

    #[test]
    fn loop_guard_same_realm_offline_user_returns_480() {
        let result = resolve_unhandled_targets(true, true, make_loc());
        let err = result.expect_err("offline same-realm user should be rejected");
        let code = err.status.unwrap();
        assert_eq!(u16::from(code), 480);
        assert!(err.error.to_string().contains("offline"));
    }

    #[test]
    fn loop_guard_external_callee_returns_404() {
        let result = resolve_unhandled_targets(false, false, make_loc());
        let err = result.expect_err("external callee with NotHandled should be rejected");
        let code = err.status.unwrap();
        assert_eq!(u16::from(code), 404);
        assert!(err.error.to_string().contains("no route"));
    }

    #[test]
    fn loop_guard_external_callee_offline_also_404() {
        // external callee — internal_lookup_empty is irrelevant but test the combination
        let result = resolve_unhandled_targets(false, true, make_loc());
        let err = result.expect_err("external callee should be rejected");
        let code = err.status.unwrap();
        assert_eq!(u16::from(code), 404);
    }

    #[test]
    fn test_parse_replaces_header_basic() {
        let mut request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:replace@example.com".try_into().unwrap(),
            version: rsipstack::sip::Version::V2,
            headers: vec![].into(),
            body: vec![],
        };
        request.headers.push(rsipstack::sip::Header::Other(
            "Replaces".into(),
            "abc123;to-tag=local456;from-tag=remote789".into(),
        ));

        let result = CallModule::parse_replaces_header(&request);
        assert!(result.is_some());
        let (call_id, to_tag, from_tag) = result.unwrap();
        assert_eq!(call_id, "abc123");
        assert_eq!(to_tag, "local456");
        assert_eq!(from_tag, "remote789");
    }

    #[test]
    fn test_parse_replaces_header_missing() {
        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:replace@example.com".try_into().unwrap(),
            version: rsipstack::sip::Version::V2,
            headers: vec![].into(),
            body: vec![],
        };

        let result = CallModule::parse_replaces_header(&request);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_refer_to_with_replaces() {
        let refer_to = "sip:charlie@example.com?Replaces=call-id%3Bto-tag%3Dtt%3Bfrom-tag%3Dft";
        let (base, replaces) = CallModule::parse_refer_to(refer_to);
        assert_eq!(base, "sip:charlie@example.com");
        assert_eq!(replaces, Some("call-id;to-tag=tt;from-tag=ft".to_string()));
    }

    #[test]
    fn test_parse_refer_to_without_replaces() {
        let refer_to = "sip:charlie@example.com";
        let (base, replaces) = CallModule::parse_refer_to(refer_to);
        assert_eq!(base, "sip:charlie@example.com");
        assert_eq!(replaces, None);
    }

    #[tokio::test]
    async fn default_resolve_uses_request_uri_for_same_realm_detection() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let mut request = crate::proxy::tests::common::create_test_request(
            rsipstack::sip::Method::Invite,
            "bp",
            None,
            "rustpbx.com",
            None,
        );
        request.uri = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();
        replace_to_header(
            &mut request,
            rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap(),
        );

        let caller = SipUser {
            username: "bp".to_string(),
            realm: Some("rustpbx.com".to_string()),
            ..Default::default()
        };

        let err = module
            .default_resolve(
                &request,
                Box::new(NotHandledRouteInvite),
                &caller,
                &TransactionCookie::default(),
            )
            .await
            .expect_err("offline internal user should reject with 480");

        assert_eq!(
            err.status,
            Some(rsipstack::sip::StatusCode::TemporarilyUnavailable)
        );
        assert!(err.error.to_string().contains("offline"));
    }

    #[tokio::test]
    async fn default_resolve_uses_request_uri_for_external_detection() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let mut request = crate::proxy::tests::common::create_test_request(
            rsipstack::sip::Method::Invite,
            "bp",
            None,
            "rustpbx.com",
            None,
        );
        request.uri =
            rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();
        replace_to_header(
            &mut request,
            rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap(),
        );

        let caller = SipUser {
            username: "bp".to_string(),
            realm: Some("rustpbx.com".to_string()),
            ..Default::default()
        };

        let err = module
            .default_resolve(
                &request,
                Box::new(NotHandledRouteInvite),
                &caller,
                &TransactionCookie::default(),
            )
            .await
            .expect_err("external destination should reject with 404");

        assert_eq!(err.status, Some(rsipstack::sip::StatusCode::NotFound));
        assert!(err.error.to_string().contains("no route"));
    }

    #[tokio::test]
    async fn default_resolve_always_forwarding_uri_bypasses_offline_locator() {
        let (server, config) = create_test_server().await;
        server
            .user_backend
            .create_user(SipUser {
                id: 99,
                username: "cfwd".to_string(),
                enabled: true,
                realm: Some("rustpbx.com".to_string()),
                call_forwarding_mode: Some("always".to_string()),
                call_forwarding_destination: Some("sip:alice@rustpbx.com".to_string()),
                ..Default::default()
            })
            .await
            .expect("create forwarding user");
        let module = CallModule::new(config, server);

        let mut request = crate::proxy::tests::common::create_test_request(
            rsipstack::sip::Method::Invite,
            "bp",
            None,
            "rustpbx.com",
            None,
        );
        request.uri = rsipstack::sip::Uri::try_from("sip:cfwd@rustpbx.com").unwrap();
        replace_to_header(
            &mut request,
            rsipstack::sip::Uri::try_from("sip:cfwd@rustpbx.com").unwrap(),
        );

        let caller = SipUser {
            username: "bp".to_string(),
            realm: Some("rustpbx.com".to_string()),
            ..Default::default()
        };

        let dialplan = module
            .default_resolve(
                &request,
                Box::new(NotHandledRouteInvite),
                &caller,
                &TransactionCookie::default(),
            )
            .await
            .expect("always forwarding should bypass offline locator check");

        let target = dialplan
            .first_target()
            .expect("forwarding target should be present")
            .aor
            .to_string();
        assert_eq!(target, "sip:alice@rustpbx.com");
    }

    #[tokio::test]
    async fn default_resolve_always_forwarding_queue_missing_returns_480() {
        let (server, config) = create_test_server().await;
        server
            .user_backend
            .create_user(SipUser {
                id: 100,
                username: "cfwdq".to_string(),
                enabled: true,
                realm: Some("rustpbx.com".to_string()),
                call_forwarding_mode: Some("always".to_string()),
                call_forwarding_destination: Some("queue:99999".to_string()),
                ..Default::default()
            })
            .await
            .expect("create queue forwarding user");
        let module = CallModule::new(config, server);

        let mut request = crate::proxy::tests::common::create_test_request(
            rsipstack::sip::Method::Invite,
            "bp",
            None,
            "rustpbx.com",
            None,
        );
        request.uri = rsipstack::sip::Uri::try_from("sip:cfwdq@rustpbx.com").unwrap();
        replace_to_header(
            &mut request,
            rsipstack::sip::Uri::try_from("sip:cfwdq@rustpbx.com").unwrap(),
        );

        let caller = SipUser {
            username: "bp".to_string(),
            realm: Some("rustpbx.com".to_string()),
            ..Default::default()
        };

        let err = module
            .default_resolve(
                &request,
                Box::new(NotHandledRouteInvite),
                &caller,
                &TransactionCookie::default(),
            )
            .await
            .expect_err("missing always-forwarding queue should fail");

        assert_eq!(
            err.status,
            Some(rsipstack::sip::StatusCode::TemporarilyUnavailable)
        );
        assert!(err.error.to_string().contains("queue '99999' not found"));
    }

    #[test]
    fn resolve_callee_uri_prefers_request_uri() {
        let request_uri = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();
        let to_uri =
            rsipstack::sip::Uri::try_from("sip:lp@172.25.52.29:63647;transport=UDP").unwrap();

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: request_uri.clone(),
            version: rsipstack::sip::Version::V2,
            headers: vec![
                rsipstack::sip::typed::To {
                    display_name: None,
                    uri: to_uri,
                    params: vec![],
                }
                .into(),
            ]
            .into(),
            body: vec![],
        };

        let resolved = resolve_callee_uri(&request).expect("expected callee uri");
        assert_eq!(resolved, request_uri);
    }

    #[test]
    fn resolve_callee_uri_falls_back_to_to_header() {
        let request_uri = rsipstack::sip::Uri::try_from("sip:rustpbx.com").unwrap();
        let to_uri = rsipstack::sip::Uri::try_from("sip:lp@rustpbx.com").unwrap();

        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: request_uri,
            version: rsipstack::sip::Version::V2,
            headers: vec![
                rsipstack::sip::typed::To {
                    display_name: None,
                    uri: to_uri.clone(),
                    params: vec![],
                }
                .into(),
            ]
            .into(),
            body: vec![],
        };

        let resolved = resolve_callee_uri(&request).expect("expected callee uri");
        assert_eq!(resolved, to_uri);
    }
}
