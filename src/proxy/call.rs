use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::{
    CallForwardingConfig, CalleeDisplayName, DialDirection, DialStrategy, Dialplan, Location,
    MediaConfig, RouteInvite, RoutingState, SipUser, TransactionCookie, TrunkContext,
};
use crate::config::{ProxyConfig, RouteResult};
use crate::media::recorder::RecorderOption;
use crate::proxy::data::ProxyDataContext;
use crate::proxy::proxy_call::CallSessionBuilder;
use crate::proxy::routing::{
    RouteRule, SourceTrunk, TrunkConfig, build_source_trunk,
    matcher::{RouteResourceLookup, inspect_invite, match_invite},
};
use anyhow::Error;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use chrono::Utc;
use glob::Pattern;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::transaction::key::TransactionRole;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipConnection;
use std::{collections::HashMap, net::IpAddr, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[async_trait]
pub trait CallRouter: Send + Sync {
    async fn resolve(
        &self,
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, (anyhow::Error, Option<rsip::StatusCode>)>;
}

fn q850_cause_from_status(code: &rsip::StatusCode) -> u16 {
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

fn q850_reason_value(code: &rsip::StatusCode, detail: Option<&str>) -> String {
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
        original: &rsip::Request,
    ) -> Result<Dialplan, (anyhow::Error, Option<rsip::StatusCode>)>;
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
        origin: &rsip::Request,
        direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let (trunks_snapshot, routes_snapshot, source_trunk) =
            self.build_context(origin, direction).await;
        if matches!(direction, DialDirection::Inbound) {
            if let Some(source) = source_trunk.as_ref() {
                if let Some(trunk_cfg) = trunks_snapshot.get(&source.name) {
                    let from_user = extract_from_user(origin);
                    let to_user = extract_to_user(origin);
                    match trunk_cfg
                        .matches_incoming_user_prefixes(from_user.as_deref(), to_user.as_deref())
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(
                                trunk = %source.name,
                                from = from_user.as_deref().unwrap_or(""),
                                to = to_user.as_deref().unwrap_or(""),
                                "dropping inbound INVITE due to SIP trunk user prefix mismatch",
                            );
                            return Ok(RouteResult::Abort(
                                rsip::StatusCode::Forbidden,
                                Some("Inbound identity rejected".to_string()),
                            ));
                        }
                        Err(err) => {
                            warn!(
                                trunk = %source.name,
                                error = %err,
                                "failed to evaluate SIP trunk user prefix",
                            );
                            return Ok(RouteResult::Abort(
                                rsip::StatusCode::ServerInternalError,
                                Some("Invalid trunk prefix configuration".to_string()),
                            ));
                        }
                    }
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
        origin: &rsip::Request,
        direction: &DialDirection,
        _cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let (trunks_snapshot, routes_snapshot, source_trunk) =
            self.build_context(origin, direction).await;

        let resource_lookup = self.data_context.as_ref() as &dyn RouteResourceLookup;
        inspect_invite(
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
        origin: &rsip::Request,
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
        origin: &rsip::Request,
        direction: &DialDirection,
    ) -> Option<SourceTrunk> {
        if !matches!(direction, DialDirection::Inbound) {
            return None;
        }

        if let Some(name) = self.source_trunk_hint.as_ref() {
            if let Some(config) = trunks.get(name) {
                return build_source_trunk(name.clone(), config, direction);
            }
        }

        let via = origin.via_header().ok()?;
        let (_, target) = SipConnection::parse_target_from_via(via).ok()?;
        let ip: IpAddr = target.host.try_into().ok()?;
        let name = self.data_context.find_trunk_by_ip(&ip).await?;
        let config = trunks.get(&name)?;
        build_source_trunk(name, config, direction)
    }
}

fn extract_from_user(origin: &rsip::Request) -> Option<String> {
    origin
        .from_header()
        .ok()
        .and_then(|header| header.uri().ok())
        .and_then(|uri| uri.user().map(|u| u.to_string()))
}

fn extract_to_user(origin: &rsip::Request) -> Option<String> {
    origin
        .to_header()
        .ok()
        .and_then(|header| header.uri().ok())
        .and_then(|uri| uri.user().map(|u| u.to_string()))
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
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, (Error, Option<rsip::StatusCode>)> {
        let callee_uri = original
            .to_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;
        let callee_realm = callee_uri.host().to_string();

        let dialog_id = original
            .call_id_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .value();
        let session_id = dialog_id.to_string();

        let media_config = MediaConfig::new()
            .with_proxy_mode(self.inner.config.media_proxy)
            .with_external_ip(self.inner.server.rtp_config.external_ip.clone())
            .with_rtp_start_port(self.inner.server.rtp_config.start_port.clone())
            .with_rtp_end_port(self.inner.server.rtp_config.end_port.clone())
            .with_ice_servers(self.inner.server.rtp_config.ice_servers.clone());

        let caller_is_same_realm = self
            .inner
            .server
            .is_same_realm(caller.realm.as_deref().unwrap_or_else(|| ""))
            .await;
        let callee_is_same_realm = self.inner.server.is_same_realm(&callee_realm).await;

        let is_from_trunk = cookie.get_extension::<TrunkContext>().is_some();

        let direction = match (caller_is_same_realm, callee_is_same_realm) {
            (true, true) if !is_from_trunk => {
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
                    Ok(None) => DialDirection::Outbound,
                    res => {
                        res.ok()
                            .flatten()
                            .and_then(|user| user.display_name)
                            .map(|display_name| {
                                cookie.insert_extension(CalleeDisplayName(display_name))
                            });
                        DialDirection::Internal
                    }
                }
            }
            (true, true) => DialDirection::Outbound,
            (true, false) => DialDirection::Outbound,
            (false, true) => DialDirection::Inbound,
            (false, false) => {
                warn!(dialog_id, caller_realm = ?caller.realm, callee_realm, "Both caller and callee are external realm, reject");
                return Err((
                    anyhow::anyhow!("Both caller and callee are external realm"),
                    Some(rsip::StatusCode::Forbidden),
                ));
            }
        };

        let mut loc = Location {
            aor: callee_uri.clone(),
            ..Default::default()
        };

        if callee_is_same_realm {
            if let Ok(results) = self.inner.server.locator.lookup(&callee_uri).await {
                loc.supports_webrtc |= results.iter().any(|item| item.supports_webrtc);
            }
        }

        let locs = vec![loc];
        let caller_uri = match caller.from.as_ref() {
            Some(uri) => uri.clone(),
            None => original
                .from_header()
                .map_err(|e| (anyhow::anyhow!(e), None))?
                .uri()
                .map_err(|e| (anyhow::anyhow!(e), None))?,
        };

        let preview_option = InviteOption {
            callee: callee_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri.clone(),
            ..Default::default()
        };

        let preview_outcome = route_invite
            .preview_route(preview_option, original, &direction, cookie)
            .await
            .map_err(|e| {
                (
                    anyhow::anyhow!(e),
                    Some(rsip::StatusCode::ServerInternalError),
                )
            })?;

        let (pending_queue, dialplan_hints) = match preview_outcome {
            RouteResult::Queue { queue, hints, .. } => (Some(queue), hints),
            RouteResult::Forward(_, hints) => (None, hints),
            RouteResult::NotHandled(_, hints) => (None, hints),
            RouteResult::Abort(code, reason) => {
                let err = anyhow::anyhow!(
                    reason.unwrap_or_else(|| "route aborted during preview".to_string())
                );
                return Err((err, Some(code)));
            }
        };

        let queue_targets = pending_queue
            .as_ref()
            .and_then(|plan| plan.dial_strategy.clone());
        let targets = queue_targets.unwrap_or_else(|| DialStrategy::Sequential(locs));
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
            .with_route_invite(route_invite);

        if let Some(queue) = pending_queue {
            dialplan = dialplan.with_queue(queue);
        } else {
            dialplan = dialplan.with_targets(targets);
        }

        if let Some(mut hints) = dialplan_hints {
            if let Some(enabled) = hints.enable_recording {
                dialplan.recording.enabled = enabled;
            }
            if let Some(bypass) = hints.bypass_media {
                if bypass {
                    dialplan.media.proxy_mode = crate::config::MediaProxyMode::None;
                }
            }
            if let Some(max_duration) = hints.max_duration {
                dialplan.max_call_duration = Some(max_duration);
            }
            if let Some(enable_sipflow) = hints.enable_sipflow {
                dialplan.enable_sipflow = enable_sipflow;
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

        if let Some(contact_uri) = self.inner.server.default_contact_uri() {
            let contact = rsip::typed::Contact {
                display_name: None,
                uri: contact_uri,
                params: vec![],
            };
            dialplan = dialplan.with_caller_contact(contact);
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

    async fn resolve_callee_forwarding(
        &self,
        request: &rsip::Request,
    ) -> Result<Option<CallForwardingConfig>> {
        let callee_uri = request.to_header()?.uri()?;
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

        match self
            .inner
            .server
            .user_backend
            .get_user(username.as_str(), Some(&callee_realm), Some(request))
            .await
            .map_err(Into::into)
        {
            Ok(Some(user)) => Ok(user.forwarding_config()),
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
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

    fn identity_from_uri(uri: rsip::Uri) -> String {
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
    ) -> Result<Dialplan, (Error, Option<rsip::StatusCode>)> {
        let trunk_context = cookie.get_extension::<TrunkContext>();
        let source_trunk_hint = trunk_context.as_ref().map(|c| c.name.clone());

        let route_invite: Box<dyn RouteInvite> =
            if let Some(f) = self.inner.server.create_route_invite.as_ref() {
                f(
                    self.inner.server.clone(),
                    self.inner.config.clone(),
                    self.inner.routing_state.clone(),
                )
                .map_err(|e| (e, None))?
            } else {
                Box::new(DefaultRouteInvite {
                    routing_state: self.inner.routing_state.clone(),
                    data_context: self.inner.server.data_context.clone(),
                    source_trunk_hint,
                })
            };

        let dialplan = if let Some(resolver) = self.inner.server.call_router.as_ref() {
            resolver
                .resolve(&tx.original, route_invite, &caller, &cookie)
                .await
        } else {
            self.default_resolve(&tx.original, route_invite, &caller, &cookie)
                .await
        }?;

        let mut dialplan = dialplan;
        for inspector in &self.inner.server.dialplan_inspectors {
            dialplan = inspector
                .inspect_dialplan(dialplan, &cookie, &tx.original)
                .await?
        }

        if dialplan.call_forwarding.is_none() {
            // Optimization: Skip call forwarding check for wholesale calls (from trunks)
            let is_wholesale = cookie
                .get_extension::<TrunkContext>()
                .map(|ctx| ctx.tenant_id.is_some())
                .unwrap_or(false);

            if !is_wholesale {
                match self.resolve_callee_forwarding(&tx.original).await {
                    Ok(Some(config)) => {
                        dialplan = dialplan.with_call_forwarding(Some(config));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "failed to resolve call forwarding for callee");
                    }
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
        code: rsip::StatusCode,
        reason: Option<String>,
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
        let dialplan = Dialplan::new(session_id, tx.original.clone(), direction);
        let proxy_call = CallSessionBuilder::new(cookie.clone(), dialplan, 70)
            .with_call_record_sender(self.inner.server.callrecord_sender.clone());
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

        let dialplan = match self.build_dialplan(tx, cookie.clone(), &caller).await {
            Ok(d) => d,
            Err((e, code)) => {
                if cookie.is_spam() {
                    return Ok(());
                }
                let code = code.unwrap_or(rsip::StatusCode::ServerInternalError);
                let reason_text = e.to_string();
                let reason_value = q850_reason_value(&code, Some(reason_text.as_str()));
                warn!(%code, key = %tx.key, reason = %reason_value, "failed to build dialplan");
                self.report_failure(tx, &cookie, code.clone(), Some(reason_text));
                tx.reply_with(
                    code.clone(),
                    vec![rsip::Header::Other("Reason".into(), reason_value)],
                    None,
                )
                .await
                .map_err(|e| anyhow!("Failed to send reply: {}", e))?;
                return Err(e);
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
            self.report_failure(tx, &cookie, rsip::StatusCode::TooManyHops, None);
            tx.reply(rsip::StatusCode::TooManyHops).await?;
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

        // For re-INVITE and UPDATE with SDP, we want to ensure they get an SDP answer.
        // We can retrieve the current answer from the session's shared state.
        let is_reinvite = tx.original.method == rsip::Method::Invite;
        let is_update = tx.original.method == rsip::Method::Update;
        let has_sdp = !tx.original.body.is_empty();

        if (is_reinvite || is_update) && has_sdp {
            if let Some(handle) = self
                .inner
                .server
                .active_call_registry
                .get_handle_by_dialog(&dialog_id.to_string())
            {
                let snapshot = handle.snapshot();
                if let Some(sdp) = snapshot.answer_sdp {
                    info!(%dialog_id, ?tx.original.method, "Replying to mid-dialog request with SDP from shared state");
                    let headers = vec![rsip::Header::ContentType("application/sdp".into())];
                    tx.reply_with(rsip::StatusCode::OK, headers, Some(sdp.into_bytes()))
                        .await
                        .map_err(|e| anyhow!(e))?;

                    // Still pass it to the dialog so it can emit events and update its state
                    return dialog.handle(tx).await.map_err(|e| anyhow!(e));
                }
            }
        }

        dialog.handle(tx).await.map_err(|e| anyhow!(e))
    }
}

#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Bye,
            rsip::Method::Info,
            rsip::Method::Ack,
            rsip::Method::Cancel,
            rsip::Method::Options,
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
            rsip::Method::Invite => {
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

                if let Err(e) = self.handle_invite(token, tx, cookie).await {
                    if tx.last_response.is_none() {
                        let code = rsip::StatusCode::ServerInternalError;
                        let reason_text = e.to_string();
                        tx.reply_with(
                            code.clone(),
                            vec![rsip::Header::Other(
                                "Reason".into(),
                                q850_reason_value(&code, Some(reason_text.as_str())),
                            )],
                            None,
                        )
                        .await
                        .map_err(|e| anyhow!(e))?;
                    }
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Options
            | rsip::Method::Info
            | rsip::Method::Ack
            | rsip::Method::Update
            | rsip::Method::Cancel
            | rsip::Method::Bye => {
                if let Err(e) = self.process_message(tx).await {
                    warn!(%dialog_id, method=%tx.original.method, "error process {}\n{}", e, tx.original.to_string());
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
