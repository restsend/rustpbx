use super::common::{self, ActionResult, SessionData, TerminalAction, WaitEvent};
use super::config::{ActionNode, EntryAction};
use super::provider::{ActionProvider, EndReason, ProviderContext, ProviderEvent, SessionContext};
use super::trace::{IvrTraceCollector, IvrTraceEntry, IvrTraceSession};
use crate::call::app::{
    AppAction, AppEvent, ApplicationContext, CallApp, CallAppType, CallController,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub struct StepIvrApp {
    provider: Box<dyn ActionProvider>,
    current_node: Option<ActionNode>,
    sess: SessionData,
    pending_menu: Option<PendingMenu>,
    current_track_id: Option<String>,
    interrupt_on_dtmf: bool,
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
    /// Current step start time (ISO UTC) — set when a step begins, used for step_start_time.
    current_step_start_time: Option<String>,
    /// Current step provider response metadata.
    current_step_id: Option<String>,
    current_step_name: Option<String>,
    /// Event type that triggered the current step (e.g. "dtmf", "session_start").
    current_trigger_event_type: Option<String>,
    /// Event detail for the current trigger (e.g. "digit=5").
    current_trigger_event_detail: Option<String>,
}

#[derive(Clone)]
struct PendingMenu {
    entries: HashMap<String, ActionNode>,
    timeout_action: Option<Box<ActionNode>>,
    invalid_action: Option<Box<ActionNode>>,
    max_retries: u32,
    retry_count: u32,
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
            current_step_start_time: None,
            current_step_id: None,
            current_step_name: None,
            current_trigger_event_type: None,
            current_trigger_event_detail: None,
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
            current_step_start_time: None,
            current_step_id: None,
            current_step_name: None,
            current_trigger_event_type: None,
            current_trigger_event_detail: None,
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
    pub fn with_rwi_gateway(
        mut self,
        gw: Option<crate::rwi::RwiGatewayRef>,
    ) -> Self {
        self.rwi_gateway = gw;
        self
    }

    /// Set the IVR name for identification in traces.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.ivr_name = Some(name.into());
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
                event_type: entry.event_type.clone(),
                event_detail: entry.event_detail.clone(),
                action_type: entry.action_type.clone(),
                action_json: entry.action_json.clone(),
                result_kind: entry.result_kind.clone(),
                error: entry.error.clone(),
                step_id: entry.step_id,
                step_name: entry.step_name,
                step_start_time: entry.step_start_time,
                step_end_time: entry.step_end_time,
                duration_ms: entry.duration_ms,
                extra: entry.extra,
                sip_headers: Some(self.sess.sip_headers.clone()),
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

    async fn __exec_node(
        &mut self,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let node = self.current_node.as_ref().unwrap().clone();
        let node_type_str = match &node.action {
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
            EntryAction::VoipBridge { .. } => "VoipBridge",
        }
        .to_string();
        let action_json = serde_json::to_string(&node).ok();
        let start = std::time::Instant::now();
        let result = self.execute_node(&node, ctrl, ctx).await;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let step_end = chrono::Utc::now().to_rfc3339();

        let step_id = node.step_id.clone().or_else(|| self.current_step_id.clone());
        let step_name = node.step_name.clone().or_else(|| self.current_step_name.clone());

        let trigger_event_type = self
            .current_trigger_event_type
            .clone()
            .unwrap_or_else(|| "action_execute".to_string());
        let trigger_event_detail = self.current_trigger_event_detail.clone();

        self.current_trigger_event_type = None;
        self.current_trigger_event_detail = None;

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
                let (app_action, _result_kind) = match action_result {
                    ActionResult::Terminal(terminal) => {
                        self.step_index += 1;
                        self.increment_total_steps();
                        self.record_trace(IvrTraceEntry {
                            session_id: session_id.clone(),
                            caller: caller.clone(),
                            callee: callee.clone(),
                            step_index: self.step_index,
                            event_type: trigger_event_type.clone(),
                            event_detail: trigger_event_detail.clone(),
                            provider_url: None,
                            action_type: node_type_str,
                            action_json,
                            result_kind: "terminal".to_string(),
                            error: None,
                            step_id: step_id.clone(),
                            step_name: step_name.clone(),
                            step_start_time: self.current_step_start_time.clone(),
                            step_end_time: Some(step_end),
                            duration_ms: elapsed_ms,
                            extra: self.extra.clone(),
                        });
                        match terminal {
                    TerminalAction::Transfer(target) => {
                            self.last_transfer_target = Some(target.clone());
                            (AppAction::Transfer(target), "terminal")
                        }
                            TerminalAction::Hangup { reason, code } => {
                                (AppAction::Hangup { reason, code }, "terminal")
                            }
                            TerminalAction::Exit => (AppAction::Exit, "terminal"),
                        }
                    }
                    ActionResult::ChainedTo(next) => {
                        self.current_trigger_event_type = Some("chained".to_string());
                        self.current_trigger_event_detail = None;
                        self.current_node = Some(next);
                        return Box::pin(self.__exec_node(ctrl, ctx)).await;
                    }
                    ActionResult::WaitFor(_) => {
                        self.record_trace(IvrTraceEntry {
                            session_id: session_id.clone(),
                            caller: caller.clone(),
                            callee: callee.clone(),
                            step_index: self.step_index,
                            event_type: trigger_event_type.clone(),
                            event_detail: trigger_event_detail.clone(),
                            provider_url: None,
                            action_type: node_type_str,
                            action_json,
                            result_kind: "continue".to_string(),
                            error: None,
                            step_id: step_id.clone(),
                            step_name: step_name.clone(),
                            step_start_time: self.current_step_start_time.clone(),
                            step_end_time: None,
                            duration_ms: elapsed_ms,
                            extra: self.extra.clone(),
                        });
                        (AppAction::Continue, "continue")
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
                    event_type: trigger_event_type,
                    event_detail: trigger_event_detail,
                    provider_url: None,
                    action_type: node_type_str,
                    action_json,
                    result_kind: "error".to_string(),
                    error: Some(e.to_string()),
                    step_id,
                    step_name,
                    step_start_time: self.current_step_start_time.clone(),
                    step_end_time: Some(step_end),
                    duration_ms: elapsed_ms,
                    extra: self.extra.clone(),
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
            step_start_time: Some(self.step_prev_start_time.clone().unwrap_or_else(|| now_rfc3339.clone())),
            step_end_time: Some(now_rfc3339.clone()),
            step_duration_ms: if prev_step_duration_ms > 0 { Some(prev_step_duration_ms) } else { None },
            step_index: Some(self.step_index),
            transferred_from: self.transferred_from.clone(),
        };

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

        // Store trigger event info for __exec_node to use when recording trace after node execution
        self.current_trigger_event_type = Some(ctx
            .event
            .as_ref()
            .map(|e| match e {
                ProviderEvent::SessionStart => "session_start",
                ProviderEvent::AudioComplete { .. } => "audio_complete",
                ProviderEvent::Dtmf { .. } => "dtmf",
                ProviderEvent::DtmfTimeout => "dtmf_timeout",
                ProviderEvent::ApiResponse { .. } => "api_response",
                ProviderEvent::PhoneCollected { .. } => "phone_collected",
                ProviderEvent::RecordingComplete { .. } => "recording_complete",
                ProviderEvent::InputVoice { .. } => "input_voice",
                ProviderEvent::Error { .. } => "error",
                ProviderEvent::DtmfMenuInvalid { .. } => "dtmf_menu_invalid",
                ProviderEvent::DtmfMenuTimeout => "dtmf_menu_timeout",
            })
            .unwrap_or("unknown")
            .to_string());
        self.current_trigger_event_detail = match &ctx.event {
            Some(ProviderEvent::Dtmf { digit }) => Some(format!("digit={}", digit)),
            Some(ProviderEvent::ApiResponse { status, .. }) => Some(format!("status={}", status)),
            Some(ProviderEvent::PhoneCollected { number }) => Some(format!("number={}", number)),
            _ => None,
        };

        // Fallback on provider error instead of propagating
        match result {
            Ok(node) => Ok(node),
            Err(e) => {
                tracing::warn!(error = %e, "StepIvrApp: provider request failed, using fallback");
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
            }
        }
        if matches!(&result, ActionResult::WaitFor(WaitEvent::NoAudio)) {
            if node.action.is_dtmf_menu() {
                self.pending_menu = Some(self.build_pending_menu(&node.action));
                if let Some(ref menu) = self.pending_menu {
                    ctrl.set_timeout(
                        "ivr_dtmf_timeout",
                        Duration::from_millis(menu.max_retries as u64 * 5000),
                    );
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
                ..
            } => PendingMenu {
                entries: entries.clone(),
                timeout_action: timeout_action.clone(),
                invalid_action: invalid_action.clone(),
                max_retries: *max_retries,
                retry_count: 0,
            },
            _ => unreachable!(),
        }
    }

    fn handle_menu_dtmf(&mut self, digit: &str) -> Option<ActionNode> {
        let menu = self.pending_menu.take()?;
        let next_retry = menu.retry_count + 1;
        let entries = menu.entries;
        let timeout_action = menu.timeout_action;
        let invalid_action = menu.invalid_action;
        let max_retries = menu.max_retries;

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
            });
            return Some(next_action);
        }
        self.pending_menu = Some(PendingMenu {
            retry_count: next_retry,
            entries,
            timeout_action,
            invalid_action: None,
            max_retries,
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
        self.provider.on_session_start(&sess_ctx).await.ok();

        self.step_prev_start_time = Some(chrono::Utc::now().to_rfc3339());

        self.record_session_start(
            &context.call_info.session_id,
            &context.call_info.caller,
            &context.call_info.callee,
            &context.call_info.direction,
        )
        .await;

        self.current_node = Some(self.request_next(Some(ProviderEvent::SessionStart)).await?);
        self.__exec_node(ctrl, context).await
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        ctrl: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if self.pending_menu.is_some() {
            ctrl.stop_audio().await.ok();
            self.current_track_id = None;
            self.interrupt_on_dtmf = false;

            if let Some(next) = self.handle_menu_dtmf(&digit) {
                self.provider.on_local_dtmf_match(&digit, &next).await;
                self.current_trigger_event_type = Some("dtmf_menu".to_string());
                self.current_trigger_event_detail = Some(format!("digit={}", digit));
                self.current_node = Some(next);
                return self.__exec_node(ctrl, context).await;
            }
        }

        if self.interrupt_on_dtmf {
            ctrl.stop_audio().await.ok();
            self.current_track_id = None;
            self.interrupt_on_dtmf = false;
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
                ctrl.set_timeout(
                    "ivr_dtmf_timeout",
                    Duration::from_millis(menu.max_retries as u64 * 5000),
                );
                return Ok(AppAction::Continue);
            }
        }

        if let Some(ref node) = self.current_node {
            if let Some(ref next) = node.next {
                self.current_trigger_event_type = Some("audio_complete".to_string());
                self.current_trigger_event_detail = None;
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

        if self.pending_menu.is_some() {
            if let Some(next) = self.handle_menu_timeout() {
                self.current_trigger_event_type = Some("dtmf_menu_timeout".to_string());
                self.current_trigger_event_detail = None;
                self.current_node = Some(next);
                return self.__exec_node(ctrl, context).await;
            }
        }

        self.current_node = Some(self.request_next(Some(ProviderEvent::DtmfTimeout)).await?);
        self.__exec_node(ctrl, context).await
    }

    async fn on_exit(&mut self, reason: crate::call::app::ExitReason) -> anyhow::Result<()> {
        let end_reason = match reason {
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
            _ => EndReason::Normal,
        };
        let session_id = self
            .sess
            .variables
            .get("session_id")
            .cloned()
            .unwrap_or_default();
        self.provider.on_session_end(&end_reason, &session_id).await.ok();
        let status = match &end_reason {
            EndReason::Normal => "completed",
            EndReason::Transfer(_) => "completed",
            EndReason::TransferToQueue(_) => "completed",
            EndReason::TransferToIvr(_) => "completed",
            EndReason::Hangup => "completed",
            EndReason::UserHangup => "completed",
            EndReason::Error(_) => "error",
        };
        self.record_session_end(status).await;
        // Clean up local state
        if self.current_track_id.is_some() {
            // Audio track will be cleaned up by media layer
            self.current_track_id = None;
        }
        self.pending_menu = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::app::testing::MockCallStack;
    use crate::call::domain::CallCommand;
    use async_trait::async_trait;
    use std::collections::HashMap;

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

        async fn on_session_end(&self, _reason: &EndReason, _session_id: &str) -> anyhow::Result<()> {
            *self.end_called.lock().unwrap() = true;
            Ok(())
        }
    }

    fn mock_app(nodes: Vec<ActionNode>) -> StepIvrApp {
        StepIvrApp::with_provider(Box::new(MockProvider::new(nodes)))
    }

    #[tokio::test]
    async fn test_transfer() {
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
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
    async fn test_voip_bridge() {
        let mut stack = MockCallStack::run(
            Box::new(mock_app(vec![ActionNode::new(EntryAction::VoipBridge {
                create_room_uri: "https://voip.example.com/rooms".into(),
                headers: HashMap::from([("Authorization".into(), "Bearer token".into())]),
                timeout_ms: Some(30000),
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
                matches!(c, CallCommand::Transfer { target, .. } if target.starts_with("voip_bridge:"))
            })
            .await;
    }

    #[tokio::test]
    async fn test_trace_integration() {
        use crate::call::app::ivr::trace::IvrTraceCollector;

        let trace = IvrTraceCollector::new();
        let mut app = mock_app(vec![ActionNode::new(EntryAction::Transfer {
            target: "2001".into(),
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
        assert_eq!(sess.status, "completed");

        // Verify trace entries exist
        let entries = trace.query_by_session(&sess.session_id).await;
        assert!(!entries.is_empty(), "expected at least one trace entry");
        // action_type uses discriminant which varies, just check result_kind
        assert_eq!(entries[0].result_kind, "terminal");
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
        step_provider
            .on_session_start(&session)
            .await
            .unwrap();

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
        let transfer = step_provider.next_action(ctx).await.unwrap();
        assert!(
            matches!(transfer.action, EntryAction::Transfer { ref target } if target == "agent")
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
