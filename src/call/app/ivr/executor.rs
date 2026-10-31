use super::common::{self, ActionResult, SessionData, TerminalAction, WaitEvent};
use super::config::{ActionNode, EntryAction};
use super::fallback::{self, IVR_FALLBACK_USED_KEY};
use super::provider::{
    ActionProvider, ProviderContext, ProviderEvent, SessionContext, SessionEndReason, SessionEndTag,
};
use super::trace::{IvrTraceCollector, IvrTraceEntry, IvrTraceSession};
use crate::call::app::{
    AppAction, AppEvent, ApplicationContext, CallApp, CallAppType, CallController, RecordingInfo,
};
use crate::config::IvrFallbackConfig;
use async_trait::async_trait;
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub(crate) const IVR_STATUS_KEY: &str = "ivr_status";
pub(crate) const IVR_NAME_KEY: &str = "ivr_name";
pub(crate) const IVR_END_REASON_KEY: &str = "ivr_end_reason";
pub(crate) const IVR_LAST_ERROR_KEY: &str = "ivr_last_error";

/// Shared session var written by `on_exit` when a resumable hand-off
/// (bridge / queue / jump) suspends the flow: the SUSPENDED execution's
/// runtime generation. The proxy's compensating `session_end` trace reads it
/// (via `app_context`) so a real hangup while suspended carries the ORIGINAL
/// execution's identity — never a successor's, never derived from `extra`.
/// Cleared by the next `on_enter` (any new instance owns the lifecycle from
/// there); internal key, never forwarded to the provider as an ivr param.
pub(crate) const IVR_SUSPENDED_EXECUTION_ID_KEY: &str = "_ivr_suspended_execution_id";

/// Default number of consecutive no-input prompt cycles before the step IVR
/// asks the provider to resolve (DtmfMenuTimeout). 0 disables the runaway guard.
const DEFAULT_MAX_REPEAT_PROMPTS: u32 = 10;

/// Session var holding the per-IVR re-entry bookkeeping (JSON map) for the
/// re-entry loop guard. Lives in shared session vars so the counter survives
/// across app restarts within one call; never forwarded to the provider
/// (filtered in `on_enter`).
pub(crate) const IVR_REENTRY_STATE_KEY: &str = "_ivr_reentry_state";

/// Default number of times the SAME IVR flow may be (re)started within the
/// re-entry window without real caller input before the executor diverts to
/// the IVR fallback (or hangs up) instead of POSTing another /start cycle.
/// Guards against a provider flow whose last node never terminates the call
/// (e.g. queue/transfer back into this IVR): each restart is a fresh
/// start→step→end cycle that a stateless provider repeats forever.
/// 0 disables the guard.
const DEFAULT_MAX_FLOW_RESTARTS: u32 = 3;

/// Default window (seconds) in which consecutive re-entries of the same IVR
/// flow are counted as a loop. Re-entries spaced further apart reset the
/// counter, so legitimate queue round-trips (ringing + wait) are not
/// misjudged as restart storms.
const DEFAULT_REENTRY_WINDOW_SECS: u64 = 60;

/// Per-flow re-entry bookkeeping persisted under [`IVR_REENTRY_STATE_KEY`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct IvrReentryEntry {
    /// (Re)entries of this flow within the current window.
    count: u32,
    /// RFC3339 timestamp of the last (re)entry.
    ts: String,
    /// `step_id` of the previous cycle's FIRST served node, when known.
    /// Resumed entries serving a different node are flow progress and never
    /// accumulate; only a repeated node counts toward the loop guard.
    /// Absent for state written by older builds (serde default) or for
    /// providers that do not return a step id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_node_id: Option<String>,
}

/// Originating bridge-step context injected by the proxy (`bridge_step_ctx`
/// ivr param) when a resumable voip_bridge returns with buffered DTMF. The
/// executor re-reports the suppressed bridge-DTMF trace from this context —
/// see `StepIvrApp::start`'s resume branch.
#[derive(Debug, Clone, serde::Deserialize)]
struct BridgeStepTraceCtx {
    step_id: Option<String>,
    step_name: Option<String>,
    extra: Option<serde_json::Value>,
    step_start_time: Option<String>,
    step_index: Option<u32>,
}

pub struct StepIvrApp {
    provider: Box<dyn ActionProvider>,
    pub(crate) provider_session: Option<SessionContext>,
    pub(crate) current_node: Option<ActionNode>,
    pub(crate) sess: SessionData,
    pending_menu: Option<PendingMenu>,
    current_track_id: Option<String>,
    interrupt_on_dtmf: bool,
    /// Whether digits must be ignored while the current non-interruptible prompt is playing.
    ignore_prompt_dtmf: bool,
    /// Whether the IVR is currently expecting DTMF input.
    ///
    /// Set to `true` once a `DtmfMenu` greeting finishes playing (or when the
    /// menu has no greeting audio), indicating that the caller may press a key.
    /// Cleared when a DTMF digit is processed, the menu times out, or a new
    /// non-DtmfMenu step begins.
    ///
    /// DTMF events that arrive while this is `false` (e.g. a key pressed during
    /// a plain `Prompt` playback) are silently ignored instead of being
    /// forwarded to the provider, which could derail the flow.
    awaiting_dtmf: bool,
    tts_service: Option<Arc<crate::tts::TtsService>>,
    pub(crate) trace: Option<Arc<IvrTraceCollector>>,
    step_index: u32,
    pub(crate) ivr_name: Option<String>,
    pub(crate) rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
    /// Name of the route that dispatched this call into the IVR.
    route_name: Option<String>,
    /// Passthrough data set by the external provider (echoed back each step).
    custom_data: Option<serde_json::Value>,
    /// Transparent extra JSON object from provider — stored and passed through in events.
    pub(crate) extra: Option<serde_json::Value>,
    /// Previous step start time (RFC3339) for timing reporting.
    step_prev_start_time: Option<String>,
    /// Previous step wall-clock duration in ms.
    step_prev_duration_ms: u64,
    /// How this session entered IVR: `None` (fresh inbound), `"agent"`, `"queue"`.
    transferred_from: Option<String>,
    /// Last transfer target string (for SessionEndReason classification).
    last_transfer_target: Option<String>,
    /// Whether the last terminal action was caused by a DTMF timeout (max
    /// retries exceeded). Used in `on_exit` to classify the end reason as
    /// `Timeout` instead of the generic `Hangup`.
    timeout_induced: bool,
    /// Current step start time (ISO UTC) — set when a step begins, used for step_start_time.
    pub(crate) current_step_start_time: Option<String>,
    /// Monotonic instant when the current step really started (edge-cli response received).
    /// Used to compute the complete step duration including async waits (playback, user input).
    step_start_instant: Option<std::time::Instant>,
    /// Monotonic instant when the pending WaitFor step started (for duration calculation).
    pending_start_instant: Option<std::time::Instant>,
    /// Pending trace entry for a WaitFor step — finalized and recorded when the next event arrives.
    pending_trace: Option<IvrTraceEntry>,
    /// Params passed from a previous IVR via JumpIvr query string, merged into
    /// session variables on `on_enter` so the provider and variable substitution
    /// can reference them.
    ivr_params: Option<HashMap<String, String>>,
    /// Current step provider response metadata.
    pub(crate) current_step_id: Option<String>,
    current_step_name: Option<String>,
    /// Structured trigger that caused the current step (e.g. dtmf with digit detail).
    current_trigger: Option<crate::rwi::TriggerInfo>,
    pub(crate) runtime_vars: Option<Arc<DashMap<String, String>>>,
    /// Session extensions clone stashed in on_enter for use in on_exit.
    /// Only populated when the IVR was started via `ivr.exec`.
    pub(crate) session_extensions: Option<crate::proxy::proxy_call::session_hooks::SessionExtensions>,
    /// Runtime generation paired with `session_extensions` for result ownership.
    pub(crate) app_execution_id: Option<u64>,
    /// DTMF digits received while the current step is non-interruptible (or the
    /// step provider response is in flight). Delivered to the provider on the
    /// next step instead of being silently dropped, so caller input is never lost.
    pub(crate) pending_dtmf: VecDeque<String>,
    /// Runaway guard: consecutive `audio_complete` cycles with no caller input
    /// after which the app probes the provider with `DtmfMenuTimeout`. 0 disables.
    max_repeat_prompts: u32,
    /// Consecutive no-input prompt cycles (reset on any real input / terminal step).
    no_input_prompts: u32,
    /// Whether a `DtmfMenuTimeout` probe was already sent and ignored.
    probe_pending: bool,
    /// Global `[proxy.ivr_fallback]` — when set, provider `/step` failures jump
    /// to a built-in IVR instead of the hardcoded error.wav hangup.
    ivr_fallback: Option<Arc<IvrFallbackConfig>>,
    /// `delay_after_ms` of the prompt currently playing, held in
    /// `on_audio_complete` before the flow advances to the next step.
    pending_audio_delay_ms: u64,
    /// True when this instance resumes an already-started logical flow
    /// (`ivr_resumed` / `_ivr_resume` markers): the first node's lifecycle
    /// trace trigger is `resume` (or `dtmf`), never a second `session_start`.
    resumed_flow: bool,
    /// Session handle captured in `on_enter`, used to route step-level errors
    /// through the unified `CallCommand::ReportCallError` path (log + RWI +
    /// CDR trace). `None` in unit tests that drive the app without a session.
    session_handle: Option<crate::proxy::proxy_call::sip_session::SipSessionHandle>,
    /// Re-entry loop guard: how many times the SAME IVR flow may restart
    /// within `reentry_window_secs` without real caller input before the app
    /// diverts to the IVR fallback / hangs up. 0 disables the guard.
    max_flow_restarts: u32,
    /// Re-entry loop guard window in seconds.
    reentry_window_secs: u64,
}

#[derive(Clone)]
struct PendingMenu {
    entries: HashMap<String, ActionNode>,
    timeout_action: Option<Box<ActionNode>>,
    invalid_action: Option<Box<ActionNode>>,
    max_retries: u32,
    retry_count: u32,
    timeout_ms: u64,
}

impl StepIvrApp {
    pub fn with_provider(provider: Box<dyn ActionProvider>) -> Self {
        Self {
            provider,
            provider_session: None,
            current_node: None,
            sess: SessionData::default(),
            pending_menu: None,
            current_track_id: None,
            interrupt_on_dtmf: false,
            ignore_prompt_dtmf: false,
            awaiting_dtmf: false,
            tts_service: None,
            trace: None,
            step_index: 0,
            ivr_name: None,
            rwi_gateway: None,
            route_name: None,
            custom_data: None,
            extra: None,
            step_prev_start_time: None,
            step_prev_duration_ms: 0,
            transferred_from: None,
            last_transfer_target: None,
            timeout_induced: false,
            current_step_start_time: None,
            step_start_instant: None,
            pending_start_instant: None,
            pending_trace: None,
            current_step_id: None,
            current_step_name: None,
            current_trigger: None,
            runtime_vars: None,
            ivr_params: None,
            session_extensions: None,
            app_execution_id: None,
            pending_dtmf: VecDeque::new(),
            max_repeat_prompts: DEFAULT_MAX_REPEAT_PROMPTS,
            no_input_prompts: 0,
            probe_pending: false,
            ivr_fallback: None,
            pending_audio_delay_ms: 0,
            resumed_flow: false,
            session_handle: None,
            max_flow_restarts: DEFAULT_MAX_FLOW_RESTARTS,
            reentry_window_secs: DEFAULT_REENTRY_WINDOW_SECS,
        }
    }

    /// Test-only convenience: build with a default `StepProvider` for `url`.
    /// Production code uses [`StepIvrApp::with_provider`].
    #[cfg(test)]
    pub fn new(url: impl Into<String>, http_client: reqwest::Client) -> Self {
        let provider = Box::new(super::provider::StepProvider::new(url, http_client));
        Self::with_provider(provider)
    }

    pub fn with_tts(mut self, tts: Option<crate::tts::TtsConfig>) -> Self {
        self.tts_service = tts.map(|cfg| Arc::new(crate::tts::TtsService::new(cfg)));
        self
    }

    /// Attach the IVR trace collector for debugging.
    /// If None is passed, falls back to the global IVR_TRACE (set by IVR Editor addon).
    pub fn with_trace(mut self, trace: Option<Arc<IvrTraceCollector>>) -> Self {
        self.trace = trace;
        self
    }

    fn effective_trace(&self) -> Option<Arc<IvrTraceCollector>> {
        self.trace.clone()
    }

    /// Attach the RWI gateway for real-time event emission.
    pub fn with_rwi_gateway(mut self, gw: Option<crate::rwi::RwiGatewayRef>) -> Self {
        self.rwi_gateway = gw;
        self
    }

    /// Set the IVR name for identification in traces.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.ivr_name = Some(name.into());
        self
    }

    /// Set extra params passed from a previous IVR via JumpIvr query string.
    /// These are merged into session variables on `on_enter`.
    pub fn with_ivr_params(mut self, params: serde_json::Value) -> Self {
        if let Some(obj) = params.as_object() {
            let mut map = HashMap::new();
            for (k, v) in obj {
                if let Some(vs) = v.as_str() {
                    map.insert(k.clone(), vs.to_string());
                }
            }
            self.ivr_params = Some(map);
        }
        self
    }

    /// Set the route name that dispatched this call into the IVR.
    pub fn with_route_name(mut self, name: Option<String>) -> Self {
        self.route_name = name;
        self
    }

    /// Mark this session as re-entered from agent or queue.
    pub fn with_transferred_from(mut self, from: Option<String>) -> Self {
        self.transferred_from = from;
        self
    }

    /// Configure the runaway-loop guard: after `n` consecutive no-input prompt
    /// cycles the app probes the provider with `DtmfMenuTimeout`, and hangs up
    /// if it is still ignored. `0` disables the guard.
    pub fn with_max_repeat_prompts(mut self, n: u32) -> Self {
        self.max_repeat_prompts = n;
        self
    }

    /// Configure the re-entry loop guard: when the same IVR flow is restarted
    /// more than `n` times within [`Self::with_reentry_window_secs`] without
    /// real caller input, the next entry diverts to the IVR fallback (or hangs
    /// up) instead of POSTing another `/start` cycle. `0` disables the guard.
    pub fn with_max_flow_restarts(mut self, n: u32) -> Self {
        self.max_flow_restarts = n;
        self
    }

    /// Re-entry loop guard window in seconds (see
    /// [`Self::with_max_flow_restarts`]). Re-entries spaced further apart
    /// reset the counter.
    pub fn with_reentry_window_secs(mut self, secs: u64) -> Self {
        self.reentry_window_secs = secs;
        self
    }

    /// Attach global `[proxy.ivr_fallback]` for session-level recovery.
    pub fn with_ivr_fallback(mut self, config: Option<Arc<IvrFallbackConfig>>) -> Self {
        self.ivr_fallback = config;
        self
    }

    fn pending_take(&mut self) -> Option<IvrTraceEntry> {
        self.pending_trace.take()
    }

    /// Seed an `IvrTraceEntry` with the fields common to every trace site
    /// (identity, current step, timings zeroed, no end markers). Callers
    /// override the per-site fields (step end time, duration, end markers,
    /// successor ids) via struct-update syntax.
    #[allow(clippy::too_many_arguments)]
    fn trace_entry(
        &self,
        session_id: &str,
        caller: &str,
        callee: &str,
        trigger: crate::rwi::TriggerInfo,
        action_type: &str,
        action_json: Option<String>,
        error: Option<String>,
        step_id: Option<String>,
        step_name: Option<String>,
    ) -> IvrTraceEntry {
        IvrTraceEntry {
            session_id: session_id.to_string(),
            caller: caller.to_string(),
            callee: callee.to_string(),
            step_index: self.step_index,
            trigger,
            provider_url: None,
            action_type: action_type.to_string(),
            action_json,
            error,
            step_id,
            step_name,
            step_start_time: self.current_step_start_time.clone(),
            step_end_time: None,
            duration_ms: 0,
            extra: self.extra.clone(),
            end_reason: None,
            end_detail: None,
            next_node_id: None,
            next_step_id: None,
            end: None,
        }
    }

    pub(crate) fn record_trace(&self, mut entry: IvrTraceEntry) {
        // Completion contract, enforced at the SINGLE choke point every step
        // trace passes through: each emitted entry carries `step_end_time`
        // (the completion marker) and `step_start_time` (falling back to the
        // end so `end >= start` always holds) — `duration_ms` is non-optional
        // by type. A site that left a stamp unset is repaired here, so
        // consumers can rely on both fields unconditionally. The same
        // normalized entry feeds the CDR collector and the RWI fan-out.
        if entry.step_end_time.is_none() {
            entry.step_end_time = Some(chrono::Utc::now().to_rfc3339());
        }
        if entry.step_start_time.is_none() {
            entry.step_start_time = entry.step_end_time.clone();
        }
        if let Some(t) = self.effective_trace() {
            let ent = entry.clone();
            crate::utils::spawn(async move {
                t.record_entry(ent).await;
            });
        }
        if let Some(ref gw) = self.rwi_gateway {
            let call_id = entry.session_id.clone();
            let ev = crate::rwi::IvrStepTrace {
                call_id: call_id.clone(),
                session_id: entry.session_id.clone(),
                // Identity comes straight from THIS runtime instance — the
                // single source of truth for every executor-emitted trace.
                app_execution_id: self.app_execution_id,
                caller: entry.caller.clone(),
                callee: entry.callee.clone(),
                step_index: entry.step_index,
                trigger: entry.trigger.clone(),
                action_type: entry.action_type.clone(),
                action_json: entry.action_json.clone(),
                error: entry.error.clone(),
                step_id: entry.step_id,
                step_name: entry.step_name,
                step_start_time: entry.step_start_time,
                step_end_time: entry.step_end_time,
                duration_ms: entry.duration_ms,
                extra: entry.extra,
                sip_headers: Some(self.sess.sip_headers.clone()),
                end_reason: entry.end_reason,
                end_detail: entry.end_detail,
                next_node_id: entry.next_node_id,
                next_step_id: entry.next_step_id,
                end: entry.end,
            }
            // Belt-and-braces: the entry was normalized above, so this is a
            // no-op today — it keeps the "emitted event always carries both
            // stamps" guarantee local to the construction site as well.
            .with_completion_stamps();
            let guard = gw.read();
            guard.fan_out(&call_id, &ev);
        }
    }

    /// Emit the unified `call_error` RWI event for a step-IVR failure, logged
    /// at the registry severity's level. Used for provider `/step` and `/fail`
    /// failures; the CDR-side trace entry is the existing `ivr_step_trace`.
    fn report_step_error(
        &self,
        info: &'static crate::call_errors::CallErrInfo,
        detail: Option<serde_json::Value>,
    ) {
        // Preferred: route through the session so the error is logged, emitted
        // as `call_error`, AND recorded as a `TraceKind::Error` CDR entry.
        if let Some(ref handle) = self.session_handle {
            let _ = handle.send_command(crate::call::domain::CallCommand::ReportCallError {
                stage: "ivr_step".to_string(),
                app: info.app.to_string(),
                code: info.code.to_string(),
                severity: info.severity,
                message: info.message.to_string(),
                sip_status: info.sip_status,
                detail,
            });
            return;
        }
        // Fallback (no session, e.g. unit tests): log + RWI event only.
        let call_id = self.provider_session_context().session_id;
        crate::call_errors::log_call_error(&call_id, "ivr_step", info, detail.clone());
        if let Some(ref gw) = self.rwi_gateway {
            let ev = crate::rwi::CallError::from_info(call_id.clone(), "ivr_step", info, detail);
            gw.read().fan_out(&call_id, &ev);
        }
    }

    fn increment_total_steps(&self) {
        if let Some(t) = self.effective_trace() {
            let sid = self.provider_session_context().session_id;
            crate::utils::spawn(async move {
                t.increment_steps(&sid).await;
            });
        }
    }

    /// Write a string into the shared session-extensions bag (visible to the
    /// owning `SipSession` via `session_ext_get` — same underlying map).
    fn set_session_ext(&self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if value.is_empty() {
            return;
        }
        if let Some(ref ext) = self.session_extensions {
            let mut guard = ext.write();
            if let Some(existing) = guard.get_mut::<std::collections::HashMap<String, String>>() {
                existing.insert(key.to_string(), value);
            } else {
                let mut m = std::collections::HashMap::new();
                m.insert(key.to_string(), value);
                guard.insert(m);
            }
        }
    }

    async fn record_session_start(
        &self,
        session_id: &str,
        caller: &str,
        callee: &str,
        direction: &str,
    ) {
        if let Some(t) = self.effective_trace() {
            let sess = IvrTraceSession {
                session_id: session_id.to_string(),
                caller: caller.to_string(),
                callee: callee.to_string(),
                direction: direction.to_string(),
                ivr_name: self.ivr_name.clone(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                total_steps: 0,
                status: "active".to_string(),
            };
            crate::utils::spawn(async move {
                t.record_session(sess).await;
            });
        }
    }

    async fn record_session_end(&self, status: &str) {
        let session_id = self.provider_session_context().session_id;
        if let Some(t) = self.effective_trace() {
            let sid = session_id;
            let st = status.to_string();
            crate::utils::spawn(async move {
                t.update_session_end(&sid, chrono::Utc::now(), &st).await;
            });
        }
    }

    fn set_runtime_status(&self, ctx: &ApplicationContext, status: &str) {
        ctx.set_var(IVR_STATUS_KEY, status);
        if let Some(name) = &self.ivr_name {
            ctx.set_var(IVR_NAME_KEY, name);
        }
    }

    fn set_runtime_error(&self, ctx: &ApplicationContext, error: &str) {
        ctx.set_var(IVR_LAST_ERROR_KEY, error);
    }

    fn set_runtime_error_shared(&self, error: &str) {
        if let Some(vars) = &self.runtime_vars {
            vars.insert(IVR_LAST_ERROR_KEY.to_string(), error.to_string());
            if let Some(name) = &self.ivr_name {
                vars.insert(IVR_NAME_KEY.to_string(), name.clone());
            }
        }
    }

    fn set_runtime_status_shared(&self, status: &str) {
        if let Some(vars) = &self.runtime_vars {
            vars.insert(IVR_STATUS_KEY.to_string(), status.to_string());
            if let Some(name) = &self.ivr_name {
                vars.insert(IVR_NAME_KEY.to_string(), name.clone());
            }
        }
    }

    fn set_runtime_end_reason_shared(&self, reason: &str) {
        if let Some(vars) = &self.runtime_vars {
            vars.insert(IVR_END_REASON_KEY.to_string(), reason.to_string());
            vars.insert(IVR_STATUS_KEY.to_string(), reason.to_string());
            if let Some(name) = &self.ivr_name {
                vars.insert(IVR_NAME_KEY.to_string(), name.clone());
            }
        }
    }

    /// Freeze the SUSPENDED execution's identity in shared session vars so
    /// the proxy's compensating `session_end` (real hangup while suspended on
    /// a bridge / queue / jump hand-off) keys to THIS execution, not the
    /// successor that may start later. No identity → no stash (the field is
    /// then omitted downstream; it is never guessed).
    pub(crate) fn stash_suspended_execution_id(&self) {
        if let (Some(vars), Some(id)) = (&self.runtime_vars, self.app_execution_id) {
            vars.insert(IVR_SUSPENDED_EXECUTION_ID_KEY.to_string(), id.to_string());
        }
    }

    fn end_reason_label(
        reason: &crate::call::app::ExitReason,
        transfer_target: Option<&str>,
    ) -> &'static str {
        match reason {
            crate::call::app::ExitReason::Normal => "normal",
            crate::call::app::ExitReason::Hangup => "hangup",
            crate::call::app::ExitReason::RemoteHangup(_) => "remote_hangup",
            crate::call::app::ExitReason::Transferred => match transfer_target {
                Some(t) => Self::transfer_reason_label(t),
                None => "transferred",
            },
            crate::call::app::ExitReason::Error(_) => "error",
            crate::call::app::ExitReason::Cancelled => "cancelled",
            crate::call::app::ExitReason::Chained => "chained",
        }
    }

    /// Coarse label for a transfer target — mirrors
    /// [`Self::classify_transfer_target`] so the `ivr_end_reason` /
    /// `ivr_status` variables agree with the structured `SessionEndTag`.
    fn transfer_reason_label(target: &str) -> &'static str {
        match Self::classify_transfer_target(target) {
            SessionEndTag::TransferToQueue => "transfer_to_queue",
            SessionEndTag::TransferToIvr => "transfer_to_ivr",
            _ => "transfer",
        }
    }

    /// Classify a transfer target into the session-level end tag.
    ///
    /// Shared by `on_exit` (SessionEndReason construction) and the terminal
    /// step trace (resumable hand-off entries carrying the end reason inline,
    /// see `terminal_handoff_end`).
    fn classify_transfer_target(target: &str) -> SessionEndTag {
        if target.starts_with("queue:") {
            SessionEndTag::TransferToQueue
        } else if target.starts_with("toivr:") || target.starts_with("ivr:") {
            SessionEndTag::TransferToIvr
        } else {
            SessionEndTag::Transfer
        }
    }

    /// Whether a terminal transfer from `node` resumes the same logical flow
    /// (bridge/queue with a return app, or a JumpIvr) — the exact condition
    /// under which `on_exit` suppresses its `session_end` entry.
    fn is_resumable_handoff(node: &ActionNode, target: &str) -> bool {
        match &node.action {
            EntryAction::Bridge {
                return_app: Some(_),
                ..
            }
            | EntryAction::Transfer {
                return_app: Some(_),
                ..
            }
            | EntryAction::Queue {
                return_app: Some(_),
                ..
            } => true,
            _ => matches!(
                Self::classify_transfer_target(target),
                SessionEndTag::TransferToIvr
            ),
        }
    }

    fn action_type_label(action: &EntryAction) -> &'static str {
        match action {
            EntryAction::Transfer { .. } => "Transfer",
            EntryAction::Queue { .. } => "Queue",
            EntryAction::Menu { .. } => "Menu",
            EntryAction::Voicemail { .. } => "Voicemail",
            EntryAction::Play { .. } => "Play",
            EntryAction::Repeat => "Repeat",
            EntryAction::Exit => "Exit",
            EntryAction::Hangup { .. } => "Hangup",
            EntryAction::CollectExtension { .. } => "CollectExtension",
            EntryAction::Collect { .. } => "Collect",
            EntryAction::Webhook { .. } => "Webhook",
            EntryAction::PlayAndHangup { .. } => "PlayAndHangup",
            EntryAction::Back => "Back",
            EntryAction::Prompt { .. } => "Prompt",
            EntryAction::DtmfMenu { .. } => "DtmfMenu",
            EntryAction::CollectDtmf { .. } => "CollectDtmf",
            EntryAction::InputPhone { .. } => "InputPhone",
            EntryAction::InputVoice { .. } => "InputVoice",
            EntryAction::Api { .. } => "Api",
            EntryAction::Torecord { .. } => "Torecord",
            EntryAction::RecordStart { .. } => "RecordStart",
            EntryAction::RecordStop { .. } => "RecordStop",
            EntryAction::JumpIvr { .. } => "JumpIvr",
            EntryAction::RouteToAgent { .. } => "RouteToAgent",
            EntryAction::Bridge { .. } => "Bridge",
            EntryAction::StartApp { .. } => "StartApp",
        }
    }

    async fn __exec_node(
        &mut self,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let node = self
            .current_node
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("IVR executor: no current node to execute"))?;

        // Publish the current node into the session-extensions string bag so
        // session-side snapshots (transfer source recording, queue dispatch
        // context) can attribute the flow position to this node.
        {
            let node_id = node
                .step_id
                .clone()
                .or_else(|| self.current_step_id.clone())
                .or_else(|| node.step_name.clone())
                .or_else(|| self.current_step_name.clone());
            if let Some(node_id) = node_id {
                self.set_session_ext("ivr_node", node_id);
            }
        }

        // Executing a node means the flow has left any DtmfMenu waiting state.
        // Cancel the `ivr_dtmf_timeout` that was armed while the menu awaited
        // input. If it were left running it would fire `dtmf_timeout` at
        // whatever node runs next (e.g. a PROMPT_BREAK step before a
        // to-agent transfer), which the provider doesn't expect and answers
        // with hangup — killing the call before the transfer ever starts.
        ctrl.cancel_timeout("ivr_dtmf_timeout");

        // Reset the awaiting_dtmf flag unless this node is a DtmfMenu (which
        // will re-arm it via execute_node once the greeting finishes).
        if !node.action.is_dtmf_menu() {
            self.awaiting_dtmf = false;
        }

        let node_type_str = Self::action_type_label(&node.action).to_string();
        let action_json = serde_json::to_string(&node).ok();
        // Stamp THIS node's start here — not only in `request_next` — so
        // chained nodes (`node.next`, which skip `request_next`) don't inherit
        // the previous step's start. Every node's trace must bracket its own
        // execution: `step_start_time` and the duration instant are captured
        // together, keeping `duration_ms == step_end_time - step_start_time`
        // meaningful for consumers.
        let start = std::time::Instant::now();
        self.current_step_start_time = Some(chrono::Utc::now().to_rfc3339());
        self.step_start_instant = Some(start);
        let result = self.execute_node(&node, ctrl, ctx).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let step_end = chrono::Utc::now().to_rfc3339();

        let step_id = node
            .step_id
            .clone()
            .or_else(|| self.current_step_id.clone());
        let step_name = node
            .step_name
            .clone()
            .or_else(|| self.current_step_name.clone());

        let trigger = self
            .current_trigger
            .clone()
            .unwrap_or_else(|| crate::rwi::TriggerInfo::ActionExecute);

        self.current_trigger = None;

        let provider_session = self.provider_session_context();
        let session_id = provider_session.session_id;
        let caller = provider_session.caller;
        let callee = provider_session.callee;

        match result {
            Ok(action_result) => {
                // Finalize any pending trace (from a previous WaitFor step) before
                // recording the current step's trace. The current node IS that
                // step's successor — carry its id as the step's next pointer.
                if let Some(mut pending) = self.pending_take() {
                    let end = std::time::Instant::now();
                    let duration = self
                        .pending_start_instant
                        .map(|s| end.duration_since(s).as_millis() as u64)
                        .unwrap_or(0);
                    let step_end = chrono::Utc::now().to_rfc3339();
                    let next_id = node
                        .step_id
                        .clone()
                        .or_else(|| step_id.clone())
                        .or_else(|| node.step_name.clone());
                    pending.next_node_id = next_id.clone();
                    pending.next_step_id = next_id;
                    self.record_trace(IvrTraceEntry {
                        step_end_time: Some(step_end),
                        duration_ms: duration,
                        ..pending
                    });
                }
                let app_action = match action_result {
                    ActionResult::Terminal(terminal) => {
                        self.step_index += 1;
                        self.increment_total_steps();
                        // Lifecycle exactly-once: resumable hand-offs (bridge/
                        // queue with a return app, JumpIvr) suppress the
                        // executor's `session_end` entry — the flow continues
                        // in the successor. Surface the hand-off reason on
                        // THIS step's completion entry so consumers still
                        // observe why the IVR segment ended (and where it
                        // went). Plain terminal transfers keep end_reason
                        // unset — on_exit's session_end carries it.
                        let handoff_end = match &terminal {
                            TerminalAction::Transfer(target) => {
                                Self::is_resumable_handoff(&node, target).then(|| {
                                    (Self::classify_transfer_target(target), target.clone())
                                })
                            }
                            _ => None,
                        };
                        // Terminal flow marker: this step ended the flow. A
                        // resumable hand-off (queue/bridge with a return app,
                        // JumpIvr resume) does NOT end the logical flow — it
                        // continues in the successor — so no `end` marker.
                        let flow_ended = !matches!(
                            &terminal,
                            TerminalAction::Transfer(target)
                                if Self::is_resumable_handoff(&node, target)
                        );
                        self.record_trace(IvrTraceEntry {
                            step_end_time: Some(step_end),
                            duration_ms: elapsed_ms,
                            end_reason: handoff_end.as_ref().map(|(tag, _)| tag.clone()),
                            end_detail: handoff_end.map(|(_, target)| target),
                            end: flow_ended.then_some(true),
                            ..self.trace_entry(
                                &session_id,
                                &caller,
                                &callee,
                                trigger.clone(),
                                &node_type_str,
                                action_json,
                                None,
                                step_id.clone(),
                                step_name.clone(),
                            )
                        });
                        match terminal {
                            TerminalAction::Transfer(target) => {
                                self.last_transfer_target = Some(target.clone());
                                AppAction::Transfer(target)
                            }
                            TerminalAction::Hangup { reason, code } => {
                                AppAction::Hangup { reason, code }
                            }
                            TerminalAction::Exit => AppAction::Exit,
                        }
                    }
                    ActionResult::ImmediateAudioComplete => {
                        // Reuse pending-trace completion so the no-media Prompt remains observable.
                        self.pending_start_instant = self.step_start_instant;
                        self.pending_trace = Some(self.trace_entry(
                            &session_id,
                            &caller,
                            &callee,
                            trigger.clone(),
                            &node_type_str,
                            action_json,
                            None,
                            step_id.clone(),
                            step_name.clone(),
                        ));
                        self.current_node = Some(
                            self.request_next(Some(ProviderEvent::AudioComplete {
                                interrupted: false,
                            }))
                            .await?,
                        );
                        return Box::pin(self.__exec_node(ctrl, ctx)).await;
                    }
                    ActionResult::ChainedTo(next) => {
                        self.current_trigger = Some(crate::rwi::TriggerInfo::Chained);
                        self.current_node = Some(next);
                        return Box::pin(self.__exec_node(ctrl, ctx)).await;
                    }
                    ActionResult::StartSubApp(sub_app) => {
                        self.step_index += 1;
                        self.increment_total_steps();
                        return Ok(AppAction::Chain(sub_app));
                    }
                    ActionResult::WaitFor(ref wait_event) => {
                        // InputPhone collects digits synchronously; forward to provider
                        if matches!(node.action, EntryAction::InputPhone { .. }) {
                            let (provider_event, step_trigger) = match wait_event {
                                WaitEvent::DtmfCollected { .. } => {
                                    let number = self
                                        .sess
                                        .variables
                                        .get("phone_number")
                                        .cloned()
                                        .unwrap_or_default();
                                    (
                                        ProviderEvent::PhoneCollected {
                                            number: number.clone(),
                                        },
                                        crate::rwi::TriggerInfo::PhoneCollected { number },
                                    )
                                }
                                WaitEvent::DtmfTimeout => (
                                    ProviderEvent::DtmfTimeout,
                                    crate::rwi::TriggerInfo::DtmfTimeout,
                                ),
                                other => {
                                    tracing::warn!(
                                        "InputPhone wait produced an unexpected event; treating as dtmf_timeout"
                                    );
                                    let _ = other;
                                    (
                                        ProviderEvent::DtmfTimeout,
                                        crate::rwi::TriggerInfo::DtmfTimeout,
                                    )
                                }
                            };
                            self.pending_start_instant = self.step_start_instant;
                            self.pending_trace = Some(self.trace_entry(
                                &session_id,
                                &caller,
                                &callee,
                                step_trigger,
                                &node_type_str,
                                action_json,
                                None,
                                step_id.clone(),
                                step_name.clone(),
                            ));
                            // The pending trace is flushed by `request_next`
                            // once the successor resolves, so this step's
                            // event carries its next pointer.
                            self.current_node =
                                Some(self.request_next(Some(provider_event)).await?);
                            return Box::pin(self.__exec_node(ctrl, ctx)).await;
                        }

                        // Mid-call record_start / record_stop: do not wait; ask
                        // provider for the next action immediately.
                        if let WaitEvent::RecordControlDone { started } = wait_event {
                            let provider_event = match &node.action {
                                EntryAction::RecordStart {
                                    segment_type, id, ..
                                } => ProviderEvent::RecordingStarted {
                                    segment_type: segment_type
                                        .clone()
                                        .unwrap_or_else(|| "ivr".into()),
                                    segment_id: id.clone().unwrap_or_default(),
                                },
                                EntryAction::RecordStop { reason } => {
                                    ProviderEvent::RecordingStopped {
                                        reason: reason.clone(),
                                    }
                                }
                                _ => ProviderEvent::RecordingStopped { reason: None },
                            };
                            let _ = started;
                            self.step_index += 1;
                            self.increment_total_steps();
                            // Resolve the successor FIRST so the record-control
                            // step's trace can carry its next pointer.
                            let (next_node, next_id) = if let Some(ref next) = node.next {
                                let id = next.step_id.clone().or_else(|| next.step_name.clone());
                                (Some(*next.clone()), id)
                            } else {
                                let next = self.request_next(Some(provider_event)).await?;
                                let id = next.step_id.clone().or_else(|| next.step_name.clone());
                                (Some(next), id)
                            };
                            self.record_trace(IvrTraceEntry {
                                step_end_time: Some(step_end),
                                duration_ms: elapsed_ms,
                                next_node_id: next_id.clone(),
                                next_step_id: next_id,
                                ..self.trace_entry(
                                    &session_id,
                                    &caller,
                                    &callee,
                                    trigger.clone(),
                                    &node_type_str,
                                    action_json,
                                    None,
                                    step_id.clone(),
                                    step_name.clone(),
                                )
                            });
                            self.current_trigger =
                                Some(crate::rwi::TriggerInfo::RecordControl);
                            self.current_node = next_node;
                            return Box::pin(self.__exec_node(ctrl, ctx)).await;
                        }

                        let step_trigger = match wait_event {
                            WaitEvent::DtmfCollected { digit } => {
                                crate::rwi::TriggerInfo::DtmfCollected {
                                    digit: digit.clone(),
                                }
                            }
                            WaitEvent::DtmfTimeout => crate::rwi::TriggerInfo::DtmfTimeout,
                            _ => trigger.clone(),
                        };
                        self.pending_start_instant = self.step_start_instant;
                        self.pending_trace = Some(self.trace_entry(
                            &session_id,
                            &caller,
                            &callee,
                            step_trigger,
                            &node_type_str,
                            action_json,
                            None,
                            step_id.clone(),
                            step_name.clone(),
                        ));
                        AppAction::Continue
                    }
                };
                Ok(app_action)
            }
            Err(e) => {
                // Recover via /fail → IVR fallback instead of ending the session,
                // then record the failed step with the recovery node as its next
                // pointer.
                let error_text = e.to_string();
                let recovery = self.recover_from_execute_failure(e).await?;
                let next_id = recovery
                    .step_id
                    .clone()
                    .or_else(|| recovery.step_name.clone());
                self.record_trace(IvrTraceEntry {
                    step_end_time: Some(step_end),
                    duration_ms: elapsed_ms,
                    next_node_id: next_id.clone(),
                    next_step_id: next_id,
                    ..self.trace_entry(
                        &session_id,
                        &caller,
                        &callee,
                        trigger,
                        &node_type_str,
                        action_json,
                        Some(error_text),
                        step_id,
                        step_name,
                    )
                });
                self.current_node = Some(recovery);
                return Box::pin(self.__exec_node(ctrl, ctx)).await;
            }
        }
    }

    fn get_sip_headers(&self) -> Option<HashMap<String, String>> {
        if self.sess.sip_headers.is_empty() {
            None
        } else {
            Some(self.sess.sip_headers.clone())
        }
    }

    fn provider_session_context(&self) -> SessionContext {
        match self.provider_session.clone() {
            Some(ctx) => ctx,
            None => self.build_session_context_from_vars(),
        }
    }

    /// Build a [`SessionContext`] from the executor's session state. Shared by
    /// `provider_session_context` (no provider session yet) and the provider
    /// failure path.
    pub(crate) fn build_session_context_from_vars(&self) -> SessionContext {
        SessionContext {
            session_id: self
                .sess
                .variables
                .get("session_id")
                .cloned()
                .unwrap_or_default(),
            app_execution_id: 0,
            caller: self
                .sess
                .variables
                .get("caller")
                .cloned()
                .unwrap_or_default(),
            callee: self
                .sess
                .variables
                .get("callee")
                .cloned()
                .unwrap_or_default(),
            direction: self
                .sess
                .variables
                .get("direction")
                .cloned()
                .unwrap_or_default(),
            tenant_id: self.sess.variables.get("tenant_id").cloned(),
            ivr_id: self.sess.variables.get("ivr_id").cloned(),
            variables: self.sess.variables.clone(),
            sip_headers: self.get_sip_headers(),
            route_name: self.route_name.clone(),
            custom_data: self.custom_data.clone(),
            transferred_from: self.transferred_from.clone(),
        }
    }

    pub(crate) fn fallback_already_used(&self) -> bool {
        self.sess
            .variables
            .get(IVR_FALLBACK_USED_KEY)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
            || self
                .runtime_vars
                .as_ref()
                .and_then(|v| v.get(IVR_FALLBACK_USED_KEY).map(|e| e.value().clone()))
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
    }

    pub(crate) fn mark_fallback_used(&mut self) {
        self.sess
            .variables
            .insert(IVR_FALLBACK_USED_KEY.into(), "1".into());
        if let Some(ref runtime) = self.runtime_vars {
            runtime.insert(IVR_FALLBACK_USED_KEY.into(), "1".into());
        }
    }

    fn hangup_error_node() -> ActionNode {
        ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("sounds/error.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
                delay_before_ms: 0,
                delay_after_ms: 0,
            },
            ActionNode::new(EntryAction::hangup_none()),
        )
    }

    /// Identity used by the re-entry loop guard: the IVR name when known,
    /// else the route name, else a shared bucket so unnamed flows still
    /// count against one counter.
    fn reentry_identity(&self) -> String {
        self.ivr_name
            .clone()
            .or_else(|| self.route_name.clone())
            .unwrap_or_else(|| "step_ivr".to_string())
    }

    /// Clear the re-entry guard bookkeeping — called when the caller provides
    /// real input (DTMF), proving the flow is making progress.
    pub(crate) fn clear_reentry_state(&self, context: &ApplicationContext) {
        context.session_vars.remove(IVR_REENTRY_STATE_KEY);
    }

    /// Load the shared per-flow re-entry bookkeeping. Tolerates state written
    /// by older builds — missing fields fall back to their serde defaults.
    fn load_reentry_state(context: &ApplicationContext) -> HashMap<String, IvrReentryEntry> {
        context
            .session_vars
            .get(IVR_REENTRY_STATE_KEY)
            .and_then(|e| serde_json::from_str(e.value()).ok())
            .unwrap_or_default()
    }

    /// Persist the re-entry bookkeeping back into shared session vars.
    fn save_reentry_state(
        &self,
        context: &ApplicationContext,
        state: &HashMap<String, IvrReentryEntry>,
    ) {
        context.session_vars.insert(
            IVR_REENTRY_STATE_KEY.to_string(),
            serde_json::to_string(state).unwrap_or_default(),
        );
    }

    /// Window-aware entry-count bump shared by both guard phases. Resets to 1
    /// when the last entry lies outside `reentry_window_secs`. Persists the
    /// state and returns the updated count.
    fn bump_reentry_count(
        &self,
        context: &ApplicationContext,
        state: &mut HashMap<String, IvrReentryEntry>,
        ivr_key: &str,
        now: chrono::DateTime<chrono::Utc>,
        window: chrono::Duration,
    ) -> u32 {
        let count = match state.get_mut(ivr_key) {
            Some(entry) => {
                let last = entry.ts.parse::<chrono::DateTime<chrono::Utc>>().ok();
                let within_window = last
                    .map(|t| now.signed_duration_since(t) < window)
                    .unwrap_or(false);
                if within_window {
                    entry.count += 1;
                    entry.ts = now.to_rfc3339();
                    entry.count
                } else {
                    // Outside the window this is a fresh, legitimate entry.
                    entry.count = 1;
                    entry.ts = now.to_rfc3339();
                    1
                }
            }
            None => {
                state.insert(
                    ivr_key.to_string(),
                    IvrReentryEntry {
                        count: 1,
                        ts: now.to_rfc3339(),
                        last_node_id: None,
                    },
                );
                1
            }
        };
        self.save_reentry_state(context, state);
        count
    }

    /// Divert the call on a proven re-entry loop: the IVR fallback (at most
    /// once per call) or, unconfigured/already used, error tone + hangup.
    fn reentry_loop_divert(&mut self, ivr_key: &str, count: u32) -> ActionNode {
        tracing::warn!(
            ivr = %ivr_key,
            count,
            max_restarts = self.max_flow_restarts,
            window_secs = self.reentry_window_secs,
            "StepIvrApp: IVR re-entry loop detected — diverting to ivr_fallback/hangup instead of restarting the flow"
        );
        self.enter_ivr_fallback_node("reentry_loop")
    }

    /// Pre-`/start` half of the session-level re-entry loop guard.
    ///
    /// State lives in shared session vars so the counter survives across app
    /// restarts (each re-entry constructs a fresh `StepIvrApp`, which would
    /// otherwise reset every in-app counter). Semantics:
    /// * a stored count already past `max_flow_restarts` blocks the entry
    ///   BEFORE `/start`, whatever kind of entry this is (backstop — the
    ///   previous cycle already proved the loop);
    /// * FRESH entries — a plain re-entry of the flow (no `ivr_resumed`
    ///   marker: `exit`-terminated flows, `start_app` returns) — count here,
    ///   where the loop evidence exists without any provider round-trip;
    /// * RESUMED entries (voip_bridge / queue returns, JumpIvr resume — the
    ///   per-node progression of one logical flow) are NOT counted here: the
    ///   served node may simply be the next one. [`Self::note_reentry_node`]
    ///   judges them by node identity after the first `/step`.
    ///
    /// Returns the fallback [`ActionNode`] to divert to, or `None` to proceed.
    pub(crate) fn check_reentry_loop(
        &mut self,
        context: &ApplicationContext,
        resumed: bool,
    ) -> Option<ActionNode> {
        if self.max_flow_restarts == 0 {
            return None;
        }
        let ivr_key = self.reentry_identity();
        let state = Self::load_reentry_state(context);

        if let Some(entry) = state.get(&ivr_key)
            && entry.count > self.max_flow_restarts
        {
            return Some(self.reentry_loop_divert(&ivr_key, entry.count));
        }

        if resumed {
            return None;
        }

        let now = chrono::Utc::now();
        let window = chrono::Duration::seconds(self.reentry_window_secs.max(1) as i64);
        let mut state = state;
        let count = self.bump_reentry_count(context, &mut state, &ivr_key, now, window);
        if count <= self.max_flow_restarts {
            return None;
        }
        Some(self.reentry_loop_divert(&ivr_key, count))
    }

    /// Post-`/step` half of the re-entry loop guard, run once the cycle's
    /// FIRST node is known.
    ///
    /// A RESUMED entry is judged by node identity: serving the same
    /// `step_id` again within the window (no real DTMF in between — input
    /// clears the state) is the stateless-provider "repeat my last node"
    /// loop the guard targets; a DIFFERENT `step_id` means the provider is
    /// advancing through its flow (per-node bridge progression, queue
    /// round-trips) — legitimate progress that resets the counter. A node
    /// without `step_id` cannot prove progress and counts conservatively.
    /// FRESH entries were already counted in [`Self::check_reentry_loop`]
    /// and only record the node id here.
    ///
    /// Returns the fallback [`ActionNode`] when the repeated-node count
    /// exceeds `max_flow_restarts`; the caller must divert instead of
    /// executing `first_node`.
    fn note_reentry_node(
        &mut self,
        context: &ApplicationContext,
        resumed: bool,
        first_node: &ActionNode,
    ) -> Option<ActionNode> {
        if self.max_flow_restarts == 0 {
            return None;
        }
        let node_id = first_node.step_id.clone();
        if !resumed {
            // Fresh cycle: counting already happened pre-`/start`; just keep
            // the node id so a later resumed entry can compare against it.
            if let Some(id) = node_id {
                let ivr_key = self.reentry_identity();
                let mut state = Self::load_reentry_state(context);
                if let Some(entry) = state.get_mut(&ivr_key) {
                    entry.last_node_id = Some(id);
                    self.save_reentry_state(context, &state);
                }
            }
            return None;
        }

        let ivr_key = self.reentry_identity();
        let now = chrono::Utc::now();
        let window = chrono::Duration::seconds(self.reentry_window_secs.max(1) as i64);
        let mut state = Self::load_reentry_state(context);

        let repeated = match state.get(&ivr_key) {
            Some(entry) => match (&entry.last_node_id, &node_id) {
                (Some(prev), Some(cur)) => prev == cur,
                // Provider served no node id — progress cannot be proven,
                // count conservatively (previous resumed behavior).
                (_, None) => true,
                // First identified node — nothing repeated yet.
                (None, Some(_)) => false,
            },
            // No bookkeeping (cleared by real input / first entry): baseline.
            None => false,
        };

        if !repeated {
            // Legitimate progress (or a fresh baseline): reset the counter
            // and anchor it at this node.
            state.insert(
                ivr_key.clone(),
                IvrReentryEntry {
                    count: 1,
                    ts: now.to_rfc3339(),
                    last_node_id: node_id,
                },
            );
            self.save_reentry_state(context, &state);
            return None;
        }

        let count = self.bump_reentry_count(context, &mut state, &ivr_key, now, window);
        if count <= self.max_flow_restarts {
            return None;
        }
        Some(self.reentry_loop_divert(&ivr_key, count))
    }

    /// Record a trace entry + RWI `ivr_step_trace` event for a fallback decision.
    fn record_fallback_trace(&self, reason: &str, target: Option<&str>) {
        let provider_session = self.provider_session_context();
        let session_id = provider_session.session_id;
        let caller = provider_session.caller;
        let callee = provider_session.callee;
        let now = chrono::Utc::now().to_rfc3339();
        let duration_ms =
            super::trace::duration_ms_between(self.current_step_start_time.as_deref(), &now);
        self.record_trace(IvrTraceEntry {
            session_id,
            caller,
            callee,
            step_index: self.step_index,
            trigger: crate::rwi::TriggerInfo::IvrFallback {
                reason: reason.to_string(),
                target: target.map(str::to_string),
            },
            provider_url: None,
            action_type: "ivr_fallback".to_string(),
            action_json: None,
            duration_ms,
            error: Some(reason.to_string()),
            step_id: self.current_step_id.clone(),
            step_name: self.current_step_name.clone(),
            step_start_time: self.current_step_start_time.clone(),
            step_end_time: Some(now),
            extra: self.extra.clone(),
            end_reason: None,
            end_detail: None,
            next_node_id: None,
            next_step_id: None,
            end: None,
        });
    }

    /// Session-level recovery: match `[proxy.ivr_fallback]` → direct IVR, else hangup.
    pub(crate) fn enter_ivr_fallback_node(&mut self, reason: &str) -> ActionNode {
        if self.fallback_already_used() {
            tracing::warn!(
                reason = %reason,
                "StepIvrApp: IVR fallback already used, hanging up"
            );
            self.record_fallback_trace(&format!("{reason}: already_used"), None);
            return Self::hangup_error_node();
        }

        let Some(config) = self.ivr_fallback.as_ref().filter(|c| c.is_configured()) else {
            tracing::warn!(
                reason = %reason,
                "StepIvrApp: no ivr_fallback configured, hanging up"
            );
            self.record_fallback_trace(&format!("{reason}: not_configured"), None);
            return Self::hangup_error_node();
        };

        let provider_session = self.provider_session_context();
        let caller = provider_session.caller;
        let callee = provider_session.callee;
        let headers = self.get_sip_headers();

        let Some(target) =
            fallback::resolve_fallback_target(config.as_ref(), &caller, &callee, headers.as_ref())
        else {
            tracing::warn!(
                reason = %reason,
                "StepIvrApp: ivr_fallback resolved to none, hanging up"
            );
            self.record_fallback_trace(&format!("{reason}: no_match"), None);
            return Self::hangup_error_node();
        };

        self.mark_fallback_used();
        tracing::warn!(
            reason = %reason,
            target = %target,
            "StepIvrApp: entering direct IVR fallback"
        );
        self.record_fallback_trace(reason, Some(&target));
        let mut params = HashMap::new();
        params.insert(IVR_FALLBACK_USED_KEY.into(), "1".into());
        ActionNode::new(EntryAction::Transfer {
            target: format!("ivr:{target}"),
            headers: HashMap::new(),
            params,
            return_app: None,
            return_target: None,
        })
    }

    fn build_fail_provider_context(&self, reason: String) -> ProviderContext {
        let now_rfc3339 = chrono::Utc::now().to_rfc3339();
        let session = self.provider_session_context();
        ProviderContext {
            session_id: session.session_id,
            app_execution_id: session.app_execution_id,
            caller: session.caller,
            callee: session.callee,
            direction: session.direction,
            tenant_id: self.sess.variables.get("tenant_id").cloned(),
            ivr_id: self.sess.variables.get("ivr_id").cloned(),
            variables: self.sess.variables.clone(),
            sip_headers: self.get_sip_headers(),
            event: Some(ProviderEvent::Fail {
                reason,
                failed_step_id: self.current_step_id.clone(),
                failed_step_name: self.current_step_name.clone(),
                failed_action: self
                    .current_node
                    .as_ref()
                    .map(|n| Self::action_type_label(&n.action).to_string()),
            }),
            route_name: self.route_name.clone(),
            custom_data: self.custom_data.clone(),
            step_start_time: self.step_prev_start_time.clone(),
            step_end_time: Some(now_rfc3339),
            step_duration_ms: if self.step_prev_duration_ms > 0 {
                Some(self.step_prev_duration_ms)
            } else {
                None
            },
            step_index: Some(self.step_index),
            transferred_from: self.transferred_from.clone(),
        }
    }

    /// Node execute failed → POST `/fail`; on failure escalate to IVR fallback.
    pub(crate) async fn recover_from_execute_failure(
        &mut self,
        err: anyhow::Error,
    ) -> anyhow::Result<ActionNode> {
        let reason = err.to_string();
        self.report_step_error(
            &crate::call::app::error_catalog::IVR_STEP_EXECUTE_FAILED,
            Some(serde_json::json!({ "error": reason })),
        );
        self.set_runtime_error_shared(&reason);
        self.set_runtime_status_shared("execute_error");

        let ctx = self.build_fail_provider_context(reason.clone());
        match self.provider.fail_action(ctx).await {
            Ok(node) => {
                tracing::info!("StepIvrApp: /fail returned recovery action");
                Ok(node)
            }
            Err(e) => {
                self.report_step_error(
                    &crate::call::app::error_catalog::IVR_STEP_FAIL_FAILED,
                    Some(serde_json::json!({ "error": e.to_string() })),
                );
                Ok(self.enter_ivr_fallback_node(&format!("fail:{e}")))
            }
        }
    }

    pub(crate) async fn request_next(
        &mut self,
        mut event: Option<ProviderEvent>,
    ) -> anyhow::Result<ActionNode> {
        // Buffered DTMF may replace only prompt completion. Typed results from
        // later steps are authoritative and discard stale buffered digits.
        if let Some(digit) = self.pending_dtmf.pop_front() {
            if matches!(&event, Some(ProviderEvent::AudioComplete { .. }) | None) {
                tracing::info!(digit = %digit, "StepIvrApp: delivering buffered DTMF to provider");
                event = Some(ProviderEvent::Dtmf { digit });
            } else if matches!(&event, Some(ProviderEvent::Dtmf { .. })) {
                self.pending_dtmf.push_front(digit);
            } else {
                self.pending_dtmf.clear();
            }
        }

        // Runaway-loop guard: a provider that keeps re-offering the same
        // non-terminal prompt on every audio_complete (with no caller input)
        // would keep the call alive forever. After N consecutive no-input
        // cycles, probe once with DtmfMenuTimeout; if that is still ignored,
        // hang up so the app is always terminated and cleaned up.
        if self.max_repeat_prompts > 0
            && self.probe_pending
            && matches!(&event, Some(ProviderEvent::AudioComplete { .. }))
        {
            tracing::warn!("StepIvrApp: provider ignored DtmfMenuTimeout probe, hanging up");
            self.probe_pending = false;
            self.no_input_prompts = 0;
            return Ok(ActionNode::new(EntryAction::hangup_none()));
        }
        match &event {
            Some(ProviderEvent::AudioComplete { .. }) => {
                self.no_input_prompts += 1;
            }
            _ => {
                self.no_input_prompts = 0;
                self.probe_pending = false;
            }
        }
        if self.max_repeat_prompts > 0 && self.no_input_prompts > self.max_repeat_prompts {
            tracing::warn!(
                cycles = self.no_input_prompts,
                "StepIvrApp: no input for too many prompts, probing provider with DtmfMenuTimeout"
            );
            self.probe_pending = true;
            self.no_input_prompts = 0;
            event = Some(ProviderEvent::DtmfMenuTimeout);
        }

        let now_rfc3339 = chrono::Utc::now().to_rfc3339();
        let prev_step_duration_ms = self.step_prev_duration_ms;
        let session = self.provider_session_context();
        let ctx = ProviderContext {
            session_id: session.session_id,
            app_execution_id: session.app_execution_id,
            caller: session.caller,
            callee: session.callee,
            direction: session.direction,
            tenant_id: self.sess.variables.get("tenant_id").cloned(),
            ivr_id: self.sess.variables.get("ivr_id").cloned(),
            variables: self.sess.variables.clone(),
            sip_headers: self.get_sip_headers(),
            event,
            route_name: self.route_name.clone(),
            custom_data: self.custom_data.clone(),
            step_start_time: Some(
                self.step_prev_start_time
                    .clone()
                    .unwrap_or_else(|| now_rfc3339.clone()),
            ),
            step_end_time: Some(now_rfc3339.clone()),
            step_duration_ms: if prev_step_duration_ms > 0 {
                Some(prev_step_duration_ms)
            } else {
                None
            },
            step_index: Some(self.step_index),
            transferred_from: self.transferred_from.clone(),
        };

        // Finalize the pending WaitFor trace's timing but HOLD it until the
        // successor resolves, so the recorded entry can carry the successor's
        // id as its next pointer (single-notification protocol: each step
        // event answers "what ran, and what runs next").
        let mut completed_pending = self.pending_take().map(|mut pending| {
            let end = std::time::Instant::now();
            let now_str = chrono::Utc::now().to_rfc3339();
            pending.duration_ms = self
                .pending_start_instant
                .map(|s| end.duration_since(s).as_millis() as u64)
                .unwrap_or_else(|| {
                    // Bridge-resume pending traces have no live Instant (the
                    // step started in a previous app instance) — derive the
                    // duration from the stashed RFC3339 start instead.
                    super::trace::duration_ms_between(pending.step_start_time.as_deref(), &now_str)
                });
            pending.step_end_time = Some(now_str);
            pending
        });

        let start = std::time::Instant::now();
        let result = self.provider.next_action(ctx.clone()).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        // Save step timing for the next ProviderContext.
        self.step_prev_start_time = Some(now_rfc3339);
        self.step_prev_duration_ms = elapsed_ms;
        self.step_index += 1;

        // Extract transparent passthrough data from provider response.
        if let Ok(ref node) = result {
            if node.step_id.is_some() {
                self.current_step_id = node.step_id.clone();
            }
            if node.step_name.is_some() {
                self.current_step_name = node.step_name.clone();
            }
            if node.extra.is_some() {
                self.extra = node.extra.clone();
            }
        }

        // Mark when this step started executing.
        self.current_step_start_time = Some(chrono::Utc::now().to_rfc3339());
        self.step_start_instant = Some(std::time::Instant::now());

        // Store trigger event info for __exec_node to use when recording trace after node execution
        self.current_trigger = Some(match &ctx.event {
            // A resumed flow's first node must carry the `resume` lifecycle
            // trigger — the flow already emitted session_start at its true
            // first entry. (Kept for the `session_start` wire-mode fallback:
            // `StepProvider` may downgrade the `Resume` event to
            // `session_start` for legacy endpoints.)
            Some(ProviderEvent::SessionStart) if self.resumed_flow => {
                crate::rwi::TriggerInfo::Resume
            }
            Some(ProviderEvent::SessionStart) => crate::rwi::TriggerInfo::SessionStart,
            Some(ProviderEvent::Resume {
                resume_from_step_id,
            }) => crate::rwi::TriggerInfo::ResumeFromStep {
                resume_from_step_id: resume_from_step_id.clone(),
            },
            Some(ProviderEvent::AudioComplete { .. }) => {
                crate::rwi::TriggerInfo::AudioComplete
            }
            Some(ProviderEvent::Dtmf { digit }) => {
                crate::rwi::TriggerInfo::Dtmf {
                    digit: digit.clone(),
                    digits: None,
                }
            }
            Some(ProviderEvent::DtmfTimeout) => crate::rwi::TriggerInfo::DtmfTimeout,
            Some(ProviderEvent::ApiResponse { status, .. }) => {
                crate::rwi::TriggerInfo::ApiResponse { status: *status }
            }
            Some(ProviderEvent::PhoneCollected { number }) => {
                crate::rwi::TriggerInfo::PhoneCollected {
                    number: number.clone(),
                }
            }
            Some(ProviderEvent::RecordingComplete { url, duration_secs }) => {
                crate::rwi::TriggerInfo::RecordingComplete {
                    url: url.clone(),
                    duration_secs: *duration_secs,
                }
            }
            Some(ProviderEvent::RecordingStarted {
                segment_type,
                segment_id,
            }) => crate::rwi::TriggerInfo::RecordingStarted {
                segment_type: segment_type.clone(),
                segment_id: segment_id.clone(),
            },
            Some(ProviderEvent::RecordingStopped { reason }) => {
                crate::rwi::TriggerInfo::RecordingStopped {
                    reason: reason.clone(),
                }
            }
            Some(ProviderEvent::InputVoice { text, confidence }) => {
                crate::rwi::TriggerInfo::InputVoice {
                    text: text.clone(),
                    confidence: *confidence,
                }
            }
            Some(ProviderEvent::Error { reason }) => {
                crate::rwi::TriggerInfo::Error {
                    reason: reason.clone(),
                }
            }
            Some(ProviderEvent::Fail {
                reason,
                failed_step_id,
                failed_step_name,
                failed_action,
            }) => crate::rwi::TriggerInfo::Fail {
                reason: reason.clone(),
                failed_step_id: failed_step_id.clone(),
                failed_step_name: failed_step_name.clone(),
                failed_action: failed_action.clone(),
            },
            Some(ProviderEvent::DtmfMenuInvalid { digit }) => {
                crate::rwi::TriggerInfo::DtmfMenuInvalid {
                    digit: digit.clone(),
                }
            }
            Some(ProviderEvent::DtmfMenuTimeout) => {
                crate::rwi::TriggerInfo::DtmfMenuTimeout
            }
            Some(ProviderEvent::TransferResult { outcome }) => {
                crate::rwi::TriggerInfo::TransferResult {
                    outcome: outcome.clone(),
                }
            }
            None => crate::rwi::TriggerInfo::Unknown,
        });

        // Record the completed predecessor now that the successor is known:
        // carry the successor's id as the predecessor's next pointer, or mark
        // the predecessor as the flow's last observable step when the provider
        // failed and the flow drops into recovery/hangup.
        if let Some(mut pending) = completed_pending.take() {
            match &result {
                Ok(node) => {
                    let next_id = node.step_id.clone().or_else(|| node.step_name.clone());
                    pending.next_node_id = next_id.clone();
                    pending.next_step_id = next_id;
                }
                Err(_) => pending.end = Some(true),
            }
            self.record_trace(pending);
        }

        // Fallback on provider error instead of propagating
        match result {
            Ok(node) => Ok(node),
            Err(e) => {
                self.report_step_error(
                    &crate::call::app::error_catalog::IVR_STEP_NEXT_FAILED,
                    Some(serde_json::json!({ "error": e.to_string(), "step_index": self.step_index })),
                );
                let error_text = e.to_string();
                if self.step_index <= 1 {
                    self.set_runtime_status_shared("startup_error");
                } else {
                    self.set_runtime_status_shared("provider_error");
                }
                self.set_runtime_error_shared(&error_text);
                Ok(self.enter_ivr_fallback_node(&format!("step:{error_text}")))
            }
        }
    }

    async fn execute_node(
        &mut self,
        node: &ActionNode,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<ActionResult> {
        // Bridge actions hand the media (and any DTMF) to an external
        // websocket bridge, so this session ends before the caller can press
        // keys. Stash the node identity: common.rs appends it to the bridge
        // URI (`_rst_*` params) and the proxy reports bridge DTMF as
        // `ivr_step_trace` events for THIS node (contract: menu nodes carry
        // `trigger.detail.digit`). The step start time is stashed too —
        // proxy-emitted traces carry `step_end_time` and consumers derive
        // duration as `event timestamp - step_start_time`; without the real
        // start they fall back to the envelope timestamp (stamped after the
        // end) which yields end < start.
        if matches!(node.action, EntryAction::Bridge { .. }) {
            self.sess.variables.insert(
                "_bridge_step_id".into(),
                node.step_id
                    .clone()
                    .or_else(|| self.current_step_id.clone())
                    .unwrap_or_default(),
            );
            self.sess.variables.insert(
                "_bridge_step_name".into(),
                node.step_name
                    .clone()
                    .or_else(|| self.current_step_name.clone())
                    .unwrap_or_default(),
            );
            if let Some(ex) = node
                .extra
                .clone()
                .or_else(|| self.extra.clone())
                .and_then(|e| serde_json::to_string(&e).ok())
            {
                self.sess.variables.insert("_bridge_extra".into(), ex);
            }
            self.sess.variables.insert(
                "_bridge_step_start_time".into(),
                self.current_step_start_time
                    .clone()
                    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            );
            // `+1` mirrors the terminal trace this node produces at hand-off
            // (the terminal arm increments before recording), so the executor
            // trace and every proxy-emitted trace for THIS node share one
            // step_index.
            self.sess.variables.insert(
                "_bridge_step_index".into(),
                (self.step_index + 1).to_string(),
            );
            // Execution identity rides along (`_rst_app_execution_id`) so
            // proxy-emitted traces for THIS node (bridge DTMF, compensating
            // session_end) key to the suspended execution.
            if let Some(id) = self.app_execution_id {
                self.sess
                    .variables
                    .insert("_bridge_app_execution_id".into(), id.to_string());
            }
        }
        let result = common::execute_action(
            &node.action,
            node.wait_for_result,
            ctrl,
            ctx,
            &mut self.sess,
            self.tts_service.as_ref(),
        )
        .await?;
        if let ActionResult::WaitFor(WaitEvent::AudioComplete { .. }) = &result {
            // Hold the node's delay_after_ms once this prompt finishes, before
            // the flow advances (see on_audio_complete).
            self.pending_audio_delay_ms = node.action.delay_after_ms();
            if node.action.is_dtmf_menu() {
                self.pending_menu = Some(self.build_pending_menu(&node.action));
            } else if node.action.is_interruptible() {
                self.interrupt_on_dtmf = true;
            } else if matches!(node.action, EntryAction::Prompt { .. }) {
                self.ignore_prompt_dtmf = node.ignore_prompt_dtmf;
            }
        }
        if matches!(&result, ActionResult::WaitFor(WaitEvent::NoAudio)) {
            if node.action.is_dtmf_menu() {
                self.pending_menu = Some(self.build_pending_menu(&node.action));
                self.awaiting_dtmf = true;
                if let Some(ref menu) = self.pending_menu {
                    ctrl.set_timeout("ivr_dtmf_timeout", Duration::from_millis(menu.timeout_ms));
                }
                return Ok(ActionResult::WaitFor(WaitEvent::AudioComplete {
                    interrupted: true,
                }));
            }
            let fallback = self
                .request_next(Some(ProviderEvent::Error {
                    reason: "TTS service not available".into(),
                }))
                .await?;
            return Ok(ActionResult::ChainedTo(fallback));
        }
        Ok(result)
    }

    fn build_pending_menu(&self, action: &EntryAction) -> PendingMenu {
        match action {
            EntryAction::DtmfMenu {
                entries,
                timeout_action,
                invalid_action,
                max_retries,
                timeout_ms,
                ..
            } => PendingMenu {
                entries: entries.clone(),
                timeout_action: timeout_action.clone(),
                invalid_action: invalid_action.clone(),
                max_retries: *max_retries,
                retry_count: 0,
                timeout_ms: *timeout_ms,
            },
            _ => {
                warn!(
                    "build_pending_menu called with unexpected action type, returning empty menu"
                );
                PendingMenu {
                    entries: HashMap::new(),
                    timeout_action: None,
                    invalid_action: None,
                    max_retries: 0,
                    retry_count: 0,
                    timeout_ms: 0,
                }
            }
        }
    }

    fn handle_menu_dtmf(&mut self, digit: &str) -> Option<ActionNode> {
        let menu = self.pending_menu.take()?;
        let next_retry = menu.retry_count + 1;
        let entries = menu.entries;
        let timeout_action = menu.timeout_action;
        let invalid_action = menu.invalid_action;
        let max_retries = menu.max_retries;
        let timeout_ms = menu.timeout_ms;

        if let Some(next) = entries.get(digit) {
            return Some(next.clone());
        }

        if let Some(action) = invalid_action {
            if next_retry >= max_retries {
                return Some(*action);
            }
            let next_action = *action;
            self.pending_menu = Some(PendingMenu {
                retry_count: next_retry,
                entries,
                timeout_action,
                invalid_action: None,
                max_retries,
                timeout_ms,
            });
            return Some(next_action);
        }
        self.pending_menu = Some(PendingMenu {
            retry_count: next_retry,
            entries,
            timeout_action,
            invalid_action: None,
            max_retries,
            timeout_ms,
        });
        None
    }

    fn handle_menu_timeout(&mut self) -> Option<ActionNode> {
        let menu = self.pending_menu.take()?;
        let next_retry = menu.retry_count + 1;
        let entries = menu.entries;
        let timeout_action = menu.timeout_action;
        let invalid_action = menu.invalid_action;
        let max_retries = menu.max_retries;
        let timeout_ms = menu.timeout_ms;

        if let Some(ta) = timeout_action {
            return Some(*ta);
        }
        if next_retry >= max_retries {
            return Some(ActionNode::new(EntryAction::hangup_none()));
        }
        self.pending_menu = Some(PendingMenu {
            retry_count: next_retry,
            entries,
            timeout_action: None,
            invalid_action,
            max_retries,
            timeout_ms,
        });
        None
    }
}

#[async_trait]
impl CallApp for StepIvrApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Ivr
    }

    fn name(&self) -> &str {
        "step_ivr"
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        self.runtime_vars = Some(context.session_vars.clone());
        // Any new instance owns the lifecycle from here: drop the identity a
        // previous execution stashed for its compensating session_end trace,
        // so a later compensation can never attribute the successor's end to
        // the predecessor.
        context.session_vars.remove(IVR_SUSPENDED_EXECUTION_ID_KEY);
        self.session_extensions = Some(context.session_extensions.clone());
        self.app_execution_id = context
            .invocation
            .as_ref()
            .map(|value| value.app_execution_id);
        // Capture the session handle so step-level failures can be routed
        // through the unified `ReportCallError` path (log + RWI + CDR trace).
        self.session_handle = Some(ctrl.session.clone());
        self.set_runtime_status(context, "starting");
        ctrl.answer().await?;

        let invocation =
            context
                .invocation
                .clone()
                .unwrap_or_else(|| crate::call::app::AppInvocationContext {
                    app_execution_id: 0,
                    callee: context.call_info.callee.clone(),
                    sip_headers: context.call_info.sip_headers.clone(),
                    variables: HashMap::new(),
                });

        self.sess
            .variables
            .insert("session_id".into(), context.call_info.session_id.clone());
        self.sess
            .variables
            .insert("caller".into(), context.call_info.caller.clone());
        self.sess
            .variables
            .insert("callee".into(), invocation.callee.clone());
        self.sess
            .variables
            .insert("direction".into(), context.call_info.direction.clone());

        // Clone SIP headers once; store in self.sess for future request_next calls,
        // then move into SessionContext to avoid a second full clone.
        let headers = invocation.sip_headers.clone();

        for (name, value) in &headers {
            let key = format!("sip_{}", name.replace(|c: char| !c.is_alphanumeric(), "_"));
            self.sess.variables.insert(key, value.clone());
        }

        self.sess.sip_headers = headers.clone();

        for variable in context.session_vars.iter() {
            // Internal guard bookkeeping is executor-private and must never
            // reach the provider payload (`ProviderContext.variables`).
            if variable.key() == IVR_REENTRY_STATE_KEY {
                continue;
            }
            self.sess
                .variables
                .insert(variable.key().clone(), variable.value().clone());
        }
        for (name, value) in &invocation.variables {
            self.sess.variables.insert(name.clone(), value.clone());
        }

        // Merge ivr_params (from JumpIvr query string) into session variables
        // so they are available for $var$ substitution and sent to the provider.
        // Reserved lifecycle keys stay out of the variable pool: they are
        // executor bookkeeping, must not reach the provider, and buffered
        // digits must not leak into provider payloads.
        const RESERVED_IVR_PARAM_KEYS: [&str; 3] =
            ["ivr_resumed", "bridge_dtmf_digits", "bridge_step_ctx"];
        if let Some(ref ivp) = self.ivr_params {
            for (k, v) in ivp {
                if RESERVED_IVR_PARAM_KEYS.contains(&k.as_str()) {
                    continue;
                }
                self.sess.variables.insert(k.clone(), v.clone());
            }
            // Also write to shared session_vars so the next chained app can see them.
            if let Some(ref runtime) = self.runtime_vars {
                for (k, v) in ivp {
                    if RESERVED_IVR_PARAM_KEYS.contains(&k.as_str()) {
                        continue;
                    }
                    runtime.insert(k.clone(), v.clone());
                }
            }
        }

        // Route-context starts (JumpIvr via `toivr:`) deliver `transferred_from`
        // as a plain invocation variable rather than ivr_params; surface it on
        // the dedicated ProviderContext field too (the ivr_params path sets it
        // at construction time in the builtin app factory).
        if self.transferred_from.is_none()
            && let Some(tf) = self.sess.variables.get("transferred_from")
        {
            self.transferred_from = Some(tf.clone());
        }

        // Lifecycle resume detection: `ivr_resumed=1` in ivr_params marks
        // this app instance as a CONTINUATION of an already-started logical
        // IVR flow (voip_bridge return, queue return — written by the
        // proxy's return-app start path from the `return_ivr_resume` marker;
        // JumpIvr — the `_ivr_resume` route-point param lands in the merged
        // variables and is consumed here). The trace lifecycle contract is
        // exactly-once per flow: `session_start` only on the true first
        // entry, `session_end` only on the true final exit. A resumed
        // instance's first node therefore carries a `dtmf` trigger (digits
        // buffered while suspended — the consumer contract for menu nodes
        // via voip_bridge) or a `resume` trigger (no digits), never a second
        // `session_start`. The provider protocol mirrors the trace: the
        // resumed instance's first event is `Resume` (with
        // `resume_from_step_id` when a bridge context is available), so a
        // stateless provider can continue the flow instead of re-entering
        // it. `StepProvider` adapts this to the configured wire format and
        // auto-downgrades to `session_start` for endpoints that reject it.
        let jump_resume_marker = self
            .sess
            .variables
            .remove("_ivr_resume")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let resumed = jump_resume_marker
            || self
                .ivr_params
                .as_ref()
                .and_then(|p| p.get("ivr_resumed"))
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false);
        let resume_digits: Vec<String> = self
            .ivr_params
            .as_ref()
            .and_then(|p| p.get("bridge_dtmf_digits"))
            .map(|v| v.as_str())
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // Buffered bridge digits are real caller input captured while the
        // bridge was up. They prove progress exactly like a live DTMF event
        // (this resume path never goes through `on_dtmf`) — reset the
        // re-entry loop guard before it is evaluated below.
        if !resume_digits.is_empty() {
            self.clear_reentry_state(context);
        }
        // Originating bridge-step context (`bridge_step_ctx` ivr param,
        // written by the proxy's return-app start path when a voip_bridge
        // suspended the flow). Parsed once here — it feeds both the
        // no-digit resume's `Resume` event (`resume_from_step_id`, so a
        // stateless provider can still route to the successor) and the
        // buffered-digits pending-trace rebuild below.
        let bridge_step_ctx: Option<BridgeStepTraceCtx> = self
            .ivr_params
            .as_ref()
            .and_then(|p| p.get("bridge_step_ctx"))
            .and_then(|v| serde_json::from_str::<BridgeStepTraceCtx>(v).ok());
        let resume_from_step_id = bridge_step_ctx.as_ref().and_then(|c| c.step_id.clone());
        let first_event = match resume_digits.first().cloned() {
            Some(digit) => {
                for d in resume_digits.iter().skip(1) {
                    self.pending_dtmf.push_back(d.clone());
                }
                tracing::info!(
                    digit = %digit,
                    buffered = resume_digits.len() - 1,
                    "StepIvrApp: resuming after bridge with buffered DTMF — first provider trigger is dtmf"
                );
                ProviderEvent::Dtmf { digit }
            }
            None => {
                if resumed {
                    tracing::info!(
                        resume_from_step_id = ?resume_from_step_id,
                        "StepIvrApp: resuming suspended flow without buffered digits — first provider event is resume"
                    );
                    ProviderEvent::Resume {
                        resume_from_step_id: resume_from_step_id.clone(),
                    }
                } else {
                    ProviderEvent::SessionStart
                }
            }
        };
        self.resumed_flow = resumed;
        if resumed {
            // A resumed instance is a CONTINUATION, not a fresh flow: mark
            // the runtime status and expose the suspension point so the
            // provider request self-describes the resume even when the
            // wire event is downgraded to `session_start`.
            self.set_runtime_status(context, "resuming");
            // The provider payload reads `sess.variables` (seeded from the
            // shared vars BEFORE this point) — refresh the status there too.
            self.sess
                .variables
                .insert(IVR_STATUS_KEY.to_string(), "resuming".to_string());
            if let Some(step_id) = &resume_from_step_id {
                self.sess
                    .variables
                    .insert("ivr_resume_from_step_id".to_string(), step_id.clone());
                if let Some(runtime) = &self.runtime_vars {
                    runtime
                        .insert("ivr_resume_from_step_id".to_string(), step_id.clone());
                }
            }
        }

        // Resumable bridge return with buffered digits: the proxy suppressed
        // its eager per-digit `ivr_step_trace` for this hand-off. Rebuild the
        // bridge step's pending trace from the stashed context —
        // `request_next` flushes it once the successor node resolves, so it
        // carries `next_node_id` (single-notification protocol). No context /
        // no digits → nothing to re-report (the successor's own trace carries
        // the `resume` trigger).
        if let Some(first_digit) = resume_digits.first().cloned() {
            if let Some(ctx) = bridge_step_ctx.clone() {
                self.pending_trace = Some(IvrTraceEntry {
                    session_id: context.call_info.session_id.clone(),
                    caller: context.call_info.caller.clone(),
                    callee: invocation.callee.clone(),
                    step_index: ctx.step_index.unwrap_or(0),
                    trigger: crate::rwi::TriggerInfo::Dtmf {
                        digit: first_digit,
                        digits: Some(resume_digits),
                    },
                    provider_url: None,
                    action_type: "Bridge".to_string(),
                    action_json: None,
                    error: None,
                    step_id: ctx.step_id,
                    step_name: ctx.step_name,
                    step_start_time: ctx.step_start_time,
                    step_end_time: None,
                    duration_ms: 0,
                    extra: ctx.extra,
                    end_reason: None,
                    end_detail: None,
                    next_node_id: None,
                    next_step_id: None,
                    end: None,
                });
            }
        }

        let sess_ctx = SessionContext {
            session_id: context.call_info.session_id.clone(),
            app_execution_id: invocation.app_execution_id,
            caller: context.call_info.caller.clone(),
            callee: invocation.callee,
            direction: context.call_info.direction.clone(),
            tenant_id: None,
            ivr_id: None,
            variables: self.sess.variables.clone(),
            sip_headers: Some(headers),
            route_name: self.route_name.clone(),
            custom_data: self.custom_data.clone(),
            transferred_from: self.transferred_from.clone(),
        };
        self.provider_session = Some(sess_ctx.clone());
        self.set_runtime_status(context, "provider_start");

        // Re-entry loop guard (pre-`/start` phase): a provider flow whose
        // last node does not terminate the call (queue/transfer back into
        // this IVR, `exit`, …) makes the session restart the flow — each
        // restart POSTs a fresh /start, so a stateless provider repeats its
        // "last node" forever. Fresh re-entries count here and a proven loop
        // blocks BEFORE `/start` and the session-start trace. Resumed entries
        // (bridge/queue returns — the per-node progression of one logical
        // flow) are deferred to `note_reentry_node`, which judges them by the
        // served node id, so normal node-by-node advance is never misjudged.
        if let Some(fallback_node) = self.check_reentry_loop(context, resumed) {
            self.set_runtime_end_reason_shared("ivr_reentry_loop");
            self.current_node = Some(fallback_node);
            return self.__exec_node(ctrl, context).await;
        }

        self.provider.on_session_start(&sess_ctx).await.ok();

        self.step_prev_start_time = Some(chrono::Utc::now().to_rfc3339());

        // A resumed flow keeps the original collector session row — the row
        // is created once at the true first entry and closed once at the
        // true final exit (exactly-once lifecycle contract).
        if !resumed {
            self.record_session_start(
                &sess_ctx.session_id,
                &sess_ctx.caller,
                &sess_ctx.callee,
                &sess_ctx.direction,
            )
            .await;
        }

        self.set_runtime_status(context, "awaiting_first_step");
        let first_node = match self.request_next(Some(first_event)).await {
            Ok(node) => node,
            Err(err) => {
                self.set_runtime_status(context, "startup_error");
                self.set_runtime_error(context, &err.to_string());
                return Err(err);
            }
        };
        // Re-entry loop guard (post-`/step` phase): a resumed entry whose
        // FIRST node repeats the previous cycle's node within the window —
        // with no real input in between — is the stateless-provider loop;
        // a different node is legitimate progress. A proven loop diverts
        // here instead of executing the node again.
        if let Some(fallback_node) = self.note_reentry_node(context, resumed, &first_node) {
            self.set_runtime_end_reason_shared("ivr_reentry_loop");
            self.current_node = Some(fallback_node);
            return self.__exec_node(ctrl, context).await;
        }
        self.current_node = Some(first_node);
        self.set_runtime_status(context, "active");
        self.__exec_node(ctrl, context).await
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // A provider may opt into legacy announcement semantics: finish the
        // prompt but ignore digits handled while that prompt is active.
        if self.ignore_prompt_dtmf {
            tracing::info!("StepIvrApp: ignoring DTMF during non-interruptible prompt");
            return Ok(AppAction::Continue);
        }

        // Real caller input proves the flow is progressing — clear the
        // re-entry loop guard so legitimate user-driven re-entries (queue
        // round-trips, menu navigation) are not misjudged as a restart loop.
        self.clear_reentry_state(context);

        // DTMF received — clear any stale timeout flag so a later hangup is
        // not misclassified as timeout-induced.
        self.timeout_induced = false;

        // A local DtmfMenu owns its own DTMF resolution: the digit must NEVER
        // be forwarded to the provider. Otherwise a single unexpected key
        // (e.g. one pressed while the greeting is still playing, or a key with
        // no matching entry and no invalid_action) would be sent to the
        // provider, which could return a terminal node and silently end the
        // whole IVR flow. Only provider-driven menus (empty entries) delegate
        // every digit.
        if self.pending_menu.is_some() {
            let is_provider_driven = self
                .pending_menu
                .as_ref()
                .map_or(false, |m| m.entries.is_empty());

            if is_provider_driven {
                self.awaiting_dtmf = false;
                ctrl.stop_audio().await.ok();
                self.current_track_id = None;
                self.interrupt_on_dtmf = false;
                self.pending_menu.take();
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::Dtmf {
                        digit: digit.clone(),
                        digits: None,
                    };
                }
                self.current_node = Some(
                    self.request_next(Some(ProviderEvent::Dtmf { digit }))
                        .await?,
                );
                return self.__exec_node(ctrl, context).await;
            }

            // Resolve the digit WITHOUT touching playback first, so a
            // non-matching key does not barge-in the greeting.
            if let Some(next) = self.handle_menu_dtmf(&digit) {
                // Matched entry (or configured invalid_action): consume it.
                self.awaiting_dtmf = false;
                ctrl.stop_audio().await.ok();
                self.current_track_id = None;
                self.interrupt_on_dtmf = false;
                self.provider.on_local_dtmf_match(&digit, &next).await;
                // Unified with the provider-driven path: a key press resolving
                // a menu always reports `dtmf` + detail.digit (consumer
                // contract: menu nodeType reads trigger.detail.digit).
                self.current_trigger = Some(crate::rwi::TriggerInfo::Dtmf {
                    digit: digit.clone(),
                    digits: None,
                });
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::Dtmf {
                        digit: digit.clone(),
                        digits: None,
                    };
                }
                self.current_node = Some(next);
                return self.__exec_node(ctrl, context).await;
            }

            // Non-matching key in a local menu with no invalid_action: keep
            // the menu alive and do NOT forward the digit to the provider.
            // If the greeting is still playing (`awaiting_dtmf == false`) we
            // let it finish untouched; otherwise the menu stays in its waiting
            // window and the existing `ivr_dtmf_timeout` timer keeps running,
            // so the caller may press again.
            tracing::info!(
                digit = %digit,
                awaiting_dtmf = self.awaiting_dtmf,
                "StepIvrApp: ignoring non-matching DTMF in local menu"
            );
            // Surface the rejected key: consumers key invalid-input analytics
            // (G-system `dtmferror`) off this event. Menu state is unchanged.
            let provider_session = self.provider_session_context();
            let end = chrono::Utc::now().to_rfc3339();
            self.record_trace(IvrTraceEntry {
                session_id: provider_session.session_id,
                caller: provider_session.caller,
                callee: provider_session.callee,
                step_index: self.step_index,
                trigger: crate::rwi::TriggerInfo::DtmfMenuInvalid { digit },
                provider_url: None,
                action_type: "DtmfMenu".to_string(),
                action_json: None,
                error: None,
                step_id: self.current_step_id.clone(),
                step_name: self.current_step_name.clone(),
                step_start_time: self.current_step_start_time.clone(),
                step_end_time: Some(end.clone()),
                duration_ms: super::trace::duration_ms_between(
                    self.current_step_start_time.as_deref(),
                    &end,
                ),
                extra: self.extra.clone(),
                end_reason: None,
                end_detail: None,
                next_node_id: None,
                next_step_id: None,
                end: None,
            });
            return Ok(AppAction::Continue);
        }

        // Interruptible Prompt barge-in (or a provider-driven menu awaiting
        // input): stop playback and forward the digit to the provider now.
        if self.interrupt_on_dtmf || self.awaiting_dtmf {
            self.awaiting_dtmf = false;
            if self.interrupt_on_dtmf {
                ctrl.stop_audio().await.ok();
                self.current_track_id = None;
                self.interrupt_on_dtmf = false;
            }

            if let Some(ref mut t) = self.pending_trace {
                t.trigger = crate::rwi::TriggerInfo::Dtmf {
                    digit: digit.clone(),
                    digits: None,
                };
            }
            self.current_node = Some(
                self.request_next(Some(ProviderEvent::Dtmf { digit }))
                    .await?,
            );
            return self.__exec_node(ctrl, context).await;
        }

        // The current step is non-interruptible (e.g. an announcement) or the
        // provider response is still in flight. Do NOT drop the digit — buffer
        // it so it is delivered to the provider on the next step.
        tracing::info!(
            digit = %digit,
            "StepIvrApp: buffering DTMF during non-interruptible step"
        );
        self.pending_dtmf.push_back(digit);
        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        track_id: String,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let was_menu = self.pending_menu.is_some();
        self.current_track_id = None;
        self.interrupt_on_dtmf = false;
        self.ignore_prompt_dtmf = false;

        // Hold the finished prompt's configured delay_after_ms before asking
        // the provider for the next step (e.g. so a goodbye prompt can sink
        // in before hangup). Reset first so an error path cannot leave it
        // stuck on.
        if self.pending_audio_delay_ms > 0 {
            let delay = common::node_delay(self.pending_audio_delay_ms);
            self.pending_audio_delay_ms = 0;
            tracing::debug!(
                delay_ms = delay.as_millis() as u64,
                "StepIvrApp: holding delay_after_ms"
            );
            tokio::time::sleep(delay).await;
        }

        if was_menu && track_id == "ivr_menu_greeting" {
            if let Some(ref menu) = self.pending_menu {
                self.awaiting_dtmf = true;
                ctrl.set_timeout("ivr_dtmf_timeout", Duration::from_millis(menu.timeout_ms));
                if let Some(digit) = self.pending_dtmf.pop_front() {
                    tracing::info!(
                        digit = %digit,
                        "StepIvrApp: delivering buffered DTMF after menu greeting"
                    );
                    return self.on_dtmf(digit, ctrl, context).await;
                }
                return Ok(AppAction::Continue);
            }
        }

        if let Some(ref node) = self.current_node {
            if let Some(ref next) = node.next {
                self.current_trigger = Some(crate::rwi::TriggerInfo::AudioComplete);
                self.current_node = Some(*next.clone());
                return self.__exec_node(ctrl, context).await;
            }
        }

        self.current_node = Some(
            self.request_next(Some(ProviderEvent::AudioComplete { interrupted: false }))
                .await?,
        );
        self.__exec_node(ctrl, context).await
    }

    async fn on_external_event(
        &mut self,
        event: AppEvent,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        match event {
            AppEvent::TransferResult { outcome } => {
                // Pending trace flushes inside `request_next` with the
                // successor attached.
                self.current_node = Some(
                    self.request_next(Some(ProviderEvent::TransferResult { outcome }))
                        .await?,
                );
                return self.__exec_node(ctrl, context).await;
            }
            AppEvent::Custom { name, data: _ } => {
                tracing::debug!(event = %name, "StepIvrApp custom event");
            }
        }
        Ok(AppAction::Continue)
    }

    async fn on_timeout(
        &mut self,
        timeout_id: String,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if timeout_id != "ivr_dtmf_timeout" {
            return Ok(AppAction::Continue);
        }

        // A stale timer can fire if a menu was exited without cancelling it
        // (e.g. a valid DTMF already moved the flow to the next node before
        // the old timeout elapsed). With no menu pending there is nothing to
        // time out — ignore it instead of forwarding `dtmf_timeout` to the
        // provider, which would derail whatever node is currently running.
        if self.pending_menu.is_none() {
            return Ok(AppAction::Continue);
        }

        self.awaiting_dtmf = false;
        // Mark that this step was exited due to a DTMF timeout. If the
        // resulting provider action is Hangup, `on_exit` will classify the
        // end reason as `Timeout` instead of the generic `Hangup`.
        self.timeout_induced = true;

        if self.pending_menu.is_some() {
            // Provider-driven menus (empty entries) forward timeout to provider
            let is_provider_driven = self
                .pending_menu
                .as_ref()
                .map_or(false, |m| m.entries.is_empty());
            if is_provider_driven {
                self.pending_menu.take();
                // Symmetric with key presses: the menu step's own trace
                // records HOW it ended — overwrite the pending trigger.
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::DtmfMenuTimeout;
                }
                self.current_node = Some(
                    self.request_next(Some(ProviderEvent::DtmfMenuTimeout))
                        .await?,
                );
                self.current_trigger = Some(crate::rwi::TriggerInfo::DtmfMenuTimeout);
                return self.__exec_node(ctrl, context).await;
            }

            if let Some(next) = self.handle_menu_timeout() {
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::DtmfMenuTimeout;
                }
                self.current_trigger = Some(crate::rwi::TriggerInfo::DtmfMenuTimeout);
                self.current_node = Some(next);
                return self.__exec_node(ctrl, context).await;
            }
            // handle_menu_timeout returned None — local menu retry (no
            // timeout_action configured, retries remaining).  The pending_menu
            // was re-created internally.  Re-arm the timeout and stay in the
            // menu instead of forwarding DtmfTimeout to the provider, which
            // would leave the stale pending_menu intercepting future DTMF.
            if let Some(ref menu) = self.pending_menu {
                self.awaiting_dtmf = true;
                ctrl.set_timeout("ivr_dtmf_timeout", Duration::from_millis(menu.timeout_ms));
            }
            return Ok(AppAction::Continue);
        }

        self.current_node = Some(self.request_next(Some(ProviderEvent::DtmfTimeout)).await?);
        self.__exec_node(ctrl, context).await
    }

    async fn on_exit(&mut self, reason: crate::call::app::ExitReason) -> anyhow::Result<()> {
        // Finalize any pending trace (from a WaitFor step) before recording
        // the session end, so the last step's trace is not lost when the call
        // ends while waiting for DTMF, audio playback, or other async input.
        if let Some(mut pending) = self.pending_take() {
            // The pending step ended because the session did — surface WHY on
            // the step's own trigger (symmetric with the dtmf /
            // dtmf_menu_timeout overwrites: a node's trigger records how it
            // finished when the session terminates it). Vocabulary matches
            // the session-level end_reason tags.
            let hangup_trigger = match &reason {
                crate::call::app::ExitReason::RemoteHangup(_) => {
                    Some(crate::rwi::TriggerInfo::UserHangup)
                }
                crate::call::app::ExitReason::Cancelled => {
                    Some(crate::rwi::TriggerInfo::Cancelled)
                }
                crate::call::app::ExitReason::Hangup => Some(crate::rwi::TriggerInfo::Hangup),
                _ => None,
            };
            if let Some(trigger) = hangup_trigger {
                pending.trigger = trigger;
            }
            let end = std::time::Instant::now();
            let duration = self
                .pending_start_instant
                .map(|s| end.duration_since(s).as_millis() as u64)
                .unwrap_or(0);
            self.pending_start_instant = None;
            let step_end = chrono::Utc::now().to_rfc3339();
            // The session terminated while this step was in flight — it is
            // the flow's last observable step.
            pending.end = Some(true);
            self.record_trace(IvrTraceEntry {
                step_end_time: Some(step_end),
                duration_ms: duration,
                ..pending
            });
        }

        let mut end_reason_label =
            Self::end_reason_label(&reason, self.last_transfer_target.as_deref()).to_string();
        // Provider notification: caller hangup IS a session end — the provider
        // receives `/end` with the `user_hangup` tag (protocol vocabulary)
        // exactly like any other termination. Only a system cancellation
        // (shutdown) skips the POST, to avoid hammering providers during a
        // restart storm. Resumable hand-offs still notify — the flow's
        // continuation is the successor session's concern.
        let skip_provider_end =
            matches!(reason, crate::call::app::ExitReason::Cancelled);
        let mut end_reason = match reason {
            crate::call::app::ExitReason::Normal => SessionEndReason {
                reason: SessionEndTag::Normal,
                detail: None,
            },
            crate::call::app::ExitReason::Hangup => SessionEndReason {
                reason: SessionEndTag::Hangup,
                detail: None,
            },
            crate::call::app::ExitReason::RemoteHangup(_) => SessionEndReason {
                reason: SessionEndTag::UserHangup,
                detail: None,
            },
            crate::call::app::ExitReason::Transferred => {
                // Determine transfer target type from the last action.
                let target = self.last_transfer_target.clone().unwrap_or_default();
                SessionEndReason {
                    reason: Self::classify_transfer_target(&target),
                    detail: Some(target),
                }
            }
            crate::call::app::ExitReason::Error(e) => SessionEndReason {
                reason: SessionEndTag::Error,
                detail: Some(e),
            },
            crate::call::app::ExitReason::Cancelled => SessionEndReason {
                reason: SessionEndTag::Hangup,
                detail: None,
            },
            _ => SessionEndReason {
                reason: SessionEndTag::Normal,
                detail: None,
            },
        };

        // If the exit was caused by a DTMF timeout (provider returned Hangup
        // in response to a DtmfTimeout/DtmfMenuTimeout event), refine the end
        // reason from `Hangup` to `Timeout`.
        if self.timeout_induced && matches!(end_reason.reason, SessionEndTag::Hangup) {
            end_reason.reason = SessionEndTag::Timeout;
            end_reason_label = "timeout".to_string();
        }
        let provider_session = self.provider_session_context();
        let session_id = provider_session.session_id;
        let end_sr = end_reason.clone();

        // Always record the session_end trace entry — including on
        // RemoteHangup/Cancelled (caller hung up or system terminated the
        // session) — so the last executed node is captured and surfaced as an
        // `ivr_step_trace` event even when the session ends mid-flow.
        let (last_action_type, last_step_id, last_step_name, last_extra) = match &self.current_node
        {
            Some(node) => (
                Self::action_type_label(&node.action).to_string(),
                node.step_id
                    .clone()
                    .or_else(|| self.current_step_id.clone()),
                node.step_name
                    .clone()
                    .or_else(|| self.current_step_name.clone()),
                node.extra.clone(),
            ),
            None => (
                "session_end".to_string(),
                self.current_step_id.clone(),
                self.current_step_name.clone(),
                None,
            ),
        };
        let caller = provider_session.caller;
        let callee = provider_session.callee;
        // Lifecycle exactly-once: a hand-off the flow will resume from
        // (voip_bridge with return_app, queue with return_app, or a jump to
        // another IVR) does NOT end the logical flow — suppress the
        // `session_end` trace and leave the collector session row active.
        // The proxy compensates with a synthetic session_end if the flow
        // dies while suspended (caller hangup / hand-off failure). Only the
        // trace lifecycle changes here: provider /end hooks and shared
        // ivr_status/ivr_end_reason variables behave exactly as before.
        let resumable_handoff = match &self.current_node {
            Some(node) => {
                Self::is_resumable_handoff(node, self.last_transfer_target.as_deref().unwrap_or(""))
            }
            None => false,
        };
        if !resumable_handoff {
            let end = chrono::Utc::now().to_rfc3339();
            self.record_trace(IvrTraceEntry {
                session_id: session_id.clone(),
                caller,
                callee,
                step_index: self.step_index,
                trigger: crate::rwi::TriggerInfo::SessionEnd,
                provider_url: None,
                action_type: last_action_type,
                action_json: None,
                error: None,
                step_id: last_step_id,
                step_name: last_step_name,
                step_start_time: self.current_step_start_time.clone(),
                step_end_time: Some(end.clone()),
                duration_ms: super::trace::duration_ms_between(
                    self.current_step_start_time.as_deref(),
                    &end,
                ),
                extra: last_extra,
                end_reason: Some(end_sr.reason.clone()),
                end_detail: end_sr.detail.clone(),
                next_node_id: None,
                next_step_id: None,
                end: Some(true),
            });
        } else {
            // Suspended-flow lifecycle: freeze the SUSPENDED execution's
            // identity for the proxy's compensating session_end (real hangup
            // while suspended on this hand-off). Cleared by the next
            // on_enter — never read once a successor owns the flow.
            self.stash_suspended_execution_id();
        }

        let status = serde_json::to_string(&end_sr.reason)
            .unwrap_or_else(|_| "\"unknown\"".to_string())
            .trim_matches('"')
            .to_string();
        if let Some(name) = &self.ivr_name {
            self.sess
                .variables
                .insert(IVR_NAME_KEY.into(), name.clone());
        }
        self.sess
            .variables
            .insert(IVR_STATUS_KEY.into(), end_reason_label.clone());
        self.sess
            .variables
            .insert(IVR_END_REASON_KEY.into(), end_reason_label.clone());
        self.set_runtime_end_reason_shared(&end_reason_label);
        // Clean up local state
        if self.current_track_id.is_some() {
            // Audio track will be cleaned up by media layer
            self.current_track_id = None;
        }
        self.pending_menu = None;

        // Publish the segment snapshot before the provider `/end` request can
        // block. A fast successor may otherwise complete and consume the
        // logical ivr.exec result before this segment becomes available.
        if let Some(ref ext) = self.session_extensions {
            let mut collected: std::collections::HashMap<String, String> = self
                .sess
                .variables
                .iter()
                .filter(|(k, _)| {
                    !["session_id", "caller", "callee", "direction"].contains(&k.as_str())
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if let Some(node) = self
                .current_node
                .as_ref()
                .filter(|node| matches!(node.action, EntryAction::Exit))
            {
                collected.extend(node.result_variables.clone());
            }
            super::exec::write_ivr_exec_result(
                ext,
                self.app_execution_id.unwrap_or(0),
                super::exec::build_ivr_exec_result(
                    super::exec::ivr_exec_status(&end_reason_label),
                    &end_reason_label,
                    self.last_transfer_target.clone(),
                    collected,
                    0,
                ),
            );
        }

        // Cancellation can win before on_enter establishes a provider invocation.
        // An unentered invocation has no provider lifecycle to finalize.
        if !skip_provider_end
            && let Some(provider_session) = self.provider_session.as_ref()
        {
            self.provider
                .on_session_end_context(&end_reason, provider_session)
                .await
                .ok();
        }
        if !resumable_handoff {
            self.record_session_end(&status).await;
        }

        Ok(())
    }

    async fn on_record_complete(
        &mut self,
        info: RecordingInfo,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // Only advance the provider when we were waiting on a torecord-style
        // capture. Mid-call record_start/stop sets notify_app=false but hangup
        // finalize may still deliver RecordingComplete — ignore those.
        let waiting_for_recording = self
            .pending_trace
            .as_ref()
            .map(|t| t.action_type == "Torecord")
            .unwrap_or(false);
        if !waiting_for_recording {
            return Ok(AppAction::Continue);
        }
        let duration_secs = info.duration.as_secs();
        self.current_node = Some(
            self.request_next(Some(ProviderEvent::RecordingComplete {
                url: info.path,
                duration_secs,
            }))
            .await?,
        );
        self.__exec_node(ctrl, context).await
    }
}
