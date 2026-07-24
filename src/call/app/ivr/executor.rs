use super::common::{self, ActionResult, SessionData, TerminalAction, WaitEvent};
use super::config::{ActionNode, EntryAction};
use super::provider::{ActionProvider, EndReason, ProviderContext, ProviderEvent, SessionContext};
use super::trace::{IvrTraceCollector, IvrTraceEntry, IvrTraceSession};
use crate::call::app::{
    AppAction, AppEvent, ApplicationContext, CallApp, CallAppType, CallController, RecordingInfo,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::warn;

const IVR_STATUS_KEY: &str = "ivr_status";
const IVR_NAME_KEY: &str = "ivr_name";
const IVR_END_REASON_KEY: &str = "ivr_end_reason";
const IVR_LAST_ERROR_KEY: &str = "ivr_last_error";

pub struct StepIvrApp {
    provider: Box<dyn ActionProvider>,
    current_node: Option<ActionNode>,
    sess: SessionData,
    pending_menu: Option<PendingMenu>,
    current_track_id: Option<String>,
    interrupt_on_dtmf: bool,
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
    /// Route-level configured headers.
    route_headers: Option<HashMap<String, String>>,
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
    /// Last transfer target string (for EndReason classification).
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
    runtime_vars: Option<Arc<RwLock<HashMap<String, String>>>>,
    /// Session extensions clone stashed in on_enter for use in on_exit.
    /// Only populated when the IVR was started via `ivr.exec`.
    session_extensions: Option<crate::proxy::proxy_call::session_hooks::SessionExtensions>,
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
    pub fn new(url: impl Into<String>) -> Self {
        let provider = Box::new(super::provider::StepProvider::new(url));
        Self {
            provider,
            current_node: None,
            sess: SessionData::default(),
            pending_menu: None,
            current_track_id: None,
            interrupt_on_dtmf: false,
            awaiting_dtmf: false,
            tts_service: None,
            trace: None,
            step_index: 0,
            ivr_name: None,
            rwi_gateway: None,
            route_name: None,
            route_headers: None,
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
        }
    }

    pub fn with_provider(provider: Box<dyn ActionProvider>) -> Self {
        Self {
            provider,
            current_node: None,
            sess: SessionData::default(),
            pending_menu: None,
            current_track_id: None,
            interrupt_on_dtmf: false,
            awaiting_dtmf: false,
            tts_service: None,
            trace: None,
            step_index: 0,
            ivr_name: None,
            rwi_gateway: None,
            route_name: None,
            route_headers: None,
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

    /// Set the route-level configured headers.
    pub fn with_route_headers(mut self, headers: Option<HashMap<String, String>>) -> Self {
        self.route_headers = headers;
        self
    }

    /// Mark this session as re-entered from agent or queue.
    pub fn with_transferred_from(mut self, from: Option<String>) -> Self {
        self.transferred_from = from;
        self
    }

    fn pending_take(&mut self) -> Option<IvrTraceEntry> {
        self.pending_trace.take()
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
            let gw = gw.clone();
            crate::utils::spawn(async move {
                let guard = gw.read();
                guard.fan_out(&call_id, &ev);
            });
        }
    }

    fn increment_total_steps(&self) {
        if let Some(t) = self.effective_trace() {
            let sid = self
                .sess
                .variables
                .get("session_id")
                .cloned()
                .unwrap_or_default();
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
        let session_id = self
            .sess
            .variables
            .get("session_id")
            .cloned()
            .unwrap_or_default();
        if let Some(t) = self.effective_trace() {
            let sid = session_id;
            let st = status.to_string();
            crate::utils::spawn(async move {
                t.update_session_end(&sid, chrono::Utc::now(), &st).await;
            });
        }
    }

    async fn set_runtime_status(&self, ctx: &ApplicationContext, status: &str) {
        ctx.set_var(IVR_STATUS_KEY, status).await;
        if let Some(name) = &self.ivr_name {
            ctx.set_var(IVR_NAME_KEY, name).await;
        }
    }

    async fn set_runtime_error(&self, ctx: &ApplicationContext, error: &str) {
        ctx.set_var(IVR_LAST_ERROR_KEY, error).await;
    }

    async fn set_runtime_error_shared(&self, error: &str) {
        if let Some(vars) = &self.runtime_vars {
            let mut vars = vars.write().await;
            vars.insert(IVR_LAST_ERROR_KEY.to_string(), error.to_string());
            if let Some(name) = &self.ivr_name {
                vars.insert(IVR_NAME_KEY.to_string(), name.clone());
            }
        }
    }

    async fn set_runtime_status_shared(&self, status: &str) {
        if let Some(vars) = &self.runtime_vars {
            let mut vars = vars.write().await;
            vars.insert(IVR_STATUS_KEY.to_string(), status.to_string());
            if let Some(name) = &self.ivr_name {
                vars.insert(IVR_NAME_KEY.to_string(), name.clone());
            }
        }
    }

    async fn set_runtime_end_reason_shared(&self, reason: &str) {
        if let Some(vars) = &self.runtime_vars {
            let mut vars = vars.write().await;
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
            EntryAction::JumpIvr { .. } => "JumpIvr",
            EntryAction::RouteToAgent { .. } => "RouteToAgent",
            EntryAction::Bridge { .. } => "Bridge",
        }
    }

    async fn __exec_node(
        &mut self,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let node = self.current_node.as_ref().unwrap().clone();

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

        let session_id = self
            .sess
            .variables
            .get("session_id")
            .cloned()
            .unwrap_or_default();
        let caller = self
            .sess
            .variables
            .get("caller")
            .cloned()
            .unwrap_or_default();
        let callee = self
            .sess
            .variables
            .get("callee")
            .cloned()
            .unwrap_or_default();

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
                    ActionResult::ChainedTo(next) => {
                        self.current_trigger = Some(crate::rwi::TriggerInfo::new("chained"));
                        self.current_node = Some(next);
                        return Box::pin(self.__exec_node(ctrl, ctx)).await;
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
                            self.current_node =
                                Some(self.request_next(Some(provider_event)).await?);
                            return Box::pin(self.__exec_node(ctrl, ctx)).await;
                        }

                        let step_trigger = match wait_event {
                            WaitEvent::DtmfCollected { digit } => {
                                crate::rwi::TriggerInfo::with_detail(
                                    "dtmf_collected",
                                    serde_json::json!({ "digit": digit }),
                                )
                            }
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
                return Err(e);
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

    async fn request_next(&mut self, event: Option<ProviderEvent>) -> anyhow::Result<ActionNode> {
        let now_rfc3339 = chrono::Utc::now().to_rfc3339();
        let prev_step_duration_ms = self.step_prev_duration_ms;
        let ctx = ProviderContext {
            session_id: self
                .sess
                .variables
                .get("session_id")
                .cloned()
                .unwrap_or_default(),
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
            event,
            route_name: self.route_name.clone(),
            route_headers: self.route_headers.clone(),
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
            Some(ProviderEvent::DtmfMenuInvalid { digit }) => crate::rwi::TriggerInfo::with_detail(
                "dtmf_menu_invalid",
                serde_json::json!({ "digit": digit }),
            ),
            Some(ProviderEvent::DtmfMenuTimeout) => {
                crate::rwi::TriggerInfo::new("dtmf_menu_timeout")
            }
            None => crate::rwi::TriggerInfo::new("unknown"),
        });

        // Fallback on provider error instead of propagating
        match result {
            Ok(node) => Ok(node),
            Err(e) => {
                tracing::warn!(error = %e, "StepIvrApp: provider request failed, using fallback");
                let error_text = e.to_string();
                if self.step_index <= 1 {
                    self.set_runtime_status_shared("startup_error").await;
                } else {
                    self.set_runtime_status_shared("provider_error").await;
                }
                self.set_runtime_error_shared(&error_text).await;
                Ok(ActionNode::with_next(
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
                ))
            }
        }
    }

    async fn execute_node(
        &mut self,
        node: &ActionNode,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<ActionResult> {
        let result = common::execute_action(
            &node.action,
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
        self.set_runtime_status(context, "starting").await;
        ctrl.answer().await?;

        self.sess
            .variables
            .insert("session_id".into(), context.call_info.session_id.clone());
        self.sess
            .variables
            .insert("caller".into(), context.call_info.caller.clone());
        self.sess
            .variables
            .insert("callee".into(), context.call_info.callee.clone());
        self.sess
            .variables
            .insert("direction".into(), context.call_info.direction.clone());

        // Clone SIP headers once; store in self.sess for future request_next calls,
        // then move into SessionContext to avoid a second full clone.
        let headers = context.call_info.sip_headers.clone();

        for (name, value) in &headers {
            let key = format!("sip_{}", name.replace(|c: char| !c.is_alphanumeric(), "_"));
            self.sess.variables.insert(key, value.clone());
        }

        self.sess.sip_headers = headers.clone();

        // Merge ivr_params (from JumpIvr query string) into session variables
        // so they are available for $var$ substitution and sent to the provider.
        if let Some(ref ivp) = self.ivr_params {
            for (k, v) in ivp {
                self.sess.variables.insert(k.clone(), v.clone());
            }
            // Also write to shared session_vars so the next chained app can see them.
            if let Some(ref runtime) = self.runtime_vars {
                let mut sv = runtime.write().await;
                for (k, v) in ivp {
                    sv.insert(k.clone(), v.clone());
                }
            }
        }

        let sess_ctx = SessionContext {
            session_id: context.call_info.session_id.clone(),
            caller: context.call_info.caller.clone(),
            callee: context.call_info.callee.clone(),
            direction: context.call_info.direction.clone(),
            tenant_id: None,
            ivr_id: None,
            sip_headers: Some(headers),
            route_name: self.route_name.clone(),
            route_headers: self.route_headers.clone(),
            custom_data: self.custom_data.clone(),
            transferred_from: self.transferred_from.clone(),
        };
        self.set_runtime_status(context, "provider_start").await;
        self.provider.on_session_start(&sess_ctx).await.ok();

        self.step_prev_start_time = Some(chrono::Utc::now().to_rfc3339());

        self.record_session_start(
            &context.call_info.session_id,
            &context.call_info.caller,
            &context.call_info.callee,
            &context.call_info.direction,
        )
        .await;

        self.set_runtime_status(context, "awaiting_first_step")
            .await;
        let first_node = match self.request_next(Some(ProviderEvent::SessionStart)).await {
            Ok(node) => node,
            Err(err) => {
                self.set_runtime_status(context, "startup_error").await;
                self.set_runtime_error(context, &err.to_string()).await;
                return Err(err);
            }
        };
        self.current_node = Some(first_node);
        self.set_runtime_status(context, "active").await;
        self.__exec_node(ctrl, context).await
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // DTMF received — clear any stale timeout flag so a later hangup is
        // not misclassified as timeout-induced.
        self.timeout_induced = false;
        // Ignore DTMF that arrives before the IVR is ready to accept input
        // (e.g. a key pressed during a plain Prompt/Play step).  Forwarding
        // such early digits to the provider would derail the flow.
        if self.pending_menu.is_none() && !self.awaiting_dtmf && !self.interrupt_on_dtmf {
            tracing::info!(
                digit = %digit,
                "StepIvrApp: ignoring early DTMF (not awaiting input)"
            );
            return Ok(AppAction::Continue);
        }

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
                let dtmf_detail = serde_json::json!({ "digit": digit.clone() });
                self.current_trigger = Some(crate::rwi::TriggerInfo::with_detail(
                    "dtmf_menu",
                    dtmf_detail,
                ));
                if let Some(ref mut t) = self.pending_trace {
                    t.trigger = crate::rwi::TriggerInfo::with_detail(
                        "dtmf_menu",
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
            return Ok(AppAction::Continue);
        }

        // Interruptible Prompt barge-in: forward the digit to the provider
        // (this is the explicit, intended interruption path).
        self.awaiting_dtmf = false;
        if self.interrupt_on_dtmf {
            ctrl.stop_audio().await.ok();
            self.current_track_id = None;
            self.interrupt_on_dtmf = false;
        }

        if let Some(ref mut t) = self.pending_trace {
            t.trigger =
                crate::rwi::TriggerInfo::with_detail("dtmf", serde_json::json!({ "digit": digit }));
        }
        self.current_node = Some(
            self.request_next(Some(ProviderEvent::Dtmf { digit }))
                .await?,
        );
        self.__exec_node(ctrl, context).await
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

        if was_menu && track_id == "ivr_menu_greeting" {
            if let Some(ref menu) = self.pending_menu {
                self.awaiting_dtmf = true;
                ctrl.set_timeout("ivr_dtmf_timeout", Duration::from_millis(menu.timeout_ms));
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
                self.current_node = Some(
                    self.request_next(Some(ProviderEvent::DtmfMenuTimeout))
                        .await?,
                );
                self.current_trigger = Some(crate::rwi::TriggerInfo::new("dtmf_menu_timeout"));
                return self.__exec_node(ctrl, context).await;
            }

            if let Some(next) = self.handle_menu_timeout() {
                self.current_trigger = Some(crate::rwi::TriggerInfo::new("dtmf_menu_timeout"));
                self.current_node = Some(next);
                return self.__exec_node(ctrl, context).await;
            }
        }

        self.current_node = Some(self.request_next(Some(ProviderEvent::DtmfTimeout)).await?);
        self.__exec_node(ctrl, context).await
    }

    async fn on_exit(&mut self, reason: crate::call::app::ExitReason) -> anyhow::Result<()> {
        // Finalize any pending trace (from a WaitFor step) before recording
        // the session end, so the last step's trace is not lost when the call
        // ends while waiting for DTMF, audio playback, or other async input.
        if let Some(pending) = self.pending_take() {
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
            crate::call::app::ExitReason::Normal => EndReason::Normal,
            crate::call::app::ExitReason::Hangup => EndReason::Hangup,
            crate::call::app::ExitReason::RemoteHangup(_) => EndReason::UserHangup,
            crate::call::app::ExitReason::Transferred => {
                // Determine transfer target type from the last action.
                let target = self.last_transfer_target.clone().unwrap_or_default();
                if target.starts_with("queue:") {
                    EndReason::TransferToQueue(target)
                } else if target.starts_with("toivr:") || target.starts_with("ivr:") {
                    EndReason::TransferToIvr(target)
                } else {
                    EndReason::Transfer(target)
                }
            }
            crate::call::app::ExitReason::Error(e) => EndReason::Error(e),
            crate::call::app::ExitReason::Cancelled => EndReason::Hangup,
            _ => EndReason::Normal,
        };

        // If the exit was caused by a DTMF timeout (provider returned Hangup
        // in response to a DtmfTimeout/DtmfMenuTimeout event), refine the end
        // reason from `Hangup` to `Timeout`.
        if self.timeout_induced && matches!(end_reason, EndReason::Hangup) {
            end_reason = EndReason::Timeout;
            end_reason_label = "timeout".to_string();
        }
        let session_id = self
            .sess
            .variables
            .get("session_id")
            .cloned()
            .unwrap_or_default();
        let end_sr = end_reason.to_session_end_reason();
        if !skip_provider_end {
            let (last_action_type, last_step_id, last_step_name) = match &self.current_node {
                Some(node) => (
                    Self::action_type_label(&node.action).to_string(),
                    node.step_id
                        .clone()
                        .or_else(|| self.current_step_id.clone()),
                    node.step_name
                        .clone()
                        .or_else(|| self.current_step_name.clone()),
                ),
                None => (
                    "session_end".to_string(),
                    self.current_step_id.clone(),
                    self.current_step_name.clone(),
                ),
            };
            let caller = self
                .sess
                .variables
                .get("caller")
                .cloned()
                .unwrap_or_default();
            let callee = self
                .sess
                .variables
                .get("callee")
                .cloned()
                .unwrap_or_default();
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
                step_start_time: None,
                step_end_time: Some(chrono::Utc::now().to_rfc3339()),
                duration_ms: 0,
                extra: None,
                end_reason: Some(end_sr.reason.clone()),
                end_detail: end_sr.detail.clone(),
            });

            self.provider
                .on_session_end(&end_reason, &session_id)
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
        self.set_runtime_end_reason_shared(&end_reason_label).await;
        // Clean up local state
        if self.current_track_id.is_some() {
            // Audio track will be cleaned up by media layer
            self.current_track_id = None;
        }
        self.pending_menu = None;

        // If this IVR was started via ivr.exec, write result to extensions.
        if let Some(ref ext) = self.session_extensions {
            let is_exec = ext
                .read()
                .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecState>()
                .is_some();
            if is_exec {
                let collected: std::collections::HashMap<String, String> = self
                    .sess
                    .variables
                    .iter()
                    .filter(|(k, _)| {
                        ![
                            "session_id", "caller", "callee", "direction",
                        ]
                        .contains(&k.as_str())
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                ext.write().insert(
                    crate::proxy::proxy_call::ivr_exec_hook::IvrExecResult {
                        status: status.clone(),
                        reason: end_reason_label.clone(),
                        routing_target: self.last_transfer_target.clone(),
                        collected,
                        trace: vec![],
                        duration_ms: 0,
                        completion_time: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        }

        Ok(())
    }

    async fn on_record_complete(
        &mut self,
        info: RecordingInfo,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
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
        end_called: std::sync::Mutex<bool>,
    }

    impl MockProvider {
        fn new(nodes: Vec<ActionNode>) -> Self {
            Self {
                nodes,
                idx: std::sync::Mutex::new(0),
                start_called: std::sync::Mutex::new(false),
                end_called: std::sync::Mutex::new(false),
            }
        }
    }

    struct MockProviderHandle(Arc<MockProvider>);

    #[async_trait]
    impl ActionProvider for MockProvider {
        async fn next_action(&self, _ctx: ProviderContext) -> anyhow::Result<ActionNode> {
            let mut idx = self.idx.lock().unwrap();
            if *idx < self.nodes.len() {
                let node = self.nodes[*idx].clone();
                *idx += 1;
                Ok(node)
            } else {
                Err(anyhow::anyhow!("no more nodes"))
            }
        }

        async fn on_session_start(&self, _ctx: &SessionContext) -> anyhow::Result<()> {
            *self.start_called.lock().unwrap() = true;
            Ok(())
        }

        async fn on_session_end(
            &self,
            _reason: &EndReason,
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

        async fn on_session_end(&self, reason: &EndReason, session_id: &str) -> anyhow::Result<()> {
            self.0.on_session_end(reason, session_id).await
        }
    }

    fn mock_app(nodes: Vec<ActionNode>) -> StepIvrApp {
        StepIvrApp::with_provider(Box::new(MockProvider::new(nodes)))
    }

    fn make_test_context() -> ApplicationContext {
        ApplicationContext::new(
            DatabaseConnection::Disconnected,
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
        )
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
                return_to_ivr: None,
            }))
        }

        async fn on_session_end(
            &self,
            _reason: &EndReason,
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
                return_to_ivr: None,
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
                return_to_ivr: None,
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
            return_to_ivr: None,
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
    async fn test_dtmf_menu_with_local_entries() {
        let mut entries = HashMap::new();
        entries.insert(
            "1".into(),
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_to_ivr: None,
            }),
        );
        entries.insert(
            "2".into(),
            ActionNode::new(EntryAction::Queue {
                target: "support".into(),
                return_to_ivr: None,
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
                return_to_ivr: None,
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
                return_to_ivr: None,
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
            menu_entry.trigger.r#type, "dtmf_menu",
            "DtmfMenu step trigger should be 'dtmf_menu', got: {:?}",
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
                matches!(c, CallCommand::Transfer { target, .. } if target.starts_with("toivr:"))
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
                return_to_ivr: None,
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
                matches!(c, CallCommand::Transfer { target, .. } if target.starts_with("bridge:"))
            })
            .await;
    }

    #[tokio::test]
    async fn test_trace_integration() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let mut app = mock_app(vec![ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_to_ivr: None,
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
                return_to_ivr: None,
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
                return_to_ivr: None,
            }),
        );
        let resp = serde_json::to_value(&entry).unwrap();

        let url = spawn_mock_provider(vec![resp]).await;
        let app = StepIvrApp::new(&url);

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
                return_to_ivr: None,
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

        let app = StepIvrApp::new(&url);

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
            return_to_ivr: None,
        });

        let url = spawn_mock_provider(vec![
            serde_json::to_value(&menu_resp).unwrap(),
            serde_json::to_value(&transfer_resp).unwrap(),
        ])
        .await;

        let app = StepIvrApp::new(&url);

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
            return_to_ivr: None,
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
                return_to_ivr: None,
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
                return_to_ivr: None,
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
                return_to_ivr: None,
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
                return_to_ivr: None,
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
            return_to_ivr: None,
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
                return_to_ivr: None,
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
    async fn test_session_end_trace_skipped_on_remote_hangup() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let provider = Arc::new(MockProvider::new(vec![ActionNode::new(
            EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_to_ivr: None,
            },
        )]));
        let mut app = StepIvrApp::with_provider(Box::new(MockProviderHandle(provider.clone())));
        app.trace = Some(trace.clone());
        app.ivr_name = Some("test-ivr".to_string());
        app.sess
            .variables
            .insert("session_id".into(), "test-session".into());

        app.on_exit(crate::call::app::ExitReason::RemoteHangup(None))
            .await
            .expect("remote hangup exit should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let entries = trace.query_by_session("test-session").await;
        let has_session_end = entries.iter().any(|e| e.trigger.r#type == "session_end");
        assert!(
            !has_session_end,
            "remote hangup must not record a session_end trace entry"
        );
        assert!(
            !*provider.end_called.lock().unwrap(),
            "remote hangup must skip provider session end"
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
        assert_eq!(
            ctx.get_var(IVR_STATUS_KEY).await.as_deref(),
            Some("cancelled")
        );
        assert_eq!(
            ctx.get_var(IVR_END_REASON_KEY).await.as_deref(),
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
            ctx.get_var(IVR_LAST_ERROR_KEY).await.as_deref(),
            Some("provider bootstrap failed")
        );
        let status_before_exit = ctx.get_var(IVR_STATUS_KEY).await;
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

        assert_eq!(ctx.get_var(IVR_STATUS_KEY).await.as_deref(), Some("hangup"));
        assert_eq!(
            ctx.get_var(IVR_NAME_KEY).await.as_deref(),
            Some("failing-ivr")
        );
        assert_eq!(
            ctx.get_var(IVR_END_REASON_KEY).await.as_deref(),
            Some("hangup")
        );
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
                    return_to_ivr: None,
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

        let provider =
            StepProvider::new(format!("http://{addr}/ivr/step")).with_retry(RetryConfig {
                max_retries: 1,
                timeout_ms: 15_000,
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
            return_to_ivr: None,
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
    async fn test_input_phone_forwards_to_provider() {
        let input_phone = ActionNode::new(EntryAction::InputPhone {
            prompt: Some("enter_phone.wav".into()),
            prompt_text: None,
            prompt_voice: None,
            min_digits: 11,
            max_digits: 11,
        });
        let followup = ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
            params: HashMap::new(),
            return_to_ivr: None,
        });

        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![input_phone, followup])),
            "1001",
            "2000",
        );
        stack
            .assert_cmd(200, "accept", |c| matches!(c, CallCommand::Answer { .. }))
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(
                    c,
                    CallCommand::Play {
                        source: crate::call::domain::MediaSource::File { path }, ..
                    } if path == "enter_phone.wav"
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
            return_to_ivr: None,
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

    // ── True E2E: HTTP → Python provider ─────────────────────────────────
    //
    // Validates the FULL IVR protocol chain:
    //   StepIvrApp → StepProvider (HTTP) → step_ivr_provider.py
    //
    // For a full E2E test with sipbot (SIP → PBX → IVR → HTTP → Provider),
    // build with `--features addon-cc` and run test_full_e2e_via_sipbot.

    /// Start the Python step provider server and return the base URL.
    /// Panics if python3 is not on PATH or the server fails to start.
    struct PythonProvider {
        url: String,
        child: std::process::Child,
    }

    impl Drop for PythonProvider {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn start_python_provider(port: u16) -> PythonProvider {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("step_ivr_provider.py");
        let child = std::process::Command::new("python3")
            .arg(&script)
            .arg(port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("Failed to start Python provider (python3 required)");

        // Wait for server to be ready
        let url = format!("http://127.0.0.1:{}/ivr/step", port);
        for _ in 0..30 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return PythonProvider { url, child };
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        panic!("Python provider did not start on port {port}");
    }

    #[tokio::test]
    async fn test_python_provider_direct_http() {
        use crate::call::app::ivr::provider::StepProvider;
        use portpicker::pick_unused_port;

        let provider_port = pick_unused_port().expect("no free port");
        let provider = start_python_provider(provider_port).await;
        let step_provider = StepProvider::new(&provider.url);

        let session = SessionContext {
            session_id: "test-session".to_string(),
            caller: "1001".to_string(),
            callee: "2000".to_string(),
            direction: "inbound".to_string(),
            tenant_id: None,
            ivr_id: None,
            sip_headers: None,
            route_name: None,
            route_headers: None,
            custom_data: None,
            transferred_from: None,
        };
        step_provider.on_session_start(&session).await.unwrap();

        let ctx = ProviderContext {
            session_id: session.session_id.clone(),
            caller: session.caller.clone(),
            callee: session.callee.clone(),
            direction: session.direction.clone(),
            tenant_id: None,
            ivr_id: None,
            variables: HashMap::new(),
            sip_headers: None,
            event: Some(ProviderEvent::SessionStart),
            route_name: None,
            route_headers: None,
            custom_data: None,
            step_start_time: None,
            step_end_time: None,
            step_duration_ms: None,
            step_index: None,
            transferred_from: None,
        };
        let prompt = step_provider.next_action(ctx).await.unwrap();
        assert!(
            matches!(prompt.action, EntryAction::Prompt { ref tts_text, interruptible: true, .. }
                if tts_text.as_deref().is_some_and(|text| text.contains("IVR step")))
        );

        let ctx = ProviderContext {
            event: Some(ProviderEvent::Dtmf {
                digit: "2".to_string(),
            }),
            ..ProviderContext {
                session_id: session.session_id.clone(),
                caller: session.caller.clone(),
                callee: session.callee.clone(),
                direction: session.direction.clone(),
                tenant_id: None,
                ivr_id: None,
                variables: HashMap::new(),
                sip_headers: None,
                event: None,
                route_name: None,
                route_headers: None,
                custom_data: None,
                step_start_time: None,
                step_end_time: None,
                step_duration_ms: None,
                step_index: None,
                transferred_from: None,
            }
        };
        let action = step_provider.next_action(ctx).await.unwrap();
        // Latest step_ivr_provider.py maps DTMF "2" to a queue action targeting
        // "sales" (see examples/step_ivr_provider.py::IvrSession.next_action).
        assert!(
            matches!(action.action, EntryAction::Queue { ref target, .. } if target == "sales")
        );

        step_provider
            .on_session_end(&EndReason::Normal, "test-session")
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires running PBX infrastructure"]
    #[cfg(feature = "addon-cc")]
    async fn test_full_e2e_via_sipbot() {
        use crate::addons::cc::tests::helpers::test_server::{TestPbx, TestPbxInject};
        use crate::proxy::routing::{MatchConditions, RouteAction, RouteRule};
        use portpicker::pick_unused_port;
        use sipbot::{
            config::{AccountConfig, Config as SipBotConfig},
            sip::SipBot,
            stats::CallStats,
        };
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let sip_port = pick_unused_port().expect("no free port");
        let provider_port = pick_unused_port().expect("no free port");

        // 1. Start Python provider
        let provider = start_python_provider(provider_port).await;

        // 2. Create step-mode IVR route pointing to Python
        let route = RouteRule {
            name: "step_ivr_e2e".into(),
            priority: 10,
            match_conditions: MatchConditions {
                to_user: Some("ivr-test".into()),
                ..Default::default()
            },
            action: RouteAction {
                app: Some("ivr".into()),
                app_params: Some(serde_json::json!({
                    "mode": "step",
                    "url": provider.url.clone(),
                })),
                ..Default::default()
            },
            ..Default::default()
        };

        // 3. Start PBX with step-mode routing
        let pbx = TestPbx::start_with_inject(
            sip_port,
            TestPbxInject {
                routes: Some(vec![route]),
                ..Default::default()
            },
        )
        .await;

        let cancel = CancellationToken::new();
        let stats = Arc::new(CallStats::default());

        // 4. sipbot caller → SIP INVITE → PBX → IVR → HTTP → Python
        let mut caller = SipBot::new(
            AccountConfig {
                username: "caller".into(),
                domain: format!("127.0.0.1:{}", sip_port),
                register: Some(false),
                target: Some(format!("sip:ivr-test@127.0.0.1:{}", sip_port)),
                ..Default::default()
            },
            SipBotConfig {
                addr: Some(format!(
                    "127.0.0.1:{}",
                    pick_unused_port().expect("no free port")
                )),
                external_ip: None,
                recorders: None,
                accounts: vec![],
            },
            stats.clone(),
            false,
            cancel.clone(),
        );
        let _ = caller.run_call(1, 1).await;

        // 5. Wait for call to complete
        tokio::time::sleep(Duration::from_secs(10)).await;

        // 6. Verify at least 1 call was placed
        assert!(
            stats.total_calls.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "sipbot should have sent a call"
        );

        cancel.cancel();
        pbx.cancel_token.cancel();
    }
}
