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

const IVR_STATUS_KEY: &str = "ivr_status";
const IVR_NAME_KEY: &str = "ivr_name";
const IVR_END_REASON_KEY: &str = "ivr_end_reason";
const IVR_LAST_ERROR_KEY: &str = "ivr_last_error";

/// Default number of consecutive no-input prompt cycles before the step IVR
/// asks the provider to resolve (DtmfMenuTimeout). 0 disables the runaway guard.
const DEFAULT_MAX_REPEAT_PROMPTS: u32 = 10;

pub struct StepIvrApp {
    provider: Box<dyn ActionProvider>,
    provider_session: Option<SessionContext>,
    current_node: Option<ActionNode>,
    sess: SessionData,
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
    trace: Option<Arc<IvrTraceCollector>>,
    step_index: u32,
    ivr_name: Option<String>,
    rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
    /// Name of the route that dispatched this call into the IVR.
    route_name: Option<String>,
    /// Passthrough data set by the external provider (echoed back each step).
    custom_data: Option<serde_json::Value>,
    /// Transparent extra JSON object from provider — stored and passed through in events.
    extra: Option<serde_json::Value>,
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
    current_step_start_time: Option<String>,
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
    current_step_id: Option<String>,
    current_step_name: Option<String>,
    /// Structured trigger that caused the current step (e.g. dtmf with digit detail).
    current_trigger: Option<crate::rwi::TriggerInfo>,
    runtime_vars: Option<Arc<DashMap<String, String>>>,
    /// Session extensions clone stashed in on_enter for use in on_exit.
    /// Only populated when the IVR was started via `ivr.exec`.
    session_extensions: Option<crate::proxy::proxy_call::session_hooks::SessionExtensions>,
    /// DTMF digits received while the current step is non-interruptible (or the
    /// step provider response is in flight). Delivered to the provider on the
    /// next step instead of being silently dropped, so caller input is never lost.
    pending_dtmf: VecDeque<String>,
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
    pub fn new(url: impl Into<String>, http_client: reqwest::Client) -> Self {
        let provider = Box::new(super::provider::StepProvider::new(url, http_client));
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
            pending_dtmf: VecDeque::new(),
            max_repeat_prompts: DEFAULT_MAX_REPEAT_PROMPTS,
            no_input_prompts: 0,
            probe_pending: false,
            ivr_fallback: None,
        }
    }

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
            pending_dtmf: VecDeque::new(),
            max_repeat_prompts: DEFAULT_MAX_REPEAT_PROMPTS,
            no_input_prompts: 0,
            probe_pending: false,
            ivr_fallback: None,
        }
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

    /// Attach global `[proxy.ivr_fallback]` for session-level recovery.
    pub fn with_ivr_fallback(mut self, config: Option<Arc<IvrFallbackConfig>>) -> Self {
        self.ivr_fallback = config;
        self
    }

    fn pending_take(&mut self) -> Option<IvrTraceEntry> {
        self.pending_trace.take()
    }

    /// Finalize the pending WaitFor trace: fill in `step_end_time` and
    /// `duration_ms`, then emit ONE `ivr_step_trace` keeping the step's
    /// original trigger (e.g. `phone_collected`, `dtmf`) with its detail.
    /// Completion is observable via `step_end_time`; `session_end` stays
    /// reserved for on_exit, the only entry carrying end_reason.
    fn record_pending_session_end(&mut self) {
        let Some(pending) = self.pending_take() else {
            return;
        };
        let duration_ms = self
            .pending_start_instant
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);
        self.record_trace(IvrTraceEntry {
            step_end_time: Some(chrono::Utc::now().to_rfc3339()),
            duration_ms,
            ..pending
        });
    }

    fn record_trace(&self, entry: IvrTraceEntry) {
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
            };
            let guard = gw.read();
            guard.fan_out(&call_id, &ev);
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

    fn end_reason_label(reason: &crate::call::app::ExitReason) -> &'static str {
        match reason {
            crate::call::app::ExitReason::Normal => "normal",
            crate::call::app::ExitReason::Hangup => "hangup",
            crate::call::app::ExitReason::RemoteHangup(_) => "remote_hangup",
            crate::call::app::ExitReason::Transferred => "transferred",
            crate::call::app::ExitReason::Error(_) => "error",
            crate::call::app::ExitReason::Cancelled => "cancelled",
            crate::call::app::ExitReason::Chained => "chained",
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
        let node = self.current_node.as_ref().unwrap().clone();

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
        let start = std::time::Instant::now();
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
            .unwrap_or_else(|| crate::rwi::TriggerInfo::new("action_execute"));

        self.current_trigger = None;

        let provider_session = self.provider_session_context();
        let session_id = provider_session.session_id;
        let caller = provider_session.caller;
        let callee = provider_session.callee;

        match result {
            Ok(action_result) => {
                // Finalize any pending trace (from a previous WaitFor step) before
                // recording the current step's trace.
                if let Some(pending) = self.pending_take() {
                    let end = std::time::Instant::now();
                    let duration = self
                        .pending_start_instant
                        .map(|s| end.duration_since(s).as_millis() as u64)
                        .unwrap_or(0);
                    let step_end = chrono::Utc::now().to_rfc3339();
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
                        self.record_trace(IvrTraceEntry {
                            session_id: session_id.clone(),
                            caller: caller.clone(),
                            callee: callee.clone(),
                            step_index: self.step_index,
                            trigger: trigger.clone(),
                            provider_url: None,
                            action_type: node_type_str,
                            action_json,
                            error: None,
                            step_id: step_id.clone(),
                            step_name: step_name.clone(),
                            step_start_time: self.current_step_start_time.clone(),
                            step_end_time: Some(step_end),
                            duration_ms: elapsed_ms,
                            extra: self.extra.clone(),
                            end_reason: None,
                            end_detail: None,
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
                        self.pending_trace = Some(IvrTraceEntry {
                            session_id: session_id.clone(),
                            caller: caller.clone(),
                            callee: callee.clone(),
                            step_index: self.step_index,
                            trigger: trigger.clone(),
                            provider_url: None,
                            action_type: node_type_str,
                            action_json,
                            error: None,
                            step_id: step_id.clone(),
                            step_name: step_name.clone(),
                            step_start_time: self.current_step_start_time.clone(),
                            step_end_time: None,
                            duration_ms: 0,
                            extra: self.extra.clone(),
                            end_reason: None,
                            end_detail: None,
                        });
                        self.current_node = Some(
                            self.request_next(Some(ProviderEvent::AudioComplete {
                                interrupted: false,
                            }))
                            .await?,
                        );
                        return Box::pin(self.__exec_node(ctrl, ctx)).await;
                    }
                    ActionResult::ChainedTo(next) => {
                        self.current_trigger = Some(crate::rwi::TriggerInfo::new("chained"));
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
                                        crate::rwi::TriggerInfo::with_detail(
                                            "phone_collected",
                                            serde_json::json!({ "number": number }),
                                        ),
                                    )
                                }
                                WaitEvent::DtmfTimeout => (
                                    ProviderEvent::DtmfTimeout,
                                    crate::rwi::TriggerInfo::new("dtmf_timeout"),
                                ),
                                _ => unreachable!(),
                            };
                            self.pending_start_instant = self.step_start_instant;
                            self.pending_trace = Some(IvrTraceEntry {
                                session_id: session_id.clone(),
                                caller: caller.clone(),
                                callee: callee.clone(),
                                step_index: self.step_index,
                                trigger: step_trigger,
                                provider_url: None,
                                action_type: node_type_str,
                                action_json,
                                error: None,
                                step_id: step_id.clone(),
                                step_name: step_name.clone(),
                                step_start_time: self.current_step_start_time.clone(),
                                step_end_time: None,
                                duration_ms: 0,
                                extra: self.extra.clone(),
                                end_reason: None,
                                end_detail: None,
                            });
                            self.record_pending_session_end();
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
                            self.record_trace(IvrTraceEntry {
                                session_id: session_id.clone(),
                                caller: caller.clone(),
                                callee: callee.clone(),
                                step_index: self.step_index,
                                trigger: trigger.clone(),
                                provider_url: None,
                                action_type: node_type_str,
                                action_json,
                                error: None,
                                step_id: step_id.clone(),
                                step_name: step_name.clone(),
                                step_start_time: self.current_step_start_time.clone(),
                                step_end_time: Some(step_end),
                                duration_ms: elapsed_ms,
                                extra: self.extra.clone(),
                                end_reason: None,
                                end_detail: None,
                            });
                            if let Some(ref next) = node.next {
                                self.current_trigger =
                                    Some(crate::rwi::TriggerInfo::new("record_control"));
                                self.current_node = Some(*next.clone());
                            } else {
                                self.current_trigger =
                                    Some(crate::rwi::TriggerInfo::new("record_control"));
                                self.current_node =
                                    Some(self.request_next(Some(provider_event)).await?);
                            }
                            return Box::pin(self.__exec_node(ctrl, ctx)).await;
                        }

                        let step_trigger = match wait_event {
                            WaitEvent::DtmfCollected { digit } => {
                                crate::rwi::TriggerInfo::with_detail(
                                    "dtmf_collected",
                                    serde_json::json!({ "digit": digit }),
                                )
                            }
                            WaitEvent::DtmfTimeout => crate::rwi::TriggerInfo::new("dtmf_timeout"),
                            _ => trigger.clone(),
                        };
                        self.pending_start_instant = self.step_start_instant;
                        self.pending_trace = Some(IvrTraceEntry {
                            session_id: session_id.clone(),
                            caller: caller.clone(),
                            callee: callee.clone(),
                            step_index: self.step_index,
                            trigger: step_trigger,
                            provider_url: None,
                            action_type: node_type_str,
                            action_json,
                            error: None,
                            step_id: step_id.clone(),
                            step_name: step_name.clone(),
                            step_start_time: self.current_step_start_time.clone(),
                            step_end_time: None,
                            duration_ms: 0,
                            extra: self.extra.clone(),
                            end_reason: None,
                            end_detail: None,
                        });
                        AppAction::Continue
                    }
                };
                Ok(app_action)
            }
            Err(e) => {
                self.record_trace(IvrTraceEntry {
                    session_id,
                    caller,
                    callee,
                    step_index: self.step_index,
                    trigger,
                    provider_url: None,
                    action_type: node_type_str,
                    action_json,
                    error: Some(e.to_string()),
                    step_id,
                    step_name,
                    step_start_time: self.current_step_start_time.clone(),
                    step_end_time: Some(step_end),
                    duration_ms: elapsed_ms,
                    extra: self.extra.clone(),
                    end_reason: None,
                    end_detail: None,
                });
                // Recover via /fail → IVR fallback instead of ending the session.
                let recovery = self.recover_from_execute_failure(e).await?;
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
        self.provider_session
            .clone()
            .unwrap_or_else(|| SessionContext {
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
            })
    }

    fn fallback_already_used(&self) -> bool {
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

    fn mark_fallback_used(&mut self) {
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
            },
            ActionNode::new(EntryAction::Hangup {
                prompt: None,
                prompt_text: None,
                prompt_voice: None,
            }),
        )
    }

    /// Record a trace entry + RWI `ivr_step_trace` event for a fallback decision.
    fn record_fallback_trace(&self, reason: &str, target: Option<&str>) {
        let provider_session = self.provider_session_context();
        let session_id = provider_session.session_id;
        let caller = provider_session.caller;
        let callee = provider_session.callee;
        let now = chrono::Utc::now().to_rfc3339();
        self.record_trace(IvrTraceEntry {
            session_id,
            caller,
            callee,
            step_index: self.step_index,
            trigger: crate::rwi::TriggerInfo::with_detail(
                "ivr_fallback",
                serde_json::json!({
                    "reason": reason,
                    "target": target,
                }),
            ),
            provider_url: None,
            action_type: "ivr_fallback".to_string(),
            action_json: None,
            duration_ms: 0,
            error: Some(reason.to_string()),
            step_id: self.current_step_id.clone(),
            step_name: self.current_step_name.clone(),
            step_start_time: self.current_step_start_time.clone(),
            step_end_time: Some(now),
            extra: self.extra.clone(),
            end_reason: None,
            end_detail: None,
        });
    }

    /// Session-level recovery: match `[proxy.ivr_fallback]` → direct IVR, else hangup.
    fn enter_ivr_fallback_node(&mut self, reason: &str) -> ActionNode {
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
    async fn recover_from_execute_failure(
        &mut self,
        err: anyhow::Error,
    ) -> anyhow::Result<ActionNode> {
        let reason = err.to_string();
        tracing::warn!(error = %reason, "StepIvrApp: node execute failed, calling /fail");
        self.set_runtime_error_shared(&reason);
        self.set_runtime_status_shared("execute_error");

        let ctx = self.build_fail_provider_context(reason.clone());
        match self.provider.fail_action(ctx).await {
            Ok(node) => {
                tracing::info!("StepIvrApp: /fail returned recovery action");
                Ok(node)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "StepIvrApp: /fail failed, entering IVR fallback"
                );
                Ok(self.enter_ivr_fallback_node(&format!("fail:{e}")))
            }
        }
    }

    async fn request_next(
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
            return Ok(ActionNode::new(EntryAction::Hangup {
                prompt: None,
                prompt_text: None,
                prompt_voice: None,
            }));
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

        // Finalize and record pending trace (WaitFor step just completed).
        if let Some(pending) = self.pending_take() {
            let end = std::time::Instant::now();
            let duration = self
                .pending_start_instant
                .map(|s| end.duration_since(s).as_millis() as u64)
                .unwrap_or(0);
            let step_end = chrono::Utc::now().to_rfc3339();
            self.record_trace(IvrTraceEntry {
                step_end_time: Some(step_end),
                duration_ms: duration,
                ..pending
            });
        }

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
            Some(ProviderEvent::SessionStart) => crate::rwi::TriggerInfo::new("session_start"),
            Some(ProviderEvent::AudioComplete { .. }) => {
                crate::rwi::TriggerInfo::new("audio_complete")
            }
            Some(ProviderEvent::Dtmf { digit }) => {
                crate::rwi::TriggerInfo::with_detail("dtmf", serde_json::json!({ "digit": digit }))
            }
            Some(ProviderEvent::DtmfTimeout) => crate::rwi::TriggerInfo::new("dtmf_timeout"),
            Some(ProviderEvent::ApiResponse { status, .. }) => {
                crate::rwi::TriggerInfo::with_detail(
                    "api_response",
                    serde_json::json!({ "status": status }),
                )
            }
            Some(ProviderEvent::PhoneCollected { number }) => crate::rwi::TriggerInfo::with_detail(
                "phone_collected",
                serde_json::json!({ "number": number }),
            ),
            Some(ProviderEvent::RecordingComplete { url, duration_secs }) => {
                crate::rwi::TriggerInfo::with_detail(
                    "recording_complete",
                    serde_json::json!({ "url": url, "duration_secs": duration_secs }),
                )
            }
            Some(ProviderEvent::RecordingStarted {
                segment_type,
                segment_id,
            }) => crate::rwi::TriggerInfo::with_detail(
                "recording_started",
                serde_json::json!({
                    "segment_type": segment_type,
                    "segment_id": segment_id,
                }),
            ),
            Some(ProviderEvent::RecordingStopped { reason }) => {
                crate::rwi::TriggerInfo::with_detail(
                    "recording_stopped",
                    serde_json::json!({ "reason": reason }),
                )
            }
            Some(ProviderEvent::InputVoice { text, confidence }) => {
                crate::rwi::TriggerInfo::with_detail(
                    "input_voice",
                    serde_json::json!({ "text": text, "confidence": confidence }),
                )
            }
            Some(ProviderEvent::Error { reason }) => crate::rwi::TriggerInfo::with_detail(
                "error",
                serde_json::json!({ "reason": reason }),
            ),
            Some(ProviderEvent::Fail {
                reason,
                failed_step_id,
                failed_step_name,
                failed_action,
            }) => crate::rwi::TriggerInfo::with_detail(
                "fail",
                serde_json::json!({
                    "reason": reason,
                    "failed_step_id": failed_step_id,
                    "failed_step_name": failed_step_name,
                    "failed_action": failed_action,
                }),
            ),
            Some(ProviderEvent::DtmfMenuInvalid { digit }) => crate::rwi::TriggerInfo::with_detail(
                "dtmf_menu_invalid",
                serde_json::json!({ "digit": digit }),
            ),
            Some(ProviderEvent::DtmfMenuTimeout) => {
                crate::rwi::TriggerInfo::new("dtmf_menu_timeout")
            }
            Some(ProviderEvent::TransferResult { outcome }) => {
                crate::rwi::TriggerInfo::with_detail(
                    "transfer_result",
                    serde_json::json!({ "outcome": outcome }),
                )
            }
            None => crate::rwi::TriggerInfo::new("unknown"),
        });

        // Fallback on provider error instead of propagating
        match result {
            Ok(node) => Ok(node),
            Err(e) => {
                tracing::warn!(error = %e, "StepIvrApp: provider /step failed, using IVR fallback");
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
        // `trigger.detail.digit`).
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
            return Some(ActionNode::new(EntryAction::Hangup {
                prompt: None,
                prompt_text: None,
                prompt_voice: None,
            }));
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
        self.session_extensions = Some(context.session_extensions.clone());
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
            self.sess
                .variables
                .insert(variable.key().clone(), variable.value().clone());
        }
        for (name, value) in &invocation.variables {
            self.sess.variables.insert(name.clone(), value.clone());
        }

        // Merge ivr_params (from JumpIvr query string) into session variables
        // so they are available for $var$ substitution and sent to the provider.
        if let Some(ref ivp) = self.ivr_params {
            for (k, v) in ivp {
                self.sess.variables.insert(k.clone(), v.clone());
            }
            // Also write to shared session_vars so the next chained app can see them.
            if let Some(ref runtime) = self.runtime_vars {
                for (k, v) in ivp {
                    runtime.insert(k.clone(), v.clone());
                }
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
        self.provider.on_session_start(&sess_ctx).await.ok();

        self.step_prev_start_time = Some(chrono::Utc::now().to_rfc3339());

        self.record_session_start(
            &sess_ctx.session_id,
            &sess_ctx.caller,
            &sess_ctx.callee,
            &sess_ctx.direction,
        )
        .await;

        self.set_runtime_status(context, "awaiting_first_step");
        let first_node = match self.request_next(Some(ProviderEvent::SessionStart)).await {
            Ok(node) => node,
            Err(err) => {
                self.set_runtime_status(context, "startup_error");
                self.set_runtime_error(context, &err.to_string());
                return Err(err);
            }
        };
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
                    t.trigger = crate::rwi::TriggerInfo::with_detail(
                        "dtmf",
                        serde_json::json!({ "digit": digit }),
                    );
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
                let dtmf_detail = serde_json::json!({ "digit": digit.clone() });
                self.current_trigger =
                    Some(crate::rwi::TriggerInfo::with_detail("dtmf", dtmf_detail));
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::with_detail(
                        "dtmf",
                        serde_json::json!({ "digit": digit }),
                    );
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
            self.record_trace(IvrTraceEntry {
                session_id: provider_session.session_id,
                caller: provider_session.caller,
                callee: provider_session.callee,
                step_index: self.step_index,
                trigger: crate::rwi::TriggerInfo::with_detail(
                    "dtmf_menu_invalid",
                    serde_json::json!({ "digit": digit }),
                ),
                provider_url: None,
                action_type: "DtmfMenu".to_string(),
                action_json: None,
                error: None,
                step_id: self.current_step_id.clone(),
                step_name: self.current_step_name.clone(),
                step_start_time: self.current_step_start_time.clone(),
                step_end_time: Some(chrono::Utc::now().to_rfc3339()),
                duration_ms: 0,
                extra: self.extra.clone(),
                end_reason: None,
                end_detail: None,
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
                t.trigger = crate::rwi::TriggerInfo::with_detail(
                    "dtmf",
                    serde_json::json!({ "digit": digit }),
                );
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
                self.current_trigger = Some(crate::rwi::TriggerInfo::new("audio_complete"));
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
            AppEvent::HttpResponse { body } => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
                    let event = ProviderEvent::ApiResponse {
                        status: 200,
                        body: value,
                    };
                    self.current_node = Some(self.request_next(Some(event)).await?);
                    return self.__exec_node(ctrl, context).await;
                }
            }
            AppEvent::TransferResult { outcome } => {
                self.record_pending_session_end();
                self.current_node = Some(
                    self.request_next(Some(ProviderEvent::TransferResult { outcome }))
                        .await?,
                );
                return self.__exec_node(ctrl, context).await;
            }
            AppEvent::Custom { name, data: _ } => {
                tracing::debug!(event = %name, "StepIvrApp custom event");
            }
            _ => {}
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
                    t.trigger = crate::rwi::TriggerInfo::new("dtmf_menu_timeout");
                }
                self.current_node = Some(
                    self.request_next(Some(ProviderEvent::DtmfMenuTimeout))
                        .await?,
                );
                self.current_trigger = Some(crate::rwi::TriggerInfo::new("dtmf_menu_timeout"));
                return self.__exec_node(ctrl, context).await;
            }

            if let Some(next) = self.handle_menu_timeout() {
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::new("dtmf_menu_timeout");
                }
                self.current_trigger = Some(crate::rwi::TriggerInfo::new("dtmf_menu_timeout"));
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
                crate::call::app::ExitReason::RemoteHangup(_) => Some("user_hangup"),
                crate::call::app::ExitReason::Cancelled => Some("cancelled"),
                crate::call::app::ExitReason::Hangup => Some("hangup"),
                _ => None,
            };
            if let Some(t) = hangup_trigger {
                pending.trigger = crate::rwi::TriggerInfo::new(t);
            }
            let end = std::time::Instant::now();
            let duration = self
                .pending_start_instant
                .map(|s| end.duration_since(s).as_millis() as u64)
                .unwrap_or(0);
            self.pending_start_instant = None;
            let step_end = chrono::Utc::now().to_rfc3339();
            self.record_trace(IvrTraceEntry {
                step_end_time: Some(step_end),
                duration_ms: duration,
                ..pending
            });
        }

        let mut end_reason_label = Self::end_reason_label(&reason).to_string();
        let skip_provider_end = matches!(
            reason,
            crate::call::app::ExitReason::RemoteHangup(_) | crate::call::app::ExitReason::Cancelled
        );
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
                if target.starts_with("queue:") {
                    SessionEndReason {
                        reason: SessionEndTag::TransferToQueue,
                        detail: Some(target),
                    }
                } else if target.starts_with("toivr:") || target.starts_with("ivr:") {
                    SessionEndReason {
                        reason: SessionEndTag::TransferToIvr,
                        detail: Some(target),
                    }
                } else {
                    SessionEndReason {
                        reason: SessionEndTag::Transfer,
                        detail: Some(target),
                    }
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
        self.record_trace(IvrTraceEntry {
            session_id: session_id.clone(),
            caller,
            callee,
            step_index: self.step_index,
            trigger: crate::rwi::TriggerInfo::new("session_end"),
            provider_url: None,
            action_type: last_action_type,
            action_json: None,
            error: None,
            step_id: last_step_id,
            step_name: last_step_name,
            step_start_time: self.current_step_start_time.clone(),
            step_end_time: Some(chrono::Utc::now().to_rfc3339()),
            duration_ms: 0,
            extra: last_extra,
            end_reason: Some(end_sr.reason.clone()),
            end_detail: end_sr.detail.clone(),
        });

        if !skip_provider_end {
            let provider_session = self.provider_session_context();
            self.provider
                .on_session_end_context(&end_reason, &provider_session)
                .await
                .ok();
        }
        let status = serde_json::to_string(&end_sr.reason)
            .unwrap_or_else(|_| "\"unknown\"".to_string())
            .trim_matches('"')
            .to_string();
        self.record_session_end(&status).await;
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

        // If this IVR was started via ivr.exec, write result to extensions.
        if let Some(ref ext) = self.session_extensions {
            let collected: std::collections::HashMap<String, String> = self
                .sess
                .variables
                .iter()
                .filter(|(k, _)| {
                    !["session_id", "caller", "callee", "direction"].contains(&k.as_str())
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            super::exec::write_ivr_exec_result(
                ext,
                super::exec::build_ivr_exec_result(
                    &status,
                    &end_reason_label,
                    self.last_transfer_target.clone(),
                    collected,
                    0,
                ),
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::app::ivr::{RetryConfig, StepProvider};
    use crate::call::app::testing::MockCallStack;
    use crate::call::app::{ApplicationContext, CallInfo};
    use crate::call::domain::CallCommand;
    use crate::config::Config;
    use async_trait::async_trait;
    use chrono::Utc;
    use sea_orm::DatabaseConnection;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    /// A mock provider that returns pre-defined nodes in sequence
    struct MockProvider {
        nodes: Vec<ActionNode>,
        idx: std::sync::Mutex<usize>,
        start_called: std::sync::Mutex<bool>,
        start_context: std::sync::Mutex<Option<SessionContext>>,
        end_called: std::sync::Mutex<bool>,
        events: std::sync::Mutex<Vec<Option<ProviderEvent>>>,
        contexts: std::sync::Mutex<Vec<ProviderContext>>,
    }

    impl MockProvider {
        fn new(nodes: Vec<ActionNode>) -> Self {
            Self {
                nodes,
                idx: std::sync::Mutex::new(0),
                start_called: std::sync::Mutex::new(false),
                start_context: std::sync::Mutex::new(None),
                end_called: std::sync::Mutex::new(false),
                events: std::sync::Mutex::new(Vec::new()),
                contexts: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    struct MockProviderHandle(Arc<MockProvider>);

    #[async_trait]
    impl ActionProvider for MockProvider {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.events.lock().unwrap().push(ctx.event.clone());
            self.contexts.lock().unwrap().push(ctx);
            let mut idx = self.idx.lock().unwrap();
            if *idx < self.nodes.len() {
                let node = self.nodes[*idx].clone();
                *idx += 1;
                Ok(node)
            } else {
                Err(anyhow::anyhow!("no more nodes"))
            }
        }

        async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
            *self.start_called.lock().unwrap() = true;
            *self.start_context.lock().unwrap() = Some(ctx.clone());
            Ok(())
        }

        async fn on_session_end(
            &self,
            _reason: &SessionEndReason,
            _session_id: &str,
        ) -> anyhow::Result<()> {
            *self.end_called.lock().unwrap() = true;
            Ok(())
        }
    }

    #[async_trait]
    impl ActionProvider for MockProviderHandle {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.0.next_action(ctx).await
        }

        async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
            self.0.on_session_start(ctx).await
        }

        async fn on_session_end(
            &self,
            reason: &SessionEndReason,
            session_id: &str,
        ) -> anyhow::Result<()> {
            self.0.on_session_end(reason, session_id).await
        }
    }

    fn mock_app(nodes: Vec<ActionNode>) -> StepIvrApp {
        StepIvrApp::with_provider(Box::new(MockProvider::new(nodes)))
    }

    fn make_test_context() -> ApplicationContext {
        ApplicationContext::new(
            DatabaseConnection::default(),
            CallInfo {
                session_id: "test-session".into(),
                caller: "1001".into(),
                callee: "2000".into(),
                direction: "inbound".into(),
                started_at: Utc::now(),
                sip_headers: HashMap::new(),
                route_name: None,
            },
            Arc::new(Config::default()),
            reqwest::Client::new(),
        )
    }

    #[tokio::test]
    async fn step_provider_uses_invocation_identity_and_keeps_business_variables_separate() {
        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
            EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            },
        )]));
        let mut context = make_test_context();
        context.invocation = Some(crate::call::app::AppInvocationContext {
            app_execution_id: 2,
            callee: "39230".into(),
            sip_headers: HashMap::from([("X-Business-Type".into(), "34".into())]),
            variables: HashMap::from([
                ("session_id".into(), "business-value".into()),
                ("order_id".into(), "order-001".into()),
            ]),
        });
        let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
        let mut stack = MockCallStack::run_with_context(Box::new(app), context);

        stack
            .assert_cmd(200, "accept", |command| {
                matches!(command, CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |command| matches!(command, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let start = provider.start_context.lock().unwrap().clone().unwrap();
        assert_eq!(start.session_id, "test-session");
        assert_eq!(start.app_execution_id, 2);
        assert_eq!(start.callee, "39230");
        assert_eq!(start.sip_headers.as_ref().unwrap()["X-Business-Type"], "34");
        assert_eq!(start.variables["session_id"], "business-value");
        let contexts = provider.contexts.lock().unwrap();
        assert_eq!(contexts[0].session_id, "test-session");
        assert_eq!(contexts[0].app_execution_id, 2);
        assert_eq!(contexts[0].variables["session_id"], "business-value");
    }

    struct BlockingProvider {
        entered_next: Arc<Notify>,
        release_next: Arc<Notify>,
        end_called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl ActionProvider for BlockingProvider {
        async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.entered_next.notify_one();
            self.release_next.notified().await;
            Ok(ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }))
        }

        async fn on_session_end(
            &self,
            _reason: &SessionEndReason,
            _session_id: &str,
        ) -> anyhow::Result<()> {
            self.end_called.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingProvider;

    #[async_trait]
    impl ActionProvider for FailingProvider {
        async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            Err(anyhow::anyhow!("provider bootstrap failed"))
        }
    }

    #[tokio::test]
    async fn test_transfer() {
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    #[tokio::test]
    async fn transfer_result_finalizes_pending_trace_with_original_trigger() {
        use crate::call::app::ControllerEvent;
        use crate::call::domain::TransferOutcome;
        use crate::rwi::gateway::RwiGateway;

        let mut transfer = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });
        transfer.wait_for_result = true;
        transfer.step_id = Some("transfer-step".into());
        transfer.extra = Some(serde_json::json!({"nodetype": "transfer"}));
        let hangup = ActionNode::new(EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        });
        let gateway = RwiGateway::new();
        let mut events = gateway.subscribe_events();
        let mut app = mock_app(vec![transfer, hangup]);
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "transfer", |c| {
                matches!(c, CallCommand::TransferAwaitResult { target, .. } if target == "2001")
            })
            .await;
        stack
            .event_sender()
            .send(ControllerEvent::TransferResult(
                TransferOutcome::NotConnected,
            ))
            .unwrap();
        stack
            .assert_cmd(200, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
            .await;

        // Single finalized trace: original trigger, end time filled, no
        // wait_finalized duplicate.
        let finalized = events.try_recv().expect("transfer trace must be enqueued");
        assert_eq!(finalized.event.payload["step_id"], "transfer-step");
        assert_eq!(finalized.event.payload["trigger"]["type"], "session_start");
        assert!(finalized.event.payload["step_end_time"].is_string());
        // Subsequent events are legitimate (hangup step trace, session_end) —
        // none may be a wait_finalized duplicate.
        while let Ok(ev) = events.try_recv() {
            assert_ne!(
                ev.event.payload["trigger"]["type"], "wait_finalized",
                "wait completion must not emit a wait_finalized duplicate"
            );
        }
    }

    #[tokio::test]
    async fn test_prompt_then_transfer_via_next() {
        let node = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("hello.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "hello.wav"
                )
            })
            .await;

        stack.audio_complete("ivr_prompt");

        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    #[tokio::test]
    async fn test_prompt_then_provider() {
        use crate::rwi::gateway::RwiGateway;

        let mut prompt = ActionNode::new(EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,
        });
        prompt.step_id = Some("prompt-step".into());
        let mut transfer = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });
        transfer.step_id = Some("transfer-step".into());
        let gateway = RwiGateway::new();
        let mut events = gateway.subscribe_events();
        let mut app = mock_app(vec![prompt, transfer]);
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "hello.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_prompt");

        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let prompt_trace = events.try_recv().expect("prompt trace must be enqueued");
        let transfer_trace = events.try_recv().expect("transfer trace must be enqueued");
        assert_eq!(prompt_trace.event.payload["step_id"], "prompt-step");
        assert_eq!(
            prompt_trace.event.payload["trigger"]["type"],
            "session_start"
        );
        assert_eq!(transfer_trace.event.payload["step_id"], "transfer-step");
    }

    #[tokio::test]
    async fn test_empty_prompt_completes_without_audio() {
        use crate::rwi::gateway::RwiGateway;

        let prompt: ActionNode = serde_json::from_value(serde_json::json!({
            "type": "prompt",
            "tts_text": "",
            "step_id": "empty-prompt-step",
            "extra": { "nodetype": "dynamic_prompt" }
        }))
        .expect("provider-shaped empty Prompt must deserialize");
        let mut transfer = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });
        transfer.step_id = Some("next-step".into());
        let gateway = RwiGateway::new();
        let mut events = gateway.subscribe_events();
        let mut app = mock_app(vec![prompt, transfer]);
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let prompt_trace = events
            .try_recv()
            .expect("empty prompt trace must be enqueued");
        let transfer_trace = events.try_recv().expect("next step trace must be enqueued");
        assert_eq!(prompt_trace.event.payload["step_id"], "empty-prompt-step");
        assert_eq!(prompt_trace.event.payload["action_type"], "Prompt");
        assert_eq!(
            prompt_trace.event.payload["extra"]["nodetype"],
            "dynamic_prompt"
        );
        assert_eq!(transfer_trace.event.payload["step_id"], "next-step");
        assert_eq!(
            transfer_trace.event.payload["trigger"]["type"],
            "audio_complete"
        );
    }

    #[tokio::test]
    async fn test_missing_prompt_audio_reports_provider_error() {
        let prompt: ActionNode = serde_json::from_value(serde_json::json!({
            "type": "prompt",
            "step_id": "missing-audio-step"
        }))
        .expect("provider-shaped Prompt without media must deserialize");
        let mut transfer = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });
        transfer.step_id = Some("error-step".into());
        let provider = Arc::new(MockProvider::new(vec![prompt, transfer]));
        let app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        assert!(provider.events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                Some(ProviderEvent::Error { reason }) if reason == "TTS service not available"
            )
        }));
    }

    #[tokio::test]
    async fn test_dtmf_menu_with_local_entries() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );
        entries.insert(
            "2".into(),
            ActionNode::new(EntryAction::Queue {
                target: "support".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            greeting_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");

        // Drain pending cmds before injecting DTMF
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");

        // DTMF triggers StopPlayback first, then Transfer
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    // ── Provider-driven menu (empty entries): DTMF must be forwarded to provider ──

    struct EventCapturingProvider {
        first_call: std::sync::atomic::AtomicBool,
        captured_events: Arc<std::sync::Mutex<Vec<Option<ProviderEvent>>>>,
    }

    impl EventCapturingProvider {
        fn new() -> Self {
            Self {
                first_call: std::sync::atomic::AtomicBool::new(false),
                captured_events: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl ActionProvider for EventCapturingProvider {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            if !self
                .first_call
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(ActionNode::new(EntryAction::DtmfMenu {
                    greeting: Some("menu.wav".into()),
                    greeting_text: None,
                    greeting_record_list: None,
                    greeting_voice: None,
                    timeout_ms: 5000,
                    max_retries: 3,
                    entries: HashMap::new(),
                    timeout_action: None,
                    invalid_action: None,
                    greeting_api_url: None,
                }));
            }
            self.captured_events.lock().unwrap().push(ctx.event.clone());
            Ok(ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }))
        }
    }

    #[tokio::test]
    async fn test_provider_driven_menu_dtmf_forwards_digit() {
        let provider = EventCapturingProvider::new();
        let events_handle = provider.captured_events.clone();
        let app = StepIvrApp::with_provider(Box::new(provider)).with_name("provider-driven-ivr");
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;

        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let events: Vec<String> = events_handle
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
                Some(ProviderEvent::SessionStart) => Some("session_start".into()),
                Some(ProviderEvent::AudioComplete { .. }) => Some("audio_complete".into()),
                Some(other) => Some(format!("{:?}", other)),
                None => Some("none".into()),
            })
            .collect();
        assert!(
            events.iter().any(|e| e == "dtmf:1"),
            "provider should have received Dtmf{{digit:\"1\"}}, got: {:?}",
            events
        );
    }

    struct GreetingTextMenuProvider {
        captured_events: Arc<std::sync::Mutex<Vec<Option<ProviderEvent>>>>,
    }

    #[async_trait]
    impl ActionProvider for GreetingTextMenuProvider {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.captured_events.lock().unwrap().push(ctx.event.clone());
            if matches!(&ctx.event, Some(ProviderEvent::Dtmf { .. })) {
                return Ok(ActionNode::new(EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                }));
            }
            Ok(ActionNode::new(EntryAction::DtmfMenu {
                greeting: Some("menu.wav".into()),
                greeting_text: Some("请按1转坐席".into()),
                greeting_record_list: None,
                greeting_voice: None,
                timeout_ms: 5000,
                max_retries: 3,
                entries: HashMap::new(),
                timeout_action: None,
                invalid_action: None,
                greeting_api_url: None,
            }))
        }
    }

    #[tokio::test]
    async fn test_provider_driven_menu_tts_dtmf_forwards_digit() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = GreetingTextMenuProvider {
            captured_events: captured.clone(),
        };
        let app = StepIvrApp::with_provider(Box::new(provider)).with_name("menu-tts-ivr");
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;

        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let events: Vec<String> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
                Some(ProviderEvent::SessionStart) => Some("session_start".into()),
                Some(other) => Some(format!("{other:?}")),
                None => Some("none".into()),
            })
            .collect();
        assert!(
            events.iter().any(|e| e == "dtmf:1"),
            "greeting_text DtmfMenu must POST digit to provider, got: {:?}",
            events
        );
    }

    #[tokio::test]
    async fn test_menu_tts_dtmf_during_greeting_forwards_digit() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = GreetingTextMenuProvider {
            captured_events: captured.clone(),
        };
        let app = StepIvrApp::with_provider(Box::new(provider)).with_name("menu-tts-bargein");
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;

        // Barge-in while the TTS/file greeting is still playing.
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("2");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let events: Vec<String> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
                _ => None,
            })
            .collect();
        assert!(
            events.iter().any(|e| e == "dtmf:2"),
            "DTMF during menu_tts greeting must be pushed to provider, got: {:?}",
            events
        );
    }

    struct FlushBufferedDtmfProvider {
        captured_events: Arc<std::sync::Mutex<Vec<Option<ProviderEvent>>>>,
    }

    #[async_trait]
    impl ActionProvider for FlushBufferedDtmfProvider {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.captured_events.lock().unwrap().push(ctx.event.clone());
            if matches!(&ctx.event, Some(ProviderEvent::Dtmf { .. })) {
                return Ok(ActionNode::new(EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                }));
            }
            let menu = ActionNode::new(EntryAction::DtmfMenu {
                greeting: Some("menu.wav".into()),
                greeting_text: Some("请按1".into()),
                greeting_record_list: None,
                greeting_voice: None,
                timeout_ms: 5000,
                max_retries: 3,
                entries: HashMap::new(),
                timeout_action: None,
                invalid_action: None,
                greeting_api_url: None,
            });
            Ok(ActionNode::with_next(
                EntryAction::Prompt {
                    file: Some("announce.wav".into()),
                    tts_text: None,
                    tts_voice: None,
                    record_name_list: None,
                    interruptible: false,
                    tts_api_url: None,
                },
                menu,
            ))
        }
    }

    #[tokio::test]
    async fn test_buffered_dtmf_flushed_after_menu_greeting() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = FlushBufferedDtmfProvider {
            captured_events: captured.clone(),
        };
        let app = StepIvrApp::with_provider(Box::new(provider)).with_name("flush-dtmf-ivr");
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "announce.wav"
                )
            })
            .await;

        // Digit during the non-interruptible announcement is buffered.
        stack.dtmf("1");
        stack.audio_complete("ivr_prompt");

        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;

        // Greeting complete must flush the buffered digit to the provider.
        stack.audio_complete("ivr_menu_greeting");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let events: Vec<String> = captured
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
                Some(ProviderEvent::SessionStart) => Some("session_start".into()),
                Some(other) => Some(format!("{other:?}")),
                None => Some("none".into()),
            })
            .collect();
        assert!(
            events.iter().any(|e| e == "dtmf:1"),
            "buffered DTMF must be delivered after menu greeting, got: {:?}",
            events
        );
    }

    // ── Verify trace trigger for local-menu DTMF ──

    #[tokio::test]
    async fn test_local_menu_dtmf_trace_trigger() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();

        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });

        let mut app: StepIvrApp = mock_app(vec![menu]);
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entry = entries.iter().find(|e| e.action_type == "DtmfMenu");
        assert!(
            menu_entry.is_some(),
            "expected a DtmfMenu trace entry, all entries: {:?}",
            entries
                .iter()
                .map(|e| (&e.action_type, &e.trigger.r#type))
                .collect::<Vec<_>>()
        );
        let menu_entry = menu_entry.unwrap();
        assert_eq!(
            menu_entry.trigger.r#type, "dtmf",
            "local DtmfMenu step trigger should be 'dtmf' (unified), got: {:?}",
            menu_entry.trigger
        );
        assert_eq!(
            menu_entry
                .trigger
                .detail
                .as_ref()
                .and_then(|d| d.get("digit").and_then(|v| v.as_str())),
            Some("1"),
            "DtmfMenu step trigger should contain digit '1'"
        );
    }

    // ── Verify trace trigger for provider-driven menu DTMF ──

    #[tokio::test]
    async fn test_provider_driven_menu_dtmf_trace_trigger() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = EventCapturingProvider::new();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("provider-driven-trace-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entry = entries.iter().find(|e| e.action_type == "DtmfMenu");
        assert!(
            menu_entry.is_some(),
            "expected a DtmfMenu trace entry, all entries: {:?}",
            entries
                .iter()
                .map(|e| (&e.action_type, &e.trigger.r#type))
                .collect::<Vec<_>>()
        );
        let menu_entry = menu_entry.unwrap();
        assert_eq!(
            menu_entry.trigger.r#type, "dtmf",
            "provider-driven DtmfMenu step trigger should be 'dtmf' (after fix), got: {:?}",
            menu_entry.trigger
        );
        assert_eq!(
            menu_entry
                .trigger
                .detail
                .as_ref()
                .and_then(|d| d.get("digit").and_then(|v| v.as_str())),
            Some("1"),
            "provider-driven DtmfMenu step trigger should contain digit '1'"
        );
        let next_entry = entries
            .iter()
            .find(|e| e.action_type == "Transfer")
            .expect("expected a Transfer trace entry");
        assert_eq!(
            next_entry.trigger.r#type, "dtmf",
            "Transfer step (after provider-driven menu) should have trigger 'dtmf', got: {:?}",
            next_entry.trigger
        );
    }

    // ── A3: local-menu unmatched key surfaces a dtmf_menu_invalid trace ──

    #[tokio::test]
    async fn test_local_menu_unmatched_dtmf_invalid_trace() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();

        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });

        let mut app: StepIvrApp = mock_app(vec![menu]);
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Unmatched key: menu must stay alive (no transfer) and the key must
        // still be reported as invalid input.
        stack.dtmf("9");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let invalid = entries
            .iter()
            .find(|e| e.trigger.r#type == "dtmf_menu_invalid")
            .expect("unmatched key must produce a dtmf_menu_invalid trace entry");
        assert_eq!(
            invalid
                .trigger
                .detail
                .as_ref()
                .and_then(|d| d.get("digit").and_then(|v| v.as_str())),
            Some("9"),
            "dtmf_menu_invalid trace must carry the rejected digit"
        );
        assert_eq!(invalid.action_type, "DtmfMenu");

        // The menu is still waiting: a subsequent valid key must resolve.
        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    // ── A2: menu timeout overwrites the pending step trigger ──

    #[tokio::test]
    async fn test_provider_driven_menu_timeout_trace_trigger() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = EventCapturingProvider::new();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("timeout-trace-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Fire the armed menu timeout → provider-driven path asks the
        // provider with DtmfMenuTimeout → Transfer.
        stack.timeout("ivr_dtmf_timeout");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entry = entries
            .iter()
            .find(|e| e.action_type == "DtmfMenu")
            .expect("expected a DtmfMenu trace entry");
        assert_eq!(
            menu_entry.trigger.r#type, "dtmf_menu_timeout",
            "menu step ended by timeout must carry trigger 'dtmf_menu_timeout' (symmetric with key presses), got: {:?}",
            menu_entry.trigger
        );
    }

    // ── B: InputPhone completion finalizes the wait with its original trigger ──

    #[tokio::test]
    async fn test_input_phone_completion_keeps_original_trigger() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let app = mock_app(vec![
            ActionNode::new(EntryAction::InputPhone {
                prompt: None,
                prompt_text: None,
                prompt_voice: None,
                min_digits: 1,
                max_digits: 1,
                timeout_ms: 5000,
                inter_digit_timeout_ms: 3000,
                terminator: "#".into(),
            }),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        ]);
        let mut app = app;
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        stack.dtmf("1");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let finalized = entries
            .iter()
            .find(|e| e.trigger.r#type == "phone_collected")
            .expect("InputPhone completion must emit a phone_collected entry");
        assert_eq!(
            finalized.action_type, "InputPhone",
            "the finalized entry must describe the step that finished"
        );
        assert!(
            finalized.step_end_time.is_some(),
            "the finalized entry must carry step_end_time (completion marker)"
        );
        assert!(
            !entries.iter().any(|e| e.trigger.r#type == "session_end"
                && e.step_id.is_some()
                && e.end_reason.is_none()),
            "mid-flow wait completion must NOT be reported as session_end (reserved for on_exit with end_reason)"
        );
    }

    // ── Barge-in trace: DTMF during menu greeting ──

    #[tokio::test]
    async fn test_menu_tts_dtmf_during_greeting_trace() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = EventCapturingProvider::new();
        let events_handle = provider.captured_events.clone();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("bargein-trace-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Barge-in while the greeting is still playing.
        stack.dtmf("2");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // No spurious AudioComplete may follow the DTMF (a stop must not be
        // reported as a natural playback completion).
        let events: Vec<String> = events_handle
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Some(ProviderEvent::Dtmf { digit }) => Some(format!("dtmf:{digit}")),
                Some(ProviderEvent::AudioComplete { .. }) => Some("audio_complete".into()),
                _ => None,
            })
            .collect();
        let dtmf_pos = events
            .iter()
            .position(|e| e == "dtmf:2")
            .expect("DTMF must be forwarded");
        assert!(
            !events[dtmf_pos + 1..].iter().any(|e| e == "audio_complete"),
            "no AudioComplete may be forwarded to the provider after barge-in DTMF, got: {:?}",
            events
        );

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entry = entries
            .iter()
            .find(|e| e.action_type == "DtmfMenu")
            .expect("expected a DtmfMenu trace entry");
        assert_eq!(
            menu_entry.trigger.r#type, "dtmf",
            "barge-in DtmfMenu step trigger should be 'dtmf', got: {:?}",
            menu_entry.trigger
        );
        assert_eq!(
            menu_entry
                .trigger
                .detail
                .as_ref()
                .and_then(|d| d.get("digit").and_then(|v| v.as_str())),
            Some("2")
        );
    }

    // ── menu_tts_api: greeting fetched via API (greeting_api_url) ──

    #[tokio::test]
    async fn test_menu_tts_api_greeting_fetch_and_trace() {
        use crate::call::app::ivr::trace::IvrTraceCollector;
        use axum::{Router, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let trace = IvrTraceCollector::new();
        let fetches = Arc::new(AtomicUsize::new(0));
        let fetches_for_handler = fetches.clone();
        let tts_app = Router::new().route(
            "/tts",
            get(move || {
                let fetches = fetches_for_handler.clone();
                async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"tts_text": "请按1"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        crate::utils::spawn(async move {
            axum::serve(listener, tts_app).await.ok();
        });
        let greeting_api = format!("http://{}:{}/tts", addr.ip(), addr.port());

        let provider = EventCapturingProvider::new();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("menu-tts-api-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
        // Sanity for the harness: the EventCapturingProvider menu above does
        // not use greeting_api_url. This test's real assertion target is the
        // greeting-fetched menu below.
        drop(stack);

        // Now the actual greeting_api_url menu: the API text is fetched, and
        // with no TTS engine configured the menu degrades to a silent wait
        // (NoAudio path) but keys/timeouts/traces must behave identically.
        let trace2 = IvrTraceCollector::new();
        let menu_node = ActionNode::new(EntryAction::DtmfMenu {
            greeting: None,
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries: HashMap::new(),
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: Some(greeting_api),
        });
        let mut app2 = mock_app(vec![
            menu_node,
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        ]);
        app2.trace = Some(trace2.clone());
        let mut stack2 = MockCallStack::run(Box::new(app2), "1001", "2000");

        stack2
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        // The fetched text is synthesized via TTS when a TTS backend is
        // available (greeting plays), otherwise the menu degrades to a
        // silent wait (NoAudio path). Both paths must wait for keys.
        if let Some(CallCommand::Play { .. }) = stack2.next_cmd(500).await {
            stack2.audio_complete("ivr_menu_greeting");
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = stack2.drain_cmds();
        stack2.dtmf("1");
        stack2
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack2
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        assert!(
            fetches.load(Ordering::SeqCst) >= 1,
            "greeting_api_url must be fetched when the menu executes"
        );

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let sessions = trace2.sessions().await;
        let sess = &sessions[0];
        let entries = trace2.query_by_session(&sess.session_id).await;
        let menu_entry = entries
            .iter()
            .find(|e| e.action_type == "DtmfMenu")
            .expect("expected a DtmfMenu trace entry");
        assert_eq!(
            menu_entry.trigger.r#type, "dtmf",
            "menu_tts_api menu step trigger should be 'dtmf', got: {:?}",
            menu_entry.trigger
        );
        assert_eq!(
            menu_entry
                .trigger
                .detail
                .as_ref()
                .and_then(|d| d.get("digit").and_then(|v| v.as_str())),
            Some("1")
        );
    }

    // ── Timeout replay loop: each replay produces its own DtmfMenu trace ──

    struct MenuReplayProvider {
        timeout_count: std::sync::atomic::AtomicUsize,
        captured_events: Arc<std::sync::Mutex<Vec<Option<ProviderEvent>>>>,
    }

    #[async_trait]
    impl ActionProvider for MenuReplayProvider {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.captured_events.lock().unwrap().push(ctx.event.clone());
            match ctx.event {
                // Initial step and first timeout replay the menu (like
                // ThirdPartyTreeProvider); then give up with a transfer.
                Some(ProviderEvent::SessionStart) | Some(ProviderEvent::DtmfMenuTimeout)
                    if self
                        .timeout_count
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        <= 1 =>
                {
                    Ok(ActionNode::new(EntryAction::DtmfMenu {
                        greeting: Some("menu.wav".into()),
                        greeting_text: None,
                        greeting_record_list: None,
                        greeting_voice: None,
                        timeout_ms: 5000,
                        max_retries: 3,
                        entries: HashMap::new(),
                        timeout_action: None,
                        invalid_action: None,
                        greeting_api_url: None,
                    }))
                }
                _ => Ok(ActionNode::new(EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                })),
            }
        }
    }

    #[tokio::test]
    async fn test_menu_tts_timeout_replay_produces_trace_per_cycle() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = MenuReplayProvider {
            timeout_count: std::sync::atomic::AtomicUsize::new(0),
            captured_events: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("replay-trace-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;

        // Cycle 1: greeting completes → timeout → provider replays menu.
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.timeout("ivr_dtmf_timeout");
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;

        // Cycle 2: second timeout → provider gives up with Transfer.
        stack.timeout("ivr_dtmf_timeout");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.timeout("ivr_dtmf_timeout");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.action_type == "DtmfMenu")
            .collect();
        assert!(
            menu_entries.len() >= 2,
            "each menu cycle (initial + replay) must produce its own DtmfMenu trace, got: {:?}",
            entries
                .iter()
                .map(|e| (&e.action_type, &e.trigger.r#type))
                .collect::<Vec<_>>()
        );
        // The replayed menu step ended by the second timeout.
        assert!(
            menu_entries
                .iter()
                .any(|e| e.trigger.r#type == "dtmf_menu_timeout"),
            "timeout-ended menu steps must carry trigger 'dtmf_menu_timeout'"
        );
    }

    #[tokio::test]
    async fn test_hangup_no_prompt() {
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::Hangup {
                prompt: None,
                prompt_text: None,
                prompt_voice: None,
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    #[tokio::test]
    async fn test_jump_ivr() {
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::JumpIvr {
                route_point: "39290".into(),
                params: HashMap::from([("businessType".into(), "7".into())]),
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "transfer", |c| {
                matches!(c, CallCommand::Transfer { target, .. }
                    if target == "toivr:39290?businessType=7")
            })
            .await;
    }

    #[tokio::test]
    async fn test_bridge() {
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::Bridge {
                create_room_uri: "https://voip.example.com/rooms".into(),
                headers: HashMap::from([("Authorization".into(), "Bearer token".into())]),
                timeout_ms: Some(30000),
                return_app: None,
                return_target: None,
                success: None,
                failure: None,
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "transfer", |c| {
                matches!(c, CallCommand::Transfer { target, .. }
                    if target == "bridge:https://voip.example.com/rooms")
            })
            .await;
    }

    #[tokio::test]
    async fn test_bridge_with_return_to_ivr() {
        // Bridge with return_to_ivr must append the resume marker as a query
        // string (common.rs execute_action: sep '?' when uri has no '?').
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::Bridge {
                create_room_uri: "wss://voip.example.com/room1".into(),
                headers: HashMap::new(),
                timeout_ms: None,
                return_app: Some("ivr".into()),
                return_target: Some("main".into()),
                success: Some(Box::new(ActionNode::new(EntryAction::Hangup {
                    prompt: None,
                    prompt_text: None,
                    prompt_voice: None,
                }))),
                failure: None,
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "transfer", |c| {
                matches!(c, CallCommand::Transfer { target, .. }
                    if target == "bridge:wss://voip.example.com/room1?return_app=ivr&return_target=main")
            })
            .await;
    }

    #[tokio::test]
    async fn test_route_to_agent() {
        // RouteToAgent is terminal: it substitutes the target and emits a plain
        // Transfer. The skill_group_id/key_id/channel_code are written to
        // internal session variables consumed by downstream routing (verified
        // end-to-end in the tier3 step-mode test).
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::RouteToAgent {
                target: "1001".into(),
                skill_group_id: Some("support".into()),
                key_id: Some("night".into()),
                channel_code: Some("chat".into()),
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "1001"),
            )
            .await;
    }

    #[tokio::test]
    async fn test_trace_integration() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let mut app = mock_app(vec![ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        })]);
        app.trace = Some(trace.clone());
        app.ivr_name = Some("test-ivr".to_string());

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        // Wait for async trace writes
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify trace was recorded
        let sessions = trace.sessions().await;
        assert!(!sessions.is_empty(), "expected at least one trace session");
        let sess = &sessions[0];
        assert_eq!(sess.caller, "1001");
        assert_eq!(sess.callee, "2000");
        assert_eq!(sess.ivr_name.as_deref(), Some("test-ivr"));
        assert_eq!(sess.status, "transfer");

        // Verify trace entries exist
        let entries = trace.query_by_session(&sess.session_id).await;
        assert!(!entries.is_empty(), "expected at least one trace entry");
        assert!(
            entries.iter().any(|e| e.action_type == "Transfer"),
            "expected a Transfer step entry"
        );
        // The session_end entry should carry the transfer end reason + detail.
        let session_end = entries
            .iter()
            .find(|e| e.trigger.r#type == "session_end")
            .expect("expected a session_end trace entry");
        assert_eq!(
            session_end.end_reason,
            Some(crate::call::app::ivr::provider::SessionEndTag::Transfer)
        );
        assert_eq!(session_end.end_detail.as_deref(), Some("2001"));
        assert_eq!(
            session_end.action_type, "Transfer",
            "session_end entry should reuse the last node's action_type"
        );
    }

    #[tokio::test]
    async fn test_next_chain_skip() {
        // When next is present, provider should NOT be called after completion
        let node = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("hello.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "hello.wav"
                )
            })
            .await;

        // After prompt completes, Transfer should fire WITHOUT provider call
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    #[tokio::test]
    async fn test_error_action_returns_error() {
        // Mock provider that returns a VoipBridge (which succeeds) initially,
        // but we test with an InputVoice which returns an error
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::InputVoice {
                scene: "test_scene".into(),
                timeout_ms: 5000,
            })])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        // InputVoice should produce an answer error in the app
        // The app currently returns the error from execute_action
        // which gets propagated up through on_enter
    }

    // ── Integration tests with real HTTP StepProvider ─────────────────────

    /// Start a lightweight HTTP server that returns pre-programmed responses.
    async fn spawn_mock_provider(responses: Vec<serde_json::Value>) -> String {
        use axum::{Json, Router, routing::post};
        use std::sync::Mutex;

        let responses = Arc::new(Mutex::new(responses.into_iter()));
        let app = Router::new().route(
            "/ivr/step",
            post(move |Json(_body): Json<serde_json::Value>| {
                let resp = {
                    let mut it = responses.lock().unwrap();
                    it.next().unwrap_or(serde_json::json!({"type": "hangup"}))
                };
                async move { Json(resp) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{}:{}/ivr/step", addr.ip(), addr.port())
    }

    /// Mock provider with separate `/step` and `/fail` scripted responses.
    async fn spawn_mock_provider_with_fail(
        step_responses: Vec<serde_json::Value>,
        fail_responses: Vec<Result<serde_json::Value, u16>>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
        use std::sync::Mutex;

        let paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let paths_step = paths.clone();
        let paths_fail = paths.clone();
        let step_q = Arc::new(Mutex::new(step_responses.into_iter()));
        let fail_q = Arc::new(Mutex::new(fail_responses.into_iter()));

        let app = Router::new()
            .route(
                "/ivr/step",
                post(move |Json(_body): Json<serde_json::Value>| {
                    paths_step.lock().unwrap().push("step".into());
                    let resp = {
                        let mut it = step_q.lock().unwrap();
                        it.next().unwrap_or(serde_json::json!({"type": "hangup"}))
                    };
                    async move { Json(resp) }
                }),
            )
            .route(
                "/ivr/step/fail",
                post(move |Json(_body): Json<serde_json::Value>| {
                    paths_fail.lock().unwrap().push("fail".into());
                    let next = {
                        let mut it = fail_q.lock().unwrap();
                        it.next()
                    };
                    async move {
                        match next {
                            Some(Ok(body)) => Json(body).into_response(),
                            Some(Err(code)) => StatusCode::from_u16(code)
                                .unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
                                .into_response(),
                            None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        }
                    }
                }),
            )
            .route(
                "/ivr/step/start",
                post(|| async { Json(serde_json::json!({"ok": true})) }),
            )
            .route(
                "/ivr/step/end",
                post(|| async { Json(serde_json::json!({"ok": true})) }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (
            format!("http://{}:{}/ivr/step", addr.ip(), addr.port()),
            paths,
        )
    }

    #[tokio::test]
    async fn test_http_e2e_fail_recovers_with_transfer() {
        // /step returns tree-only `repeat` → execute fails → /fail returns transfer.
        let (url, paths) = spawn_mock_provider_with_fail(
            vec![serde_json::json!({"type": "repeat"})],
            vec![Ok(
                serde_json::json!({"type": "transfer", "target": "2001"}),
            )],
        )
        .await;

        let provider = StepProvider::new(&url, reqwest::Client::new()).with_retry(RetryConfig {
            max_retries: 1,
            timeout_ms: 2000,
            retry_delay_ms: 10,
            fallback_action: None,
        });
        let mut stack = MockCallStack::run(
            Box::new(StepIvrApp::with_provider(Box::new(provider)).with_name("fail-ok")),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(
                2000,
                "transfer after /fail",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let seen = paths.lock().unwrap().clone();
        assert!(
            seen.iter().any(|p| p == "fail"),
            "expected /fail hit, paths={seen:?}"
        );
    }

    #[tokio::test]
    async fn test_http_e2e_fail_down_jumps_ivr_fallback() {
        let (url, paths) = spawn_mock_provider_with_fail(
            vec![serde_json::json!({"type": "repeat"})],
            vec![Err(503)],
        )
        .await;

        let fb = Arc::new(crate::config::IvrFallbackConfig {
            default: Some("default_ivr".into()),
            rules: vec![crate::config::IvrFallbackRule {
                name: Some("vip".into()),
                priority: 10,
                match_conditions: crate::proxy::routing::MatchConditions {
                    from_user: Some("1001".into()),
                    ..Default::default()
                },
                target: "builtin_vip".into(),
            }],
        });
        let provider = StepProvider::new(&url, reqwest::Client::new())
            .with_retry(RetryConfig {
                max_retries: 1,
                timeout_ms: 500,
                retry_delay_ms: 10,
                fallback_action: None,
            })
            .with_prefer_ivr_fallback(true);
        let mut context = make_test_context();
        context.invocation = Some(crate::call::app::AppInvocationContext {
            app_execution_id: 2,
            callee: "39230".into(),
            sip_headers: HashMap::new(),
            variables: HashMap::from([
                ("caller".into(), "business-caller".into()),
                ("callee".into(), "business-callee".into()),
                ("session_id".into(), "business-session".into()),
            ]),
        });
        let mut stack = MockCallStack::run_with_context(
            Box::new(
                StepIvrApp::with_provider(Box::new(provider))
                    .with_name("fail-fb")
                    .with_ivr_fallback(Some(fb)),
            ),
            context,
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(2000, "direct IVR fallback", |c| {
                matches!(
                    c,
                    CallCommand::Transfer { target, .. }
                        if target.starts_with("ivr:builtin_vip")
                )
            })
            .await;

        let seen = paths.lock().unwrap().clone();
        assert!(
            seen.iter().any(|p| p == "fail"),
            "expected /fail before fallback, paths={seen:?}"
        );
    }

    #[tokio::test]
    async fn test_http_e2e_step_down_jumps_default_fallback() {
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        let app = Router::new().route(
            "/ivr/step",
            post(|| async { StatusCode::SERVICE_UNAVAILABLE.into_response() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let url = format!("http://{}:{}/ivr/step", addr.ip(), addr.port());

        let fb = Arc::new(crate::config::IvrFallbackConfig {
            default: Some("default_ivr".into()),
            rules: vec![],
        });
        let provider = StepProvider::new(&url, reqwest::Client::new())
            .with_retry(RetryConfig {
                max_retries: 1,
                timeout_ms: 200,
                retry_delay_ms: 10,
                fallback_action: Some(ActionNode::new(EntryAction::Hangup {
                    prompt: Some("sounds/error.wav".into()),
                    prompt_text: None,
                    prompt_voice: None,
                })),
            })
            .with_prefer_ivr_fallback(true);
        let mut stack = MockCallStack::run(
            Box::new(
                StepIvrApp::with_provider(Box::new(provider))
                    .with_name("step-down")
                    .with_ivr_fallback(Some(fb)),
            ),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        // prefer_ivr_fallback must skip retry.fallback hangup and use direct IVR instead.
        stack
            .assert_cmd(2000, "direct default IVR", |c| {
                matches!(
                    c,
                    CallCommand::Transfer { target, .. }
                        if target.starts_with("ivr:default_ivr")
                )
            })
            .await;
    }

    #[tokio::test]
    async fn test_http_provider_full_flow() {
        // Provider returns: Prompt(with next:Transfer) → ...
        let entry = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("hello.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );
        let resp = serde_json::to_value(&entry).unwrap();

        let url = spawn_mock_provider(vec![resp]).await;
        let app = StepIvrApp::new(&url, reqwest::Client::new());

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "hello.wav"
                )
            })
            .await;
        // Audio complete triggers the next chain
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    #[tokio::test]
    async fn test_http_provider_dtmf_flow() {
        // Provider returns DtmfMenu → user presses 1 → provider returns Transfer
        use std::collections::HashMap;

        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu_resp = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            greeting_api_url: None,
        });

        let url = spawn_mock_provider(vec![serde_json::to_value(&menu_resp).unwrap()]).await;

        let app = StepIvrApp::new(&url, reqwest::Client::new());

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");

        // DTMF "1" → local entries match → Transfer without provider call
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(100));
        stack.dtmf("1");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = stack.drain_cmds();
        stack
            .assert_cmd(500, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    #[tokio::test]
    async fn test_http_provider_provider_called_after_menu() {
        // Provider returns DtmfMenu WITHOUT entries → user presses 1
        // → provider should be called with dtmf event
        let menu_resp = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries: std::collections::HashMap::new(),
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });
        let transfer_resp = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });

        let url = spawn_mock_provider(vec![
            serde_json::to_value(&menu_resp).unwrap(),
            serde_json::to_value(&transfer_resp).unwrap(),
        ])
        .await;

        let app = StepIvrApp::new(&url, reqwest::Client::new());

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");

        // DTMF "1" → no local entry → provider should be called
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(100));
        stack.dtmf("1");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(100));
        // After provider returns, StopPlayback + Transfer
        stack
            .assert_cmd(500, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    // ── Early DTMF (awaiting_dtmf flag) tests ───────────────────────────────

    /// DTMF pressed during a non-interruptible Prompt should be silently
    /// ignored — the provider must NOT be called with the digit, and the flow
    /// must continue normally after audio completes.
    #[tokio::test]
    async fn test_early_dtmf_during_prompt_is_ignored() {
        let prompt = ActionNode::new(EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,
        });
        let transfer = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });

        let mut stack =
            MockCallStack::run(Box::new(mock_app(vec![prompt, transfer])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "hello.wav"
                )
            })
            .await;

        // User presses a key WHILE the prompt is still playing.
        // This must be ignored — no extra commands should be generated.
        let _ = stack.drain_cmds();
        stack.dtmf("5");
        // Give the event loop a moment to process the DTMF event.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // No stop/transfer/provider command should have been emitted.
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "early DTMF should be ignored, but got commands: {cmds:?}"
        );

        // Now audio completes normally — flow should proceed to Transfer.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// Same as above but the Prompt has `next` chained — verifying that the
    /// chained node fires correctly after audio complete even if a stray DTMF
    /// was received during playback.
    #[tokio::test]
    async fn test_early_dtmf_with_chained_next() {
        let node = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("intro.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "3003".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "intro.wav"
                )
            })
            .await;

        // Stray DTMF during playback — should be ignored.
        let _ = stack.drain_cmds();
        stack.dtmf("9");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(stack.drain_cmds().is_empty());

        // Audio completes → chained Transfer fires.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "3003"),
            )
            .await;
    }

    /// Verify that DTMF still works correctly when the IVR IS expecting input
    /// (i.e. after a DtmfMenu greeting finishes).  This is a regression guard
    /// ensuring the `awaiting_dtmf` flag doesn't block legitimate input.
    #[tokio::test]
    async fn test_dtmf_accepted_after_menu_greeting() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            greeting_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;

        // Greeting finishes → awaiting_dtmf becomes true.
        stack.audio_complete("ivr_menu_greeting");

        // Now DTMF should be accepted (pending_menu is set, so it goes through
        // the local menu lookup path).
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stack.dtmf("1");

        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// Regression (bug): after a valid DTMF resolves a DtmfMenu and moves the
    /// flow to the next node, the old `ivr_dtmf_timeout` (armed when the
    /// greeting finished) must be cancelled. If it is left running it fires
    /// `dtmf_timeout` at the *current* node — e.g. a PROMPT_BREAK step before
    /// the to-agent transfer — which the provider doesn't expect and answers
    /// with hangup, killing the call before the transfer ever starts.
    #[tokio::test]
    async fn test_stale_menu_timeout_after_valid_dtmf_is_ignored() {
        let mut entries = HashMap::new();
        // Matched key → a non-terminal prompt chain (mirrors PROMPT_BREAK →
        // toagent_by_kfb). The stale menu timeout must NOT derail it.
        entries.insert(
            "1".into(),
            ActionNode::with_next(
                EntryAction::Prompt {
                    file: Some("prompt_break.wav".into()),
                    tts_text: None,
                    tts_voice: None,
                    record_name_list: None,
                    interruptible: false,
                    tts_api_url: None,
                },
                ActionNode::new(EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                }),
            ),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 3000,
            max_retries: 3,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;

        // Greeting finishes → menu waits for input and arms ivr_dtmf_timeout.
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Valid key press → local menu match → next node (prompt_break) runs.
        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(200, "play prompt_break", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "prompt_break.wav"
                )
            })
            .await;

        // The stale 3s menu timeout now fires while prompt_break is playing.
        // It must be ignored — no hangup, no provider contact, no commands.
        stack.timeout("ivr_dtmf_timeout");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "stale menu timeout must not derail the current node, got: {cmds:?}"
        );

        // The current node chain completes normally → to-agent transfer fires.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// A *legitimate* menu timeout (no DTMF pressed, pending_menu still set)
    /// must keep working: the timeout_action executes and the flow moves on.
    /// Guards against the stale-timeout guard accidentally swallowing real
    /// timeouts.
    #[tokio::test]
    async fn test_local_menu_legit_timeout_fires_timeout_action() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2002".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 3000,
            max_retries: 3,
            entries,
            timeout_action: Some(Box::new(ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }))),
            invalid_action: None,
            greeting_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // No DTMF — the menu genuinely times out → timeout_action (Transfer) runs.
        stack.timeout("ivr_dtmf_timeout");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// Provider-driven DtmfMenu: a DTMF digit is forwarded to the provider and
    /// the flow moves to a non-terminal node. The stale `ivr_dtmf_timeout`
    /// armed while the menu waited must be cancelled so it can't fire
    /// `dtmf_timeout` at the provider-driven node.
    #[tokio::test]
    async fn test_provider_driven_menu_dtmf_no_stale_timeout() {
        // Provider sequence: session_start → provider-driven DtmfMenu (empty
        // entries); dtmf → non-terminal Prompt chained to Transfer.
        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 3000,
            max_retries: 3,
            entries: HashMap::new(),
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });
        let prompt = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("prompt_break.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu, prompt])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Digit forwarded to provider → provider returns prompt_break node.
        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(200, "play prompt_break", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "prompt_break.wav"
                )
            })
            .await;

        // Stale menu timeout must be ignored — no commands emitted.
        stack.timeout("ivr_dtmf_timeout");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "stale menu timeout must not derail provider-driven node, got: {cmds:?}"
        );

        // prompt_break chain completes → Transfer fires.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// Local menu with no timeout_action: each timeout must retry silently
    /// (re-arm the timeout and stay in the menu) instead of forwarding
    /// DtmfTimeout to the provider, which would leave stale pending_menu
    /// intercepting future DTMF.
    #[tokio::test]
    async fn test_local_menu_timeout_retry_without_timeout_action() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2002".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 100,
            max_retries: 2,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "menu.wav"
                )
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First timeout (retry_count 0→1, not yet >= max_retries=2):
        // must stay in menu, re-arm timeout, no commands emitted.
        stack.timeout("ivr_dtmf_timeout");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "timeout retry without timeout_action must not contact provider, got: {cmds:?}"
        );

        // Second timeout (retry_count 1→2, >= max_retries=2):
        // must fall through to Hangup.
        stack.timeout("ivr_dtmf_timeout");
        stack
            .assert_cmd(200, "hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    /// Regression (bug): pressing a NON-matching key while a DtmfMenu greeting
    /// is still playing must NOT stop the greeting and must NOT be forwarded to
    /// the provider. Previously the unmatched digit fell through to the
    /// provider, which could return a terminal node and silently end the IVR.
    /// Here the provider's fallback node is a `Hangup` to simulate that
    /// scenario — with the fix the flow stays alive and the greeting finishes.
    #[tokio::test]
    async fn test_non_matching_dtmf_during_menu_greeting_keeps_flow_alive() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });
        // Fallback provider node — would be executed only if the bug forwarded
        // the stray digit to the provider.
        let hangup = ActionNode::new(EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu, hangup])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;

        // Press a non-matching key WHILE the greeting is still playing.
        let _ = stack.drain_cmds();
        stack.dtmf("5");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Nothing should have happened: no StopPlayback, no provider call, no
        // Hangup. The greeting must continue uninterrupted.
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "non-matching DTMF during greeting must not stop playback or contact provider, got: {cmds:?}"
        );

        // Greeting finishes normally → menu enters the waiting state.
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // A subsequent VALID key must still work — proving the flow survived.
        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// Regression (bug): a non-matching key pressed while the menu is in its
    /// waiting window (greeting finished) and no `invalid_action` is configured
    /// must be ignored, keeping the flow alive for a subsequent valid key.
    #[tokio::test]
    async fn test_non_matching_dtmf_in_menu_waiting_keeps_flow_alive() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });
        let hangup = ActionNode::new(EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![menu, hangup])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "menu.wav"
                )
            })
            .await;
        // Greeting finished → waiting for input.
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Non-matching key during the waiting window — must NOT be forwarded.
        stack.dtmf("5");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "non-matching DTMF in waiting window must not contact provider, got: {cmds:?}"
        );

        // Valid key still works → flow is alive.
        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// Regression: DTMF during an *interruptible* Prompt must be forwarded to
    /// the provider.  The `awaiting_dtmf` / `interrupt_on_dtmf` flags should
    /// NOT block legitimate input when the node explicitly allows interruption.
    #[tokio::test]
    async fn test_interruptible_prompt_forwards_dtmf_to_provider() {
        let prompt = ActionNode::new(EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: true,
            tts_api_url: None,
        });
        let transfer = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });

        let mut stack =
            MockCallStack::run(Box::new(mock_app(vec![prompt, transfer])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "hello.wav"
                )
            })
            .await;

        // While the interruptible prompt is still playing, inject a DTMF digit.
        // It MUST be forwarded to the provider (not silently ignored).
        let _ = stack.drain_cmds();
        stack.dtmf("5");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The app should stop the audio first, then ask the provider for
        // the next action (MockProvider returns Transfer("2001")).
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    // ── interruptible parameter verification tests ─────────────────────────

    /// The Play command emitted for a **non-interruptible** Prompt must carry
    /// `interrupt_on_dtmf: false` in its PlayOptions.
    #[tokio::test]
    async fn test_non_interruptible_prompt_play_cmd_has_flag_false() {
        let node = ActionNode::new(EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        // Grab the Play command and inspect its options.
        let play_cmd = stack.next_cmd(200).await.expect("expected a Play command");
        match &play_cmd {
            CallCommand::Play { options, .. } => {
                let opts = options.as_ref().expect("PlayOptions must be set");
                assert!(
                    !opts.interrupt_on_dtmf,
                    "non-interruptible Prompt must have interrupt_on_dtmf=false, got true"
                );
            }
            other => panic!("expected Play command, got {other:?}"),
        }
    }

    /// The Play command emitted for an **interruptible** Prompt must carry
    /// `interrupt_on_dtmf: true` in its PlayOptions.
    #[tokio::test]
    async fn test_interruptible_prompt_play_cmd_has_flag_true() {
        let node = ActionNode::new(EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: true,
            tts_api_url: None,
        });

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        let play_cmd = stack.next_cmd(200).await.expect("expected a Play command");
        match &play_cmd {
            CallCommand::Play { options, .. } => {
                let opts = options.as_ref().expect("PlayOptions must be set");
                assert!(
                    opts.interrupt_on_dtmf,
                    "interruptible Prompt must have interrupt_on_dtmf=true, got false"
                );
            }
            other => panic!("expected Play command, got {other:?}"),
        }
    }

    /// After an interruptible Prompt finishes naturally (no DTMF), a chained
    /// non-interruptible Prompt must NOT be interruptible — the
    /// `interrupt_on_dtmf` flag must not leak from the previous node.
    #[tokio::test]
    async fn test_interruptible_then_non_interruptible_no_leak() {
        let node = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("first.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: true,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Prompt {
                file: Some("second.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            }),
        );

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        // First prompt (interruptible) starts playing.
        stack
            .assert_cmd(200, "play first", |c| {
                matches!(c, CallCommand::Play { source, options, .. }
                    if matches!(source, crate::call::domain::MediaSource::File { path } if path == "first.wav")
                    && options.as_ref().map_or(false, |o| o.interrupt_on_dtmf))
            })
            .await;

        // First prompt finishes naturally — no DTMF pressed.
        stack.audio_complete("ivr_prompt");

        // Second prompt (non-interruptible) starts playing.
        stack
            .assert_cmd(200, "play second", |c| {
                matches!(c, CallCommand::Play { source, options, .. }
                    if matches!(source, crate::call::domain::MediaSource::File { path } if path == "second.wav")
                    && options.as_ref().map_or(false, |o| !o.interrupt_on_dtmf))
            })
            .await;

        // Press a key during the second (non-interruptible) prompt.
        let _ = stack.drain_cmds();
        stack.dtmf("5");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // No stop / transfer / any command should have been emitted.
        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "DTMF during non-interruptible prompt (after interruptible) should be ignored, got: {cmds:?}"
        );

        // Audio completes normally → provider is asked for next node.
        // MockProvider has no more nodes → fallback plays error.wav then hangup.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "fallback play", |c| {
                matches!(c, CallCommand::Play { source, .. }
                    if matches!(source, crate::call::domain::MediaSource::File { path } if path.contains("error.wav")))
            })
            .await;
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "fallback hangup", |c| {
                matches!(c, CallCommand::Hangup(_))
            })
            .await;
    }

    /// Multiple DTMF digits pressed during a non-interruptible Prompt must
    /// ALL be silently ignored — none should trigger stop_audio or a provider
    /// call.
    #[tokio::test]
    async fn test_multiple_dtmf_during_non_interruptible_all_ignored() {
        let node = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("hello.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );

        let mut stack = MockCallStack::run(Box::new(mock_app(vec![node])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| matches!(c, CallCommand::Play { .. }))
            .await;

        // Hammer multiple digits while playback is in progress.
        let _ = stack.drain_cmds();
        for d in &["1", "2", "3", "*", "#"] {
            stack.dtmf(*d);
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let cmds = stack.drain_cmds();
        assert!(
            cmds.is_empty(),
            "all DTMF during non-interruptible prompt should be ignored, got: {cmds:?}"
        );

        // After natural completion, the chained Transfer fires.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    /// JSON without an explicit `interruptible` field must default to `false`
    /// (non-interruptible) per the `#[serde(default)]` attribute.
    #[test]
    fn test_prompt_json_without_interruptible_defaults_false() {
        let json = r#"{"type":"prompt","file":"hello.wav"}"#;
        let node: ActionNode = serde_json::from_str(json).expect("parse JSON");
        match node.action {
            EntryAction::Prompt { interruptible, .. } => {
                assert!(
                    !interruptible,
                    "missing `interruptible` must default to false"
                );
            }
            other => panic!("expected Prompt action, got {other:?}"),
        }
    }

    /// JSON with `"interruptible": true` must parse correctly.
    #[test]
    fn test_prompt_json_with_interruptible_true() {
        let json = r#"{"type":"prompt","file":"hello.wav","interruptible":true}"#;
        let node: ActionNode = serde_json::from_str(json).expect("parse JSON");
        match node.action {
            EntryAction::Prompt { interruptible, .. } => {
                assert!(interruptible, "interruptible=true must parse");
            }
            other => panic!("expected Prompt action, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_remote_hangup_skips_provider_session_end() {
        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
            EntryAction::Prompt {
                file: Some("hello.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },
        )]));
        let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
        app.ivr_name = Some("test-ivr".to_string());
        app.sess
            .variables
            .insert("session_id".into(), "test-session".into());

        app.on_exit(crate::call::app::ExitReason::RemoteHangup(None))
            .await
            .expect("remote hangup exit should succeed");

        assert!(
            !*provider.end_called.lock().unwrap(),
            "remote hangup must skip provider session end"
        );
    }

    #[tokio::test]
    async fn test_session_end_trace_recorded_on_remote_hangup() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
            EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            },
        )]));
        let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
        app.trace = Some(trace.clone());
        app.ivr_name = Some("test-ivr".to_string());
        let mut current_node = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });
        current_node.extra = Some(serde_json::json!({"nodetype": "transfer"}));
        app.current_node = Some(current_node);
        app.extra = Some(serde_json::json!({"nodetype": "previous-node"}));
        app.current_step_id = Some("step-7".to_string());
        app.current_step_start_time = Some("2026-01-01T00:00:00+00:00".to_string());
        app.sess
            .variables
            .insert("session_id".into(), "test-session".into());

        app.on_exit(crate::call::app::ExitReason::RemoteHangup(None))
            .await
            .expect("remote hangup exit should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let entries = trace.query_by_session("test-session").await;
        let session_end = entries
            .iter()
            .find(|e| e.trigger.r#type == "session_end")
            .expect("remote hangup must record a session_end trace entry");
        assert_eq!(
            session_end.step_start_time.as_deref(),
            Some("2026-01-01T00:00:00+00:00"),
            "session_end trace must carry the in-flight step's start time (duration_ms contract)"
        );
        assert_eq!(
            session_end.step_id.as_deref(),
            Some("step-7"),
            "session_end trace must record the last executed node"
        );
        assert_eq!(
            session_end.action_type, "Transfer",
            "session_end trace must carry the last node action type"
        );
        assert_eq!(
            session_end.extra,
            Some(serde_json::json!({"nodetype": "transfer"})),
            "session_end trace must preserve the last node metadata"
        );
        assert_eq!(
            session_end.end_reason,
            Some(crate::call::app::ivr::provider::SessionEndTag::UserHangup),
            "session_end trace must carry the end reason"
        );
        assert!(
            !*provider.end_called.lock().unwrap(),
            "remote hangup must skip provider session end"
        );
    }

    #[tokio::test]
    async fn test_step_trace_events_are_enqueued_in_record_order() {
        use crate::rwi::gateway::RwiGateway;

        let mut app = mock_app(vec![]);
        app.sess
            .variables
            .insert("session_id".into(), "test-session".into());
        let gateway = RwiGateway::new();
        let mut events = gateway.subscribe_events();
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

        let entry = |step_id: &str, trigger: crate::rwi::TriggerInfo| IvrTraceEntry {
            session_id: "test-session".into(),
            caller: "1001".into(),
            callee: "2000".into(),
            step_index: 1,
            trigger,
            provider_url: None,
            action_type: "Prompt".into(),
            action_json: None,
            duration_ms: 0,
            error: None,
            step_id: Some(step_id.into()),
            step_name: None,
            step_start_time: None,
            step_end_time: None,
            extra: None,
            end_reason: None,
            end_detail: None,
        };

        app.record_trace(entry(
            "step-normal",
            crate::rwi::TriggerInfo::new("audio_complete"),
        ));
        app.record_trace(entry(
            "step-end",
            crate::rwi::TriggerInfo::new("session_end"),
        ));

        let first = events.try_recv().expect("ordinary trace must be enqueued");
        let second = events
            .try_recv()
            .expect("session end trace must be enqueued");
        assert_eq!(first.event.payload["step_id"], "step-normal");
        assert_eq!(second.event.payload["step_id"], "step-end");
    }

    // ── Invariant: step_end_time implies step_start_time ──
    //
    // Consumers derive duration_ms as `event timestamp - step_start_time`.
    // An entry that carries step_end_time without step_start_time yields an
    // empty duration. Every finalized entry (ordinary step, pending finalize,
    // dtmf_menu_invalid, ivr_fallback, session_end) must carry both stamps.

    #[tokio::test]
    async fn test_all_step_traces_with_end_time_carry_start_time() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();

        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
        );
        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries,
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });

        let mut app: StepIvrApp = mock_app(vec![menu]);
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Rejected key → dtmf_menu_invalid trace; matching key → pending
        // finalize + terminal transfer step trace; the transfer ends the
        // session and on_exit appends the session_end entry.
        stack.dtmf("9");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        assert!(
            entries.iter().any(|e| e.trigger.r#type == "dtmf_menu_invalid"),
            "expected a dtmf_menu_invalid entry in the trace"
        );
        assert!(
            entries.iter().any(|e| e.trigger.r#type == "session_end"),
            "expected a session_end entry in the trace"
        );
        for e in &entries {
            assert!(
                !(e.step_end_time.is_some() && e.step_start_time.is_none()),
                "trace entry with trigger '{}' carries step_end_time ({:?}) but no step_start_time — duration_ms would be empty for consumers",
                e.trigger.r#type,
                e.step_end_time
            );
        }
    }

    #[tokio::test]
    async fn test_rwi_step_trace_events_carry_start_time_when_ended() {
        use crate::rwi::gateway::RwiGateway;

        let provider = EventCapturingProvider::new();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("start-time-ivr");
        let gateway = RwiGateway::new();
        let mut events = gateway.subscribe_events();
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Caller hangs up while the menu step is still waiting: on_exit
        // finalizes the pending step trace, then records session_end.
        stack.remote_hangup();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut ended = 0;
        while let Ok(ev) = events.try_recv() {
            if ev.event.payload["step_end_time"].is_string() {
                ended += 1;
                assert!(
                    ev.event.payload["step_start_time"].is_string(),
                    "ivr_step_trace event with trigger {:?} carries step_end_time but no step_start_time — duration_ms would be empty for consumers",
                    ev.event.payload["trigger"]
                );
            }
        }
        assert!(
            ended >= 2,
            "expected the finalized menu step and the session_end traces, got {ended}"
        );
    }

    #[tokio::test]
    async fn test_fallback_trace_carries_step_start_time() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let mut app: StepIvrApp = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider));
        app.trace = Some(trace.clone());
        app.sess
            .variables
            .insert("session_id".into(), "test-session".into());
        app.current_step_start_time = Some("2026-01-01T00:00:00+00:00".to_string());

        // No ivr_fallback configured → `not_configured` trace emitted via
        // record_fallback_trace.
        app.enter_ivr_fallback_node("step:test");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let entries = trace.query_by_session("test-session").await;
        let fallback = entries
            .iter()
            .find(|e| e.action_type == "ivr_fallback")
            .expect("fallback decision must be traced");
        assert_eq!(
            fallback.step_start_time.as_deref(),
            Some("2026-01-01T00:00:00+00:00"),
            "ivr_fallback trace must carry step_start_time — duration_ms would be empty for consumers"
        );
        assert!(
            fallback.step_end_time.is_some(),
            "ivr_fallback trace must carry step_end_time"
        );
    }

    // ── Hangup while a step waits: the step's trigger records WHY it ended ──

    /// Menu waiting for keys + caller hangs up: the menu step's own trace
    /// must carry trigger `user_hangup` (symmetric with dtmf /
    /// dtmf_menu_timeout), while the session_end entry still carries the
    /// session-level end_reason.
    #[tokio::test]
    async fn test_menu_hangup_overwrites_pending_trigger() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = EventCapturingProvider::new();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("hangup-trace-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Caller hangs up while the menu is waiting for keys.
        stack.remote_hangup();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entry = entries
            .iter()
            .find(|e| e.action_type == "DtmfMenu")
            .expect("expected a DtmfMenu trace entry");
        assert_eq!(
            menu_entry.trigger.r#type, "user_hangup",
            "menu step terminated by caller hangup must carry trigger 'user_hangup', got: {:?}",
            menu_entry.trigger
        );
        // Session-level end_reason remains on the final session_end entry only.
        let session_end = entries
            .iter()
            .find(|e| e.trigger.r#type == "session_end")
            .expect("expected a session_end entry");
        assert_eq!(
            session_end.end_reason,
            Some(crate::call::app::ivr::provider::SessionEndTag::UserHangup)
        );
    }

    /// Same contract for system cancellation: the outstanding wait step is
    /// reported with trigger `cancelled`.
    #[tokio::test]
    async fn test_menu_cancel_overwrites_pending_trigger() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = EventCapturingProvider::new();
        let mut app: StepIvrApp =
            StepIvrApp::with_provider(Box::new(provider)).with_name("cancel-trace-ivr");
        app.trace = Some(trace.clone());
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, CallCommand::Play { source: crate::call::domain::MediaSource::File { path }, .. } if path == "menu.wav")
            })
            .await;
        stack.audio_complete("ivr_menu_greeting");
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));

        stack.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let sessions = trace.sessions().await;
        let sess = &sessions[0];
        let entries = trace.query_by_session(&sess.session_id).await;
        let menu_entry = entries
            .iter()
            .find(|e| e.action_type == "DtmfMenu")
            .expect("expected a DtmfMenu trace entry");
        assert_eq!(
            menu_entry.trigger.r#type, "cancelled",
            "menu step terminated by cancellation must carry trigger 'cancelled', got: {:?}",
            menu_entry.trigger
        );
    }

    #[tokio::test]
    async fn test_remote_hangup_emits_step_trace_event() {
        use crate::rwi::gateway::EventCacheEntry;

        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
            EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            },
        )]));
        let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
        app.ivr_name = Some("test-ivr".to_string());
        app.current_node = Some(ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        }));
        app.current_step_id = Some("step-9".to_string());
        app.sess
            .variables
            .insert("session_id".into(), "test-session".into());
        app.sess.variables.insert("caller".into(), "1001".into());
        app.sess.variables.insert("callee".into(), "2000".into());

        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        app.on_exit(crate::call::app::ExitReason::Cancelled)
            .await
            .expect("cancel exit should succeed");

        let mut saw_trace = false;
        let mut saw_end_reason = false;
        for _ in 0..20 {
            while let Ok(entry) = rx.try_recv() {
                if entry.event.event_type == "ivr_step_trace" {
                    saw_trace = true;
                    if entry.event.payload["end_reason"]
                        .as_str()
                        .map(|r| r == "hangup")
                        .unwrap_or(false)
                    {
                        saw_end_reason = true;
                    }
                }
            }
            if saw_trace && saw_end_reason {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(saw_trace, "cancel must emit an ivr_step_trace event");
        assert!(
            saw_end_reason,
            "cancel-emitted step trace must carry end_reason"
        );
        assert!(
            !*provider.end_called.lock().unwrap(),
            "cancel must skip provider session end"
        );
    }

    #[tokio::test]
    async fn test_cancel_during_provider_next_skips_session_end() {
        let entered_next = Arc::new(Notify::new());
        let release_next = Arc::new(Notify::new());
        let end_called = Arc::new(AtomicBool::new(false));

        let provider = BlockingProvider {
            entered_next: entered_next.clone(),
            release_next: release_next.clone(),
            end_called: end_called.clone(),
        };
        let ctx = make_test_context();
        let mut stack = MockCallStack::run_with_context(
            Box::new(StepIvrApp::with_provider(Box::new(provider)).with_name("blocking-step-ivr")),
            ctx.clone(),
        );

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        tokio::time::timeout(Duration::from_secs(1), entered_next.notified())
            .await
            .expect("provider next_action should start");

        stack.cancel();
        stack.join().await.expect("cancel should stop app");

        assert!(!end_called.load(Ordering::SeqCst), "cancel must skip /end");
        assert_eq!(ctx.get_var(IVR_STATUS_KEY).as_deref(), Some("cancelled"));
        assert_eq!(
            ctx.get_var(IVR_END_REASON_KEY).as_deref(),
            Some("cancelled")
        );

        release_next.notify_waiters();
    }

    #[tokio::test]
    async fn test_startup_failure_sets_runtime_status() {
        let ctx = make_test_context();
        let mut stack = MockCallStack::run_with_context(
            Box::new(StepIvrApp::with_provider(Box::new(FailingProvider)).with_name("failing-ivr")),
            ctx.clone(),
        );

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        stack
            .assert_cmd(500, "play fallback", |c| {
                matches!(c, CallCommand::Play { .. })
            })
            .await;
        assert_eq!(
            ctx.get_var(IVR_LAST_ERROR_KEY).as_deref(),
            Some("provider bootstrap failed")
        );
        let status_before_exit = ctx.get_var(IVR_STATUS_KEY);
        assert!(
            matches!(
                status_before_exit.as_deref(),
                Some("startup_error") | Some("active")
            ),
            "unexpected startup status before fallback exit: {status_before_exit:?}"
        );
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "hangup fallback", |c| {
                matches!(c, CallCommand::Hangup(_))
            })
            .await;
        stack
            .join()
            .await
            .expect("fallback path should exit cleanly");

        assert_eq!(ctx.get_var(IVR_STATUS_KEY).as_deref(), Some("hangup"));
        assert_eq!(ctx.get_var(IVR_NAME_KEY).as_deref(), Some("failing-ivr"));
        assert_eq!(ctx.get_var(IVR_END_REASON_KEY).as_deref(), Some("hangup"));
    }

    #[tokio::test]
    async fn test_http_provider_remote_hangup_skips_end_webhook() {
        use axum::{Json, Router, extract::State, routing::post};

        #[derive(Default)]
        struct ProviderState {
            start_calls: tokio::sync::Mutex<Vec<serde_json::Value>>,
            step_calls: tokio::sync::Mutex<Vec<serde_json::Value>>,
            end_calls: tokio::sync::Mutex<Vec<serde_json::Value>>,
            step_entered: Notify,
            release_step: Notify,
        }

        async fn start_handler(
            State(state): State<Arc<ProviderState>>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            state.start_calls.lock().await.push(body);
            Json(serde_json::json!({ "ok": true }))
        }

        async fn step_handler(
            State(state): State<Arc<ProviderState>>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            state.step_calls.lock().await.push(body);
            state.step_entered.notify_waiters();
            state.release_step.notified().await;
            Json(
                serde_json::to_value(ActionNode::new(EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                }))
                .unwrap(),
            )
        }

        async fn end_handler(
            State(state): State<Arc<ProviderState>>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            state.end_calls.lock().await.push(body);
            Json(serde_json::json!({ "ok": true }))
        }

        let state = Arc::new(ProviderState::default());
        let app = Router::new()
            .route("/ivr/step/start", post(start_handler))
            .route("/ivr/step", post(step_handler))
            .route("/ivr/step/end", post(end_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let provider = StepProvider::new(format!("http://{addr}/ivr/step"), reqwest::Client::new())
            .with_retry(RetryConfig {
                max_retries: 1,
                timeout_ms: 15_000,
                retry_delay_ms: 100,
                fallback_action: None,
            });
        let ctx = make_test_context();
        let mut stack = MockCallStack::run_with_context(
            Box::new(StepIvrApp::with_provider(Box::new(provider)).with_name("http-ivr")),
            ctx,
        );

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        tokio::time::timeout(Duration::from_secs(2), state.step_entered.notified())
            .await
            .expect("step provider should receive first /step request");

        stack.remote_hangup();
        stack.join().await.expect("remote hangup should stop app");

        assert_eq!(state.start_calls.lock().await.len(), 1);
        assert_eq!(state.step_calls.lock().await.len(), 1);
        assert_eq!(state.end_calls.lock().await.len(), 0);

        state.release_step.notify_waiters();
    }

    // ── Bug 6: Provider-driven menu timeout forwards to provider ──────────

    #[tokio::test]
    async fn test_provider_driven_menu_timeout_forwards_to_provider() {
        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: None,
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 3000,
            max_retries: 1,
            entries: HashMap::new(),
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: None,
        });
        let followup = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });

        let mut stack =
            MockCallStack::run(Box::new(mock_app(vec![menu, followup])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        // Menu has no greeting → NoAudio path → timeout set, app waiting
        tokio::time::sleep(Duration::from_millis(50)).await;
        stack.timeout("ivr_dtmf_timeout");

        // With fix: provider called with DtmfMenuTimeout → returns Transfer
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    // ── Bug 7: InputPhone forwards collected digits to provider ───────────

    #[tokio::test]
    async fn input_phone_emits_completion_before_followup() {
        use crate::rwi::gateway::RwiGateway;

        let mut input_phone = ActionNode::new(EntryAction::InputPhone {
            prompt: Some("enter_phone.wav".into()),
            prompt_text: None,
            prompt_voice: None,
            min_digits: 11,
            max_digits: 11,
            timeout_ms: 10_000,
            inter_digit_timeout_ms: 3_000,
            terminator: "#".into(),
        });
        input_phone.step_id = Some("input-phone-step".into());
        let followup = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });
        let gateway = RwiGateway::new();
        let mut events = gateway.subscribe_events();
        let mut app = mock_app(vec![input_phone, followup]);
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gateway)));

        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        options: Some(options),
                        ..
                    } if path == "enter_phone.wav" && options.interrupt_on_dtmf
                )
            })
            .await;

        // Inject DTMF digits while collect_dtmf is waiting
        stack.dtmf("12345678901");

        // With fix: provider called with PhoneCollected → returns Transfer
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        // Single finalized trace: original phone_collected trigger, end time
        // filled, no wait_finalized duplicate.
        let finalized = events
            .try_recv()
            .expect("input phone trace must be enqueued");
        assert_eq!(finalized.event.payload["step_id"], "input-phone-step");
        assert_eq!(
            finalized.event.payload["trigger"]["type"],
            "phone_collected"
        );
        assert_eq!(
            finalized.event.payload["trigger"]["detail"]["number"],
            "12345678901"
        );
        assert!(finalized.event.payload["step_end_time"].is_string());
        // Subsequent events are legitimate (transfer step trace, session_end) —
        // none may be a wait_finalized duplicate.
        while let Ok(ev) = events.try_recv() {
            assert_ne!(
                ev.event.payload["trigger"]["type"], "wait_finalized",
                "input phone completion must not emit a wait_finalized duplicate"
            );
        }
    }

    #[tokio::test]
    async fn typed_result_discards_stale_buffered_dtmf() {
        let provider = EventCapturingProvider::new();
        provider.first_call.store(true, Ordering::SeqCst);
        let events = provider.captured_events.clone();
        let mut app = StepIvrApp::with_provider(Box::new(provider));
        app.pending_dtmf.push_back("1".to_string());

        app.request_next(Some(ProviderEvent::PhoneCollected {
            number: "10000000000".to_string(),
        }))
        .await
        .unwrap();

        assert!(app.pending_dtmf.is_empty());
        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [Some(ProviderEvent::PhoneCollected { number })] if number == "10000000000"
        ));
    }

    // ── Bug 8: Torecord forwards recording complete to provider ──────────

    #[tokio::test]
    async fn test_torecord_recording_complete_forwards_to_provider() {
        let torecord = ActionNode::new(EntryAction::Torecord {
            prompt: Some("record.wav".into()),
            beep: true,
            max_duration_secs: Some(5),
        });
        let followup = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });

        let mut stack =
            MockCallStack::run(Box::new(mock_app(vec![torecord, followup])), "1001", "2000");
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "record.wav"
                )
            })
            .await;
        stack
            .assert_cmd(200, "start_record", |c| {
                matches!(c, CallCommand::StartRecording { .. })
            })
            .await;

        // Inject recording complete
        tokio::time::sleep(Duration::from_millis(50)).await;
        stack.record_complete("recordings/test.wav", Duration::from_secs(3), 12345);

        // With fix: provider called with RecordingComplete → returns Transfer
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    // ── Mid-call record_start / record_stop ──────────────────────────────

    #[tokio::test]
    async fn test_record_start_stop_continues_without_waiting() {
        let start = ActionNode::new(EntryAction::RecordStart {
            segment_type: Some("ivr".into()),
            id: Some("seg1".into()),
            beep: false,
            max_duration_secs: None,
        });
        let stop = ActionNode::new(EntryAction::RecordStop {
            reason: Some("before_transfer".into()),
        });
        let followup = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_app: None,
            return_target: None,
        });

        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![start, stop, followup])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "start_record", |c| {
                matches!(
                    c,
                    CallCommand::StartRecording { config }
                        if config.segment_type.as_deref() == Some("ivr")
                            && config.segment_id.as_deref() == Some("seg1")
                            && config.notify_app == Some(false)
                            && config.path.is_empty()
                )
            })
            .await;
        stack
            .assert_cmd(200, "stop_record", |c| {
                matches!(c, CallCommand::StopRecording)
            })
            .await;
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;
    }

    #[tokio::test]
    async fn test_segment_recording_complete_does_not_hijack_flow() {
        // record_start then prompt; a late RecordingComplete must not replace
        // the prompt with a provider RecordingComplete branch.
        let start = ActionNode::new(EntryAction::RecordStart {
            segment_type: Some("ivr".into()),
            id: Some("s1".into()),
            beep: false,
            max_duration_secs: None,
        });
        let prompt = ActionNode::new(EntryAction::Prompt {
            file: Some("hello.wav".into()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,
        });
        let hangup = ActionNode::new(EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        });

        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![start, prompt, hangup])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "start_record", |c| {
                matches!(c, CallCommand::StartRecording { .. })
            })
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path },
                        ..
                    } if path == "hello.wav"
                )
            })
            .await;

        // Spurious RecordingComplete while waiting for audio — must be ignored
        // (pending action is Prompt, not Torecord).
        stack.record_complete("/tmp/seg.wav", Duration::from_secs(1), 100);
        tokio::time::sleep(Duration::from_millis(30)).await;

        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "hangup", |c| matches!(c, CallCommand::Hangup(_)))
            .await;
    }

    // ── StepProvider ↔ HTTP contract test ────────────────────────────────
    //
    // Exercises the StepProvider HTTP client end-to-end against an in-process
    // mock provider (no external Python/example dependency): request bodies are
    // POSTed to /ivr/step, responses parsed into ActionNodes, and
    // session_start/end notifications are delivered.

    /// Minimal in-process step provider. Accepts keep-alive HTTP/1.1
    /// connections and answers `/ivr/step` based on the event type — mirroring
    /// what a real step provider (e.g. `examples/unified_ivr_provider.py`)
    /// returns.
    struct MockStepProviderServer {
        url: String,
        requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
        _listener: std::net::TcpListener,
    }

    fn start_mock_step_provider() -> MockStepProviderServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock step provider");
        let url = format!(
            "http://{}/ivr/step",
            listener.local_addr().expect("mock provider addr")
        );
        let accept_listener = listener.try_clone().expect("clone mock provider listener");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded_requests = requests.clone();
        std::thread::spawn(move || {
            for stream in accept_listener.incoming() {
                let Ok(stream) = stream else { continue };
                let connection_requests = recorded_requests.clone();
                std::thread::spawn(move || {
                    let _ = serve_connection(stream, connection_requests);
                });
            }
        });
        MockStepProviderServer {
            url,
            requests,
            _listener: listener,
        }
    }

    fn serve_connection(
        mut stream: std::net::TcpStream,
        requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    ) -> std::io::Result<()> {
        use std::io::{BufRead, BufReader, Read, Write};

        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        loop {
            // Request line: `POST /ivr/step HTTP/1.1`
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            // Headers
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header)? == 0 {
                    return Ok(());
                }
                if header.trim().is_empty() {
                    break;
                }
                // HTTP header names are case-insensitive; reqwest sends
                // `content-length:` lowercase. A case-sensitive match here
                // mis-reads every request body and desyncs the keep-alive
                // connection (bodies leak into the next request line).
                let h = header.trim_start();
                if h.len() >= 15 && h[..15].eq_ignore_ascii_case("content-length:") {
                    content_length = h[15..].trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body)?;
            if let Ok(value) = serde_json::from_slice(&body) {
                requests.lock().unwrap().push((path.clone(), value));
            }
            let payload = mock_step_response(&path, &body);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                payload.len()
            );
            stream.write_all(head.as_bytes())?;
            stream.write_all(&payload)?;
            stream.flush()?;
        }
    }

    fn mock_step_response(path: &str, body: &[u8]) -> Vec<u8> {
        if path != "/ivr/step" {
            // /ivr/step/start and /ivr/step/end are fire-and-forget.
            return br#"{"status":"ok"}"#.to_vec();
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
            let event_type = value["event"]["type"].as_str().unwrap_or("");
            if event_type == "session_start" {
                return br#"{"type":"prompt","tts_text":"IVR step announcement.","interruptible":false,"ignore_prompt_dtmf":true}"#
                    .to_vec();
            }
            if event_type == "dtmf" && value["event"]["digit"].as_str() == Some("2") {
                return br#"{"type":"queue","target":"sales"}"#.to_vec();
            }
        }
        br#"{"type":"hangup"}"#.to_vec()
    }

    #[tokio::test]
    async fn test_step_provider_http_contract() {
        use crate::call::app::ivr::provider::StepProvider;

        let provider = start_mock_step_provider();
        let step_provider = StepProvider::new(&provider.url, reqwest::Client::new());

        let session = SessionContext {
            session_id: "test-session".to_string(),
            app_execution_id: 2,
            caller: "1001".to_string(),
            callee: "2000".to_string(),
            direction: "inbound".to_string(),
            tenant_id: None,
            ivr_id: None,
            variables: HashMap::from([("order_id".to_string(), "order-001".to_string())]),
            sip_headers: Some(HashMap::from([(
                "X-Business-Type".to_string(),
                "34".to_string(),
            )])),
            route_name: None,
            custom_data: None,
            transferred_from: None,
        };
        step_provider.on_session_start(&session).await.unwrap();

        let ctx = ProviderContext {
            session_id: session.session_id.clone(),
            app_execution_id: session.app_execution_id,
            caller: session.caller.clone(),
            callee: session.callee.clone(),
            direction: session.direction.clone(),
            tenant_id: None,
            ivr_id: None,
            variables: HashMap::new(),
            sip_headers: None,
            event: Some(ProviderEvent::SessionStart),
            route_name: None,
            custom_data: None,
            step_start_time: None,
            step_end_time: None,
            step_duration_ms: None,
            step_index: None,
            transferred_from: None,
        };
        let prompt = step_provider.next_action(ctx).await.unwrap();
        assert!(
            matches!(prompt.action, EntryAction::Prompt { ref tts_text, interruptible: false, .. }
                if tts_text.as_deref().is_some_and(|text| text.contains("IVR step")))
        );
        assert!(prompt.ignore_prompt_dtmf);

        let ctx = ProviderContext {
            event: Some(ProviderEvent::Dtmf {
                digit: "2".to_string(),
            }),
            ..ProviderContext {
                session_id: session.session_id.clone(),
                app_execution_id: session.app_execution_id,
                caller: session.caller.clone(),
                callee: session.callee.clone(),
                direction: session.direction.clone(),
                tenant_id: None,
                ivr_id: None,
                variables: HashMap::new(),
                sip_headers: None,
                event: None,
                route_name: None,
                custom_data: None,
                step_start_time: None,
                step_end_time: None,
                step_duration_ms: None,
                step_index: None,
                transferred_from: None,
            }
        };
        let action = step_provider.next_action(ctx).await.unwrap();
        // The mock maps DTMF "2" → queue "sales", matching the documented
        // step-provider menu flow (see examples/unified_ivr_provider.py).
        assert!(
            matches!(action.action, EntryAction::Queue { ref target, .. } if target == "sales")
        );

        step_provider
            .on_session_end_context(
                &SessionEndReason {
                    reason: SessionEndTag::Normal,
                    detail: None,
                },
                &session,
            )
            .await
            .unwrap();

        let requests = provider.requests.lock().unwrap();
        let start = requests
            .iter()
            .find(|(path, _)| path == "/ivr/step/start")
            .map(|(_, body)| body)
            .expect("start request");
        assert_eq!(start["session_id"], "test-session");
        assert_eq!(start["app_execution_id"], 2);
        assert_eq!(start["variables"]["order_id"], "order-001");
        assert_eq!(start["sip_headers"]["X-Business-Type"], "34");

        let step_requests = requests
            .iter()
            .filter(|(path, _)| path == "/ivr/step")
            .map(|(_, body)| body)
            .collect::<Vec<_>>();
        assert_eq!(step_requests.len(), 2);
        assert!(
            step_requests.iter().all(|body| {
                body["session_id"] == "test-session" && body["app_execution_id"] == 2
            })
        );

        let end = requests
            .iter()
            .find(|(path, _)| path == "/ivr/step/end")
            .map(|(_, body)| body)
            .expect("end request");
        assert_eq!(end["session_id"], "test-session");
        assert_eq!(end["app_execution_id"], 2);
    }

    // ── DTMF delivery in step-provider mode ──────────────────────────────
    //
    // DTMF delivery must preserve the current prompt's interruption contract.

    fn event_label(ev: &Option<ProviderEvent>) -> String {
        match ev {
            Some(ProviderEvent::SessionStart) => "session_start".to_string(),
            Some(ProviderEvent::AudioComplete { .. }) => "audio_complete".to_string(),
            Some(ProviderEvent::Dtmf { digit }) => format!("dtmf:{digit}"),
            Some(ProviderEvent::DtmfMenuTimeout) => "dtmf_menu_timeout".to_string(),
            Some(ProviderEvent::Error { .. }) => "error".to_string(),
            _ => "other".to_string(),
        }
    }

    /// Provider that records every event it is called with, and answers:
    /// session_start → interruptible welcome; audio_complete → non-interruptible
    /// announcement; dtmf "2" → transfer to 2001; anything else → hangup.
    struct ScriptedProvider {
        log: Arc<std::sync::Mutex<Vec<String>>>,
        ignore_prompt_dtmf: bool,
    }

    #[async_trait]
    impl ActionProvider for ScriptedProvider {
        async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            self.log.lock().unwrap().push(event_label(&ctx.event));
            match &ctx.event {
                Some(ProviderEvent::SessionStart) => Ok(ActionNode::new(EntryAction::Prompt {
                    file: Some("welcome.wav".into()),
                    tts_text: None,
                    tts_voice: None,
                    record_name_list: None,
                    interruptible: true,
                    tts_api_url: None,
                })),
                Some(ProviderEvent::AudioComplete { .. }) => {
                    let mut node = ActionNode::new(EntryAction::Prompt {
                        file: Some("announce.wav".into()),
                        tts_text: None,
                        tts_voice: None,
                        record_name_list: None,
                        interruptible: false,
                        tts_api_url: None,
                    });
                    node.ignore_prompt_dtmf = self.ignore_prompt_dtmf;
                    Ok(node)
                }
                Some(ProviderEvent::Dtmf { digit }) if digit == "2" => {
                    Ok(ActionNode::new(EntryAction::Transfer {
                        target: "2001".into(),
                        params: HashMap::new(),
                        return_app: None,
                        return_target: None,
                    }))
                }
                _ => Ok(ActionNode::new(EntryAction::Hangup {
                    prompt: None,
                    prompt_text: None,
                    prompt_voice: None,
                })),
            }
        }

        async fn on_session_start(&self, _ctx: &SessionContext) -> anyhow::Result<()> {
            Ok(())
        }

        async fn on_session_end(
            &self,
            _reason: &SessionEndReason,
            _session_id: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn play_file_cmd(cmd: &CallCommand, path: &str) -> bool {
        matches!(
            cmd,
            CallCommand::Play {
                source: crate::call::domain::MediaSource::File { path: p }, ..
            } if p == path
        )
    }

    #[tokio::test]
    async fn test_step_provider_buffers_dtmf_by_default_during_non_interruptible_prompt() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = StepIvrApp::with_provider(Box::new(ScriptedProvider {
            log: log.clone(),
            ignore_prompt_dtmf: false,
        }));
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(500, "play-welcome", |c| play_file_cmd(c, "welcome.wav"))
            .await;
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "play-announce", |c| play_file_cmd(c, "announce.wav"))
            .await;

        stack.dtmf("2");
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let log = log.lock().unwrap();
        assert_eq!(
            log.as_slice(),
            ["session_start", "audio_complete", "dtmf:2"]
        );
    }

    #[tokio::test]
    async fn test_step_provider_ignores_dtmf_during_non_interruptible_prompt() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = StepIvrApp::with_provider(Box::new(ScriptedProvider {
            log: log.clone(),
            ignore_prompt_dtmf: true,
        }));
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(500, "play-welcome", |c| play_file_cmd(c, "welcome.wav"))
            .await;

        // welcome (interruptible) completes → announcement (non-interruptible).
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "play-announce", |c| play_file_cmd(c, "announce.wav"))
            .await;

        // Press 2 during the non-interruptible announcement. The announcement
        // must finish naturally without forwarding the digit to the provider.
        stack.dtmf("2");

        // Announcement completion remains an audio_complete event, so the
        // provider returns its next announcement instead of the DTMF transfer.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "play-next-announce", |c| {
                play_file_cmd(c, "announce.wav")
            })
            .await;

        let log = log.lock().unwrap();
        assert_eq!(
            log.as_slice(),
            ["session_start", "audio_complete", "audio_complete"]
        );
    }

    #[tokio::test]
    async fn test_step_provider_barges_in_interruptible_prompt() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = StepIvrApp::with_provider(Box::new(ScriptedProvider {
            log: log.clone(),
            ignore_prompt_dtmf: false,
        }));
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(500, "play-welcome", |c| play_file_cmd(c, "welcome.wav"))
            .await;

        // Press 2 during the interruptible welcome → immediate barge-in.
        stack.dtmf("2");
        stack
            .assert_cmd(500, "stop", |c| {
                matches!(c, CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let log = log.lock().unwrap();
        assert!(
            log.iter().any(|e| e == "dtmf:2"),
            "provider never received dtmf:2 — got {log:?}"
        );
    }

    // ── Runaway-loop guard ────────────────────────────────────────────────

    /// Provider that answers every event with the same interruptible prompt,
    /// simulating a broken/looping provider that never terminates.
    struct LoopingProvider;

    #[async_trait]
    impl ActionProvider for LoopingProvider {
        async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            Ok(ActionNode::new(EntryAction::Prompt {
                file: Some("menu.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: true,
                tts_api_url: None,
            }))
        }

        async fn on_session_start(&self, _ctx: &SessionContext) -> anyhow::Result<()> {
            Ok(())
        }

        async fn on_session_end(
            &self,
            _reason: &SessionEndReason,
            _session_id: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_step_provider_runaway_loop_guard_hangs_up() {
        let app = StepIvrApp::with_provider(Box::new(LoopingProvider)).with_max_repeat_prompts(2);
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2000");

        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(500, "play-menu", |c| play_file_cmd(c, "menu.wav"))
            .await;

        // Cycle 1..2: no input → still re-offered by the provider.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "play-menu-2", |c| play_file_cmd(c, "menu.wav"))
            .await;
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "play-menu-3", |c| play_file_cmd(c, "menu.wav"))
            .await;

        // Cycle 3: probe with DtmfMenuTimeout; a looping provider ignores it.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "play-menu-4", |c| play_file_cmd(c, "menu.wav"))
            .await;

        // Cycle 4: probe already ignored → hang up.
        stack.audio_complete("ivr_prompt");
        stack
            .assert_cmd(500, "hangup", |c| matches!(c, CallCommand::Hangup { .. }))
            .await;
    }

    struct FailRecoveryProvider {
        fail_called: Arc<std::sync::Mutex<u32>>,
    }

    #[async_trait]
    impl ActionProvider for FailRecoveryProvider {
        async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            Ok(ActionNode::new(EntryAction::Prompt {
                file: Some("welcome.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            }))
        }

        async fn fail_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            *self.fail_called.lock().unwrap() += 1;
            assert!(
                matches!(ctx.event, Some(ProviderEvent::Fail { .. })),
                "fail_action must receive Fail event"
            );
            Ok(ActionNode::new(EntryAction::Hangup {
                prompt: Some("sounds/recovered.wav".into()),
                prompt_text: None,
                prompt_voice: None,
            }))
        }
    }

    #[tokio::test]
    async fn test_execute_failure_calls_fail_then_continues() {
        let fail_called = Arc::new(std::sync::Mutex::new(0u32));
        let mut app = StepIvrApp::with_provider(Box::new(FailRecoveryProvider {
            fail_called: fail_called.clone(),
        }));
        app.sess.variables.insert("session_id".into(), "s1".into());
        app.sess.variables.insert("caller".into(), "1001".into());
        app.sess.variables.insert("callee".into(), "4000".into());

        let node = app
            .recover_from_execute_failure(anyhow::anyhow!("transfer start failed"))
            .await
            .expect("fail recovery");
        assert_eq!(*fail_called.lock().unwrap(), 1);
        match node.action {
            EntryAction::Hangup {
                prompt: Some(p), ..
            } => assert_eq!(p, "sounds/recovered.wav"),
            other => panic!("expected recovered hangup, got {other:?}"),
        }
    }

    struct FailThenEscalateProvider;

    #[async_trait]
    impl ActionProvider for FailThenEscalateProvider {
        async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            Err(anyhow::anyhow!("step unreachable"))
        }

        async fn fail_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            Err(anyhow::anyhow!("fail unreachable"))
        }
    }

    #[tokio::test]
    async fn test_step_fail_with_ivr_fallback_jumps_to_target() {
        let fb = Arc::new(crate::config::IvrFallbackConfig {
            default: Some("default_ivr".into()),
            rules: vec![crate::config::IvrFallbackRule {
                name: Some("vip".into()),
                priority: 10,
                match_conditions: crate::proxy::routing::MatchConditions {
                    from_user: Some("1001".into()),
                    ..Default::default()
                },
                target: "builtin_vip".into(),
            }],
        });
        let mut app = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider))
            .with_ivr_fallback(Some(fb));
        app.sess.variables.insert("session_id".into(), "s1".into());
        app.sess.variables.insert("caller".into(), "1001".into());
        app.sess.variables.insert("callee".into(), "4000".into());

        let node = app
            .request_next(Some(ProviderEvent::SessionStart))
            .await
            .expect("fallback node");
        match node.action {
            EntryAction::Transfer { target, params, .. } => {
                assert_eq!(target, "ivr:builtin_vip");
                assert_eq!(
                    params
                        .get(crate::call::app::ivr::fallback::IVR_FALLBACK_USED_KEY)
                        .map(String::as_str),
                    Some("1")
                );
            }
            other => panic!("expected direct IVR fallback, got {other:?}"),
        }
        assert!(app.fallback_already_used());
    }

    #[tokio::test]
    async fn test_ivr_fallback_uses_default_when_no_rule() {
        let fb = Arc::new(crate::config::IvrFallbackConfig {
            default: Some("default_ivr".into()),
            rules: vec![crate::config::IvrFallbackRule {
                name: Some("vip".into()),
                priority: 10,
                match_conditions: crate::proxy::routing::MatchConditions {
                    from_user: Some("^9".into()),
                    ..Default::default()
                },
                target: "vip_ivr".into(),
            }],
        });
        let mut app = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider))
            .with_ivr_fallback(Some(fb));
        app.sess.variables.insert("caller".into(), "1001".into());
        app.sess.variables.insert("callee".into(), "4000".into());

        let node = app.enter_ivr_fallback_node("step:test");
        match node.action {
            EntryAction::Transfer { target, .. } => assert_eq!(target, "ivr:default_ivr"),
            other => panic!("expected default direct IVR transfer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_ivr_fallback_rejected_when_already_used() {
        let fb = Arc::new(crate::config::IvrFallbackConfig {
            default: Some("default_ivr".into()),
            rules: vec![],
        });
        let mut app = StepIvrApp::with_provider(Box::new(FailThenEscalateProvider))
            .with_ivr_fallback(Some(fb));
        app.mark_fallback_used();
        let node = app.enter_ivr_fallback_node("again");
        match node.action {
            EntryAction::Prompt { file: Some(f), .. } => assert_eq!(f, "sounds/error.wav"),
            other => panic!("expected error hangup chain, got {other:?}"),
        }
    }
}
