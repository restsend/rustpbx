use crate::call::cookie::TransactionCookie;
use crate::call::domain::{CallCommand, LegId};
use crate::call::sip::ClientDialogGuard;

use crate::call::runtime::ConferenceId;
use crate::call::runtime::ConferenceManager;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::proxy_call::sip_session::SipSessionHandle;
use crate::proxy::server::SipServerRef;
use crate::rwi::RwiGatewayRef;
use crate::rwi::session::{
    ConferenceCreateRequest, DtmfCollectRequest, OriginateRequest, QueueEnqueueRequest,
    RecordStartRequest, RwiCommandPayload,
};
use crate::rwi::transfer::TransferController;
use dashmap::DashMap;
use futures::FutureExt;
use std::collections::HashMap;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Soft cap that triggers opportunistic eviction of expired dedup-cache
/// entries inside [`CommandDeduplicationCache::record`]. Picked generously to
/// avoid any per-command overhead in normal operation while still bounding
/// memory for long-lived sessions.
pub const COMMAND_DEDUP_SOFT_CAP: usize = 256;

#[derive(Clone)]
pub struct CommandDeduplicationCache {
    entries: Arc<DashMap<String, Instant>>,
    ttl: Duration,
}

impl CommandDeduplicationCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(60)
    }

    pub fn is_duplicate(&self, action_id: &str) -> bool {
        self.entries
            .get(action_id)
            .is_some_and(|received_at| received_at.elapsed() < self.ttl)
    }

    pub fn record(&self, action_id: String) {
        // Opportunistic GC: when the cache grows past a soft cap, evict expired
        // entries before inserting the new one. Keeps memory bounded for
        // long-lived sessions without adding a background task.
        if self.entries.len() >= COMMAND_DEDUP_SOFT_CAP {
            let now = Instant::now();
            self.entries
                .retain(|_, received_at| now.duration_since(*received_at) < self.ttl);
        }
        self.entries.insert(action_id, Instant::now());
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Clone)]
struct QueueState {
    queue_id: String,
}

pub struct RwiCommandProcessor {
    call_registry: Arc<ActiveProxyCallRegistry>,
    gateway: RwiGatewayRef,
    sip_server: Option<SipServerRef>,
    queue_states: Arc<DashMap<String, QueueState>>,
    /// call_id → recorder file path for recordings started via RWI (either a
    /// mid-call `record.start` or `call.originate` with a `record` option).
    /// The originate task reads this when it emits its CDR so the call record
    /// carries the recording (→ `recording_url`, and the call-record hooks can
    /// emit `recording_metadata_available` / `record_end`).
    record_files: Arc<DashMap<String, String>>,
    conference_manager: Arc<ConferenceManager>,
    transfer_controller: Arc<RwLock<TransferController>>,
    command_dedup_cache: CommandDeduplicationCache,
}

/// Removes a REFER NOTIFY subscription from the server-wide subscriber list
/// when dropped, so disconnected WebSocket sessions never leak listeners.
pub struct TransferNotifyListener {
    server: SipServerRef,
    tx: crate::call::domain::ReferNotifyTx,
    cancel: tokio_util::sync::CancellationToken,
}

impl Drop for TransferNotifyListener {
    fn drop(&mut self) {
        // Stop the consumer task; its receiver then drops, closing our sender.
        self.cancel.cancel();
        let server = self.server.clone();
        let tx = self.tx.clone();
        crate::utils::spawn(async move {
            let mut subscribers = server.transfer_notify_subscribers.lock().await;
            subscribers.retain(|s| !s.is_closed() || s.same_channel(&tx));
        });
    }
}

impl RwiCommandProcessor {
    pub fn new(
        call_registry: Arc<ActiveProxyCallRegistry>,
        gateway: RwiGatewayRef,
        conference_manager: Arc<ConferenceManager>,
    ) -> Self {
        let transfer_controller = Arc::new(RwLock::new(TransferController::with_default_config(
            call_registry.clone(),
            gateway.clone(),
        )));
        Self {
            call_registry,
            gateway,
            sip_server: None,
            queue_states: Arc::new(DashMap::new()),
            record_files: Arc::new(DashMap::new()),
            conference_manager,
            transfer_controller,
            command_dedup_cache: CommandDeduplicationCache::with_default_ttl(),
        }
    }

    pub fn with_sip_server(mut self, server: SipServerRef) -> Self {
        self.sip_server = Some(server.clone());
        self.conference_manager = server.conference_manager.clone();

        let new_controller = TransferController::with_default_config(
            self.call_registry.clone(),
            self.gateway.clone(),
        )
        .with_sip_server(server);
        self.transfer_controller = Arc::new(RwLock::new(new_controller));

        self
    }

    pub fn is_duplicate_action(&self, action_id: &str) -> bool {
        if action_id.is_empty() {
            return false;
        }
        self.command_dedup_cache.is_duplicate(action_id)
    }

    pub fn record_action(&self, action_id: String) {
        if action_id.is_empty() {
            return;
        }
        self.command_dedup_cache.record(action_id);
    }

    pub fn conference_manager(&self) -> Arc<ConferenceManager> {
        self.conference_manager.clone()
    }

    /// Register this processor as a subscriber for REFER NOTIFY events from
    /// `SipSession` and spawn a background task to feed them into the
    /// `TransferController`.
    ///
    /// Returns a guard that removes the subscription and stops the task when
    /// dropped, so a disconnected WebSocket never leaks a listener.
    pub async fn register_transfer_notify_listener(&self) -> Option<TransferNotifyListener> {
        let server = self.sip_server.clone()?;
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::call::domain::ReferNotifyEvent>();
        {
            let mut subscribers = server.transfer_notify_subscribers.lock().await;
            // Opportunistically prune dead (disconnected) subscribers so the
            // vec stays bounded across many connect/disconnect cycles.
            subscribers.retain(|tx| !tx.is_closed());
            subscribers.push(tx.clone());
        }

        let controller = self.transfer_controller.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = cancel.clone();
        crate::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    event = rx.recv() => {
                        let Some(event) = event else { break };
                        let c = controller.read().await;
                        match event.event_type {
                            crate::call::domain::ReferNotifyEventType::ReferResponse => {
                                c.handle_refer_response_by_call_id(&event.call_id, event.sip_status)
                                    .await;
                            }
                            crate::call::domain::ReferNotifyEventType::Notify => {
                                c.handle_notify_by_call_id(&event.call_id, event.sip_status)
                                    .await;
                            }
                        }
                    }
                }
            }
        });

        Some(TransferNotifyListener { server, tx, cancel })
    }

    fn dispatch_unified_command(
        &self,
        call_id: &str,
        command: RwiCommandPayload,
    ) -> Option<Result<CommandResult, CommandError>> {
        use crate::call::runtime::dispatch_rwi_command;

        match dispatch_rwi_command(&self.call_registry, Some(call_id), command) {
            Ok(result) => {
                if result.success {
                    Some(Ok(CommandResult::Success))
                } else {
                    let msg = result
                        .message
                        .unwrap_or_else(|| "command failed".to_string());

                    if msg.contains("not supported") || msg.contains("not implemented") {
                        return None;
                    }

                    if msg.to_lowercase().contains("not found") {
                        Some(Err(CommandError::CallNotFound(call_id.to_string())))
                    } else {
                        Some(Err(CommandError::CommandFailed(msg)))
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.to_lowercase().contains("not found") {
                    Some(Err(CommandError::CallNotFound(call_id.to_string())))
                } else {
                    Some(Err(CommandError::CommandFailed(msg)))
                }
            }
        }
    }

    pub async fn process_command(
        &self,
        command: RwiCommandPayload,
    ) -> Result<CommandResult, CommandError> {
        // Bridge requires both legs to exist up-front.
        if let RwiCommandPayload::Bridge { leg_a, leg_b } = &command {
            if self.call_registry.get_handle(leg_a).is_none() {
                return Err(CommandError::CallNotFound(leg_a.clone()));
            }
            if self.call_registry.get_handle(leg_b).is_none() {
                return Err(CommandError::CallNotFound(leg_b.clone()));
            }
        }

        // Commands handled entirely at the processor level (no session dispatch).
        match &command {
            RwiCommandPayload::Originate(req) => {
                return self.originate_call(req.clone()).await;
            }
            RwiCommandPayload::CallHold { call_id, music } => {
                return self.call_hold(call_id, music.clone()).await;
            }
            RwiCommandPayload::CallUnhold { call_id } => {
                return self.call_unhold(call_id).await;
            }
            RwiCommandPayload::QueueEnqueue(req) => {
                return self.queue_enqueue(req.clone()).await;
            }
            RwiCommandPayload::QueueDequeue { call_id } => {
                return self.queue_dequeue(call_id).await;
            }
            RwiCommandPayload::QueueHold { call_id } => {
                return self.queue_hold(call_id).await;
            }
            RwiCommandPayload::QueueUnhold { call_id } => {
                return self.queue_unhold(call_id).await;
            }
            RwiCommandPayload::QueueSetPriority { call_id, priority } => {
                return self.queue_set_priority(call_id, *priority).await;
            }
            RwiCommandPayload::QueueAssignAgent { call_id, agent_id } => {
                return self.queue_assign_agent(call_id, agent_id).await;
            }
            RwiCommandPayload::QueueRequeue {
                call_id,
                queue_id,
                priority,
            } => {
                return self.queue_requeue(call_id, queue_id, *priority).await;
            }
            RwiCommandPayload::RecordStart(req) => {
                return self.record_start(req.clone()).await;
            }
            RwiCommandPayload::RecordPause { call_id } => {
                return self.record_pause(call_id).await;
            }
            RwiCommandPayload::RecordResume { call_id } => {
                return self.record_resume(call_id).await;
            }
            RwiCommandPayload::RecordStop { call_id } => {
                return self.record_stop(call_id).await;
            }
            RwiCommandPayload::SetRingbackSource {
                target_call_id,
                source_call_id,
            } => {
                return self
                    .set_ringback_source(target_call_id, source_call_id)
                    .await;
            }
            RwiCommandPayload::SupervisorListen {
                supervisor_call_id,
                target_call_id,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
                return self
                    .supervisor_listen(supervisor_call_id, target_call_id)
                    .await;
            }
            RwiCommandPayload::SupervisorWhisper {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
                if !agent_leg.is_empty() {
                    self.get_handle(agent_leg).await?;
                }
                return self
                    .supervisor_whisper(supervisor_call_id, target_call_id, agent_leg)
                    .await;
            }
            RwiCommandPayload::SupervisorBarge {
                supervisor_call_id,
                target_call_id,
                agent_leg,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
                if !agent_leg.is_empty() {
                    self.get_handle(agent_leg).await?;
                }
                return self
                    .supervisor_barge(supervisor_call_id, target_call_id, agent_leg)
                    .await;
            }
            RwiCommandPayload::SupervisorTakeover {
                supervisor_call_id,
                target_call_id,
            } => {
                self.get_handle(supervisor_call_id).await?;
                self.get_handle(target_call_id).await?;
                return self
                    .supervisor_takeover(supervisor_call_id, target_call_id)
                    .await;
            }
            RwiCommandPayload::SupervisorStop {
                supervisor_call_id,
                target_call_id,
            } => {
                return self
                    .supervisor_stop(supervisor_call_id, target_call_id)
                    .await;
            }
            RwiCommandPayload::Transfer { call_id, target } => {
                return self
                    .handle_transfer(call_id.clone(), target.clone(), false)
                    .await;
            }
            RwiCommandPayload::TransferReplace { call_id, target } => {
                return self
                    .handle_transfer_replace(call_id.clone(), target.clone())
                    .await;
            }
            RwiCommandPayload::TransferAttended {
                call_id, target, ..
            } => {
                return self
                    .handle_attended_transfer(call_id.clone(), target.clone())
                    .await;
            }
            RwiCommandPayload::TransferComplete {
                call_id,
                consultation_call_id,
            } => {
                return self
                    .handle_transfer_complete(call_id.clone(), consultation_call_id.clone())
                    .await;
            }
            RwiCommandPayload::TransferCancel {
                consultation_call_id,
            } => {
                return self
                    .handle_transfer_cancel(consultation_call_id.clone())
                    .await;
            }
            _ => {}
        }

        // Unified session dispatch (Answer/Reject/Ring/Hangup/Bridge/Unbridge).
        if let Some(call_id) = command.dispatch_call_id()
            && let Some(result) = self.dispatch_unified_command(call_id, command.clone())
        {
            tracing::debug!(
                call_id = %call_id,
                "Command handled via unified session runtime"
            );

            match &command {
                RwiCommandPayload::Bridge { leg_a, leg_b } => {
                    let gw = self.gateway.read();
                    let event = crate::rwi::CallBridged {
                        leg_a: leg_a.clone(),
                        leg_b: leg_b.clone(),
                    };
                    gw.send_to_owner_at(leg_a, &event);
                    gw.send_to_owner_at(leg_b, &event);
                }
                RwiCommandPayload::Unbridge { call_id } => {
                    let gw = self.gateway.read();
                    gw.send_to_owner(&crate::rwi::CallUnbridged {
                        call_id: call_id.clone(),
                    });
                }
                _ => {}
            }
            return result;
        }

        // Processor-level commands that are not session-dispatchable.
        match &command {
            RwiCommandPayload::ListCalls => {
                let calls = self.list_calls().await;
                return Ok(CommandResult::ListCalls(calls));
            }
            RwiCommandPayload::AttachCall { call_id, mode: _ } => {
                if self.call_registry.get_handle(call_id).is_some() {
                    return Ok(CommandResult::CallFound {
                        call_id: call_id.clone(),
                    });
                } else {
                    return Err(CommandError::CallNotFound(call_id.clone()));
                }
            }
            RwiCommandPayload::DetachCall { call_id } => {
                if self.call_registry.get_handle(call_id).is_some() {
                    return Ok(CommandResult::Success);
                } else {
                    return Err(CommandError::CallNotFound(call_id.clone()));
                }
            }
            RwiCommandPayload::ConferenceCreate(req) => {
                return self.conference_create(req.clone()).await;
            }
            RwiCommandPayload::ConferenceAdd { conf_id, call_id } => {
                return self.conference_add(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceRemove { conf_id, call_id } => {
                return self.conference_remove(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceMute { conf_id, call_id } => {
                return self.conference_mute(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceUnmute { conf_id, call_id } => {
                return self.conference_unmute(conf_id, call_id).await;
            }
            RwiCommandPayload::ConferenceDestroy { conf_id } => {
                return self.conference_destroy(conf_id).await;
            }
            RwiCommandPayload::ConferenceEnd {
                conf_id,
                host_call_id,
            } => {
                return self.conference_end(conf_id, host_call_id).await;
            }
            RwiCommandPayload::ConferenceMerge {
                conf_id,
                call_id,
                consultation_call_id,
            } => {
                return self
                    .conference_merge(conf_id, call_id, consultation_call_id)
                    .await;
            }
            RwiCommandPayload::ConferenceSeatReplace {
                conf_id,
                old_call_id,
                new_call_id,
            } => {
                return self
                    .conference_seat_replace(conf_id, old_call_id, new_call_id)
                    .await;
            }
            RwiCommandPayload::Subscribe { .. } => {
                return Ok(CommandResult::Success);
            }
            RwiCommandPayload::Unsubscribe { .. } => {
                return Ok(CommandResult::Success);
            }
            RwiCommandPayload::SetVar {
                call_id,
                key,
                value,
            } => {
                let mut gw = self.gateway.write();
                gw.set_call_var(call_id, key.clone(), value.clone());
                return Ok(CommandResult::Success);
            }
            RwiCommandPayload::GetVar { call_id, key } => {
                let gw = self.gateway.read();
                let value = gw.get_call_var(call_id, key);
                return Ok(CommandResult::CallVar {
                    key: key.clone(),
                    value,
                });
            }
            RwiCommandPayload::SipMessage {
                call_id,
                content_type,
                body,
            } => {
                return self.sip_message(call_id, content_type, body).await;
            }
            RwiCommandPayload::SipNotify {
                call_id,
                event,
                content_type,
                body,
            } => {
                return self.sip_notify(call_id, event, content_type, body).await;
            }
            RwiCommandPayload::LegAdd {
                call_id,
                target,
                leg_id,
            } => {
                return self.leg_add(call_id, target, leg_id.as_deref()).await;
            }
            RwiCommandPayload::LegRemove { call_id, leg_id } => {
                return self.leg_remove(call_id, leg_id).await;
            }
            RwiCommandPayload::AppStart {
                call_id,
                app_name,
                params,
            } => {
                return self.app_start(call_id, app_name, params.clone()).await;
            }
            RwiCommandPayload::AppStop { call_id, reason } => {
                return self.app_stop(call_id, reason.clone()).await;
            }
            RwiCommandPayload::AppChain {
                call_id,
                app_name,
                params,
            } => {
                return self
                    .app_chain(call_id.as_str(), app_name.clone(), params.clone())
                    .await;
            }
            RwiCommandPayload::SipOptionsPing { call_id } => {
                return self.sip_options_ping(call_id).await;
            }
            RwiCommandPayload::CallSendDtmf {
                call_id,
                leg_id,
                digits,
            } => {
                return self
                    .send_dtmf(call_id.clone(), leg_id.clone(), digits.clone())
                    .await;
            }
            RwiCommandPayload::DtmfCollect(req) => {
                return self.dtmf_collect(req.clone()).await;
            }
            RwiCommandPayload::MediaPlay(req) => {
                return self
                    .media_play(
                        &req.call_id,
                        req.source.clone(),
                        req.interrupt_on_dtmf,
                        req.loop_playback || req.source.looped.unwrap_or(false),
                        req.leg_id.clone(),
                    )
                    .await;
            }
            RwiCommandPayload::MediaStop { call_id, leg_id } => {
                return self.media_stop(call_id, leg_id.clone()).await;
            }
            RwiCommandPayload::SessionResume {} => {
                return self.handle_session_resume();
            }
            RwiCommandPayload::CallResume { call_id } => {
                return self.handle_call_resume(call_id);
            }
            _ => {}
        }

        if let Some(call_id) = command.dispatch_call_id() {
            if self.call_registry.get_handle(call_id).is_some() {
                return Err(CommandError::CommandFailed(
                    "command not implemented in unified runtime".to_string(),
                ));
            } else {
                return Err(CommandError::CallNotFound(call_id.to_string()));
            }
        }

        Err(CommandError::CommandFailed(
            "command requires call_id".to_string(),
        ))
    }

    fn handle_session_resume(&self) -> Result<CommandResult, CommandError> {
        let gw = self.gateway.read();
        let entries = gw.resume_session();
        let replayed_count = entries.len() as u64;
        let events: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "timestamp": e.cached_at.to_rfc3339(),
                    "call_id": e.call_id,
                    "event": e.event,
                })
            })
            .collect();
        Ok(CommandResult::SessionResumed {
            replayed_count,
            events,
        })
    }

    fn handle_call_resume(&self, call_id: &String) -> Result<CommandResult, CommandError> {
        let gw = self.gateway.read();
        let entries = gw.resume_call(call_id);
        let replayed_count = entries.len() as u64;
        let events: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "timestamp": e.cached_at.to_rfc3339(),
                    "call_id": e.call_id,
                    "event": e.event,
                })
            })
            .collect();
        Ok(CommandResult::CallResumed {
            call_id: call_id.to_string(),
            replayed_count,
            events,
        })
    }

    /// Normalize an RWI-supplied `caller_id` into a valid SIP URI string for the
    /// originate `From` header.
    ///
    /// `caller_id` is commonly a bare phone number ("+16142159851") — caller ID
    /// *is* a number. A bare token parses into a user-less URI, and a trunk's
    /// `rewrite_hostport` then restamps the host, yielding an invalid `From` like
    /// `<carrier.example.com>` (no user) that carriers reject with
    /// "400 Invalid From". Rules:
    ///   * `None`/blank → `sip:rwi@<realm>` (unchanged fallback).
    ///   * a `sip:`/`sips:` URI that already has a user → kept verbatim.
    ///   * anything else (bare number, `user@host`, scheme-without-user, values
    ///     with params) → the leading token is taken as the user and wrapped as
    ///     `sip:<user>@<realm>`. Any scheme prefix is stripped case-insensitively
    ///     and the token is cut at the first `@`/`;`/`?` so we never double-affix
    ///     a host or leak params into the URI host.
    pub fn normalize_originate_caller_id(caller_id: Option<&str>, realm: &str) -> String {
        let raw = match caller_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(c) => c,
            None => return format!("sip:rwi@{realm}"),
        };

        // Keep it as-is only if it's a sip/sips URI that already carries a user.
        if let Ok(uri) = rsipstack::sip::Uri::try_from(raw) {
            let has_scheme = matches!(
                uri.scheme,
                Some(rsipstack::sip::Scheme::Sip) | Some(rsipstack::sip::Scheme::Sips)
            );
            let has_user = uri.auth.as_ref().is_some_and(|a| !a.user.is_empty());
            if has_scheme && has_user {
                return raw.to_string();
            }
        }

        // Otherwise treat the input as a bare user token: strip a leading sip/sips
        // scheme (case-insensitive), cut at the first host/param/header delimiter.
        let no_scheme = raw
            .get(..4)
            .filter(|p| p.eq_ignore_ascii_case("sip:"))
            .map(|_| &raw[4..])
            .or_else(|| {
                raw.get(..5)
                    .filter(|p| p.eq_ignore_ascii_case("sips:"))
                    .map(|_| &raw[5..])
            })
            .unwrap_or(raw);
        let user = no_scheme
            .split(['@', ';', '?'])
            .next()
            .unwrap_or(no_scheme)
            .trim();
        // Degenerate inputs (`sip:`, `@host`, `;user=phone`, …) yield an empty
        // user, which would produce `sip:@<realm>` — the very user-less From this
        // helper exists to prevent. Fall back to the safe default instead.
        if user.is_empty() {
            return format!("sip:rwi@{realm}");
        }
        format!("sip:{user}@{realm}")
    }

    /// Resolve a named originate trunk to its config, rejecting unknown/unloaded
    /// trunks and DISABLED trunks (apply_trunk_config only stamps routing fields and
    /// does not enforce `disabled`). Shared by the apply helper and the parallel
    /// pre-validation so an explicit trunk fails the same way in both — synchronously,
    /// before any call is started.
    fn resolve_originate_trunk(
        server: &SipServerRef,
        trunk_name: &str,
    ) -> Result<crate::proxy::routing::TrunkConfig, String> {
        let trunk = server
            .data_context
            .get_trunk(trunk_name)
            .ok_or_else(|| format!("unknown or unloaded trunk: {}", trunk_name))?;
        if trunk.disabled.unwrap_or(false) {
            return Err(format!("trunk is disabled: {}", trunk_name));
        }
        Ok(trunk)
    }

    /// Apply an explicit carrier-trunk override to an originate's InviteOption.
    ///
    /// When `trunk_name` names a configured (and enabled) trunk, stamp that trunk's
    /// destination next-hop, transport, digest credential, host rewrite, and
    /// P-Asserted-Identity header onto `option` (the same pure mutator the inbound
    /// proxy path uses), so the resulting INVITE goes straight to the carrier. When
    /// `trunk_name` is None/blank the option is left untouched and the legacy
    /// direct-to-callee behavior is preserved.
    ///
    /// Returns Err(message) on an unknown/unloaded/disabled trunk or an invalid trunk
    /// destination — surfaced synchronously to the caller before any call is spawned.
    fn apply_explicit_originate_trunk(
        server: &SipServerRef,
        invite_option: &mut rsipstack::dialog::invitation::InviteOption,
        trunk_name: Option<&str>,
        call_id: &str,
    ) -> Result<(), String> {
        let Some(trunk_name) = trunk_name.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(());
        };

        let trunk = Self::resolve_originate_trunk(server, trunk_name)?;

        crate::proxy::routing::matcher::apply_trunk_config(invite_option, &trunk)
            .map_err(|e| format!("trunk config failed for '{}': {}", trunk_name, e))?;

        tracing::info!(
            call_id = %call_id,
            trunk = %trunk_name,
            dest = ?invite_option.destination,
            has_cred = invite_option.credential.is_some(),
            "originate routed direct-to-trunk"
        );

        Ok(())
    }

    pub async fn originate_call(
        &self,
        req: OriginateRequest,
    ) -> Result<CommandResult, CommandError> {
        let server = self
            .sip_server
            .as_ref()
            .ok_or_else(|| CommandError::CommandFailed("SIP server not available".into()))?
            .clone();

        let destination_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(req.destination.as_str()).map_err(|_| {
                CommandError::CommandFailed(format!("invalid destination: {}", req.destination))
            })?;

        let realm = server.proxy_config.load().first_realm();
        let caller_str = Self::normalize_originate_caller_id(req.caller_id.as_deref(), &realm);
        let caller_uri: rsipstack::sip::Uri = rsipstack::sip::Uri::try_from(caller_str.as_str())
            .map_err(|_| CommandError::CommandFailed("invalid caller_id".into()))?;

        let mut headers: Vec<rsipstack::sip::Header> =
            vec![rsipstack::sip::headers::MaxForwards::from(70u32).into()];
        // Root session id via RFC 7433 UUI — the originate call is a session
        // root; the header lets external legs re-attach on the way back in.
        headers.push(crate::call::uui::build_uui_header(
            &req.call_id,
            None,
            None,
            None,
        ));
        for (k, v) in &req.extra_headers {
            headers.push(rsipstack::sip::Header::Other(k.clone(), v.clone()));
        }

        let media = server.default_media_config();

        // If routing out a named carrier trunk, respect its audio codec policy.
        // Invalid or empty policies fall back to the default audio codec set.
        let configured_audio_codecs = req
            .trunk
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|trunk_name| Self::resolve_originate_trunk(&server, trunk_name).ok())
            .map(|trunk| {
                trunk
                    .codec
                    .iter()
                    .filter_map(|codec| audio_codec::CodecType::try_from(codec.as_str()).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| crate::media::MediaNegotiator::default_rtp_codecs());
        let originate_codecs =
            crate::media::MediaNegotiator::build_local_rtp_codec_offer(&configured_audio_codecs);

        let mut invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri.clone(),
            caller: caller_uri.clone(),
            // Contact must be the proxy's OWN reachable address so the carrier
            // routes in-dialog requests (BYE / re-INVITE) back here — not the
            // caller-id URI, which is not a proxy endpoint (and whose host is
            // rewritten to the carrier when a trunk override is applied). Falls
            // back to the caller URI only if no local contact is available.
            contact: server
                .default_contact_uri()
                .unwrap_or_else(|| caller_uri.clone()),
            content_type: Some("application/sdp".to_string()),
            // The offer is generated below from the session's real
            // MediaBridge A leg after the session has been constructed.
            offer: None,
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(req.call_id.clone()),
            ..Default::default()
        };

        // Direct-to-trunk routing. The base option above has destination:None, so
        // rsipstack resolves the next hop from the callee request-URI — which only
        // reaches a registered/reachable SIP URI (and self-INVITEs this proxy, which
        // 407s a non-local callee). When the caller names a `trunk`, stamp that
        // trunk's next-hop + credential + P-Asserted-Identity onto the option so
        // rsipstack sends ONE INVITE straight to the carrier and auto-answers its
        // 401/407 from the credential. When `trunk` is absent the option is left
        // untouched and the legacy direct-to-callee behavior is preserved
        // (byte-identical).
        //
        // Originate is an API command, so the caller declares which carrier gateway
        // it wants by name; the named trunk's config is applied here. We deliberately
        // do NOT consult the proxy route table here — that would duplicate
        // CallModule's direction + admission semantics and drift.
        //
        // KNOWN LIMITATIONS on this direct path (vs. the inbound-proxy route path):
        //   * Authorization: any RWI caller permitted to originate may select any
        //     enabled trunk by name. There is no per-token trunk allowlist/scope; the
        //     only gate is whatever authorizes the originate command itself.
        //   * Admission: per-trunk CAC/CPS/max-duration and route-layer media policy
        //     are NOT enforced here — the caller is responsible for pacing.
        // Both are acceptable for a caller-driven control API but are documented so a
        // deployment can add a trunk allowlist/scope if its threat model needs one.
        //
        // Runs synchronously (before the spawn below), so an unknown/disabled trunk
        // returns an immediate failed ack rather than leaking a half-built call.
        Self::apply_explicit_originate_trunk(
            &server,
            &mut invite_option,
            req.trunk.as_deref(),
            &req.call_id,
        )
        .map_err(CommandError::CommandFailed)?;

        // Route-table consultation for originates without an explicit trunk.
        // An explicit `trunk` (above) always wins and skips the route table.
        // When enabled (request-level override → global default), the target is
        // matched against outbound route rules so rewrite + trunk selection
        // apply. Routing hints (concurrency holds / concurrent-call lease) are
        // threaded into the session Dialplan below so they are released with the
        // session on teardown.
        let explicit_trunk = req
            .trunk
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some();
        let routing_enabled = req
            .route_originated_calls
            .unwrap_or(server.proxy_config.load().route_originated_calls);
        let routed_hints: Option<crate::config::DialplanHints> =
            if explicit_trunk || !routing_enabled {
                None
            } else {
                let contact = invite_option.contact.clone();
                match crate::proxy::proxy_call::sip_session::route_outbound_leg(
                    &server,
                    &destination_uri,
                    &caller_uri,
                    &contact,
                    None,
                    TransactionCookie::default(),
                )
                .await
                {
                    Ok(Some(crate::config::RouteResult::Forward(routed_option, hints))) => {
                        invite_option.callee = routed_option.callee.clone();
                        if let Some(dest) = routed_option.destination.clone() {
                            invite_option.destination = Some(dest);
                        }
                        if let Some(cred) = routed_option.credential.clone() {
                            invite_option.credential = Some(cred);
                        }
                        if let Some(headers) = routed_option.headers.clone() {
                            invite_option
                                .headers
                                .get_or_insert_with(Vec::new)
                                .extend(headers);
                        }
                        tracing::info!(
                            call_id = %req.call_id,
                            trunk_dest = ?invite_option.destination,
                            "originate routed through route table"
                        );
                        hints
                    }
                    Ok(Some(crate::config::RouteResult::Abort(code, reason))) => {
                        return Err(CommandError::CommandFailed(format!(
                            "route aborted for originate: {} {}",
                            code.code(),
                            reason.unwrap_or_default()
                        )));
                    }
                    _ => None, // NotHandled / Queue / Application / disabled → legacy direct dial
                }
            };

        let call_id = req.call_id.clone();
        let gateway = self.gateway.clone();
        let registry = self.call_registry.clone();
        let timeout_secs = req.timeout_secs.unwrap_or(60);
        let dialog_layer = server.dialog_layer.clone();
        let caller_display = req.caller_id.unwrap_or_else(|| caller_str.clone());
        let callee_display = req.destination.clone();
        let record_on_answer = req.record.clone();
        let record_channels = record_on_answer
            .as_ref()
            .map(RecordStartRequest::channels)
            .transpose()
            .map_err(CommandError::CommandFailed)?;
        let record_files = self.record_files.clone();

        // RWI UAC sessions use the cleanup closure below for their CDR. Keep a
        // guard in the originate task first, then move it into the CallRecord.
        // Dropping either the task or the record releases ownership, including
        // setup failures that abort before a CDR can be queued.
        let rwi_call_record_guard =
            crate::rwi::RwiCallRecordGuard::new(&self.gateway, call_id.clone());

        // CDR data for call completion reporting
        let cdr_sender = server.callrecord_sender.clone();
        let cdr_answered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cdr_start_time = chrono::Utc::now();

        let cancel_token = tokio_util::sync::CancellationToken::new();

        crate::utils::spawn(async move {
            use crate::call::cookie::TransactionCookie;
            use crate::call::{DialDirection, Dialplan};
            use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallStatus};
            use crate::proxy::proxy_call::sip_session::SipSession;
            use crate::proxy::proxy_call::state::CallContext;

            // CDR cleanup closure — sends a call record when the call ends.
            let cdr_call_id = call_id.clone();
            let cdr_caller = caller_display.clone();
            let cdr_callee = callee_display.clone();
            let cdr_start = cdr_start_time;
            let cdr_sender_owned = cdr_sender.clone();
            let cdr_answered = cdr_answered.clone();
            let cdr_answered_for_store = cdr_answered.clone();
            let cdr_record_files = record_files.clone();
            let mut cdr_rwi_call_record_guard = Some(rwi_call_record_guard);
            let mut cleanup = move || {
                let Some(rwi_call_record_guard) = cdr_rwi_call_record_guard.take() else {
                    return;
                };
                let recorder_file = cdr_record_files.remove(&cdr_call_id).map(|(_, path)| path);
                if let Some(ref sender) = cdr_sender_owned.as_ref() {
                    use crate::callrecord::CallRecordHangupReason;
                    let end_time = chrono::Utc::now();
                    let answered = cdr_answered.load(std::sync::atomic::Ordering::Relaxed);
                    // Attach the recorder media when a recording was started on
                    // this call (originate `record` option or mid-call
                    // record.start) so the CDR carries recording_url and the
                    // call-record hooks can emit recording_metadata_available /
                    // record_end.
                    let recorder: Vec<crate::callrecord::CallRecordMedia> = recorder_file
                        .map(|path| {
                            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                            crate::callrecord::CallRecordMedia {
                                track_id: "mixed".to_string(),
                                path,
                                size,
                                extra: None,
                            }
                        })
                        .into_iter()
                        .collect();
                    let mut record = crate::callrecord::CallRecord {
                        call_id: cdr_call_id.clone(),
                        // Originates are root sessions: session_id == call_id.
                        session_id: Some(cdr_call_id.clone()),
                        caller: cdr_caller.clone(),
                        callee: cdr_callee.clone(),
                        start_time: cdr_start,
                        ring_time: None,
                        answer_time: if answered { Some(cdr_start) } else { None },
                        end_time,
                        status_code: if answered { 200 } else { 0 },
                        hangup_reason: Some(if answered {
                            CallRecordHangupReason::BySystem
                        } else {
                            CallRecordHangupReason::Canceled
                        }),
                        hangup_messages: vec![],
                        recorder,
                        sip_leg_roles: std::collections::HashMap::new(),
                        leg_timeline: crate::callrecord::LegTimeline::default(),
                        details: crate::callrecord::CallDetails {
                            direction: "outbound".to_string(),
                            status: if answered {
                                "answered".to_string()
                            } else {
                                "no_answer".to_string()
                            },
                            from_number: Some(cdr_caller.clone()),
                            to_number: Some(cdr_callee.clone()),
                            ..Default::default()
                        },
                        extensions: http::Extensions::new(),
                    };
                    record.extensions.insert(rwi_call_record_guard);
                    if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) =
                        sender.try_send(record)
                    {
                        tracing::warn!(call_id = %cdr_call_id, "call record channel full; dropping RWI-originated CDR");
                    }
                }
            };

            // These peers are logical registry anchors. The first outbound
            // SIP/RTP connection is owned by MediaBridge A below; no duplicate
            // RtcTrack is attached to either peer.
            let caller_media_builder = crate::media::MediaStreamBuilder::new()
                .with_id(format!("{}-caller", call_id))
                .with_cancel_token(cancel_token.clone());
            let caller_peer: Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer> =
                Arc::new(caller_media_builder.build());

            let callee_media_builder = crate::media::MediaStreamBuilder::new()
                .with_id(format!("{}-callee", call_id))
                .with_cancel_token(cancel_token.clone());
            let callee_peer: Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer> =
                Arc::new(callee_media_builder.build());

            // Construct a UAC SipSession. The first answered outbound INVITE
            // becomes the caller/A dialog; the callee channel is reserved for
            // dialogs added later through call.leg_add.
            let synthetic_request = rsipstack::sip::Request {
                method: rsipstack::sip::Method::Invite,
                uri: destination_uri.clone(),
                version: rsipstack::sip::Version::V2,
                headers: rsipstack::sip::Headers::default(),
                body: Vec::new(),
            };
            let mut metadata = HashMap::new();
            if let Some(t) = req.trunk.as_ref() {
                metadata.insert("trunk".to_string(), t.clone());
            }
            // Attribute the originated call to the originating agent (CC
            // click-to-dial): the call-session hook resolves
            // `resolved_agent_id` first (priority 1), which fires the full
            // cc_* webhook chain and keeps agent context through transfers.
            //
            // Scope: only for true agent→customer dials — the caller user part
            // IS a registered agent AND the destination is NOT one. When both
            // parties are agents (internal assist/agent-to-agent), leave
            // attribution to the existing callee-based heuristics (CDR expects
            // the callee there).
            if let Some(user) = caller_uri.user()
                && !user.is_empty()
                && let Some(registry) = server.agent_registry.as_ref()
                && registry.get_agent(user).await.is_some()
            {
                let dest_user = destination_uri.user().unwrap_or_default().to_string();
                let dest_is_agent =
                    !dest_user.is_empty() && registry.get_agent(&dest_user).await.is_some();
                if !dest_is_agent {
                    metadata.insert("resolved_agent_id".to_string(), user.to_string());
                }
            }
            let mut dialplan =
                Dialplan::new(call_id.clone(), synthetic_request, DialDirection::Outbound)
                    .with_caller(caller_uri.clone())
                    .with_media(media.clone());
            // Every RWI-originated call prepares the capture sender/task before
            // constructing its caller media leg. A `record` option activates
            // the file recorder on answer; otherwise record.start may activate
            // it later without rebuilding the media leg.
            dialplan.recording.enabled = true;
            dialplan.recording.auto_start = false;
            dialplan.recording.recording_type = crate::config::RecordingType::Local;
            if let Some(hints) = routed_hints {
                dialplan = dialplan.with_hints(hints);
            }
            let context = CallContext {
                session_id: call_id.clone(),
                dialplan: Arc::new(dialplan),
                cookie: TransactionCookie::default(),
                start_time: std::time::Instant::now(),
                original_caller: caller_display.clone(),
                original_callee: callee_display.clone(),
                max_forwards: 70,
                created_at: chrono::Utc::now().to_rfc3339(),
                metadata: if metadata.is_empty() {
                    None
                } else {
                    Some(metadata)
                },
            };

            let use_media_proxy = true;
            let (mut session, handle, cmd_rx) = SipSession::new_uac(
                server.clone(),
                cancel_token.clone(),
                cdr_sender.clone(),
                context,
                use_media_proxy,
                caller_peer.clone(),
                callee_peer.clone(),
            );

            // Build the real A leg first and send its exact local description
            // in the INVITE. The answer will be applied back to this same leg.
            let sdp_offer = match session.prepare_originate_caller_leg(originate_codecs).await {
                Ok(offer) => offer,
                Err(e) => {
                    tracing::warn!(call_id = %call_id, error = %e, "failed to prepare originate media");
                    let gw = gateway.read();
                    gw.send_to_owner(&crate::rwi::CallHangup {
                        call_id: call_id.clone(),
                        reason: Some(format!("media_setup_failed: {}", e)),
                        hangup_by: None,
                        sip_status: None,
                    });
                    cancel_token.cancel();
                    cleanup();
                    return;
                }
            };
            invite_option.offer = Some(sdp_offer.into_bytes());

            let (caller_state_tx, mut caller_state_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut invitation = dialog_layer
                .do_invite(invite_option, caller_state_tx)
                .boxed();

            let entry = ActiveProxyCallEntry {
                session_id: call_id.clone(),
                caller: Some(caller_display.clone()),
                callee: Some(callee_display.clone()),
                direction: "outbound".to_string(),
                started_at: chrono::Utc::now(),
                answered_at: None,
                status: ActiveProxyCallStatus::Ringing,
            };
            registry.upsert(entry, handle.clone());

            // Populate the CallMetaStore so events emitted from this originate
            // (call_created, call_ringing, ...) are enriched with call context
            // — notably `direction: "outbound"`. Originates are root sessions
            // (session_id == call_id); UAC legs carry no cross-session root.
            // Cleanup rides on `call_finished` via the call-record guard.
            gateway.read().meta_store.insert(
                call_id.clone(),
                crate::rwi::proto::CallMeta {
                    session_id: Some(call_id.clone()),
                    caller: Some(caller_display.clone()),
                    callee: Some(callee_display.clone()),
                    direction: Some("outbound".to_string()),
                    ..Default::default()
                },
            );

            // Publish the originated session's owning node in the cluster
            // session registry (no-op backend in single-node mode).
            session.register_in_session_registry().await;

            // Callee dialog-state channel reserved for later call.leg_add dialogs.
            let (callee_evt_tx, callee_evt_rx) = tokio::sync::mpsc::unbounded_channel();
            session.callee_event_tx = Some(callee_evt_tx);

            // Wait for the outbound INVITE to complete.
            let result = tokio::time::timeout(
                Duration::from_secs(timeout_secs as u64),
                async {
                    loop {
                        tokio::select! {
                            res = &mut invitation => break res,
                            state = caller_state_rx.recv() => {
                                match state {
                                    Some(rsipstack::dialog::dialog::DialogState::Calling(_)) => {
                                        let gw = gateway.read();
                                        gw.send_to_owner(&crate::rwi::CallCreated {
                                            call_id: call_id.clone(),
                                            context: "default".into(),
                                            caller: caller_display.clone(),
                                            callee: callee_display.clone(),
                                            trunk: None,
                                            sip_headers: Default::default(),
                                            caller_name: None,
                                            callee_name: None,
                                            called_phone: None,
                                            app_id: None,
                                            routing_target: None,
                                            uuid: None,
                                            routing_path: None,
                                        });
                                    }
                                    Some(rsipstack::dialog::dialog::DialogState::Early(_, ref response)) => {
                                        let body = response.body();
                                        if !body.is_empty()
                                            && String::from_utf8_lossy(body).contains("v=0")
                                        {
                                            tracing::debug!(%call_id, "Early media SDP received");
                                        }
                                        let gw = gateway.read();
                                        let code = response.status_code().code();
                                        if code == 180 {
                                            // 180 Ringing — remote side is alerting.
                                            gw.send_to_owner(&crate::rwi::CallRinging {
                                                call_id: call_id.clone(),
                                            });
                                        } else {
                                            // 183 or other provisional — treat as early media.
                                            gw.send_to_owner(&crate::rwi::CallEarlyMedia {
                                                call_id: call_id.clone(),
                                            });
                                        }
                                    }
                                    Some(rsipstack::dialog::dialog::DialogState::Terminated(_, _)) => {}
                                    _ => {}
                                }
                            }
                        }
                    }
                },
            ).await;

            match result {
                Ok(Ok((dialog, Some(resp))))
                    if resp.status_code().kind() == rsipstack::sip::StatusCodeKind::Successful =>
                {
                    cdr_answered_for_store.store(true, std::sync::atomic::Ordering::Relaxed);

                    // A successful do_invite registers the confirmed client dialog in
                    // DialogLayer. Keep its guard alive for the entire UAC session so
                    // every exit path removes that registry entry.
                    let caller_dialog_guard =
                        ClientDialogGuard::new(dialog_layer.clone(), dialog.id());

                    let sdp_answer = if resp.body().is_empty() {
                        None
                    } else {
                        let body_str = String::from_utf8_lossy(resp.body()).to_string();
                        if body_str.contains("v=0") {
                            Some(body_str)
                        } else {
                            None
                        }
                    };

                    // Attach the answered first INVITE as the primary caller
                    // (MediaBridge A). Its SDP answer is applied to the exact
                    // leg whose offer was sent above. B remains empty until a
                    // genuine second endpoint is added through call.leg_add.
                    if let Err(e) = session.attach_caller_dialog(dialog, sdp_answer).await {
                        tracing::warn!(call_id = %call_id, error = %e, "failed to complete originate media negotiation");
                        {
                            let gw = gateway.read();
                            gw.send_to_owner(&crate::rwi::CallHangup {
                                call_id: call_id.clone(),
                                reason: Some(format!("media_setup_failed: {}", e)),
                                hangup_by: None,
                                sip_status: Some(resp.status_code().code()),
                            });
                        }
                        cancel_token.cancel();
                        registry.remove(&call_id);
                        cleanup();
                        return;
                    }

                    // Spawn the UAC command loop now that the caller/A dialog
                    // and its one real media connection are attached.
                    let session_cancel = cancel_token.clone();
                    let session_call_id = call_id.clone();
                    crate::utils::spawn(async move {
                        if let Err(e) = session
                            .process_uac(
                                caller_state_rx,
                                callee_evt_rx,
                                cmd_rx,
                                caller_dialog_guard,
                            )
                            .await
                        {
                            tracing::warn!(call_id = %session_call_id, error = %e, "UAC session loop exited with error");
                        }
                        session_cancel.cancel();
                    });

                    // Auto-start recording when the originate carried a
                    // `record` option (recording on answer). The command goes
                    // through the normal StartRecording path (MediaBridge
                    // recorder), and the file is tracked so the CDR picks it
                    // up. Use a short grace so the UAC command loop is armed
                    // before the command lands in cmd_rx (mpsc is buffered, so
                    // sending early is safe regardless).
                    if let Some(rec) = record_on_answer.as_ref() {
                        let path = if rec.storage.path.trim().is_empty() {
                            default_originate_recorder_path(&server, &call_id)
                        } else {
                            rec.storage.path.clone()
                        };
                        let send_result = handle.send_command(CallCommand::StartRecording {
                            config: crate::call::domain::RecordConfig {
                                path: path.clone(),
                                max_duration_secs: rec.max_duration_secs,
                                beep: rec.beep.unwrap_or(false),
                                format: None,
                                channels: record_channels,
                                mono_caller_only: Some(false),
                                segment_type: rec.segment_type.clone(),
                                segment_id: rec.id.clone(),
                                notify_app: Some(false),
                            },
                        });
                        match send_result {
                            Ok(()) => match handle.query_recorder_status().await {
                                Ok(status) if status.active => {
                                    record_files
                                        .insert(call_id.clone(), status.file_path.unwrap_or(path));
                                    let gw = gateway.read();
                                    gw.send_to_owner(&crate::rwi::RecordStarted {
                                        call_id: call_id.clone(),
                                    });
                                }
                                Ok(_) => {
                                    tracing::warn!(call_id = %call_id, "originate record option: recorder did not start");
                                }
                                Err(error) => {
                                    tracing::warn!(call_id = %call_id, %error, "originate record option: failed to query recorder");
                                }
                            },
                            Err(error) => {
                                tracing::warn!(call_id = %call_id, %error, "originate record option: failed to send StartRecording");
                            }
                        }
                    }

                    use crate::proxy::active_call_registry::ActiveProxyCallStatus;
                    registry.update(&call_id, |entry| {
                        entry.answered_at = Some(chrono::Utc::now());
                        entry.status = ActiveProxyCallStatus::Talking;
                    });
                    {
                        let gw = gateway.read();
                        gw.send_to_owner(&crate::rwi::CallAnswered {
                            call_id: call_id.clone(),
                        });
                    }

                    // Keep the call alive until cancelled / timed out.
                    tokio::select! {
                        _ = cancel_token.cancelled() => {
                            tracing::info!(%call_id, "Originate task cancelled");
                        }
                        _ = tokio::time::sleep(Duration::from_secs(3600)) => {
                            tracing::info!(%call_id, "Call timeout after 1 hour");
                        }
                    }
                    cleanup();
                }
                Ok(Ok((_dialog, resp_opt))) => {
                    let sip_status = resp_opt.as_ref().map(|r| r.status_code.code());
                    {
                        let gw = gateway.read();
                        if sip_status == Some(486) || sip_status == Some(600) {
                            gw.send_to_owner(&crate::rwi::CallBusy {
                                call_id: call_id.clone(),
                            });
                        } else if matches!(sip_status, Some(408) | Some(480) | Some(487)) {
                            gw.send_to_owner(&crate::rwi::CallNoAnswer {
                                call_id: call_id.clone(),
                            });
                        } else {
                            gw.send_to_owner(&crate::rwi::CallHangup {
                                call_id: call_id.clone(),
                                reason: Some("originate_failed".to_string()),
                                hangup_by: None,
                                sip_status,
                            });
                        }
                    }
                    cancel_token.cancel();
                    registry.remove(&call_id);
                    cleanup();
                }
                Ok(Err(e)) => {
                    {
                        let gw = gateway.read();
                        gw.send_to_owner(&crate::rwi::CallHangup {
                            call_id: call_id.clone(),
                            reason: Some(e.to_string()),
                            hangup_by: None,
                            sip_status: None,
                        });
                    }
                    cancel_token.cancel();
                    registry.remove(&call_id);
                    cleanup();
                }
                Err(_) => {
                    {
                        let gw = gateway.read();
                        gw.send_to_owner(&crate::rwi::CallNoAnswer {
                            call_id: call_id.clone(),
                        });
                    }
                    cancel_token.cancel();
                    registry.remove(&call_id);
                    cleanup();
                }
            }
        });

        Ok(CommandResult::Originated {
            call_id: req.call_id,
        })
    }

    pub async fn list_calls(&self) -> Vec<CallInfo> {
        self.call_registry
            .list_recent(100)
            .into_iter()
            .map(|entry| CallInfo {
                session_id: entry.session_id,
                caller: entry.caller,
                callee: entry.callee,
                direction: entry.direction,
                status: entry.status.to_string(),
                started_at: entry.started_at.to_rfc3339(),
                answered_at: entry.answered_at.map(|t| t.to_rfc3339()),
            })
            .collect()
    }

    async fn get_handle(&self, call_id: &str) -> Result<SipSessionHandle, CommandError> {
        self.call_registry
            .get_handle(call_id)
            .ok_or_else(|| CommandError::CallNotFound(call_id.to_string()))
    }

    /// Query a leg's recorder status, mapping transport errors onto
    /// `CommandError::CommandFailed`.
    async fn recorder_status(
        handle: &SipSessionHandle,
    ) -> Result<crate::media::media_recorder::RecorderStatus, CommandError> {
        handle
            .query_recorder_status()
            .await
            .map_err(|e| CommandError::CommandFailed(e.to_string()))
    }

    async fn call_hold(
        &self,
        call_id: &str,
        music: Option<String>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let music = music.map(crate::call::domain::MediaSource::file);

        handle
            .send_command(CallCommand::Hold {
                leg_id: crate::call::domain::LegId::new("callee"),
                music,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::MediaHoldStarted {
            call_id: call_id.to_string(),
        });

        Ok(CommandResult::Success)
    }

    async fn call_unhold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        handle
            .send_command(CallCommand::Unhold {
                leg_id: crate::call::domain::LegId::new("callee"),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::MediaHoldStopped {
            call_id: call_id.to_string(),
        });

        Ok(CommandResult::Success)
    }

    async fn leg_add(
        &self,
        call_id: &str,
        target: &str,
        leg_id: Option<&str>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        let leg_id_opt = leg_id.map(|id| crate::call::domain::LegId::new(id));

        handle
            .send_command(CallCommand::LegAdd {
                target: target.to_string(),
                leg_id: leg_id_opt,
                headers: Vec::new(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        Ok(CommandResult::Success)
    }

    async fn leg_remove(&self, call_id: &str, leg_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        handle
            .send_command(CallCommand::LegRemove {
                leg_id: crate::call::domain::LegId::new(leg_id),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        Ok(CommandResult::Success)
    }

    async fn app_start(
        &self,
        call_id: &str,
        app_name: &str,
        params: Option<serde_json::Value>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        handle
            .send_command(CallCommand::StartApp {
                app_name: app_name.to_string(),
                params,
                auto_answer: false,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        Ok(CommandResult::Success)
    }

    async fn app_stop(
        &self,
        call_id: &str,
        reason: Option<String>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        handle
            .send_command(CallCommand::StopApp { reason })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        Ok(CommandResult::Success)
    }

    async fn app_chain(
        &self,
        call_id: &str,
        app_name: String,
        params: Option<serde_json::Value>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;

        // Stop current app first
        handle
            .send_command(CallCommand::StopApp {
                reason: Some("chaining".into()),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        // Brief yield to let the app stop complete
        tokio::task::yield_now().await;

        // Start new app
        handle
            .send_command(CallCommand::StartApp {
                app_name: app_name.clone(),
                params: params.clone(),
                auto_answer: false,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        info!(
            call_id = %call_id,
            new_app = %app_name,
            "App chained successfully"
        );

        Ok(CommandResult::Success)
    }

    async fn media_play(
        &self,
        call_id: &str,
        source: crate::rwi::session::MediaSource,
        _interrupt_on_dtmf: bool,
        loop_playback: bool,
        leg_id: Option<String>,
    ) -> Result<CommandResult, CommandError> {
        use crate::call::domain::{MediaSource as DomainMediaSource, PlayOptions};

        let handle = self.get_handle(call_id).await?;

        let domain_source = if source.source_type == "file" || source.source_type == "url" {
            if source.source_type == "url" {
                DomainMediaSource::Url {
                    url: source.uri.unwrap_or_default(),
                }
            } else {
                DomainMediaSource::File {
                    path: source.uri.unwrap_or_default(),
                }
            }
        } else {
            DomainMediaSource::Silence
        };

        let track_id = uuid::Uuid::new_v4().to_string();
        let event_leg_id = leg_id.clone();
        handle
            .send_command(CallCommand::Play {
                leg_id: leg_id.map(LegId::new),
                source: domain_source,
                options: Some(PlayOptions {
                    loop_playback,
                    await_completion: false,
                    interrupt_on_dtmf: _interrupt_on_dtmf,
                    track_id: Some(track_id.clone()),
                    send_progress: false,
                    side_only: false,
                }),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::MediaPlayStarted {
            call_id: call_id.to_string(),
            leg_id: event_leg_id,
            track_id: track_id.clone(),
        });

        Ok(CommandResult::MediaPlay { track_id })
    }

    async fn media_stop(
        &self,
        call_id: &str,
        leg_id: Option<String>,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::StopPlayback {
                leg_id: leg_id.map(LegId::new),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn send_dtmf(
        &self,
        call_id: String,
        leg_id: Option<String>,
        digits: String,
    ) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(&call_id).await?;
        handle
            .send_command(CallCommand::SendDtmf {
                leg_id: leg_id.map(LegId::new).unwrap_or(LegId::from("caller")),
                digits,
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn dtmf_collect(&self, req: DtmfCollectRequest) -> Result<CommandResult, CommandError> {
        if req.call_id.is_empty() {
            return Err(CommandError::CallNotFound(
                "call_id is required for DtmfCollect".into(),
            ));
        }
        // Verify the call exists.
        let _ = self.get_handle(&req.call_id).await?;

        let call_id = req.call_id.clone();
        let leg_id = req.leg_id.clone().unwrap_or_else(|| "caller".to_string());
        let min_digits = req.min_digits;
        let max_digits = req.max_digits;
        let timeout_ms = req.timeout_ms;
        let terminator = req.terminator;

        let (tap_tx, mut tap_rx) = tokio::sync::mpsc::unbounded_channel::<(Option<String>, char)>();

        // Register the tap in the gateway.
        {
            let gw = self.gateway.read();
            gw.add_dtmf_tap(call_id.clone(), tap_tx);
        }

        let gateway = self.gateway.clone();

        crate::utils::spawn(async move {
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
            let mut collected = String::new();

            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }

                match tokio::time::timeout(remaining, tap_rx.recv()).await {
                    Ok(Some((incoming_leg, digit))) => {
                        // Filter by leg_id if specified in the request.
                        if let Some(ref filter) = req.leg_id {
                            if incoming_leg.as_deref() != Some(filter.as_str()) {
                                continue;
                            }
                        }

                        // Handle terminator.
                        if let Some(term) = terminator {
                            if digit == term {
                                if collected.len() >= min_digits as usize {
                                    let gw = gateway.read();
                                    gw.remove_dtmf_tap(&call_id);
                                    gw.send_to_owner(&crate::rwi::DtmfCollected {
                                        call_id: call_id.clone(),
                                        leg_id: leg_id.clone(),
                                        digits: collected,
                                    });
                                }
                                return;
                            }
                        }

                        collected.push(digit);

                        if collected.len() >= max_digits as usize {
                            let gw = gateway.read();
                            gw.remove_dtmf_tap(&call_id);
                            gw.send_to_owner(&crate::rwi::DtmfCollected {
                                call_id: call_id.clone(),
                                leg_id: leg_id.clone(),
                                digits: collected,
                            });
                            return;
                        }
                    }
                    Ok(None) => break, // channel closed (call ended)
                    Err(_) => break,   // timeout
                }
            }

            // Timeout reached.
            let gw = gateway.read();
            gw.remove_dtmf_tap(&call_id);
            if collected.len() >= min_digits as usize {
                gw.send_to_owner(&crate::rwi::DtmfCollected {
                    call_id: call_id.clone(),
                    leg_id: leg_id.clone(),
                    digits: collected,
                });
            } else {
                gw.send_to_owner(&crate::rwi::DtmfCollectionTimeout {
                    call_id: call_id.clone(),
                    leg_id: leg_id.clone(),
                });
            }
        });

        Ok(CommandResult::Success)
    }

    async fn queue_enqueue(&self, req: QueueEnqueueRequest) -> Result<CommandResult, CommandError> {
        let _handle = self.get_handle(&req.call_id).await?;

        let queue_state = QueueState {
            queue_id: req.queue_id.clone(),
        };

        self.queue_states.insert(req.call_id.clone(), queue_state);

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::QueueJoined {
            call_id: req.call_id.clone(),
            queue_id: req.queue_id.clone(),
        });

        info!(
            call_id = %req.call_id,
            queue_id = %req.queue_id,
            "Call enqueued"
        );

        Ok(CommandResult::Success)
    }

    async fn queue_dequeue(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let _handle = self.get_handle(call_id).await?;
        let queue_id = self.queue_states.get(call_id).map(|s| s.queue_id.clone());
        self.queue_states.remove(call_id);
        if let Some(qid) = queue_id {
            let gw = self.gateway.read();
            gw.send_to_owner(&crate::rwi::QueueLeft {
                call_id: call_id.to_string(),
                queue_id: qid,
                reason: None,
            });
        }
        Ok(CommandResult::Success)
    }

    async fn queue_hold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        use crate::call::domain::{MediaSource as DomainMediaSource, PlayOptions};

        let handle = self.get_handle(call_id).await?;
        if !self.queue_states.contains_key(call_id) {
            return Err(CommandError::CommandFailed("Call not in queue".to_string()));
        }
        handle
            .send_command(CallCommand::Play {
                leg_id: Some(LegId::new(call_id)),
                source: DomainMediaSource::Silence,
                options: Some(PlayOptions {
                    loop_playback: true,
                    await_completion: true,
                    interrupt_on_dtmf: false,
                    track_id: None,
                    send_progress: false,
                    side_only: false,
                }),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::MediaHoldStarted {
            call_id: call_id.to_string(),
        });
        Ok(CommandResult::Success)
    }

    async fn queue_unhold(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        if !self.queue_states.contains_key(call_id) {
            return Err(CommandError::CommandFailed("Call not in queue".to_string()));
        }
        handle
            .send_command(CallCommand::StopPlayback {
                leg_id: Some(LegId::new(call_id)),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::MediaHoldStopped {
            call_id: call_id.to_string(),
        });
        Ok(CommandResult::Success)
    }

    async fn queue_set_priority(
        &self,
        call_id: &str,
        priority: u32,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        if !self.queue_states.contains_key(call_id) {
            return Err(CommandError::CommandFailed("Call not in queue".to_string()));
        }

        info!(call_id = %call_id, priority = %priority, "Queue priority updated");
        Ok(CommandResult::Success)
    }

    async fn queue_assign_agent(
        &self,
        call_id: &str,
        agent_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        let queue_id = {
            let state = self
                .queue_states
                .get(call_id)
                .ok_or_else(|| CommandError::CommandFailed("Call not in queue".to_string()))?;
            state.queue_id.clone()
        };

        let gw = self.gateway.read();
        gw.broadcast(&crate::rwi::QueueAgentOffered {
            call_id: call_id.to_string(),
            queue_id: queue_id.clone(),
            agent_id: agent_id.to_string(),
        });

        info!(call_id = %call_id, agent_id = %agent_id, "Agent assigned to queue call");
        Ok(CommandResult::Success)
    }

    async fn queue_requeue(
        &self,
        call_id: &str,
        queue_id: &str,
        _priority: Option<u32>,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        let old_queue_id = {
            let mut state = self
                .queue_states
                .get_mut(call_id)
                .ok_or_else(|| CommandError::CommandFailed("Call not in queue".to_string()))?;
            let old = state.queue_id.clone();
            state.queue_id = queue_id.to_string();
            old
        };

        let gw = self.gateway.read();
        gw.broadcast(&crate::rwi::QueueLeft {
            call_id: call_id.to_string(),
            queue_id: old_queue_id,
            reason: Some("requeued".to_string()),
        });

        gw.broadcast(&crate::rwi::QueueJoined {
            call_id: call_id.to_string(),
            queue_id: queue_id.to_string(),
        });

        info!(call_id = %call_id, new_queue = %queue_id, "Call requeued");
        Ok(CommandResult::Success)
    }

    async fn record_start(&self, req: RecordStartRequest) -> Result<CommandResult, CommandError> {
        use crate::call::domain::RecordConfig;

        let handle = self.get_handle(&req.call_id).await?;
        let status = Self::recorder_status(&handle).await?;
        if status.active {
            return Err(CommandError::CommandFailed(
                "Recording is already in progress".to_string(),
            ));
        }

        let path = req.storage.path.clone();
        let channels = req.channels().map_err(CommandError::CommandFailed)?;
        handle
            .send_command(CallCommand::StartRecording {
                config: RecordConfig {
                    path: path.clone(),
                    max_duration_secs: req.max_duration_secs,
                    beep: req.beep.unwrap_or(false),
                    format: None,
                    channels: Some(channels),
                    mono_caller_only: Some(false),
                    segment_type: req.segment_type.clone(),
                    segment_id: req.id.clone(),
                    notify_app: Some(false),
                },
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let status = Self::recorder_status(&handle).await?;
        if !status.active {
            return Err(CommandError::CommandFailed(
                "Recording failed to start".to_string(),
            ));
        }
        if let Some(path) = status.file_path {
            self.record_files.insert(req.call_id.clone(), path);
        } else if !path.trim().is_empty() {
            self.record_files.insert(req.call_id.clone(), path);
        }
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::RecordStarted {
            call_id: req.call_id.clone(),
        });
        Ok(CommandResult::Success)
    }

    async fn record_pause(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let status = Self::recorder_status(&handle).await?;
        if !status.active {
            return Err(CommandError::CommandFailed(
                "No recording in progress".to_string(),
            ));
        }
        handle
            .send_command(CallCommand::PauseRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let status = Self::recorder_status(&handle).await?;
        if !status.paused {
            return Err(CommandError::CommandFailed(
                "Recording failed to pause".to_string(),
            ));
        }
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::RecordPaused {
            call_id: call_id.to_string(),
        });
        Ok(CommandResult::Success)
    }

    async fn record_resume(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let status = Self::recorder_status(&handle).await?;
        if !status.active {
            return Err(CommandError::CommandFailed(
                "No recording in progress".to_string(),
            ));
        }
        handle
            .send_command(CallCommand::ResumeRecording)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let status = Self::recorder_status(&handle).await?;
        if !status.active || status.paused {
            return Err(CommandError::CommandFailed(
                "Recording failed to resume".to_string(),
            ));
        }
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::RecordResumed {
            call_id: call_id.to_string(),
        });
        Ok(CommandResult::Success)
    }

    async fn record_stop(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        let handle = self.get_handle(call_id).await?;
        let status = Self::recorder_status(&handle).await?;
        if !status.active {
            return Err(CommandError::CommandFailed(
                "No recording in progress".to_string(),
            ));
        }
        let file_path = status.file_path;
        handle
            .send_command(CallCommand::StopRecording)
            .map_err(|error| CommandError::CommandFailed(error.to_string()))?;
        let status = Self::recorder_status(&handle).await?;
        if status.active {
            return Err(CommandError::CommandFailed(
                "Recording failed to stop".to_string(),
            ));
        }
        if let Some(path) = status.file_path.or(file_path) {
            self.record_files.insert(call_id.to_string(), path);
        }
        Ok(CommandResult::Success)
    }

    async fn sip_message(
        &self,
        call_id: &str,
        content_type: &str,
        body: &str,
    ) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::SendSipMessage {
                content_type: content_type.to_string(),
                body: body.to_string(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::SipMessageReceived {
            call_id: call_id.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        });
        Ok(CommandResult::Success)
    }

    async fn sip_notify(
        &self,
        call_id: &str,
        event: &str,
        content_type: &str,
        body: &str,
    ) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::SendSipNotify {
                event: event.to_string(),
                content_type: content_type.to_string(),
                body: body.to_string(),
            })
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::SipNotifyReceived {
            call_id: call_id.to_string(),
            event: event.to_string(),
            content_type: content_type.to_string(),
            body: body.to_string(),
        });
        Ok(CommandResult::Success)
    }

    async fn sip_options_ping(&self, call_id: &str) -> Result<CommandResult, CommandError> {
        if self.sip_server.is_none() {
            return Err(CommandError::CommandFailed(
                "SIP server not available".to_string(),
            ));
        }
        let handle = self.get_handle(call_id).await?;
        handle
            .send_command(CallCommand::SendSipOptionsPing)
            .map_err(|e| CommandError::CommandFailed(e.to_string()))?;
        Ok(CommandResult::Success)
    }

    async fn conference_create(
        &self,
        req: ConferenceCreateRequest,
    ) -> Result<CommandResult, CommandError> {
        let conf_id = req.conf_id.clone();

        let manager = self.conference_manager();
        let max_participants = req.max_members.map(|m| m as usize);
        let host_leg_id = req.host_call_id.map(|h| LegId::new(&h));
        let max_dur = req.max_duration_secs;

        if let Err(e) = manager
            .create_conference_ex(
                conf_id.clone().into(),
                max_participants,
                host_leg_id,
                max_dur,
            )
            .await
        {
            let gw = self.gateway.read();
            gw.broadcast(&crate::rwi::ConferenceError {
                conf_id: conf_id.clone(),
                error: e.to_string(),
            });
            return Err(CommandError::CommandFailed(e.to_string()));
        }

        let gw = self.gateway.read();
        gw.broadcast(&crate::rwi::ConferenceCreated {
            conf_id: conf_id.clone(),
        });

        info!(conf_id = %conf_id, "Conference created");
        Ok(CommandResult::ConferenceCreated { conf_id })
    }

    /// Shared driver for per-member conference ops (add/remove/mute/unmute):
    /// run the manager op, broadcast a ConferenceError on failure, else
    /// broadcast the success event and return the success result.
    async fn run_conference_member_op<E, F, T, R>(
        &self,
        conf_id: &str,
        call_id: &str,
        op: F,
        success_event: E,
        success: R,
        action: &str,
    ) -> Result<CommandResult, CommandError>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
        E: crate::rwi::RwiEventSpec,
        R: FnOnce(String, String) -> CommandResult,
    {
        if let Err(e) = op.await {
            let gw = self.gateway.read();
            gw.broadcast(&crate::rwi::ConferenceError {
                conf_id: conf_id.to_string(),
                error: e.to_string(),
            });
            return Err(CommandError::CommandFailed(e.to_string()));
        }

        let gw = self.gateway.read();
        gw.broadcast(&success_event);

        info!(conf_id = %conf_id, call_id = %call_id, "Conference member {}", action);
        Ok(success(conf_id.to_string(), call_id.to_string()))
    }

    async fn conference_add(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(call_id).await?;

        let manager = self.conference_manager();
        self.run_conference_member_op(
            conf_id,
            call_id,
            manager.add_participant(&conf_id.into(), LegId::new(call_id)),
            crate::rwi::ConferenceMemberJoined {
                conf_id: conf_id.to_string(),
                call_id: call_id.to_string(),
            },
            |conf_id, call_id| CommandResult::ConferenceMemberAdded { conf_id, call_id },
            "added",
        )
        .await
    }

    async fn conference_remove(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        self.run_conference_member_op(
            conf_id,
            call_id,
            manager.remove_participant(&conf_id.into(), &LegId::new(call_id)),
            crate::rwi::ConferenceMemberLeft {
                conf_id: conf_id.to_string(),
                call_id: call_id.to_string(),
            },
            |conf_id, call_id| CommandResult::ConferenceMemberRemoved { conf_id, call_id },
            "removed",
        )
        .await
    }

    async fn conference_mute(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        self.run_conference_member_op(
            conf_id,
            call_id,
            manager.mute_participant(&conf_id.into(), &LegId::new(call_id)),
            crate::rwi::ConferenceMemberMuted {
                conf_id: conf_id.to_string(),
                call_id: call_id.to_string(),
            },
            |conf_id, call_id| CommandResult::ConferenceMemberMuted { conf_id, call_id },
            "muted",
        )
        .await
    }

    async fn conference_unmute(
        &self,
        conf_id: &str,
        call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        self.run_conference_member_op(
            conf_id,
            call_id,
            manager.unmute_participant(&conf_id.into(), &LegId::new(call_id)),
            crate::rwi::ConferenceMemberUnmuted {
                conf_id: conf_id.to_string(),
                call_id: call_id.to_string(),
            },
            |conf_id, call_id| CommandResult::ConferenceMemberUnmuted { conf_id, call_id },
            "unmuted",
        )
        .await
    }

    async fn conference_destroy(&self, conf_id: &str) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        if let Err(e) = manager.destroy_conference(&conf_id.into()).await {
            let gw = self.gateway.read();
            gw.broadcast(&crate::rwi::ConferenceError {
                conf_id: conf_id.to_string(),
                error: e.to_string(),
            });
            return Err(CommandError::CommandFailed(e.to_string()));
        }

        let gw = self.gateway.read();
        gw.broadcast(&crate::rwi::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        });

        info!(conf_id = %conf_id, "Conference destroyed");
        Ok(CommandResult::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        })
    }

    async fn conference_end(
        &self,
        conf_id: &str,
        host_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        let host_leg = LegId::new(host_call_id);
        let conf_id_obj = ConferenceId::from(conf_id);

        let removed = match manager.end_by_host(&conf_id_obj, &host_leg).await {
            Ok(legs) => legs,
            Err(e) => {
                let gw = self.gateway.read();
                gw.broadcast(&crate::rwi::ConferenceError {
                    conf_id: conf_id.to_string(),
                    error: e.to_string(),
                });
                return Err(CommandError::CommandFailed(e.to_string()));
            }
        };

        let gw = self.gateway.read();
        gw.broadcast(&crate::rwi::ConferenceEndedByHost {
            conf_id: conf_id.to_string(),
            host_call_id: host_call_id.to_string(),
            removed_call_ids: removed.iter().map(|l| l.to_string()).collect(),
        });

        info!(conf_id = %conf_id, host_call_id = %host_call_id, "Conference ended by host");
        Ok(CommandResult::ConferenceDestroyed {
            conf_id: conf_id.to_string(),
        })
    }

    async fn conference_merge(
        &self,
        conf_id: &str,
        call_id: &str,
        consultation_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        if manager.get_conference(&conf_id.into()).await.is_none() {
            return Err(CommandError::CommandFailed(format!(
                "conference {} not found",
                conf_id
            )));
        }

        self.get_handle(call_id).await?;
        self.get_handle(consultation_call_id).await?;

        {
            let gw = self.gateway.read();
            gw.broadcast(&crate::rwi::ConferenceMergeRequested {
                call_id: call_id.to_string(),
                consultation_call_id: consultation_call_id.to_string(),
            });
        }

        info!(
            conf_id = %conf_id,
            call_id = %call_id,
            consultation_call_id = %consultation_call_id,
            "Conference merge requested"
        );

        if manager
            .get_conference_id_for_leg(&LegId::new(call_id))
            .await
            .is_none()
        {
            let _ = self.conference_add(conf_id, call_id).await;
        }

        match self.conference_add(conf_id, consultation_call_id).await {
            Ok(_) => {
                // Ensure both legs are unheld after merge so media flows correctly
                if let Some(handle) = self.call_registry.get_handle(call_id) {
                    let _ = handle.send_command(CallCommand::Unhold {
                        leg_id: LegId::new(call_id),
                    });
                }
                if let Some(handle) = self.call_registry.get_handle(consultation_call_id) {
                    let _ = handle.send_command(CallCommand::Unhold {
                        leg_id: LegId::new(consultation_call_id),
                    });
                }

                let gw = self.gateway.read();
                gw.broadcast(&crate::rwi::ConferenceMerged {
                    conf_id: conf_id.to_string(),
                    call_id: call_id.to_string(),
                });

                info!(conf_id = %conf_id, "Conference merge successful");
                Ok(CommandResult::Success)
            }
            Err(e) => {
                let gw = self.gateway.read();
                gw.broadcast(&crate::rwi::ConferenceError {
                    conf_id: conf_id.to_string(),
                    error: e.to_string(),
                });

                gw.broadcast(&crate::rwi::ConferenceMergeFailed {
                    conf_id: conf_id.to_string(),
                    call_id: call_id.to_string(),
                    reason: e.to_string(),
                });

                warn!(conf_id = %conf_id, error = %e, "Conference merge failed");
                Err(CommandError::CommandFailed(format!(
                    "Failed to merge consultation call: {}",
                    e
                )))
            }
        }
    }

    async fn conference_seat_replace(
        &self,
        conf_id: &str,
        old_call_id: &str,
        new_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        let manager = self.conference_manager();
        if manager.get_conference(&conf_id.into()).await.is_none() {
            return Err(CommandError::CommandFailed(format!(
                "conference {} not found",
                conf_id
            )));
        }

        self.get_handle(old_call_id).await?;
        self.get_handle(new_call_id).await?;

        {
            let gw = self.gateway.read();
            gw.broadcast(&crate::rwi::ConferenceSeatReplaceStarted {
                conf_id: conf_id.to_string(),
                old_call_id: old_call_id.to_string(),
                new_call_id: new_call_id.to_string(),
            });
        }

        let old_leg = LegId::new(old_call_id);
        let new_leg = LegId::new(new_call_id);
        let old_was_member = manager.get_conference_id_for_leg(&old_leg).await.is_some();

        // Add new participant first so the conference never becomes empty
        match manager
            .add_participant(&conf_id.into(), new_leg.clone())
            .await
        {
            Ok(_) => {
                {
                    let gw = self.gateway.read();
                    gw.broadcast(&crate::rwi::ConferenceMemberJoined {
                        conf_id: conf_id.to_string(),
                        call_id: new_call_id.to_string(),
                    });
                }

                // Now remove old participant
                if old_was_member {
                    if let Err(e) = manager.remove_participant(&conf_id.into(), &old_leg).await {
                        warn!(conf_id = %conf_id, old_call_id = %old_call_id, error = %e, "Failed to remove old participant during seat replace");
                    }

                    {
                        let gw = self.gateway.read();
                        gw.broadcast(&crate::rwi::ConferenceMemberLeft {
                            conf_id: conf_id.to_string(),
                            call_id: old_call_id.to_string(),
                        });
                    }
                }

                if old_was_member && let Ok(handle) = self.get_handle(old_call_id).await {
                    let _ = handle
                        .send_command_async(CallCommand::Hangup(
                            crate::call::domain::HangupCommand::local(
                                "conference_seat_replace",
                                Some(crate::callrecord::CallRecordHangupReason::BySystem),
                                Some(200),
                            ),
                        ))
                        .await;
                }

                let gw = self.gateway.read();
                gw.broadcast(&crate::rwi::ConferenceSeatReplaceSucceeded {
                    conf_id: conf_id.to_string(),
                    old_call_id: old_call_id.to_string(),
                    new_call_id: new_call_id.to_string(),
                });

                Ok(CommandResult::Success)
            }
            Err(e) => {
                // New participant could not be added; old one was never removed,
                // so no rollback is needed
                let gw = self.gateway.read();
                gw.broadcast(&crate::rwi::ConferenceSeatReplaceFailed {
                    conf_id: conf_id.to_string(),
                    old_call_id: old_call_id.to_string(),
                    new_call_id: new_call_id.to_string(),
                    reason: e.to_string(),
                });

                Err(CommandError::CommandFailed(format!(
                    "Failed to add new participant: {}",
                    e
                )))
            }
        }
    }

    async fn set_ringback_source(
        &self,
        target_call_id: &str,
        source_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.get_handle(target_call_id).await?;
        self.get_handle(source_call_id).await?;
        let event = crate::rwi::MediaRingbackPassthroughStarted {
            source: source_call_id.to_string(),
            target: target_call_id.to_string(),
        };
        let gw = self.gateway.read();
        gw.send_to_owner_at(&target_call_id.to_string(), &event);
        gw.send_to_owner_at(&source_call_id.to_string(), &event);
        Ok(CommandResult::Success)
    }

    /// Shared driver for supervisor listen/whisper/barge/takeover: dispatch the
    /// mode command to the target session and fan the "started" event out to
    /// the supervisor, target, and (optionally) agent-owner sessions.
    async fn start_supervisor_mode<E>(
        &self,
        action: &str,
        command: CallCommand,
        event: E,
        supervisor_call_id: &str,
        target_call_id: &str,
        agent_leg: Option<&str>,
    ) -> Result<CommandResult, CommandError>
    where
        E: crate::rwi::RwiEventSpec,
    {
        let mixer_id = format!("supervisor-{}-{}", supervisor_call_id, target_call_id);
        tracing::info!(
            "supervisor_{}: creating mixer id={} sup={} target={}",
            action,
            mixer_id,
            supervisor_call_id,
            target_call_id
        );

        if let Ok(handle) = self.get_handle(target_call_id).await {
            let _ = handle.send_command(command);
        }

        info!(
            audit_event = "supervisor_action",
            action = %format!("{}_start", action),
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            agent_leg = ?agent_leg,
            result = "success",
            "Supervisor {} mode started", action
        );

        self.gateway
            .read()
            .send_to_owner_at(&supervisor_call_id.to_string(), &event);
        if self.get_handle(target_call_id).await.is_ok() {
            self.gateway
                .read()
                .send_to_owner_at(&target_call_id.to_string(), &event);
        }
        if let Some(agent_leg) = agent_leg
            && !agent_leg.is_empty()
            && self.get_handle(agent_leg).await.is_ok()
        {
            self.gateway
                .read()
                .send_to_owner_at(&agent_leg.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }

    async fn supervisor_listen(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.start_supervisor_mode(
            "listen",
            CallCommand::SupervisorListen {
                supervisor_leg: LegId::new(supervisor_call_id),
                target_leg: LegId::new(target_call_id),
                supervisor_session_id: Some(supervisor_call_id.to_string()),
            },
            crate::rwi::SupervisorListenStarted {
                supervisor_call_id: supervisor_call_id.to_string(),
                target_call_id: target_call_id.to_string(),
            },
            supervisor_call_id,
            target_call_id,
            None,
        )
        .await
    }

    async fn supervisor_whisper(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
        agent_leg: &str,
    ) -> Result<CommandResult, CommandError> {
        self.start_supervisor_mode(
            "whisper",
            CallCommand::SupervisorWhisper {
                supervisor_leg: LegId::new(supervisor_call_id),
                target_leg: LegId::new(target_call_id),
                supervisor_session_id: None,
            },
            crate::rwi::SupervisorWhisperStarted {
                supervisor_call_id: supervisor_call_id.to_string(),
                target_call_id: target_call_id.to_string(),
            },
            supervisor_call_id,
            target_call_id,
            Some(agent_leg),
        )
        .await
    }

    async fn supervisor_barge(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
        agent_leg: &str,
    ) -> Result<CommandResult, CommandError> {
        self.start_supervisor_mode(
            "barge",
            CallCommand::SupervisorBarge {
                supervisor_leg: LegId::new(supervisor_call_id),
                target_leg: LegId::new(target_call_id),
                supervisor_session_id: None,
            },
            crate::rwi::SupervisorBargeStarted {
                supervisor_call_id: supervisor_call_id.to_string(),
                target_call_id: target_call_id.to_string(),
            },
            supervisor_call_id,
            target_call_id,
            Some(agent_leg),
        )
        .await
    }

    async fn supervisor_takeover(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        self.start_supervisor_mode(
            "takeover",
            CallCommand::SupervisorTakeover {
                supervisor_leg: LegId::new(supervisor_call_id),
                target_leg: LegId::new(target_call_id),
                supervisor_session_id: None,
            },
            crate::rwi::SupervisorTakeoverStarted {
                supervisor_call_id: supervisor_call_id.to_string(),
                target_call_id: target_call_id.to_string(),
            },
            supervisor_call_id,
            target_call_id,
            None,
        )
        .await
    }

    async fn supervisor_stop(
        &self,
        supervisor_call_id: &str,
        target_call_id: &str,
    ) -> Result<CommandResult, CommandError> {
        if let Ok(handle) = self.get_handle(target_call_id).await {
            let _ = handle.send_command(CallCommand::SupervisorStop {
                supervisor_leg: LegId::new(supervisor_call_id),
            });
        }

        info!(
            audit_event = "supervisor_action",
            action = "stop",
            supervisor_call_id = %supervisor_call_id,
            target_call_id = %target_call_id,
            result = "success",
            "Supervisor mode stopped"
        );

        let event = crate::rwi::SupervisorModeStopped {
            supervisor_call_id: supervisor_call_id.to_string(),
            target_call_id: target_call_id.to_string(),
        };
        self.gateway
            .read()
            .send_to_owner_at(&supervisor_call_id.to_string(), &event);
        if self.get_handle(target_call_id).await.is_ok() {
            self.gateway
                .read()
                .send_to_owner_at(&target_call_id.to_string(), &event);
        }
        Ok(CommandResult::Success)
    }
}

#[derive(Debug)]
pub enum CommandResult {
    Success,
    ListCalls(Vec<CallInfo>),
    CallFound {
        call_id: String,
    },
    Originated {
        call_id: String,
    },
    MediaPlay {
        track_id: String,
    },
    TransferAttended {
        original_call_id: String,
        consultation_call_id: String,
    },
    ConferenceCreated {
        conf_id: String,
    },
    ConferenceMemberAdded {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberRemoved {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberMuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceMemberUnmuted {
        conf_id: String,
        call_id: String,
    },
    ConferenceDestroyed {
        conf_id: String,
    },
    SessionResumed {
        replayed_count: u64,
        events: Vec<serde_json::Value>,
    },
    CallResumed {
        call_id: String,
        replayed_count: u64,
        events: Vec<serde_json::Value>,
    },
    CallVar {
        key: String,
        value: Option<String>,
    },
}

/// Default recorder file for an originated call's `record` option:
/// `<[recording].path>/<sanitized call_id>.wav` (mirrors
/// `AppState::get_recorder_file`), creating the directory when needed.
fn default_originate_recorder_path(server: &SipServerRef, call_id: &str) -> String {
    let policy_guard = server.recording_policy.load();
    let root = policy_guard
        .as_ref()
        .as_ref()
        .map(|policy| policy.recorder_path())
        .unwrap_or_else(crate::config::default_config_recorder_path);
    let root = std::path::Path::new(&root);
    let _ = std::fs::create_dir_all(root);
    let mut file = root.join(crate::utils::sanitize_id(call_id));
    file.set_extension("wav");
    file.to_string_lossy().to_string()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallInfo {
    pub session_id: String,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub direction: String,
    pub status: String,
    pub started_at: String,
    pub answered_at: Option<String>,
}

#[derive(Debug)]
pub enum CommandError {
    CallNotFound(String),
    CommandFailed(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::CallNotFound(id) => write!(f, "Call not found: {}", id),
            CommandError::CommandFailed(msg) => write!(f, "Command failed: {}", msg),
        }
    }
}

impl serde::Serialize for CommandError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Transfer handling methods for RwiCommandProcessor
impl RwiCommandProcessor {
    /// Handle blind transfer
    async fn handle_transfer(
        &self,
        call_id: String,
        target: String,
        _attended: bool,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        if self.call_registry.get_handle(&call_id).is_none() {
            return Err(CommandError::CallNotFound(call_id));
        }

        // Use TransferController to execute transfer
        let controller = self.transfer_controller.read().await;

        match controller
            .execute_blind_transfer(call_id.clone(), target.clone())
            .await
        {
            Ok(_tx) => {
                // Transfer initiated successfully (REFER accepted)
                Ok(CommandResult::Success)
            }
            Err(e) => {
                // Transfer failed
                Err(CommandError::CommandFailed(format!(
                    "Transfer failed: {}",
                    e.as_str()
                )))
            }
        }
    }

    /// Handle attended transfer initiation
    async fn handle_attended_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<CommandResult, CommandError> {
        // Verify call exists
        if self.call_registry.get_handle(&call_id).is_none() {
            return Err(CommandError::CallNotFound(call_id));
        }

        let controller = self.transfer_controller.read().await;

        match controller
            .initiate_attended_transfer(call_id.clone(), target)
            .await
        {
            Ok(tx) => {
                let consultation_call_id = tx
                    .consultation_call_id
                    .clone()
                    .unwrap_or_else(|| tx.transfer_id.clone());
                Ok(CommandResult::TransferAttended {
                    original_call_id: call_id,
                    consultation_call_id,
                })
            }
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Attended transfer failed: {}",
                e.as_str()
            ))),
        }
    }

    async fn handle_transfer_replace(
        &self,
        call_id: String,
        target: String,
    ) -> Result<CommandResult, CommandError> {
        if self.call_registry.get_handle(&call_id).is_none() {
            return Err(CommandError::CallNotFound(call_id));
        }

        let controller = self.transfer_controller.read().await;

        match controller
            .execute_replace_transfer(call_id.clone(), target)
            .await
        {
            Ok(_) => Ok(CommandResult::Success),
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Replace transfer failed: {}",
                e.as_str()
            ))),
        }
    }

    /// Handle attended transfer completion
    async fn handle_transfer_complete(
        &self,
        call_id: String,
        consultation_call_id: String,
    ) -> Result<CommandResult, CommandError> {
        let controller = self.transfer_controller.read().await;

        match controller
            .complete_attended_transfer(call_id, consultation_call_id)
            .await
        {
            Ok(_tx) => Ok(CommandResult::Success),
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Transfer complete failed: {}",
                e.as_str()
            ))),
        }
    }

    /// Handle attended transfer cancellation
    async fn handle_transfer_cancel(
        &self,
        consultation_call_id: String,
    ) -> Result<CommandResult, CommandError> {
        let controller = self.transfer_controller.read().await;

        match controller
            .cancel_attended_transfer(consultation_call_id)
            .await
        {
            Ok(_tx) => Ok(CommandResult::Success),
            Err(e) => Err(CommandError::CommandFailed(format!(
                "Transfer cancel failed: {}",
                e.as_str()
            ))),
        }
    }
}
