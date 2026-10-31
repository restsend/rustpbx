use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::{
    CalleeDisplayName, CalleeOfflineMarker, DialDirection, DialStrategy, Dialplan, DialplanFlow,
    Location, MediaConfig, RouteInvite, RoutingState, SipUser, TransactionCookie, TrunkContext,
};
use crate::config::{ProxyConfig, RecordingPolicy, RouteResult};
use crate::media::recorder::RecorderOption;
use crate::proxy::active_call_registry::ActiveProxyCallStatus;
use crate::proxy::data::ProxyDataContext;
use crate::proxy::proxy_call::CallSessionBuilder;
use crate::proxy::routing::{
    RouteRule, SourceTrunk, TrunkConfig, build_source_trunk,
    matcher::{RouteResourceLookup, match_invite},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use chrono::Utc;
use glob::Pattern;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::transaction::key::TransactionRole;
use rsipstack::transaction::transaction::Transaction;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Routing context for a SIP conference focus dialog.
#[derive(Clone)]
pub(crate) struct ConferenceFocusContext {
    pub uri: String,
    pub room: crate::call::runtime::ConferenceId,
}

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

impl RouteError {
    /// Attach a standardized error code (from the [`crate::call_errors`]
    /// registry) to this error so the call-record reporter can persist it as
    /// `error_code` metadata.  The code rides the existing `extensions` seam.
    pub fn with_code(mut self, info: &'static crate::call_errors::CallErrInfo) -> Self {
        let mut exts = self.extensions.unwrap_or_default();
        exts.insert("error_code".to_string(), info.code.to_string());
        self.extensions = Some(exts);
        self
    }

    /// Attach a standardized error code plus a dynamic detail string.
    pub fn with_code_detail(
        mut self,
        info: &'static crate::call_errors::CallErrInfo,
        detail: impl Into<String>,
    ) -> Self {
        let mut exts = self.extensions.unwrap_or_default();
        exts.insert("error_code".to_string(), info.code.to_string());
        exts.insert("error_detail".to_string(), detail.into());
        self.extensions = Some(exts);
        self
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
    crate::proxy::routing::escape_sip_quoted(text)
}

/// Decide what to do when routing returned NotHandled (no explicit forward/queue).
/// Returns `Ok(targets)` to proceed with the given locations, or `Err` to reject.
///
/// `parallel_fork` controls how multiple registered devices for the same
/// callee are treated: when `true` (default) all bindings ring simultaneously
/// (first to answer wins); when `false` only the most recently registered
/// device is dialed.
pub(crate) fn resolve_unhandled_targets(
    callee_is_same_realm: bool,
    internal_lookup_empty: bool,
    locs: Vec<Location>,
    parallel_fork: bool,
) -> Result<DialStrategy, RouteError> {
    if callee_is_same_realm && internal_lookup_empty {
        // Return empty targets — the offline check is deferred to build_dialplan
        // so dialplan inspectors (e.g. zhongan inbound, HTTP router) can attempt
        // alternative routing before rejecting.
        Ok(DialStrategy::Sequential(vec![]))
    } else if parallel_fork && locs.len() > 1 {
        // Parallel-fork only when there is something to fork. The fork logic
        // races all registered devices and cancels the losers once the first
        // 200 OK arrives.
        Ok(DialStrategy::Parallel(locs))
    } else {
        // A single target uses the simpler sequential dialer. When parallel
        // forking is disabled and several bindings exist, locs are sorted
        // most-recent first, so dial only the newest registration.
        Ok(DialStrategy::Sequential(locs.into_iter().take(1).map(Into::into).collect()))
    }
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

pub fn route_error_reason(route_err: &RouteError) -> String {
    let code = route_err
        .status
        .clone()
        .unwrap_or(rsipstack::sip::StatusCode::ServerInternalError);
    let reason_text = route_err.error.to_string();
    if reason_text.contains(";cause=") {
        reason_text
    } else {
        q850_reason_value(&code, Some(reason_text.as_str()))
    }
}

/// Verdict returned by a [`DialplanInspector`].
///
/// Allows an inspector to continue the chain, finalize the dialplan
/// (skipping remaining inspectors), or reject the call outright.
pub enum DialplanVerdict {
    /// Pass the (possibly modified) dialplan to the next inspector.
    Continue(Dialplan),
    /// The dialplan is final — skip remaining inspectors.
    Final(Dialplan),
    /// Reject the call with the given error.
    Reject(RouteError),
}

impl From<Dialplan> for DialplanVerdict {
    fn from(dp: Dialplan) -> Self {
        DialplanVerdict::Continue(dp)
    }
}

#[async_trait]
pub trait DialplanInspector: Send + Sync {
    async fn inspect_dialplan(
        &self,
        dialplan: Dialplan,
        cookie: &TransactionCookie,
        original: &rsipstack::sip::Request,
    ) -> DialplanVerdict;

    /// Downcast hook so the server can reach reloadable inspector state during
    /// a hot-reload (e.g. `EmergencyInspector`). Implementors that keep
    /// reloadable state should override this.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

/// Context passed to [`QueueLocationEnricher::enrich`].
pub struct QueueEnrichContext<'a> {
    /// Proxy session identifier — suitable for CDR / screen-pop lookup.
    pub session_id: &'a str,
    /// Canonical queue key / name.
    pub queue_name: &'a str,
    /// Queue identifier (may equal `queue_name` when the PBX keys queues by
    /// name — the CC addon treats the skill-group id as the queue id).
    pub queue_id: &'a str,
    /// Caller user part (RFC 3261 `From` user) as the desk sees it.
    pub caller: &'a str,
    /// Callee user part (RFC 3261 `To` user).
    pub callee: &'a str,
    /// Call direction ("inbound" | "outbound").
    pub direction: &'a str,
    /// Optional CC skill-group id (overrides `queue_id` for the `sg` UUI key).
    pub skill_group_id: Option<&'a str>,
    /// Optional IVR / flow node id the call entered the queue from (UUI `ivr`).
    pub ivr_node_id: Option<&'a str>,
    /// Raw SIP headers from the *caller's* INVITE (e.g. `X-CRM-*`).
    pub caller_headers: &'a [rsipstack::sip::Header],
}

/// Resolves CC quick-route feature codes (`*81<skill-group-id>` /
/// `*82<ivr-name>` / `*83<room-id>`) into internal transfer targets
/// (`queue:skill-group:<id>` / `ivr:<name>` / `conference:<room>`).
/// Consulted by the inbound-REFER hand-off *before* the route table so the
/// codes reach the ACD / IVR / conference with zero route configuration.
/// Unknown user parts return `None` and fall through to normal routing.
#[async_trait::async_trait]
pub trait QuickRouteResolver: Send + Sync {
    async fn resolve_quick_target(&self, user: &str) -> Option<String>;
}

/// Hook called by the queue executor after agent locations are resolved but
/// *before* dialing begins, allowing addons to inject extra SIP headers
/// (screen-pop context, CRM correlation IDs, etc.) into each target location.
///
/// The cc addon uses this to attach a RFC 7433 `User-to-User` header
/// (`purpose=call-center`, carrying the root session id and queue context)
/// plus any caller-supplied `X-CRM-*` headers so the outbound INVITE to the
/// agent carries full screen-pop metadata.
///
/// # Default behaviour
/// If no enricher is registered the locations are forwarded unchanged.
#[async_trait::async_trait]
pub trait QueueLocationEnricher: Send + Sync {
    async fn enrich(
        &self,
        locations: Vec<crate::call::Location>,
        ctx: &QueueEnrichContext<'_>,
    ) -> Vec<crate::call::Location>;
}

pub struct DefaultRouteInvite {
    pub routing_state: Arc<RoutingState>,
    pub data_context: Arc<ProxyDataContext>,
}

#[async_trait]
impl RouteInvite for DefaultRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
        cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let (trunks_snapshot, routes_snapshot, source_trunk) =
            self.build_context(direction, cookie);
        let resource_lookup = self.data_context.as_ref() as &dyn RouteResourceLookup;
        // Check debug routes before standard routing
        if let Some(callee_user) = origin.uri.user() {
            if let Some(entry) = self.data_context.debug_routes.get(callee_user) {
                let (app_name, app_params) = entry.value();
                return Ok(RouteResult::Application {
                    option,
                    app_name: app_name.clone(),
                    app_params: app_params.clone(),
                    auto_answer: true,
                    hints: None,
                });
            }
        }
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

/// Try a primary [`RouteInvite`]; on `NotHandled`, delegate to a fallback
/// (typically wholesale → PBX default routes).
pub struct ChainedRouteInvite {
    primary: Box<dyn RouteInvite>,
    fallback: Box<dyn RouteInvite>,
}

impl ChainedRouteInvite {
    pub fn new(primary: Box<dyn RouteInvite>, fallback: Box<dyn RouteInvite>) -> Self {
        Self { primary, fallback }
    }

    async fn dispatch(
        &self,
        option: InviteOption,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
        cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        let primary_result = self
            .primary
            .route_invite(option, origin, direction, cookie)
            .await?;
        match primary_result {
            RouteResult::NotHandled(option, _) => {
                self.fallback
                    .route_invite(option, origin, direction, cookie)
                    .await
            }
            other => Ok(other),
        }
    }
}

#[async_trait]
impl RouteInvite for ChainedRouteInvite {
    async fn route_invite(
        &self,
        option: InviteOption,
        origin: &rsipstack::sip::Request,
        direction: &DialDirection,
        cookie: &TransactionCookie,
    ) -> Result<RouteResult> {
        self.dispatch(option, origin, direction, cookie).await
    }
}

/// Compose every registered [`FnCreateRouteInvite`] factory into one
/// `RouteInvite` chain, with the PBX [`DefaultRouteInvite`] as the final
/// fallback. Factories contribute nodes in registration order; each node is
/// consulted only when the previous one returns `NotHandled`.
///
/// Replaces the previous "first factory wins" behaviour so multiple addons
/// (e.g. wholesale + cc) can each contribute a node to the chain.
pub fn compose_route_invites(
    server: &SipServerRef,
    routing_state: Arc<RoutingState>,
) -> Result<Box<dyn RouteInvite>> {
    let mut chain: Option<Box<dyn RouteInvite>> = None;
    for factory in server.create_route_invites.iter() {
        let invite = factory(
            server.clone(),
            server.proxy_config.load_full(),
            routing_state.clone(),
        )?;
        chain = Some(match chain {
            Some(existing) => {
                Box::new(ChainedRouteInvite::new(existing, invite)) as Box<dyn RouteInvite>
            }
            None => invite,
        });
    }
    let default = Box::new(DefaultRouteInvite {
        routing_state,
        data_context: server.data_context.clone(),
    }) as Box<dyn RouteInvite>;
    Ok(match chain {
        Some(existing) => Box::new(ChainedRouteInvite::new(existing, default)),
        None => default,
    })
}

impl DefaultRouteInvite {
    fn build_context(
        &self,
        direction: &DialDirection,
        cookie: &TransactionCookie,
    ) -> (
        std::collections::HashMap<String, TrunkConfig>,
        Vec<RouteRule>,
        Option<SourceTrunk>,
    ) {
        let trunks_snapshot = self.data_context.trunks_snapshot();
        let routes_snapshot = self.data_context.routes_snapshot();
        let source_trunk = if matches!(direction, DialDirection::Inbound) {
            cookie.get_extension::<TrunkContext>().and_then(|context| {
                trunks_snapshot
                    .get(&context.name)
                    .and_then(|config| build_source_trunk(context.name, config, direction))
            })
        } else {
            None
        };
        (trunks_snapshot, routes_snapshot, source_trunk)
    }
}

pub(crate) fn resolve_callee_uri(origin: &rsipstack::sip::Request) -> Result<rsipstack::sip::Uri> {
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

pub(crate) fn parse_allowed_codecs(codec_names: &[String]) -> Vec<CodecType> {
    let mut allow_codecs = Vec::new();
    for codec_name in codec_names {
        let codec_name = codec_name.trim();
        if codec_name.is_empty() {
            continue;
        }
        match CodecType::try_from(codec_name) {
            Ok(codec) if !allow_codecs.contains(&codec) => allow_codecs.push(codec),
            Ok(_) => {}
            Err(_) => warn!(codec = %codec_name, "Ignoring unsupported audio codec in allow list"),
        }
    }
    allow_codecs
}

pub(crate) fn apply_allowed_codecs(
    dialplan: &mut Dialplan,
    preferred: Option<&[String]>,
    fallback: Option<&[String]>,
) {
    if let Some(codec_names) = preferred {
        let allow_codecs = parse_allowed_codecs(codec_names);
        if !allow_codecs.is_empty() {
            dialplan.allow_codecs = allow_codecs;
            return;
        }
    }

    if let Some(codec_names) = fallback {
        let allow_codecs = parse_allowed_codecs(codec_names);
        if !allow_codecs.is_empty() {
            dialplan.allow_codecs = allow_codecs;
        }
    }
}

#[derive(Clone)]
pub struct CallModuleInner {
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

        let routing_state = Arc::new(routing_state);
        // Share the routing state with the server so app/transfer/RWI-originated
        // legs routed via `route_outbound_leg` use the same round-robin counters
        // and policy guard as the inbound path.
        *server.routing_state.write() = routing_state.clone();

        let inner = Arc::new(CallModuleInner {
            server,
            dialog_layer,
            routing_state,
        });
        Self { inner }
    }

    /// Session id for an inbound INVITE — the single seam `default_resolve`
    /// resolves every inbound call through.
    ///
    /// Cluster child legs of an existing logical call (queue dispatch,
    /// consult, transfer — including RE-DIAL rounds whose wire Call-ID carries
    /// the `-r{n}` suffix) adopt the root session id their peer declares on
    /// the wire, so the agent-answered session keeps the caller's `call_id`
    /// and every event / CDR of the logical call stays on one id. Everything
    /// else keeps the legacy derivation: reuse the Call-ID when it is
    /// id-shaped (unless `session_id_inherit_callid` is disabled), else a
    /// fresh id; plain P2P internal calls keep the raw Call-ID verbatim.
    pub(crate) fn resolve_inbound_session_id(
        server: &SipServerRef,
        cookie: &TransactionCookie,
        original: &rsipstack::sip::Request,
        dialog_id: &str,
        session_id_enabled: bool,
    ) -> String {
        if let Some(root) = Self::inherited_child_session_id(server, cookie, original) {
            return root;
        }
        if session_id_enabled {
            crate::call::session_id::derive_from_call_id(dialog_id)
        } else {
            dialog_id.to_string()
        }
    }

    /// Root id a CLUSTER PEER declares for a child leg of an existing logical
    /// call: a CC UUI `leg=` intent (`agent` / `consult` / `transfer`) plus
    /// the root id on the `Session-ID` header (UUI data fallback).
    ///
    /// Why: the FIRST agent-facing leg dials with the root id as its wire
    /// Call-ID, so Call-ID derivation lands on it. Every later leg of the
    /// same logical call must NOT reuse a Call-ID the phone may have already
    /// declined — it dials `{root}-r{n}`, which `derive_from_call_id` cannot
    /// normalize, and the child session used to fall back to a fresh id (the
    /// "sessionid changed when the agent picked up" divergence). The
    /// Session-ID header / UUI are explicit correlation channels that survive
    /// the re-dial, so adoption restores one id across the whole logical
    /// call.
    ///
    /// `None` → legacy derivation: non-peer sources (an external PBX planting
    /// the headers must not merge calls), INVITEs without a CC leg intent,
    /// and the parallel-leg case where a live local session already holds the
    /// root id (parallel agent legs on one node must not collapse into one
    /// id; they still correlate through the enriched `session_id`).
    fn inherited_child_session_id(
        server: &SipServerRef,
        cookie: &TransactionCookie,
        original: &rsipstack::sip::Request,
    ) -> Option<String> {
        if !crate::proxy::proxy_call::sip_session::SipSession::from_cluster_peer(server, cookie) {
            return None;
        }
        let cc_uui = crate::call::uui::extract_cc_uui(&original.headers)?;
        // A declared dial intent marks this INVITE as a child leg of an
        // existing logical call — the same trust seam `SipSession::new_inner`
        // uses for `child_leg_role`.
        cc_uui
            .leg
            .as_deref()
            .filter(|leg| crate::call::uui::is_known_leg_role(leg))?;
        let root = crate::call::session_id::extract_inherited(original)
            .or_else(|| crate::call::session_id::normalize_inherited(&cc_uui.session_id))?;
        if server.active_call_registry.get(&root).is_some() {
            debug!(
                root = %root,
                "child-leg root id already live on this node; keeping a unique session id"
            );
            return None;
        }
        info!(
            root = %root,
            "inbound cluster child leg adopts the root session id"
        );
        Some(root)
    }

    pub(crate) async fn default_resolve(
        &self,
        original: &rsipstack::sip::Request,
        route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, RouteError> {
        let callee_uri = resolve_callee_uri(original).map_err(|e| {
            RouteError::from((e, None)).with_code(&crate::proxy::error_catalog::CALLEE_URI_INVALID)
        })?;
        let callee_realm = callee_uri.host().to_string();

        let dialog_id = original
            .call_id_header()
            .map_err(|e| {
                RouteError::from((anyhow::anyhow!(e), None))
                    .with_code(&crate::proxy::error_catalog::MISSING_CALL_ID)
            })?
            .value();

        let media_config = {
            let rtp = self.inner.server.rtp_config.load();
            let proxy_mode = **self.inner.server.media_proxy.load();
            MediaConfig::new()
                .with_proxy_mode(proxy_mode)
                .with_external_ip(rtp.external_ip.clone())
                .with_bind_ip(rtp.bind_ip.clone())
                .with_rtp_start_port(rtp.start_port)
                .with_rtp_end_port(rtp.end_port)
                .with_webrtc_start_port(rtp.webrtc_start_port)
                .with_webrtc_end_port(rtp.webrtc_end_port)
                .with_ice_servers(rtp.ice_servers.clone())
                .with_enable_latching(self.inner.server.proxy_config.load().enable_latching)
                .with_probation_max_packets(
                    self.inner
                        .server
                        .proxy_config
                        .load()
                        .latching_probation_max_packets,
                )
                .with_comfort_noise(rtp.comfort_noise, rtp.comfort_noise_level_db)
                .with_ice_lite(rtp.ice_lite || caller.ice_lite)
                .with_relay_ready_timeout_secs(rtp.relay_ready_timeout_secs)
                .with_stall_detect_secs(rtp.stall_detect_secs)
                .with_media_trace_interval_secs(rtp.media_trace_interval_secs)
                .with_quality_stats(rtp.quality_stats.clone())
                .with_volume_stats(rtp.volume_stats.clone())
                .with_inband_dtmf(rtp.inband_dtmf)
        };

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
                ))
                .with_code(&crate::proxy::error_catalog::EXTERNAL_REALM_BOTH));
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

        let mut forced_forward: Option<InviteOption> = None;
        let mut forced_pending_queue: Option<crate::call::QueuePlan> = None;
        let mut forced_pending_app: Option<(String, Option<serde_json::Value>, bool)> = None;
        let mut forced_route_point: Option<String> = None;

        if let Some(config) = callee_forwarding.as_ref()
            && matches!(config.mode, crate::call::CallForwardingMode::Always)
        {
            match &config.endpoint {
                crate::call::TransferEndpoint::Uri(uri) => {
                    let realm = self.inner.server.proxy_config.load().select_realm("");
                    let normalized = crate::call::build_sip_uri(uri, &realm);
                    let forwarded_uri = rsipstack::sip::Uri::try_from(normalized.as_str())
                        .map_err(|e| {
                            RouteError::from((
                                anyhow!("invalid always-forwarding target '{}': {}", uri, e),
                                Some(rsipstack::sip::StatusCode::ServerInternalError),
                            ))
                            .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_URI_INVALID)
                        })?;
                    forced_forward = Some(InviteOption {
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
                        ))
                        .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_QUEUE_EMPTY));
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
                        .await
                        .map_err(|e| {
                            RouteError::from((
                                anyhow!(
                                    "failed to resolve always-forwarding queue '{}': {}",
                                    reference,
                                    e
                                ),
                                Some(rsipstack::sip::StatusCode::ServerInternalError),
                            ))
                            .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_QUEUE_RESOLVE)
                        })?
                        .ok_or_else(|| {
                            RouteError::from((
                                anyhow!("always-forwarding queue '{}' not found", reference),
                                Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                            ))
                            .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_QUEUE_NOT_FOUND)
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
                        .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_QUEUE_BUILD)
                    })?;
                    if queue_plan.label.is_none() {
                        queue_plan.label = Some(reference.to_string());
                    }
                    forced_pending_queue = Some(queue_plan);
                }
                crate::call::TransferEndpoint::RoutePoint(route_point) => {
                    forced_route_point = Some(route_point.clone());
                }
                crate::call::TransferEndpoint::Ivr(ivr_name) => {
                    let name = ivr_name.trim();
                    if name.is_empty() {
                        return Err(RouteError::from((
                            anyhow!("always-forwarding IVR name is empty"),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        ))
                        .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_IVR_EMPTY));
                    }
                    let ivr_file = self.inner.server.data_context.resolve_ivr_file(name).await;
                    forced_pending_app = Some((
                        "ivr".to_string(),
                        Some(serde_json::json!({ "file": ivr_file })),
                        true,
                    ));
                }
                crate::call::TransferEndpoint::Voicemail(ext) => {
                    let extension = ext.trim();
                    if extension.is_empty() {
                        return Err(RouteError::from((
                            anyhow!("always-forwarding voicemail extension is empty"),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        ))
                        .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_VOICEMAIL_EMPTY));
                    }
                    forced_pending_app = Some((
                        "voicemail".to_string(),
                        Some(serde_json::json!({ "extension": extension })),
                        true,
                    ));
                }
                crate::call::TransferEndpoint::Conference(id) => {
                    let conf_id = id.trim();
                    if conf_id.is_empty() {
                        return Err(RouteError::from((
                            anyhow!("always-forwarding conference id is empty"),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        ))
                        .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_CONFERENCE_EMPTY));
                    }
                    forced_pending_app = Some((
                        "conference".to_string(),
                        Some(serde_json::json!({ "id": conf_id })),
                        true,
                    ));
                }
                crate::call::TransferEndpoint::Realtime(preset) => {
                    let preset_name = preset.trim();
                    if preset_name.is_empty() {
                        return Err(RouteError::from((
                            anyhow!("always-forwarding realtime preset name is empty"),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        ))
                        .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_REALTIME_EMPTY));
                    }
                    forced_pending_app = Some((
                        "realtime".to_string(),
                        Some(serde_json::json!({ "preset": preset_name })),
                        true,
                    ));
                }
            }
        }

        if callee_is_same_realm && !always_forwarding {
            let call_id = original
                .call_id_header()
                .map(|h| h.value().to_string())
                .unwrap_or_default();
            match self.inner.server.locator.lookup(&callee_uri).await {
                Ok(mut results) => {
                    // A hop addressed to a registration's home node is terminal:
                    // do not fork shared contacts back to another cluster node.
                    //
                    // But an empty local set must NOT be treated as offline: the
                    // callee may be homed on a peer node (client dials its own
                    // registrar while the callee registered on a sibling). In
                    // that case keep the peer-owned locations so the hop can be
                    // forwarded cross-node instead of 480 "target user offline".
                    if let Some(home) = self.inner.server.cluster_self_addr.as_ref()
                        && callee_uri.host_with_port == home.addr
                    {
                        let local: Vec<_> = results
                            .iter()
                            .filter(|location| {
                                location
                                    .home_proxy
                                    .as_ref()
                                    .is_none_or(|owner| owner.addr == home.addr)
                            })
                            .cloned()
                            .collect();
                        if !local.is_empty() {
                            results = local;
                        }
                    }
                    internal_lookup_empty = results.is_empty();
                    if internal_lookup_empty {
                        // NB: an empty locator result is NOT a failure yet — a
                        // route/dialplan inspector may still handle the call.
                        // The terminal offline decision emits
                        // `proxy.callee_offline` (see CalleeOfflineMarker).
                        locs.clear();
                        warn!(
                            callee_uri = %callee_uri,
                            callee_realm = %callee_realm,
                            caller_realm = ?caller.realm,
                            "locator lookup returned empty results for same-realm callee"
                        );
                    } else {
                        // Keep locator-provided target metadata
                        // (destination/home_proxy/path/etc.) so SipSession can
                        // route cross-node calls via remote home_proxy.
                        locs = results;
                    }
                }
                Err(e) => {
                    crate::call_errors::emit_call_error(
                        self.inner.server.rwi_gateway.as_ref(),
                        &call_id,
                        "locator",
                        &crate::proxy::locator_error_catalog::LOOKUP_FAILED,
                        Some(serde_json::json!({
                            "callee": callee_uri.to_string(),
                            "error": e.to_string(),
                        })),
                    );
                }
            }
        }

        let caller_uri = match caller.from.as_ref() {
            Some(uri) => uri.clone(),
            None => original
                .from_header()
                .map_err(|e| {
                    RouteError::from((anyhow::anyhow!(e), None))
                        .with_code(&crate::proxy::error_catalog::FROM_HEADER_PARSE)
                })?
                .uri()
                .map_err(|e| {
                    RouteError::from((anyhow::anyhow!(e), None))
                        .with_code(&crate::proxy::error_catalog::FROM_HEADER_PARSE)
                })?,
        };

        let mut route_option = InviteOption {
            callee: callee_uri.clone(),
            caller: caller_uri.clone(),
            contact: caller_uri.clone(),
            ..Default::default()
        };
        let mut route_origin = None;
        if let Some(route_point) = forced_route_point.as_deref() {
            let target = crate::call::build_sip_uri(route_point, &callee_realm);
            let target_uri = rsipstack::sip::Uri::try_from(target.as_str()).map_err(|e| {
                RouteError::from((
                    anyhow!(
                        "invalid always-forwarding route point '{}': {}",
                        route_point,
                        e
                    ),
                    Some(rsipstack::sip::StatusCode::ServerInternalError),
                ))
                .with_code(&crate::proxy::error_catalog::ALWAYS_FWD_URI_INVALID)
            })?;
            route_option.callee = target_uri.clone();

            // Route points are new routing identities, so every matcher view must see the target.
            let mut synthetic_origin = original.clone();
            synthetic_origin.uri = target_uri.clone();
            synthetic_origin
                .headers
                .retain(|header| !matches!(header, rsipstack::sip::Header::To(_)));
            synthetic_origin.headers.push(
                rsipstack::sip::typed::To {
                    display_name: None,
                    uri: target_uri,
                    params: vec![],
                }
                .into(),
            );
            route_origin = Some(synthetic_origin);
        }

        let mut routed_headers: Option<Vec<rsipstack::sip::Header>> = None;
        let (routed_targets, pending_queue, pending_app, dialplan_hints) =
            if always_forwarding && forced_route_point.is_none() {
                (
                    forced_forward.map(|option| vec![crate::config::RouteTarget::new(option, None)]),
                    forced_pending_queue,
                    forced_pending_app,
                    None,
                )
            } else {
                let mut route_outcome = route_invite
                    .route_invite(
                        route_option,
                        route_origin.as_ref().unwrap_or(original),
                        &direction,
                        cookie,
                    )
                    .await
                    .map_err(|e| {
                        RouteError::from((
                            anyhow::anyhow!(e),
                            Some(rsipstack::sip::StatusCode::ServerInternalError),
                        ))
                        .with_code(&crate::proxy::error_catalog::ROUTE_RESOLUTION_ERROR)
                    })?;

                if forced_route_point.is_some()
                    && !matches!(
                        &route_outcome,
                        RouteResult::Application { .. } | RouteResult::Abort(_, _)
                    )
                {
                    // Rejecting a non-application result must release policy slots acquired by routing.
                    // Every routed target may hold slots, not only the first.
                    let hints: Vec<_> = match &mut route_outcome {
                        RouteResult::Forward(targets) => {
                            targets.iter_mut().filter_map(|target| target.hints.as_mut()).collect()
                        }
                        RouteResult::Queue { hints, .. }
                        | RouteResult::NotHandled(_, hints) => hints.as_mut().into_iter().collect(),
                        _ => Vec::new(),
                    };
                    for hints in hints {
                        if let Some(guard) = self.inner.routing_state.policy_guard.as_ref() {
                            crate::call::policy::PolicyGuard::release_concurrency_holds(
                                &hints.concurrency_holds,
                                guard.limiter().as_ref(),
                            )
                            .await;
                        }
                        hints.concurrency_holds.clear();
                    }
                    return Err(RouteError::from((
                        anyhow!("always-forwarding route point did not resolve to an application"),
                        Some(rsipstack::sip::StatusCode::ServerInternalError),
                    ))
                    .with_code(&crate::proxy::error_catalog::ROUTE_RESOLUTION_ERROR));
                }

                match route_outcome {
                    RouteResult::Forward(mut targets) => {
                        // A single target's hints apply to the whole dialplan, as a plain
                        // forward; several targets keep their hints scoped to each attempt,
                        // except the call-wide recording, taken from the first.
                        let hints = match targets.as_mut_slice() {
                            [single] => single.hints.take(),
                            [first, ..] => first.hints.as_ref().map(|hints| crate::config::DialplanHints {
                                recording: hints.recording.clone(),
                                enable_recording: hints.enable_recording,
                                ..Default::default()
                            }),
                            [] => None,
                        };
                        (Some(targets), None, None, hints)
                    }
                    RouteResult::Queue { queue, hints, .. } => (None, Some(queue), None, hints),
                    RouteResult::NotHandled(_, hints) => (None, None, None, hints),
                    RouteResult::Abort(code, reason) => {
                        let err =
                            anyhow::anyhow!(reason.unwrap_or_else(|| "route aborted".to_string()));
                        return Err(RouteError::from((err, Some(code)))
                            .with_code(&crate::proxy::error_catalog::ROUTE_ABORTED));
                    }
                    RouteResult::Application {
                        option,
                        app_name,
                        app_params,
                        auto_answer,
                        hints,
                    } => {
                        routed_headers = option.headers;
                        (None, None, Some((app_name, app_params, auto_answer)), hints)
                    }
                }
            };

        let queue_targets = pending_queue
            .as_ref()
            .and_then(|plan| plan.dial_strategy.clone());
        let routed = routed_targets.is_some();
        let routed_caller = routed_targets
            .as_ref()
            .and_then(|targets| targets.first())
            .map(|target| target.option.caller.clone());
        let targets = if pending_app.is_some() {
            DialStrategy::Sequential(vec![])
        } else if let Some(queue_targets) = queue_targets {
            queue_targets
        } else if let Some(mut routed_targets) = routed_targets {
            if routed_targets.len() == 1 {
                let option = routed_targets.pop().expect("one routed target").option;
                let target = Location {
                    aor: option.callee.clone(),
                    destination: option.destination.clone(),
                    credential: option.credential.clone(),
                    headers: option.headers.clone(),
                    contact_raw: Some(option.callee.to_string()),
                    ..Default::default()
                };
                // A dest-less forward route that did not rewrite the callee (e.g.
                // a per-extension route carrying only `[busy_wait]`) targets a
                // same-realm extension. Prefer the locator's registered contacts
                // over the bare AOR so the INVITE goes straight to the endpoint
                // instead of looping back through the proxy.
                let unchanged_callee = option.callee == callee_uri;
                if option.destination.is_none()
                    && unchanged_callee
                    && callee_is_same_realm
                    && !locs.is_empty()
                {
                    resolve_unhandled_targets(
                        callee_is_same_realm,
                        false,
                        locs,
                        self.inner.server.proxy_config.load().parallel_fork,
                    )?
                } else {
                    DialStrategy::Sequential(vec![target.into()])
                }
            } else {
                // Several targets are tried in order with their own caller and hints;
                // an empty list has no destination and dials nothing.
                DialStrategy::Sequential(routed_targets.into_iter().map(Into::into).collect())
            }
        } else {
            resolve_unhandled_targets(
                callee_is_same_realm,
                internal_lookup_empty,
                locs,
                self.inner.server.proxy_config.load().parallel_fork,
            )?
        };
        let recording = self
            .inner
            .server
            .proxy_config
            .load()
            .recording
            .as_ref()
            .map(|r| r.new_recording_config())
            .unwrap_or_default();

        // Global session id: use the Call-ID when id-shaped (unless
        // `session_id_inherit_callid` is disabled), else a fresh id. Plain
        // P2P extension dialling keeps the raw Call-ID — no Session-ID is
        // generated or injected for those calls. Cross-node/cross-entity
        // root inheritance happens later in the session constructor via the
        // Session-ID header (UUI fallback), so this leg id stays unique per
        // node (cluster session registry keys on it).
        let session_id_enabled = !(direction == DialDirection::Internal
            && pending_app.is_none()
            && pending_queue.is_none()
            && !routed);
        let session_id = Self::resolve_inbound_session_id(
            &self.inner.server,
            cookie,
            original,
            &dialog_id,
            session_id_enabled,
        );
        let mut dialplan = Dialplan::new(session_id.clone(), original.clone(), direction)
            .with_session_id_enabled(session_id_enabled)
            .with_caller(routed_caller.unwrap_or(caller_uri))
            .with_media(media_config)
            .with_recording(recording)
            .with_route_invite(route_invite)
            .with_diversion_chain_from_request(original)
            .with_passthrough_failure(self.inner.server.proxy_config.load().passthrough_failure);

        // Resolve the original-header passthrough rule for the (direct) callee
        // target(s) before moving `targets` into the dialplan. Internal
        // destinations (same realm / registered / home proxy) always
        // passthrough custom headers; external destinations use the destination
        // trunk's `header_passthrough` config (a trunk without one forwards no
        // custom headers). App/queue flows resolve their targets later inside
        // the session, so those legs rely on the per-target fallback in
        // `build_target_invite_option`.
        let header_passthrough = {
            let locs = targets.locations();
            let routed_trunk = dialplan_hints.as_ref().and_then(|h| {
                h.extensions
                    .get::<crate::call::OutboundTrunkContext>()
                    .map(|ctx| ctx.name.clone())
            });
            // A call routed to a trunk is external even when the dialed
            // number is in our own realm: the trunk's rule applies.
            let internal = routed_trunk.is_none()
                && (callee_is_same_realm
                    || locs
                        .iter()
                        .any(|t| t.registered_aor.is_some() || t.home_proxy.is_some()));
            if internal {
                Some(crate::proxy::routing::HeaderPassthrough::all())
            } else if let Some(target) = locs.first() {
                self.inner
                    .server
                    .header_passthrough_for(
                        target,
                        false,
                        &original.uri,
                        routed_trunk.as_deref(),
                    )
                    .await
            } else {
                None
            }
        };
        dialplan.header_passthrough = header_passthrough;

        if let Some((app_name, app_params, auto_answer)) = pending_app {
            dialplan = dialplan.with_application(app_name, app_params, auto_answer);
            dialplan.routed_headers = routed_headers;
        } else if let Some(queue) = pending_queue {
            dialplan = dialplan.with_queue(queue);
        } else {
            dialplan = dialplan.with_targets(targets);
        }

        let trunk_codecs = cookie
            .get_extension::<TrunkContext>()
            .and_then(|ctx| {
                self.inner
                    .server
                    .data_context
                    .get_trunk(&ctx.name)
                    .map(|trunk| trunk.codec)
            })
            .filter(|codecs| !codecs.is_empty());
        let fallback_codecs = {
            let cfg = self.inner.server.proxy_config.load();
            trunk_codecs
                .as_ref()
                .cloned()
                .or_else(|| cfg.audio_codecs.clone())
        };

        // Failure-tone audio profile: the global `[proxy.audio_profile]`
        // profile (or built-in tones when the table is absent) is the base for
        // every call. An explicitly declared empty table disables the built-in
        // tones. A per-trunk `ringback` (carried in `hints`) overrides fields.
        let mut audio_profile = self
            .inner
            .server
            .proxy_config
            .load()
            .audio_profile
            .clone()
            .unwrap_or_else(crate::proxy::routing::RingbackAudio::builtin_defaults);

        if let Some(mut hints) = dialplan_hints {
            let mut recording_policy = hints.recording.take();
            if let Some(enabled) = hints.enable_recording {
                recording_policy
                    .get_or_insert_with(RecordingPolicy::default)
                    .enabled = Some(enabled);
            }
            if let Some(policy) = recording_policy {
                dialplan.recording = policy.new_recording_config();
                dialplan.recording_policy = Some(policy);
            }
            if let Some(bypass) = hints.bypass_media
                && bypass
            {
                dialplan.media.proxy_mode = crate::config::MediaProxyMode::Bypass;
            }
            if let Some(media_mode) = hints.media_mode {
                dialplan.media.proxy_mode = media_mode;
            }
            if let Some(video_policy) = hints.video_policy {
                dialplan.media.video_policy = Some(video_policy);
            }
            if let Some(profile_id) = hints.network_profile_id.as_deref() {
                if let Some(profile) = self
                    .inner
                    .server
                    .network_profile(profile_id)
                    .or_else(|| Some(self.inner.server.default_network_profile()))
                {
                    self.inner
                        .server
                        .apply_network_profile_to_media(&profile, &mut dialplan.media);
                }
            }
            // Per-trunk external_ip / bind_ip overrides the profile/global value.
            if let Some(external_ip) = hints.external_ip.take() {
                dialplan.media.external_ip = Some(external_ip);
            }
            if let Some(bind_ip) = hints.bind_ip.take() {
                dialplan.media.bind_ip = Some(bind_ip);
            }
            // Per-trunk ice_lite: Some(true)/Some(false) override both the
            // global default and the per-extension `;+sip.ice` detection
            // (applied earlier, before this hints block).
            if let Some(ice_lite) = hints.ice_lite.take() {
                dialplan.media.ice_lite = ice_lite;
            }
            if let Some(max_duration) = hints.max_duration {
                dialplan.max_call_duration = Some(max_duration);
            }
            if hints.max_ring_time.is_some() {
                dialplan.max_ring_time = hints.max_ring_time;
            }
            if let Some(enable_sipflow) = hints.enable_sipflow {
                dialplan.enable_sipflow = enable_sipflow;
            }
            if hints.disable_ice_servers == Some(true) {
                dialplan.media.ice_servers = None;
            }
            apply_allowed_codecs(
                &mut dialplan,
                hints.allow_codecs.as_deref(),
                fallback_codecs.as_deref(),
            );
            if let Some(ringback) = hints.ringback.take() {
                audio_profile.merge_from(ringback);
            }
            if let Some(busy_wait) = hints.busy_wait.take() {
                dialplan.busy_wait = Some(busy_wait.to_plan());
            }
            dialplan.diversion_policy = hints.diversion_policy;
            dialplan.amd = hints.amd.take();
            dialplan.extensions = std::mem::take(&mut hints.extensions);
            *dialplan.concurrency_holds.lock() = std::mem::take(&mut hints.concurrency_holds);
            dialplan.concurrent_call_lease = hints.concurrent_call_lease;
        } else {
            apply_allowed_codecs(&mut dialplan, None, fallback_codecs.as_deref());
        }

        dialplan.audio_profile = Some(audio_profile);

        if callee_is_same_realm && internal_lookup_empty {
            dialplan.extensions.insert(CalleeOfflineMarker);
        }

        Ok(dialplan)
    }

    pub(crate) fn apply_recording_policy(&self, mut dialplan: Dialplan, caller: &SipUser) -> Dialplan {
        let policy = match dialplan.recording_policy.as_ref() {
            Some(overrides) => {
                let mut merged = self
                    .inner
                    .server
                    .proxy_config
                    .load()
                    .recording
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| overrides.clone());

                if overrides.enabled.is_some() {
                    merged.enabled = overrides.enabled;
                }
                if overrides.recording_type.is_some() {
                    merged.recording_type = overrides.recording_type;
                }
                if !overrides.directions.is_empty() {
                    merged.directions = overrides.directions.clone();
                }
                if !overrides.caller_allow.is_empty() {
                    merged.caller_allow = overrides.caller_allow.clone();
                }
                if !overrides.caller_deny.is_empty() {
                    merged.caller_deny = overrides.caller_deny.clone();
                }
                if !overrides.callee_allow.is_empty() {
                    merged.callee_allow = overrides.callee_allow.clone();
                }
                if !overrides.callee_deny.is_empty() {
                    merged.callee_deny = overrides.callee_deny.clone();
                }
                if overrides.auto_start.is_some() {
                    merged.auto_start = overrides.auto_start;
                }
                if overrides.auto_start_at.is_some() {
                    merged.auto_start_at = overrides.auto_start_at;
                }
                if !overrides.auto_start_except.is_empty() {
                    merged.auto_start_except = overrides.auto_start_except.clone();
                }
                if overrides.filename_pattern.is_some() {
                    merged.filename_pattern = overrides.filename_pattern.clone();
                }
                if overrides.samplerate.is_some() {
                    merged.samplerate = overrides.samplerate;
                }
                if overrides.ptime.is_some() {
                    merged.ptime = overrides.ptime;
                }
                if overrides.stereo_swap.is_some() {
                    merged.stereo_swap = overrides.stereo_swap;
                }
                if overrides.stereo_mode.is_some() {
                    merged.stereo_mode = overrides.stereo_mode;
                }
                if overrides.path.is_some() {
                    merged.path = overrides.path.clone();
                }
                if overrides.url.is_some() {
                    merged.url = overrides.url.clone();
                }
                if overrides.headers.is_some() {
                    merged.headers = overrides.headers.clone();
                }
                if overrides.vendor.is_some() {
                    merged.vendor = overrides.vendor.clone();
                }
                if overrides.bucket.is_some() {
                    merged.bucket = overrides.bucket.clone();
                }
                if overrides.region.is_some() {
                    merged.region = overrides.region.clone();
                }
                if overrides.access_key.is_some() {
                    merged.access_key = overrides.access_key.clone();
                }
                if overrides.secret_key.is_some() {
                    merged.secret_key = overrides.secret_key.clone();
                }
                if overrides.endpoint.is_some() {
                    merged.endpoint = overrides.endpoint.clone();
                }
                if overrides.root.is_some() {
                    merged.root = overrides.root.clone();
                }
                if overrides.recording_type.is_some() {
                    merged.recording_type = overrides.recording_type;
                }
                // force_file is deprecated; still merged so
                // effective_recording_type() can apply the migration hint.
                if overrides.force_file.is_some() {
                    merged.force_file = overrides.force_file;
                }

                merged
            }
            None => match self.inner.server.recording_policy.load().as_ref() {
                Some(policy) => policy.clone(),
                None => return dialplan,
            },
        };
        if !policy.enabled.unwrap_or(false) {
            return dialplan;
        }

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

        // Media destination is selected by `recording.type`:
        //   local|http|s3 → WAV file (+ [recording] upload)
        //   sipflow       → SipflowRecorder (+ [sipflow.upload].media)
        // SIP signalling always goes to [sipflow] when a backend is present.
        let recording_type = policy.effective_recording_type();
        let has_sipflow_backend = dialplan.enable_sipflow
            && self
                .inner
                .server
                .sip_flow
                .as_ref()
                .and_then(|sf| sf.backend())
                .is_some();

        if recording_type.is_file_media() {
            let recorder_option = match self.build_recorder_option(
                &dialplan,
                &policy,
                &caller_identity,
                &callee_identity,
            ) {
                Some(option) => option,
                None => return dialplan,
            };

            if let Some(existing) = dialplan.recording.option.as_mut() {
                if existing.recorder_file.is_empty() {
                    existing.recorder_file = recorder_option.recorder_file.clone();
                }
                existing.samplerate = existing.samplerate.or(recorder_option.samplerate);
                existing.ptime = existing.ptime.or(recorder_option.ptime);
                existing.stereo_swap = existing.stereo_swap.or(recorder_option.stereo_swap);
                existing.stereo_mode = existing.stereo_mode.or(recorder_option.stereo_mode);
            } else {
                dialplan.recording.option = Some(recorder_option);
            }
        } else if !has_sipflow_backend {
            warn!(
                session_id = dialplan.session_id.as_deref(),
                "recording.type=sipflow but no sipflow backend; media will not be captured"
            );
        }

        debug!(
            session_id = dialplan.session_id.as_deref(),
            caller = %caller_identity,
            callee = %callee_identity,
            ?recording_type,
            has_sipflow_backend,
            "recording policy enabled for dialplan"
        );

        dialplan.recording.enabled = true;
        dialplan.recording.auto_start = policy.auto_start.unwrap_or(true);
        dialplan.recording.auto_start_at = policy.auto_start_at.unwrap_or_default();
        dialplan.recording.recording_type = recording_type;
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

        let option = RecorderOption {
            recorder_file: path.to_string_lossy().into_owned(),
            samplerate: policy.samplerate,
            ptime: policy.ptime,
            stereo_swap: policy.stereo_swap,
            stereo_mode: policy.stereo_mode,
        };
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

    async fn reply_route_error(
        &self,
        tx: &mut Transaction,
        cookie: &TransactionCookie,
        route_err: RouteError,
    ) -> Result<()> {
        if cookie.is_spam() {
            return Ok(());
        }
        let reason_value = route_error_reason(&route_err);
        let code = route_err
            .status
            .unwrap_or(rsipstack::sip::StatusCode::ServerInternalError);
        let reason_text = route_err.error.to_string();
        self.report_failure(
            tx,
            cookie,
            code.clone(),
            Some(reason_text),
            route_err.extensions,
        );
        tx.reply_with(
            code,
            vec![rsipstack::sip::Header::Other("Reason".into(), reason_value)],
            None,
        )
        .await
        .map_err(|e| anyhow!("Failed to send reply: {}", e))?;
        Err(route_err.error)
    }

    fn extract_max_forwards(tx: &Transaction) -> u32 {
        tx.original
            .max_forwards_header()
            .ok()
            .and_then(|h| h.value().parse::<u32>().ok())
            .unwrap_or(70)
    }

    /// Build a session cancel token.
    ///
    /// For reliable transports (TCP/TLS/WS/WSS) the token is derived from the
    /// transport's cancel token, so a connection drop automatically cancels
    /// every session on that transport — no watcher task needed.
    ///
    /// For UDP (connectionless, shared socket) the token is derived from the
    /// server's cancel token, matching pre-`bac8444` behaviour.
    fn build_session_token(&self, tx: &Transaction) -> CancellationToken {
        if let Some(ref conn) = tx.connection {
            if conn.is_reliable() {
                if let Some(transport_token) = conn.cancel_token() {
                    return transport_token.child_token();
                }
            }
        }
        self.inner.server.cancel_token.child_token()
    }

    async fn build_and_serve_dialplan(
        &self,
        tx: &mut Transaction,
        cookie: TransactionCookie,
        dialplan: Dialplan,
    ) -> Result<()> {
        let max_forwards = Self::extract_max_forwards(tx);
        if max_forwards == 0 {
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
        let builder = CallSessionBuilder::new(cookie, dialplan, max_forwards - 1)
            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
            .with_cancel_token(self.build_session_token(tx));
        builder.build_and_serve(self.inner.server.clone(), tx).await
    }

    pub(crate) async fn build_dialplan(
        &self,
        tx: &mut Transaction,
        cookie: TransactionCookie,
        caller: &SipUser,
    ) -> Result<Dialplan, RouteError> {
        let route_invite: Box<dyn RouteInvite> =
            compose_route_invites(&self.inner.server, self.inner.routing_state.clone()).map_err(
                |e| {
                    RouteError {
                        error: e,
                        status: None,
                        extensions: None,
                    }
                    .with_code(&crate::proxy::error_catalog::CREATE_ROUTE_INVITE_FAILED)
                },
            )?;

        let dialplan = if let Some(resolver) = self.inner.server.call_router.as_ref() {
            resolver
                .resolve(&tx.original, route_invite, caller, &cookie)
                .await
        } else {
            self.default_resolve(&tx.original, route_invite, caller, &cookie)
                .await
        }?;

        let mut dialplan = dialplan;

        let inspectors: Vec<Arc<crate::proxy::routing::inspector_stack::OrderedDialplanInspector>> =
            self.inner
                .server
                .dialplan_inspectors
                .read()
                .iter()
                .cloned()
                .collect();
        for entry in inspectors {
            if !entry.enabled {
                continue;
            }
            if matches!(
                entry.eval_mode,
                crate::proxy::routing::stack::EvalMode::PostRoute
            ) && !dialplan.is_empty()
            {
                continue;
            }
            match entry
                .inspector
                .inspect_dialplan(dialplan, &cookie, &tx.original)
                .await
            {
                DialplanVerdict::Continue(dp) => dialplan = dp,
                DialplanVerdict::Final(dp) => {
                    dialplan = dp;
                    break;
                }
                DialplanVerdict::Reject(err) => return Err(err),
            }
        }

        // Standard step for all dialplans: resolve target destinations through
        // the locator for same-realm targets.  After the router has set target
        // AoRs and inspectors have rewritten them, each target's AoR is resolved
        // to the user's actual registered contact.  Without this step, targets
        // with bare AoRs (e.g. from HttpCallRouter or JSON-RPC inspector) would
        // cause INVITEs to hairpin back to the PBX's own IP.
        if let DialplanFlow::Targets(ref mut strategy) = dialplan.flow {
            let targets: Vec<&mut Location> = match strategy {
                DialStrategy::Sequential(attempts) => attempts.iter_mut().map(|attempt| &mut attempt.location).collect(),
                DialStrategy::Parallel(locations) => locations.iter_mut().collect(),
            };
            for target in targets {
                if target.destination.is_none() {
                    let realm = target.aor.host().to_string();
                    if self.inner.server.is_same_realm(&realm).await {
                        if let Ok(locs) = self.inner.server.locator.lookup(&target.aor).await {
                            if let Some(loc) = locs.first() {
                                if let Some(dest) = loc.destination.clone() {
                                    debug!(
                                        aor = %target.aor,
                                        destination = %dest,
                                        webrtc = loc.supports_webrtc,
                                        "resolved target destination via locator"
                                    );
                                    target.destination = Some(dest);
                                    target.supports_webrtc = loc.supports_webrtc;
                                    if target.transport.is_none() {
                                        target.transport = loc.transport;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let callee = match self.resolve_callee_user(&tx.original).await {
            Ok(callee) => callee,
            Err(err) => {
                error!(error = %err, "failed to resolve callee user for forwarding/voicemail");
                None
            }
        };
        if let Some(callee) = callee.as_ref()
            && dialplan.call_forwarding.is_none()
            && let Some(config) = callee.forwarding_config()
        {
            dialplan = dialplan.with_call_forwarding(Some(config));
        }
        // An unconditional (always) forward is a diversion hop: record it so
        // the outgoing legs carry the Diversion / History-Info chain (RFC
        // 5806 / RFC 7044) when enabled. The diverting party is the originally
        // dialed target.
        if dialplan
            .call_forwarding
            .as_ref()
            .is_some_and(|config| matches!(config.mode, crate::call::CallForwardingMode::Always))
            && !dialplan
                .diversion_chain
                .iter()
                .any(|e| e.reason == Some(crate::call::diversion::DiversionReason::Unconditional))
        {
            dialplan.push_diversion(
                dialplan.original.uri.to_string(),
                Some(crate::call::diversion::DiversionReason::Unconditional),
            );
        }
        dialplan.voicemail_enabled = callee.as_ref().is_some_and(|callee| !callee.voicemail_disabled);

        // After dialplan inspectors have had a chance to fill in routes,
        // if the callee was deemed offline at resolution time and no
        // inspector provided targets, go to its voicemail or reject with 480.
        if dialplan.extensions.get::<CalleeOfflineMarker>().is_some() && dialplan.is_empty() {
            let Some(callee) = callee.filter(|_| dialplan.voicemail_enabled) else {
                return Err(RouteError::from((
                    anyhow!("target user is offline"),
                    Some(rsipstack::sip::StatusCode::TemporarilyUnavailable),
                ))
                .with_code(&crate::proxy::error_catalog::CALLEE_OFFLINE));
            };
            info!(extension = %callee.username, "callee offline, routing to voicemail");
            dialplan = dialplan.with_application(
                "voicemail".to_string(),
                Some(serde_json::json!({ "extension": callee.username })),
                true,
            );
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

        // A-leg SIP peer (bare ip:port) for the CDR `callerPeer` field: prefer
        // the inbound INVITE's transport connection, fall back to the top-Via
        // address when the connection is unavailable (may be a NAT address).
        // Rides the cookie so early-failure CDRs (no SipSession yet) keep it.
        // NB: `SipAddr::to_string()` prefixes the transport ("UDP ip:port");
        // the bare `addr` (HostWithPort) matches the signaling table's peer
        // format and the plugin's anchored LIKE matching.
        let caller_peer = tx
            .connection
            .as_ref()
            .and_then(|conn| conn.get_remote_addr())
            .map(|addr| addr.addr.to_string())
            .or_else(|| {
                crate::proxy::routing::extract_via_ip(&tx.original).map(|ip| ip.to_string())
            });
        if let Some(peer) = caller_peer {
            cookie.insert_extension(crate::call::CallerPeerContext(peer));
        }

        // Immediately acknowledge the INVITE with 100 Trying BEFORE any routing work.
        // Routing (esp. wholesale route_wholesale) may block on CPS locks, DB lookups or
        // semaphores; without an early 100 the upstream retransmits (Timer A: 500ms..16s)
        // and may give up, inflating observed 408s under load.
        tx.send_trying().await.ok();

        if self.handle_conference_invite(tx, cookie.clone()).await? { return Ok(()); }

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

            // The ownership registry uses the replaced dialog's bare Call-ID.
            let old_handle = registry.get_handle_by_dialog(&replaces_call_id);

            if let Some(ref old_handle) = old_handle {
                let old_session_id = old_handle.session_id().to_string();
                let old_handle_clone = old_handle.clone();
                info!(%old_session_id, "Found session to replace via Replaces header");

                // Check if the old session is in a conference
                let old_leg = crate::call::domain::LegId::new(&old_session_id);
                let conf_id = conference_manager.get_conference_id_for_leg(&old_leg).await;

                if conf_id.is_some() {
                    info!(%old_session_id, "Replaces target is in a conference; proceeding with seat replacement");
                    let dialplan = self.build_dialplan(tx, cookie.clone(), &caller).await;
                    let dialplan = match dialplan {
                        Ok(d) => d,
                        Err(route_err) => {
                            return self.reply_route_error(tx, &cookie, route_err).await;
                        }
                    };

                    let server = self.inner.server.clone();
                    let max_forwards = Self::extract_max_forwards(tx);
                    if max_forwards == 0 {
                        tx.reply(rsipstack::sip::StatusCode::TooManyHops).await?;
                        return Ok(());
                    }
                    let builder =
                        CallSessionBuilder::new(cookie.clone(), dialplan, max_forwards - 1)
                            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
                            .with_cancel_token(self.build_session_token(tx));
                    let result = builder.build_and_serve(server.clone(), tx).await;

                    // Spawn background task to perform seat replacement once new call answers
                    let new_session_id = tx
                        .original
                        .call_id_header()
                        .map(|h| h.value().to_string())
                        .unwrap_or_default();
                    if !new_session_id.is_empty() {
                        let reg = registry.clone();
                        let sid = new_session_id.clone();
                        crate::utils::spawn(async move {
                            if reg
                                .wait_for_status(
                                    &sid,
                                    ActiveProxyCallStatus::Talking,
                                    std::time::Duration::from_secs(30),
                                )
                                .await
                            {
                                info!(session_id = %sid, "New Replaces call answered; executing conference seat replacement");

                                if let Some(ref conf) = conf_id {
                                    let _ =
                                        conference_manager.remove_participant(conf, &old_leg).await;
                                    let new_leg = crate::call::domain::LegId::new(&sid);
                                    let _ = conference_manager.add_participant(conf, new_leg).await;
                                    info!(%old_session_id, %sid, "Conference seat replacement completed for Replaces");
                                }

                                let _ = old_handle_clone
                                    .send_command_async(crate::call::domain::CallCommand::Hangup(
                                        crate::call::domain::HangupCommand::local(
                                            "replaced_by_replaces",
                                            Some(
                                                crate::callrecord::CallRecordHangupReason::BySystem,
                                            ),
                                            Some(200),
                                        ),
                                    ))
                                    .await;

                                if let Some(ref gw) = server.rwi_gateway {
                                    let g = gw.read();
                                    g.broadcast(&crate::rwi::ConferenceSeatReplaceSucceeded {
                                        conf_id: conf_id.map(|c| c.0).unwrap_or_default(),
                                        old_call_id: old_session_id.clone(),
                                        new_call_id: sid.clone(),
                                    });
                                }
                            }
                        });
                    }

                    return result;
                } else {
                    info!(%old_session_id, "Replaces target is not in a conference; creating conference for attended transfer");
                    let dialplan = self.build_dialplan(tx, cookie.clone(), &caller).await;
                    let dialplan = match dialplan {
                        Ok(d) => d,
                        Err(route_err) => {
                            return self.reply_route_error(tx, &cookie, route_err).await;
                        }
                    };

                    let server = self.inner.server.clone();
                    let max_forwards = Self::extract_max_forwards(tx);
                    if max_forwards == 0 {
                        tx.reply(rsipstack::sip::StatusCode::TooManyHops).await?;
                        return Ok(());
                    }
                    let builder =
                        CallSessionBuilder::new(cookie.clone(), dialplan, max_forwards - 1)
                            .with_call_record_sender(self.inner.server.callrecord_sender.clone())
                            .with_cancel_token(self.build_session_token(tx));
                    let result = builder.build_and_serve(server.clone(), tx).await;

                    // Spawn background task to perform conference merge once new call answers
                    let new_session_id = tx
                        .original
                        .call_id_header()
                        .map(|h| h.value().to_string())
                        .unwrap_or_default();
                    if !new_session_id.is_empty() {
                        let reg = registry.clone();
                        let sid = new_session_id.clone();
                        crate::utils::spawn(async move {
                            if reg
                                .wait_for_status(
                                    &sid,
                                    ActiveProxyCallStatus::Talking,
                                    std::time::Duration::from_secs(30),
                                )
                                .await
                            {
                                info!(session_id = %sid, "New Replaces call answered; creating conference for attended transfer");

                                let conf_id = crate::call::runtime::ConferenceId::from(
                                    format!("conf-replaces-{}", sid).as_str(),
                                );
                                let _ = conference_manager
                                    .create_conference(conf_id.clone(), None)
                                    .await;

                                let old_leg = crate::call::domain::LegId::new(&old_session_id);
                                let _ = conference_manager
                                    .add_participant(&conf_id, old_leg.clone())
                                    .await;

                                let new_leg = crate::call::domain::LegId::new(&sid);
                                let _ = conference_manager
                                    .add_participant(&conf_id, new_leg.clone())
                                    .await;

                                info!(%old_session_id, %sid, "Conference created for attended transfer");

                                let _ = old_handle_clone
                                    .send_command_async(crate::call::domain::CallCommand::Hangup(
                                        crate::call::domain::HangupCommand::local(
                                            "replaced_by_replaces",
                                            Some(
                                                crate::callrecord::CallRecordHangupReason::BySystem,
                                            ),
                                            Some(200),
                                        )
                                        .with_cascade(
                                            crate::call::domain::HangupCascade::AllExcept(vec![
                                                crate::call::domain::LegId::from("caller"),
                                            ]),
                                        ),
                                    ))
                                    .await;

                                if let Some(ref gw) = server.rwi_gateway {
                                    let g = gw.read();
                                    g.broadcast(&crate::rwi::CallTransferred {
                                        call_id: old_session_id.clone(),
                                        transfer_target: None,
                                        // Replaces takeover — the effective
                                        // target is opaque to this node.
                                        transfer_target_type: None,
                                        transfer_source: g
                                            .meta_store
                                            .get_sync(&old_session_id)
                                            .and_then(|m| m.transfer_source),
                                    });
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

        let dialplan = self.build_dialplan(tx, cookie.clone(), &caller).await;
        let dialplan = match dialplan {
            Ok(d) => d,
            Err(route_err) => {
                let err_text = route_err.error.to_string();
                // Zhongan business rejections (quota / DND / AXB binding) are
                // expected per-policy outcomes, not system faults — keep them
                // visible at WARN without tripping ERROR alerting.
                if err_text.contains("zhongan_invite;cause=") {
                    warn!(key = %tx.key, error = %route_err.error, status = ?route_err.status, reason = %route_error_reason(&route_err), "failed to build dialplan (business rejection)");
                } else {
                    error!(key = %tx.key, error = %route_err.error, status = ?route_err.status, reason = %route_error_reason(&route_err), "failed to build dialplan");
                }
                return self.reply_route_error(tx, &cookie, route_err).await;
            }
        };

        self.build_and_serve_dialplan(tx, cookie, dialplan).await
    }

    /// Conference factory and room routing precede ordinary extension routing.
    async fn handle_conference_invite(&self, tx: &mut Transaction, cookie: TransactionCookie) -> Result<bool> {
        let Some(factory) = self.inner.server.proxy_config.load().conference_factory_uri.clone() else { return Ok(false) };
        let factory = rsipstack::sip::Uri::try_from(factory.as_str())?;
        let requested = &tx.original.uri;
        let is_factory = requested.user() == factory.user() && requested.host_with_port == factory.host_with_port;
        let existing = self.inner.server.conference_server.list_conferences_detail().await.into_iter()
            .find(|room| room.focus_uri.as_deref() == Some(requested.to_string().as_str()));
        if !is_factory && existing.is_none() {
            if requested.host_with_port == factory.host_with_port && requested.user().is_some_and(|user| user.starts_with("room-")) {
                tx.reply(rsipstack::sip::StatusCode::NotFound).await?;
                return Ok(true);
            }
            return Ok(false);
        }
        if Self::parse_replaces_header(&tx.original).is_some() {
            tx.reply(rsipstack::sip::StatusCode::NotImplemented).await?;
            return Ok(true);
        }
        let session_id = tx.original.call_id_header()?.value().to_string();
        let room = if is_factory {
            let id = crate::call::runtime::ConferenceId::from(format!("room-{}", uuid::Uuid::new_v4()));
            self.inner.server.conference_server.create_conference_ex(
                id.clone(), None, Some(crate::call::domain::LegId::from(format!("{session_id}-caller"))), None,
            ).await?;
            let mut uri = factory.clone();
            uri.auth = Some(rsipstack::sip::Auth { user: id.0.clone(), password: None });
            self.inner.server.conference_server.set_focus(&id, uri.to_string())?;
            self.inner.server.conference_server.get_conference(&id).await.unwrap()
        } else { existing.unwrap() };
        info!(conference_id = %room.id.0, %session_id, factory = is_factory, "Routing SIP conference INVITE");
        let focus = ConferenceFocusContext { uri: room.focus_uri.clone().unwrap(), room: room.id.clone() };
        let mut dialplan = Dialplan::new(session_id, tx.original.clone(), DialDirection::Inbound);
        dialplan.media.proxy_mode = crate::config::MediaProxyMode::All;
        dialplan.flow = DialplanFlow::Application {
            app_name: "conference".into(), app_params: Some(serde_json::json!({"id": room.id.0})), auto_answer: true,
        };
        dialplan.extensions.insert(focus);
        let result = self.build_and_serve_dialplan(tx, cookie, dialplan).await;
        if result.is_err() && is_factory { self.inner.server.conference_server.destroy_conference(&room.id).await.ok(); }
        result?;
        Ok(true)
    }

    /// When an in-dialog BYE/INFO arrives on a node that does not own the
    /// dialog (typical after mid-call WS reconnect to another home_proxy),
    /// forward a textual copy to the owner via AMI. Returns `Some(Ok(()))`
    /// when forward succeeded (and a local 200 was already sent to the UA).
    #[cfg(feature = "commerce")]
    async fn try_forward_indialog_to_owner(
        &self,
        tx: &mut Transaction,
        dialog_id: &DialogId,
    ) -> Option<Result<()>> {
        let peers = self.inner.server.cluster_peers.clone();
        if peers.is_empty() {
            return None;
        }
        let call_id = dialog_id.call_id.clone();
        let owner = self
            .inner
            .server
            .session_registry
            .lookup_owner(&call_id)
            .await?;
        let self_id = self
            .inner
            .server
            .cluster_self_addr
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_default();
        if owner == self_id || owner == "local" {
            return None;
        }

        let ami_path = self
            .inner
            .server
            .proxy_config
            .load()
            .ami_path
            .clone()
            .unwrap_or_else(|| crate::config::DEFAULT_AMI_PATH.to_string());
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        let client = CLIENT.get_or_init(|| {
            crate::http_util::build_keepalive_client(None, None)
                .unwrap_or_else(|_| reqwest::Client::new())
        });

        // Reconstruct a minimal first-line + Call-ID for the owner handler.
        let sip_text = format!(
            "{} {}\r\nCall-ID: {}\r\n\r\n",
            tx.original.method, tx.original.uri, call_id
        );
        let resp = crate::proxy::cluster_forward::dispatch_indialog_sip(
            &self.inner.server.session_registry,
            &peers,
            &ami_path,
            client,
            &call_id,
            &sip_text,
        )
        .await?;

        if resp.0.is_success() {
            let _ = tx.reply(rsipstack::sip::StatusCode::OK).await;
            info!(%dialog_id, owner = %owner, "Forwarded in-dialog request to call owner");
            Some(Ok(()))
        } else {
            None
        }
    }

    /// A peer that owns an old dialog may reach the phone through its new
    /// registration home. This is terminal delivery, not a request from the
    /// phone to be forwarded back to the dialog owner.
    async fn try_deliver_peer_bye_to_local_ws(
        &self,
        tx: &mut Transaction,
        dialog_id: &DialogId,
    ) -> Result<bool> {
        use rsipstack::sip::{Header, Method, SipMessage, StatusCode, Transport};
        use rsipstack::transaction::key::TransactionKey;

        let server = &self.inner.server;
        if tx.original.method != Method::Bye
            || dialog_id.local_tag.is_empty()
            || dialog_id.remote_tag.is_empty()
        {
            return Ok(false);
        }
        let Some(source) = tx
            .connection
            .as_ref()
            .and_then(|conn| conn.get_remote_addr())
        else {
            return Ok(false);
        };
        if matches!(source.r#type, Some(Transport::Ws | Transport::Wss))
            || !source
                .addr
                .host
                .clone()
                .try_into()
                .ok()
                .is_some_and(|ip: std::net::IpAddr| server.cluster_peer_ips.contains(&ip))
        {
            return Ok(false);
        }
        let Some(home) = server.cluster_self_addr.as_ref() else {
            return Ok(false);
        };
        let location = server
            .locator
            .lookup(&tx.original.uri)
            .await?
            .into_iter()
            .find(|loc| {
                loc.aor == tx.original.uri
                    && loc
                        .home_proxy
                        .as_ref()
                        .is_some_and(|owner| owner.addr == home.addr)
                    && loc.destination.as_ref().is_some_and(|dest| {
                        matches!(dest.r#type, Some(Transport::Ws | Transport::Wss))
                    })
            });
        let Some(destination) = location.and_then(|loc| loc.destination) else {
            return Ok(false);
        };
        // Avoid dialing a client's NAT address when no registered flow is
        // cached. Resolution and sending remain bounded if a close races this check.
        if !server
            .endpoint
            .inner
            .transport_layer
            .get_addrs()
            .contains(&destination)
        {
            tx.reply(StatusCode::TemporarilyUnavailable).await?;
            return Ok(true);
        }
        let original = tx.original.clone();
        let original_vias: Vec<_> = original
            .headers
            .iter()
            .filter(|header| matches!(header, Header::Via(_)))
            .cloned()
            .collect();
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (connection, _) = server
                .endpoint
                .inner
                .transport_layer
                .lookup(&destination, None)
                .await?;
            if connection
                .cancel_token()
                .is_some_and(|token| token.is_cancelled())
            {
                return Err(anyhow!("registered WebSocket flow is closed"));
            }
            let mut via_addr = home.clone();
            via_addr.r#type = destination.r#type;
            let via = server.endpoint.inner.get_via(Some(via_addr), None)?;
            let mut request = original;
            request.headers = std::iter::once(Header::from(via))
                .chain(request.headers.iter().cloned())
                .collect::<Vec<_>>()
                .into();
            let key = TransactionKey::from_request(&request, TransactionRole::Client)?;
            let mut relay = Transaction::new_client(
                key,
                request,
                server.endpoint.inner.clone(),
                Some(connection),
            );
            relay.destination = Some(destination);
            relay.send().await?;
            while let Some(message) = relay.receive().await {
                if let SipMessage::Response(mut response) = message
                    && response.status_code.code() >= 200
                {
                    // Remove this hop's Via while retaining the source peer's
                    // transaction routing, including received/rport parameters.
                    response.headers = original_vias
                        .iter()
                        .cloned()
                        .chain(
                            response
                                .headers
                                .iter()
                                .filter(|h| !matches!(h, Header::Via(_)))
                                .cloned(),
                        )
                        .collect::<Vec<_>>()
                        .into();
                    return Ok(response);
                }
            }
            Err(anyhow!(
                "registered WebSocket BYE transaction ended without a response"
            ))
        })
        .await;
        match response {
            Ok(Ok(response)) => tx.respond(response).await?,
            Ok(Err(_)) => tx.reply(StatusCode::ServiceUnavailable).await?,
            Err(_) => tx.reply(StatusCode::from(504_u16)).await?,
        }
        Ok(true)
    }

    async fn process_message(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id =
            DialogId::try_from((&tx.original, TransactionRole::Server)).map_err(|e| anyhow!(e))?;
        let mut dialog = match self.inner.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => dialog,
            None => {
                if self.try_deliver_peer_bye_to_local_ws(tx, &dialog_id).await? {
                    return Ok(());
                }
                // Cluster: if this node has no dialog, try forwarding in-dialog
                // BYE/INFO to the session owner (agent may have re-registered
                // here after a mid-call WS reconnect).
                #[cfg(feature = "commerce")]
                {
                    if matches!(
                        tx.original.method,
                        rsipstack::sip::Method::Bye | rsipstack::sip::Method::Info
                    ) {
                        if let Some(forwarded) =
                            self.try_forward_indialog_to_owner(tx, &dialog_id).await
                        {
                            return forwarded;
                        }
                    }
                }
                // Peers retransmit BYE if we already tore the dialog down. Answer
                // with 200 so endpoints clear; other methods stay silent at debug.
                // Only do this after cluster lookup failed (or single-node).
                if matches!(tx.original.method, rsipstack::sip::Method::Bye) {
                    info!(%dialog_id, "BYE with no matching dialog; replying 200 so peer clears");
                    let _ = tx.reply(rsipstack::sip::StatusCode::OK).await;
                } else if tx.original.method == rsipstack::sip::Method::Refer {
                    tx.reply(rsipstack::sip::StatusCode::CallTransactionDoesNotExist).await?;
                } else {
                    debug!(%dialog_id, method=%tx.original.method, "dialog not found for message");
                }
                return Ok(());
            }
        };

        dialog.handle(tx).await.map_err(|e| anyhow!(e))
    }

    /// Parse Replaces header from incoming request headers.
    /// Returns (call_id, to_tag, from_tag) if present and well-formed.
    pub(crate) fn parse_replaces_header(
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

    /// Handle incoming SIP MESSAGE request from CC Phone.
    /// Supports consult transfer commands via JSON body.
    pub(crate) async fn handle_message(
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

        // Admin relays carry client credentials; never include their envelope in logs.
        let logged_body = if content_type.as_deref() == Some(crate::call::sip::ADMIN_CMD_RELAY_CT) {
            "[redacted admin command]"
        } else {
            body.as_ref()
        };
        info!(content_type = ?content_type, body = %logged_body, "Received SIP MESSAGE");
        if content_type.as_deref() == Some(crate::call::sip::ADMIN_CMD_RELAY_CT) {
            // 节点间管理指令中继：peer 节点把 console 指令转给本坐席的
            // home 节点。信任边界与 CALL_LEG_EVENT_CT 相同（cluster peer）。
            let server = &self.inner.server;
            let trusted = tx.connection.as_ref()
                .and_then(|connection| connection.get_remote_addr())
                .and_then(|source| source.addr.host.clone().try_into().ok())
                .is_some_and(|ip: std::net::IpAddr| server.cluster_peer_ips.contains(&ip));
            if !trusted {
                tx.reply(rsipstack::sip::StatusCode::Forbidden).await?;
                return Ok(());
            }
            // 中继信封自带 `agent`（创建节点注入），按它在本节点终端投递。
            // 只做终端（home==self）：locator 中途漂移 → 404（创建侧长轮询
            // 超时收场），中继消息不再二次转发。
            //
            // 应答以**本 RELAY MESSAGE 事务的 200 body** 带回创建节点（坐席
            // 跳同一机制）：home 在 [`ADMIN_CMD_AGENT_WAIT`] 内等坐席 answer，
            // 拿到即 200+body；无本地注册 404；坐席无应答 200+空 body（创建
            // 侧按 timeout 收场）。
            let relay_agent = serde_json::from_slice::<serde_json::Value>(tx.original.body())
                .ok()
                .and_then(|v| v.get("agent").and_then(|a| a.as_str()).map(str::to_string));
            match match relay_agent {
                Some(agent) => {
                    crate::call::sip::deliver_admin_cmd_relayed(server, &agent, tx.original.body())
                        .await
                }
                None => Err(anyhow::anyhow!("admin relay body missing agent")),
            } {
                Ok(crate::call::sip::AdminCmdOutcome::Answered(answer)) => {
                    info!(
                        bytes = answer.body.len(),
                        "admin cmd relay: agent answer relayed via 200 body"
                    );
                    let headers = vec![rsipstack::sip::Header::ContentType(
                        answer
                            .content_type
                            .unwrap_or_else(|| crate::call::sip::ADMIN_CMD_ANSWER_CT.into())
                            .into(),
                    )];
                    tx.reply_with(rsipstack::sip::StatusCode::OK, headers, Some(answer.body))
                        .await?;
                }
                Ok(crate::call::sip::AdminCmdOutcome::NoAnswer) => {
                    // 坐席无应答（离线/崩溃/老版本）：200 + 空 body，创建侧
                    // 按 timeout 收场——不让创建侧空等到自己的超时。
                    info!("admin cmd relay: agent did not answer in time");
                    tx.reply(rsipstack::sip::StatusCode::OK).await?;
                }
                Ok(crate::call::sip::AdminCmdOutcome::NotRegistered) => {
                    // 目标坐席在本节点无本地注册（stale locator / 已注销）。
                    tx.reply(rsipstack::sip::StatusCode::NotFound).await?;
                }
                Err(error) => {
                    warn!(%error, "admin cmd relay terminal delivery failed");
                    tx.reply(rsipstack::sip::StatusCode::ServerInternalError).await?;
                }
            }
            return Ok(());
        }
        if content_type.as_deref() == Some(crate::call::sip::CALL_LEG_EVENT_CT) {
            use rsipstack::sip::ToTypedHeader;
            let server = &self.inner.server;
            let trusted = tx.connection.as_ref()
                .and_then(|connection| connection.get_remote_addr())
                .and_then(|source| source.addr.host.clone().try_into().ok())
                .is_some_and(|ip: std::net::IpAddr| server.cluster_peer_ips.contains(&ip));
            if !trusted {
                tx.reply(rsipstack::sip::StatusCode::Forbidden).await?;
                return Ok(());
            }
            // This is a terminal home hop: a live local registration is required.
            // Never route an expiry back to another home or fall back to URI resolution.
            let locations = server.locator.lookup(&tx.original.uri).await?;
            let location = locations.into_iter().find(|location| {
                location.registered_aor.is_some() && location.destination.is_some()
                    && location.home_proxy.as_ref().is_none_or(|owner| {
                        server.cluster_self_addr.as_ref().is_some_and(|home| home.addr == owner.addr)
                    })
            });
            let Some(location) = location else {
                tx.reply(rsipstack::sip::StatusCode::NotFound).await?;
                return Ok(());
            };
            let offer = Box::new(InviteOption {
                callee: location.aor.clone(),
                caller: tx.original.from_header()?.typed()?.uri,
                contact: server.contact_uri_for_location(&location)
                    .ok_or_else(|| anyhow!("No SIP Contact for ring timeout relay"))?,
                destination: location.destination,
                ..Default::default()
            });
            crate::call::sip::send_call_leg_event(
                &server.dialog_layer, server.endpoint.inner.clone(), offer, tx.original.body.clone(),
            )?;
            tx.reply(rsipstack::sip::StatusCode::OK).await?;
            return Ok(());
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
        if tx.original.method == rsipstack::sip::Method::Options && dialog_id.local_tag.is_empty() {
            let requested = tx.original.uri.to_string();
            let room = self.inner.server.conference_server.list_conferences_detail().await.into_iter()
                .find(|room| room.focus_uri.as_deref() == Some(requested.as_str()));
            let factory = self.inner.server.proxy_config.load().conference_factory_uri.clone();
            if room.is_some() || factory.as_deref() == Some(requested.as_str()) {
                let contact = if room.is_some() { format!("<{}>;isfocus", requested) } else { format!("<{}>", requested) };
                tx.reply_with(rsipstack::sip::StatusCode::OK, vec![
                    rsipstack::sip::Header::Contact(contact.into()),
                    rsipstack::sip::Header::Allow("INVITE, ACK, CANCEL, BYE, OPTIONS, REFER, NOTIFY".into()),
                ], None).await?;
                return Ok(ProxyAction::Abort);
            }
        }
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
            | rsipstack::sip::Method::Refer
            | rsipstack::sip::Method::Bye => {
                if let Err(e) = self.process_message(tx).await {
                    warn!(%dialog_id, method=%tx.original.method, "error process {}\n{}", e, tx.original.to_string());
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
