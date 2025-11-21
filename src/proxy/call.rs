use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::{
    CallForwardingConfig, DialDirection, DialStrategy, Dialplan, Location, MediaConfig,
    RouteInvite, RoutingState, SipUser, TransactionCookie,
};
use crate::callrecord::{
    CallRecord, CallRecordHangupMessage, CallRecordHangupReason, CallRecordPersistArgs,
    apply_record_file_extras, extract_sip_username, extras_map_to_metadata, extras_map_to_option,
    persist_and_dispatch_record, sipflow::SipMessageItem,
};
use crate::config::{ProxyConfig, RouteResult};
use crate::media::recorder::RecorderOption;
use crate::proxy::data::ProxyDataContext;
use crate::proxy::proxy_call::ProxyCall;
use crate::proxy::proxy_call::ProxyCallBuilder;
use crate::proxy::routing::{
    RouteRule, SourceTrunk, TrunkConfig, build_source_trunk,
    matcher::{RouteResourceLookup, inspect_invite, match_invite},
};
use anyhow::Error;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use glob::Pattern;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipConnection;
use serde_json::{Number as JsonNumber, Value};
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
    ) -> Result<Dialplan, (anyhow::Error, Option<rsip::StatusCode>)>;
}

fn dialog_call_id(dialog_id: &DialogId) -> Option<String> {
    let candidate = dialog_id.call_id.trim();
    if !candidate.is_empty() {
        return Some(candidate.to_string());
    }

    let raw = dialog_id.to_string();
    let trimmed = raw
        .split(|c| matches!(c, ';' | ':' | ' ' | '\t'))
        .next()
        .map(|s| s.trim())
        .unwrap_or_default();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn q850_cause_from_status(code: &rsip::StatusCode) -> u16 {
    match u16::from(code.clone()) {
        400 | 401 | 402 | 403 | 405 | 406 | 407 | 421 | 603 => 21, // call rejected / not allowed
        404 | 484 | 604 => 1,                                      // unallocated number
        410 => 22,                                                 // number changed
        413 | 414 | 416 | 420 => 127,                              // interworking / feature not supported
        480 | 408 => 18,                                           // no user responding / timeout
        486 | 600 => 17,                                           // user busy
        487 => 31,                                                 // request terminated / normal unspecified
        488 | 606 => 79,                                           // service or option not available
        502 | 503 => 38,                                           // network out of order
        500 | 580 => 41,                                           // temporary failure
        504 => 34,                                                 // no circuit / channel available
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

#[async_trait]
pub trait ProxyCallInspector: Send + Sync {
    async fn on_start(&self, call: ProxyCall) -> Result<ProxyCall, (rsip::StatusCode, String)>;
    async fn on_end(&self, call: &ProxyCall);
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
        let trunks_snapshot = self.data_context.trunks_snapshot().await;
        let routes_snapshot = self.data_context.routes_snapshot().await;
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
        let inner = Arc::new(CallModuleInner {
            config,
            server,
            dialog_layer,
            routing_state: Arc::new(RoutingState::new()),
        });
        Self { inner }
    }

    async fn default_resolve(
        &self,
        original: &rsip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
    ) -> Result<Dialplan, (Error, Option<rsip::StatusCode>)> {
        let callee_uri = original
            .to_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;
        let callee_realm = callee_uri.host().to_string();
        let dialog_id = DialogId::try_from(original).map_err(|e| (anyhow!(e), None))?;
        let session_id: String = format!("{}-{}", rand::random::<u32>(), dialog_id);

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

        let direction = match (caller_is_same_realm, callee_is_same_realm) {
            (true, true) => {
                match self
                    .inner
                    .server
                    .user_backend
                    .get_user(callee_uri.user().unwrap_or_default(), Some(&callee_realm))
                    .await
                {
                    Ok(None) => DialDirection::Outbound,
                    _ => DialDirection::Internal,
                }
            }
            (true, false) => DialDirection::Outbound,
            (false, true) => DialDirection::Inbound,
            (false, false) => {
                warn!(%dialog_id, caller_realm = ?caller.realm, callee_realm, "Both caller and callee are external realm, reject");
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
            .preview_route(preview_option, original, &direction)
            .await
            .map_err(|e| {
                (
                    anyhow::anyhow!(e),
                    Some(rsip::StatusCode::ServerInternalError),
                )
            })?;

        let mut pending_queue = None;
        let mut pending_ivr = None;

        match preview_outcome {
            RouteResult::Queue { queue, .. } => pending_queue = Some(queue),
            RouteResult::Ivr { ivr, .. } => pending_ivr = Some(ivr),
            RouteResult::Abort(code, reason) => {
                let err = anyhow::anyhow!(
                    reason.unwrap_or_else(|| "route aborted during preview".to_string())
                );
                return Err((err, Some(code)));
            }
            _ => {}
        }

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
            .with_route_invite(route_invite)
            .with_targets(targets);

        if let Some(queue_plan) = pending_queue {
            dialplan = dialplan.with_queue(queue_plan);
        }

        if let Some(ivr_config) = pending_ivr {
            dialplan = dialplan.with_ivr(ivr_config);
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

        let mut recorder_option =
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
            if existing.format.is_none() {
                existing.format = recorder_option.format;
            }
            if let Some(rate) = policy.samplerate {
                existing.samplerate = rate;
            }
            if let Some(ptime) = policy.ptime {
                existing.ptime = ptime;
            }
            let fallback_format = existing.format.unwrap_or(policy.recorder_format());
            existing.ensure_path_extension(fallback_format);
        } else {
            recorder_option.ensure_path_extension(policy.recorder_format());
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
            .get_user(username.as_str(), Some(&callee_realm))
            .await
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
        let direction = match dialplan.direction {
            DialDirection::Inbound => "inbound",
            DialDirection::Outbound => "outbound",
            DialDirection::Internal => "internal",
        };
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
        let rendered =
            Self::render_filename(pattern, &session_id, caller, callee, direction, &timestamp);
        let sanitized = Self::sanitize_filename_component(&rendered, &session_id);
        let mut path = PathBuf::from(root);
        if sanitized.is_empty() {
            return None;
        }
        path.push(sanitized);
        if path.extension().is_none() {
            path.set_extension(policy.recorder_format().extension());
        }
        let mut option = RecorderOption::new(path.to_string_lossy().to_string());
        if let Some(rate) = policy.samplerate {
            option.samplerate = rate;
        }
        if let Some(ptime) = policy.ptime {
            option.ptime = ptime;
        }
        let policy_format = policy.recorder_format();
        option.format = Some(policy_format);
        option.ensure_path_extension(policy_format);
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
        let source_trunk_hint = cookie.get_source_trunk();

        let route_invite: Box<dyn RouteInvite> =
            if let Some(f) = self.inner.server.create_route_invite.as_ref() {
                f(self.inner.server.clone(), self.inner.config.clone()).map_err(|e| (e, None))?
            } else {
                Box::new(DefaultRouteInvite {
                    routing_state: self.inner.routing_state.clone(),
                    data_context: self.inner.server.data_context.clone(),
                    source_trunk_hint,
                })
            };

        let dialplan = if let Some(resolver) = self.inner.server.call_router.as_ref() {
            resolver.resolve(&tx.original, route_invite, &caller).await
        } else {
            self.default_resolve(&tx.original, route_invite, &caller)
                .await
        }?;

        let mut dialplan = if let Some(inspector) = self.inner.server.dialplan_inspector.as_ref() {
            inspector
                .inspect_dialplan(dialplan, &cookie, &tx.original)
                .await?
        } else {
            dialplan
        };

        if dialplan.call_forwarding.is_none() {
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

        let dialplan = self.apply_recording_policy(dialplan, caller);
        Ok(dialplan)
    }

    pub(crate) async fn handle_invite(
        &self,
        cancel_token: CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<()> {
        let caller = cookie
            .get_user()
            .ok_or_else(|| anyhow::anyhow!("Missing caller user in transaction cookie"))?;

        let dialplan = match self.build_dialplan(tx, cookie.clone(), &caller).await {
            Ok(d) => d,
            Err((e, code)) => {
                let code = code.unwrap_or(rsip::StatusCode::ServerInternalError);
                let reason_text = e.to_string();
                let reason_value = q850_reason_value(&code, Some(reason_text.as_str()));
                warn!(%code, key = %tx.key, reason = %reason_value, "failed to build dialplan");
                self.record_failed_call(tx, &cookie, code.clone(), Some(reason_text.clone()))
                    .await
                    .ok();
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
        let builder = ProxyCallBuilder::new(cookie.clone(), dialplan)
            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
            .with_cancel_token(cancel_token);

        let proxy_call = builder.build(self.inner.server.clone());
        let proxy_call = if let Some(inspector) = self.inner.server.proxycall_inspector.as_ref() {
            match inspector.on_start(proxy_call).await {
                Ok(call) => call,
                Err((code, reason_phrase)) => {
                    warn!(%code, key = %tx.key,"failed to proxy call {}", reason_phrase);
                    self.record_failed_call(tx, &cookie, code.clone(), Some(reason_phrase.clone()))
                        .await
                        .ok();
                    let reason_value = q850_reason_value(&code, Some(reason_phrase.as_str()));
                    tx.reply_with(
                        code.clone(),
                        vec![rsip::Header::Other("Reason".into(), reason_value)],
                        None,
                    )
                        .await
                        .map_err(|e| anyhow!("failed to proxy call: {}", e))?;
                    return Err(anyhow::anyhow!("failed toproxy call"));
                }
            }
        } else {
            proxy_call
        };

        let r = proxy_call.process(tx).await;
        if let Some(inspector) = self.inner.server.proxycall_inspector.as_ref() {
            inspector.on_end(&proxy_call).await;
        }

        match r {
            Ok(()) => {
                info!(session_id=proxy_call.id(), elapsed = ?proxy_call.elapsed(), "session successfully");
                Ok(())
            }
            Err(e) => {
                warn!(session_id=proxy_call.id(), elapsed = ?proxy_call.elapsed(), "error establishing session: {}", e);
                Err(e)
            }
        }
    }

    async fn process_message(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = DialogId::try_from(&tx.original).map_err(|e| anyhow!(e))?;
        let mut dialog = match self.inner.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => dialog,
            None => {
                debug!(%dialog_id, method=%tx.original.method, "dialog not found for message");
                return Ok(());
            }
        };
        dialog.handle(tx).await.map_err(|e| anyhow!(e))
    }

    async fn record_failed_call(
        &self,
        tx: &Transaction,
        cookie: &TransactionCookie,
        status_code: rsip::StatusCode,
        reason_phrase: Option<String>,
    ) -> Result<()> {
        let dialog_id = DialogId::try_from(&tx.original)?;
        let now = Utc::now();
        let caller = tx.original.from_header()?.uri()?.to_string();
        let callee = tx.original.to_header()?.uri()?.to_string();
        let offer = Some(String::from_utf8_lossy(&tx.original.body).to_string());

        let mut extras_map: HashMap<String, Value> = HashMap::new();
        extras_map.insert(
            "status_code".to_string(),
            Value::Number(JsonNumber::from(u16::from(status_code.clone()))),
        );

        let mut hangup_messages: Vec<CallRecordHangupMessage> = Vec::new();
        hangup_messages.push(CallRecordHangupMessage {
            code: u16::from(status_code.clone()),
            reason: reason_phrase.clone(),
            target: None,
        });
        if let Ok(value) = serde_json::to_value(&hangup_messages) {
            extras_map.insert("hangup_messages".to_string(), value);
        }

        let mut sip_flows: HashMap<String, Vec<SipMessageItem>> = HashMap::new();
        let leg_call_id = dialog_call_id(&dialog_id).unwrap_or_else(|| dialog_id.to_string());
        if let Some(items) = self.inner.server.drain_sip_flow(&leg_call_id) {
            if !items.is_empty() {
                sip_flows.insert(leg_call_id.clone(), items);
            }
        }

        let mut sip_leg_roles: HashMap<String, String> = HashMap::new();
        sip_leg_roles.insert(leg_call_id.clone(), "primary".to_string());

        let mut record = CallRecord {
            call_type: crate::call::ActiveCallType::Sip,
            option: None,
            call_id: dialog_id.to_string(),
            start_time: now.clone(),
            ring_time: None,
            answer_time: None,
            end_time: now,
            caller: caller.clone(),
            callee: callee.clone(),
            status_code: status_code.into(),
            offer,
            answer: None,
            hangup_reason: Some(CallRecordHangupReason::BySystem),
            hangup_messages,
            recorder: Vec::new(),
            extras: None,
            dump_event_file: None,
            refer_callrecord: None,
            sip_flows,
            sip_leg_roles,
        };

        apply_record_file_extras(&record, &mut extras_map);
        record.extras = extras_map_to_option(&extras_map);

        let direction = if cookie.is_from_trunk() {
            "inbound".to_string()
        } else {
            "internal".to_string()
        };
        let trunk_name = cookie.get_source_trunk();
        let (sip_gateway, sip_trunk_id) = if let Some(ref name) = trunk_name {
            let trunks = self.inner.server.data_context.trunks_snapshot().await;
            let trunk_id = trunks.get(name).and_then(|config| config.id);
            (Some(name.clone()), trunk_id)
        } else {
            (None, None)
        };

        let metadata_value = extras_map_to_metadata(&extras_map);

        let mut persist_args = CallRecordPersistArgs::default();
        persist_args.direction = direction;
        persist_args.status = "failed".to_string();
        persist_args.from_number = extract_sip_username(&caller);
        persist_args.to_number = extract_sip_username(&callee);
        persist_args.sip_trunk_id = sip_trunk_id;
        persist_args.sip_gateway = sip_gateway;
        persist_args.metadata = metadata_value;

        let (persist_error, send_error) = persist_and_dispatch_record(
            self.inner.server.database.as_ref(),
            self.inner.server.callrecord_sender.as_ref(),
            record,
            persist_args,
        )
        .await;

        if let Some(err) = persist_error {
            warn!(dialog_id = %dialog_id, error = %err, "failed to persist failed call record");
        }
        if let Some(err) = send_error {
            warn!(dialog_id = %dialog_id, error = %err, "failed to send call record");
        }
        Ok(())
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
        let dialog_id = DialogId::try_from(&tx.original).map_err(|e| anyhow!(e))?;
        info!(
            %dialog_id,
            method = %tx.original.method,
            uri = %tx.original.uri,
            caller = %cookie.get_user().as_ref().map(|u|u.to_string()).unwrap_or_default(),
            "call transaction begin",
        );
        match tx.original.method {
            rsip::Method::Invite => {
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
