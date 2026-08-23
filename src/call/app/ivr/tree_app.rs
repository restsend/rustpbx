//! IVR application — built-in, config-driven interactive voice response.
//!
//! Reads TOML configuration from `config/ivr/{name}.toml` and drives a
//! menu-based state machine through the [`CallApp`] trait.
//!
//! # State Machine
//!
//! ```text
//! Init → PlayingGreeting → WaitingDtmf ──→ (action)
//!                              ↑   │
//!                              │   ├─ timeout → PlayingInvalid/retry
//!                              │   └─ invalid → PlayingInvalid/retry
//!                              │       │
//!                              └───────┘
//!       PlayingAnnouncement → (return to menu)
//!       CollectingExtension → Transfer
//!       Webhook → (response determines next action)
//! ```

use super::config::{EntryAction, IvrDefinition, WebhookResponse};
use crate::call::app::{
    AppAction, ApplicationContext, CallApp, CallAppType, CallController, DtmfCollectConfig,
};
use crate::callrecord::CallRecordHangupReason;
use crate::models::call_record::extract_sip_username;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Internal state of the IVR state machine.
#[derive(Debug, Clone, PartialEq)]
enum IvrState {
    /// Initial state before `on_enter`.
    Init,
    /// Playing the greeting audio for a menu.
    PlayingGreeting { menu_key: String },
    /// Waiting for a DTMF key press.
    WaitingDtmf { menu_key: String, retry_count: u32 },
    /// Playing the "invalid input" prompt, will retry afterwards.
    PlayingInvalid { menu_key: String, retry_count: u32 },
    /// Playing an announcement (from `play` action), returns to `return_menu`.
    PlayingAnnouncement { return_menu: String },
    /// Playing a hangup/goodbye prompt before disconnecting.
    PlayingHangup,
    /// Playing a prompt before hanging up with a specific SIP code.
    PlayingAndHangup { code: Option<u16> },
    /// Collecting multi-digit extension input.
    CollectingExtension,
    /// Terminal state.
    Done,
}

/// Payload sent to the webhook endpoint with call context information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookPayload {
    /// Unique call session identifier.
    pub session_id: String,
    /// Caller number/URI.
    pub caller: String,
    /// Callee number/URI.
    pub callee: String,
    /// Call direction ("inbound" / "outbound").
    pub direction: String,
    /// IVR definition name.
    pub ivr_name: String,
    /// Current menu key.
    pub menu: String,
    /// Collected variables from Collect actions.
    #[serde(default)]
    pub variables: std::collections::HashMap<String, String>,
    /// Last DTMF digit that triggered this webhook, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digit: Option<String>,
}

/// A built-in IVR application driven by TOML configuration.
///
/// Supports nested menus, DTMF routing, timeouts, retries, transfer,
/// queue, voicemail, play-and-return, extension collection, and more.
pub struct IvrApp {
    /// Parsed IVR definition (menus, entries, actions).
    definition: IvrDefinition,
    /// Current state machine state.
    state: IvrState,
    /// Menu navigation stack (e.g. `["root", "sales"]`).
    menu_stack: Vec<String>,
    /// Retry count carried across greeting replay (since `PlayingGreeting`
    /// state itself doesn't track retries).
    pending_retry_count: u32,
    /// Variables collected via Collect actions.
    collected_variables: std::collections::HashMap<String, String>,
    /// First digit collected for unknown_key_action (direct dial scenario).
    pending_unknown_digit: Option<String>,
    /// Last DTMF digit that triggered an action (for IvrNodeExited / webhook).
    last_dtmf_digit: Option<String>,
    /// Optional TTS service synthesized from the IVR's own TTS config.
    tts_service: Option<Arc<crate::tts::TtsService>>,
    /// Menu to start from on `on_enter` (used by return-to-IVR resume).
    /// When `Some`, `on_enter` navigates directly to this menu instead of "root".
    start_menu: Option<String>,
    /// Number of nodes traversed (for IvrFlowCompleted).
    nodes_traversed: u32,
    /// Timestamp when IVR flow started (for total_duration_ms).
    flow_started_at: Option<std::time::Instant>,
    /// Timestamp when the current node was entered (for IvrNodeExited.duration_ms).
    node_entered_at: Option<std::time::Instant>,
    /// Shared session variables, stashed in `on_enter` so `on_exit` (which has
    /// no context) can publish the IVR end reason.
    runtime_vars: Option<Arc<dashmap::DashMap<String, String>>>,
    /// RWI gateway, stashed in `on_enter` so `on_exit` can emit events.
    rwi_gateway: Option<crate::rwi::RwiGatewayRef>,
    /// Call session id, stashed in `on_enter` for `on_exit` event payloads.
    session_id: Option<String>,
    /// Session extensions clone stashed in `on_enter`, used in `on_exit` to
    /// write an `IvrExecResult` when the IVR was started via `ivr.exec`.
    session_extensions: Option<crate::proxy::proxy_call::session_hooks::SessionExtensions>,
    /// Set when a terminal action already emitted `IvrFlowCompleted`, so
    /// `on_exit` does not double-report an aborted flow.
    flow_completed: bool,
}

impl IvrApp {
    /// Create a new `IvrApp` from a parsed [`IvrDefinition`].
    pub fn new(definition: IvrDefinition) -> Self {
        let tts_service = definition
            .tts
            .as_ref()
            .map(|cfg| Arc::new(crate::tts::TtsService::new(cfg.clone())));
        Self {
            definition,
            state: IvrState::Init,
            menu_stack: vec!["root".to_string()],
            pending_retry_count: 0,
            collected_variables: std::collections::HashMap::new(),
            pending_unknown_digit: None,
            last_dtmf_digit: None,
            tts_service,
            nodes_traversed: 0,
            flow_started_at: None,
            node_entered_at: None,
            start_menu: None,
            runtime_vars: None,
            rwi_gateway: None,
            session_id: None,
            session_extensions: None,
            flow_completed: false,
        }
    }

    /// Set a non-root starting menu (used for return-to-IVR resume).
    pub fn with_start_menu(mut self, menu: String) -> Self {
        self.start_menu = Some(menu);
        self
    }

    /// Create a new `IvrApp` with an explicit TTS config override.
    pub fn with_tts(mut self, tts: Option<crate::tts::TtsConfig>) -> Self {
        self.tts_service = tts.map(|cfg| Arc::new(crate::tts::TtsService::new(cfg)));
        self
    }

    /// Load an `IvrApp` from a TOML file path.
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read IVR config '{}': {}", path, e))?;
        let file_config: super::config::IvrFileConfig = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse IVR config '{}': {}", path, e))?;
        file_config
            .ivr
            .validate()
            .map_err(|e| anyhow::anyhow!("IVR config validation failed '{}': {}", path, e))?;
        Ok(Self::new(file_config.ivr))
    }

    /// Emit an RWI event via the gateway in the application context, if configured.
    fn emit_rwi_event_typed(
        &self,
        ctx: &ApplicationContext,
        event: &impl crate::rwi::RwiEventSpec,
    ) {
        if let Some(ref gw) = ctx.rwi_gateway {
            let gw = gw.read();
            gw.fan_out(&ctx.call_info.session_id, event);
        }
    }

    /// Emit IvrFlowCompleted when the IVR flow ends via a terminal action.
    /// Also writes [`IvrExecResult`] to session extensions if the IVR was
    /// started via `ivr.exec`, so the post-exit hook can send the result back.
    ///
    /// `status` is the coarse outcome category (`"completed"`, `"transferred"`,
    /// `"hangup"`, etc.) and `reason` provides finer detail (e.g.
    /// `"agent_transfer"`, `"queue"`, `"voicemail"`, `"caller_hangup"`).
    async fn ivr_flow_completed(
        &mut self,
        ctx: &ApplicationContext,
        status: &str,
        reason: &str,
        target: Option<&str>,
    ) {
        // Guard `on_exit` against double-reporting this flow.
        self.flow_completed = true;
        let total_duration_ms = self
            .flow_started_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let completion_time = chrono::Utc::now().to_rfc3339();

        // If this IVR was started via ivr.exec, write result to extensions.
        super::exec::write_ivr_exec_result(
            &ctx.session_extensions,
            super::exec::build_ivr_exec_result(
                status,
                reason,
                target.map(|s| s.to_string()),
                self.collected_variables.clone(),
                total_duration_ms,
            ),
        );

        self.emit_rwi_event_typed(
            ctx,
            &crate::rwi::IvrFlowCompleted {
                call_id: ctx.call_info.session_id.clone(),
                app_id: self.definition.name.clone(),
                total_nodes_traversed: self.nodes_traversed,
                total_duration_ms: total_duration_ms as u32,
                final_result: status.to_string(),
                completion_time,
                final_routing_target: target.map(|s| s.to_string()),
                extra: None,
            },
        );
    }

    /// Check if the current time falls within business hours.
    fn is_within_business_hours(&self, bh: &super::config::BusinessHours) -> bool {
        use chrono::{Datelike, Utc};

        let tz: chrono_tz::Tz = match bh.timezone.parse() {
            Ok(tz) => tz,
            Err(_) => {
                warn!(
                    ivr = %self.definition.name,
                    timezone = %bh.timezone,
                    "Invalid timezone, defaulting to UTC"
                );
                chrono_tz::UTC
            }
        };

        let now = Utc::now().with_timezone(&tz);
        let weekday = match now.weekday() {
            chrono::Weekday::Mon => "mon",
            chrono::Weekday::Tue => "tue",
            chrono::Weekday::Wed => "wed",
            chrono::Weekday::Thu => "thu",
            chrono::Weekday::Fri => "fri",
            chrono::Weekday::Sat => "sat",
            chrono::Weekday::Sun => "sun",
        };

        for schedule in &bh.schedules {
            if !schedule
                .days
                .iter()
                .any(|d| d.eq_ignore_ascii_case(weekday))
            {
                continue;
            }

            let start = match chrono::NaiveTime::parse_from_str(&schedule.start, "%H:%M") {
                Ok(t) => t,
                Err(_) => continue,
            };
            let end = match chrono::NaiveTime::parse_from_str(&schedule.end, "%H:%M") {
                Ok(t) => t,
                Err(_) => continue,
            };

            let current_time = now.time();
            if current_time >= start && current_time <= end {
                return true;
            }
        }

        // If no schedules defined, always open
        bh.schedules.is_empty()
    }

    /// Get the current menu key (top of stack).
    fn current_menu_key(&self) -> &str {
        self.menu_stack.last().map(|s| s.as_str()).unwrap_or("root")
    }

    /// Navigate to a menu. If `"root"`, reset the stack. Otherwise push only
    /// if the menu is not already the current top (avoids unbounded growth on Repeat).
    fn navigate_to_menu(&mut self, menu_key: &str) {
        let old_stack = self.menu_stack.clone();
        if menu_key == "root" {
            self.menu_stack.clear();
            self.menu_stack.push("root".to_string());
        } else if self.current_menu_key() != menu_key {
            self.menu_stack.push(menu_key.to_string());
        }
        if old_stack != self.menu_stack {
            info!(
                ivr = %self.definition.name,
                old_stack = ?old_stack,
                new_stack = ?self.menu_stack,
                "IVR menu stack changed"
            );
        }
        // If already on this menu (e.g. Repeat), keep the stack as-is.
    }

    fn navigate_back(&mut self) -> String {
        if self.menu_stack.len() > 1 {
            let popped = self.menu_stack.pop();
            info!(
                ivr = %self.definition.name,
                popped = ?popped,
                new_top = ?self.menu_stack.last(),
                "IVR navigating back"
            );
        } else {
            info!(ivr = %self.definition.name, "IVR Back called at root, staying on root");
        }
        self.menu_stack
            .last()
            .cloned()
            .unwrap_or_else(|| "root".to_string())
    }

    async fn resolve_audio(
        &self,
        file: Option<&str>,
        text: Option<&str>,
        voice: Option<&str>,
    ) -> Option<String> {
        super::common::resolve_audio(file, text, voice, self.tts_service.as_ref()).await
    }

    /// Start playing the greeting for the specified menu.
    async fn enter_menu(
        &mut self,
        menu_key: &str,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        self.navigate_to_menu(menu_key);
        if self.flow_started_at.is_none() {
            self.flow_started_at = Some(std::time::Instant::now());
        }
        self.node_entered_at = Some(std::time::Instant::now());
        self.nodes_traversed += 1;
        info!(
            ivr = %self.definition.name,
            menu = menu_key,
            menu_stack = ?self.menu_stack,
            "IVR entering menu"
        );

        // Emit IvrNodeEntered event
        let previous_node = self.menu_stack.iter().rev().nth(1).cloned();
        self.emit_rwi_event_typed(
            ctx,
            &crate::rwi::IvrNodeEntered {
                call_id: ctx.call_info.session_id.clone(),
                node_id: menu_key.to_string(),
                node_name: menu_key.to_string(),
                node_type: "menu".to_string(),
                app_id: self.definition.name.clone(),
                entry_time: chrono::Utc::now().to_rfc3339(),
                caller_name: extract_sip_username(&ctx.call_info.caller),
                callee_name: extract_sip_username(&ctx.call_info.callee),
                routing_target: Some(menu_key.to_string()),
                previous_node_id: previous_node,
                extra: None,
            },
        );
        let menu = self
            .definition
            .get_menu(menu_key)
            .ok_or_else(|| anyhow::anyhow!("IVR menu '{}' not found", menu_key))?;
        let greeting = self
            .resolve_audio(
                Some(&menu.greeting),
                menu.greeting_text.as_deref(),
                menu.greeting_voice.as_deref(),
            )
            .await;
        self.state = IvrState::PlayingGreeting {
            menu_key: menu_key.to_string(),
        };
        if let Some(path) = greeting {
            info!(ivr = %self.definition.name, menu = menu_key, "Playing greeting: {}", path);
            ctrl.play_audio(&path, false).await?;
        } else {
            info!(
                ivr = %self.definition.name,
                menu = menu_key,
                "No greeting audio, waiting DTMF immediately"
            );
            self.start_waiting_dtmf(menu_key, self.pending_retry_count, ctrl);
        }
        Ok(AppAction::Continue)
    }

    /// Start waiting for DTMF input with a timeout.
    fn start_waiting_dtmf(&mut self, menu_key: &str, retry_count: u32, ctrl: &CallController) {
        let menu = self.definition.get_menu(menu_key);
        let timeout_ms = menu.map(|m| m.timeout_ms).unwrap_or(5000);
        self.state = IvrState::WaitingDtmf {
            menu_key: menu_key.to_string(),
            retry_count,
        };
        ctrl.set_timeout("ivr_dtmf_timeout", Duration::from_millis(timeout_ms));
        info!(
            ivr = %self.definition.name,
            menu = menu_key,
            retry_count,
            timeout_ms,
            "IVR waiting for DTMF input"
        );
    }

    /// Execute an action from a DTMF press or timeout/max-retries fallback.
    async fn execute_action(
        &mut self,
        action: &EntryAction,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
        dtmf_digit: Option<&str>,
    ) -> anyhow::Result<AppAction> {
        ctrl.cancel_timeout("ivr_dtmf_timeout");

        // Emit IvrNodeExited event when leaving a menu node.
        if let IvrState::WaitingDtmf { ref menu_key, .. }
        | IvrState::PlayingGreeting { ref menu_key } = self.state
        {
            let node_name = menu_key.clone();
            let duration_ms = self
                .node_entered_at
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let action_type = match action {
                EntryAction::Transfer { .. } => "transfer",
                EntryAction::Queue { .. } => "queue",
                EntryAction::Menu { .. } => "menu",
                EntryAction::JumpIvr { .. } => "jump_ivr",
                EntryAction::RouteToAgent { .. } => "route_to_agent",
                _ => "other",
            };
            let result_value = dtmf_digit
                .map(|d| d.to_string())
                .unwrap_or_else(|| action_type.to_string());
            self.emit_rwi_event_typed(
                ctx,
                &crate::rwi::IvrNodeExited {
                    call_id: ctx.call_info.session_id.clone(),
                    node_id: menu_key.clone(),
                    node_name,
                    result_value: Some(result_value),
                    duration_ms: duration_ms as u32,
                    exit_time: chrono::Utc::now().to_rfc3339(),
                    next_node_id: None,
                    hangup_reason: None,
                    call_result: None,
                    extra: Some(serde_json::json!({ "action_type": action_type })),
                },
            );
        }
        match action {
            EntryAction::Transfer {
                target,
                params,
                return_app,
                return_target,
            } => {
                let mut t = target.clone();
                let mut query = String::new();
                for (i, (k, v)) in params.iter().enumerate() {
                    if i > 0 {
                        query.push('&');
                    }
                    query.push_str(&format!("{}={}", k, urlencoding::encode(v)));
                }
                super::exec::append_return_app_query(
                    &mut query,
                    return_app,
                    return_target,
                    Some(self.current_menu_key()),
                );
                if !query.is_empty() {
                    t.push('?');
                    t.push_str(&query);
                }
                info!(ivr = %self.definition.name, target = %t, "IVR transferring call");
                self.ivr_flow_completed(ctx, "transferred", "agent_transfer", Some(t.as_str()))
                    .await;
                self.state = IvrState::Done;
                Ok(AppAction::Transfer(t))
            }
            EntryAction::Queue {
                target,
                return_app,
                return_target,
            } => {
                info!(
                    ivr = %self.definition.name,
                    queue = target,
                    return_app = ?return_app,
                    "IVR sending to queue"
                );
                self.ivr_flow_completed(ctx, "transferred", "queue", Some(target))
                    .await;
                self.state = IvrState::Done;
                let mut queue_uri = format!("queue:{}", target);
                let mut query = String::new();
                super::exec::append_return_app_query(
                    &mut query,
                    return_app,
                    return_target,
                    Some(self.current_menu_key()),
                );
                if !query.is_empty() {
                    queue_uri.push('?');
                    queue_uri.push_str(&query);
                }
                Ok(AppAction::Transfer(queue_uri))
            }
            EntryAction::Menu { menu } => {
                info!(ivr = %self.definition.name, from = %self.current_menu_key(), to = %menu, "IVR navigating to menu");
                self.enter_menu(menu, ctrl, ctx).await
            }
            EntryAction::Back => {
                let target = self.navigate_back();
                info!(ivr = %self.definition.name, menu = %target, "IVR entering parent menu after Back");
                self.enter_menu(&target, ctrl, ctx).await
            }
            EntryAction::Voicemail { target } => {
                info!(ivr = %self.definition.name, target, "IVR transferring to voicemail");
                self.ivr_flow_completed(ctx, "transferred", "voicemail", Some(target))
                    .await;
                self.state = IvrState::Done;
                Ok(AppAction::Transfer(format!("voicemail:{}", target)))
            }
            EntryAction::StartApp {
                app,
                params,
                return_app,
                return_target,
                return_menu,
            } => {
                info!(
                    ivr = %self.definition.name,
                    sub_app = %app,
                    return_app = ?return_app,
                    return_target = ?return_target,
                    "IVR chaining to sub-app"
                );
                let sub = super::exec::prepare_start_app(
                    ctx,
                    app,
                    params.clone(),
                    return_app,
                    return_target,
                    return_menu,
                )
                .await?;
                self.state = IvrState::Done;
                Ok(AppAction::Chain(sub))
            }
            EntryAction::Bridge {
                create_room_uri,
                headers,
                return_app,
                return_target,
                success,
                failure,
                ..
            } => {
                let mut vars: std::collections::HashMap<String, String> = ctx
                    .session_vars
                    .iter()
                    .map(|e| (e.key().clone(), e.value().clone()))
                    .collect();
                for (k, v) in &self.collected_variables {
                    vars.insert(k.clone(), v.clone());
                }
                let mut uri = super::common::substitute_vars(create_room_uri, &vars);
                ctx.session_vars
                    .insert("bridge_room_uri".into(), uri.clone());
                for (k, v) in headers {
                    ctx.session_vars
                        .insert(format!("bridge_hdr_{}", k), v.clone());
                }
                if success.is_some() || failure.is_some() {
                    ctx.session_vars
                        .insert("bridge_branch".into(), "true".into());
                }
                super::exec::append_return_app_to_uri(&mut uri, return_app, return_target);
                let target = format!("bridge:{}", uri);
                info!(ivr = %self.definition.name, target = %target, "IVR bridging to WebSocket endpoint");
                self.ivr_flow_completed(ctx, "transferred", "bridge", Some(target.as_str()))
                    .await;
                self.state = IvrState::Done;
                Ok(AppAction::Transfer(target))
            }
            EntryAction::Play {
                prompt,
                prompt_text,
                prompt_voice,
            } => {
                let return_menu = self.current_menu_key().to_string();
                self.state = IvrState::PlayingAnnouncement {
                    return_menu: return_menu.clone(),
                };
                if let Some(path) = self
                    .resolve_audio(
                        Some(prompt),
                        prompt_text.as_deref(),
                        prompt_voice.as_deref(),
                    )
                    .await
                {
                    info!(ivr = %self.definition.name, prompt = %path, return_menu, "IVR playing announcement");
                    ctrl.play_audio(&path, false).await?;
                    Ok(AppAction::Continue)
                } else {
                    info!(ivr = %self.definition.name, return_menu, "IVR announcement has no audio, returning to menu");
                    return self.enter_menu(&return_menu, ctrl, ctx).await;
                }
            }
            EntryAction::Repeat => {
                let current = self.current_menu_key().to_string();
                info!(ivr = %self.definition.name, menu = %current, "IVR repeating menu");
                self.enter_menu(&current, ctrl, ctx).await
            }
            EntryAction::Exit => {
                self.state = IvrState::Done;
                Ok(AppAction::Exit)
            }
            EntryAction::Hangup {
                prompt,
                prompt_text,
                prompt_voice,
                ..
            } => {
                if let Some(path) = self
                    .resolve_audio(
                        prompt.as_deref(),
                        prompt_text.as_deref(),
                        prompt_voice.as_deref(),
                    )
                    .await
                {
                    self.state = IvrState::PlayingAndHangup { code: None };
                    debug!(ivr = %self.definition.name, prompt = %path, "Playing prompt before hangup");
                    ctrl.play_audio(&path, false).await?;
                    Ok(AppAction::Continue)
                } else {
                    info!(ivr = %self.definition.name, "IVR hanging up");
                    self.ivr_flow_completed(ctx, "hangup", "caller_hangup", None)
                        .await;
                    self.state = IvrState::Done;
                    Ok(AppAction::Hangup {
                        reason: None,
                        code: None,
                    })
                }
            }
            EntryAction::PlayAndHangup {
                prompt,
                prompt_text,
                prompt_voice,
                code,
            } => {
                self.state = IvrState::PlayingAndHangup { code: *code };
                if let Some(path) = self
                    .resolve_audio(
                        prompt.as_deref(),
                        prompt_text.as_deref(),
                        prompt_voice.as_deref(),
                    )
                    .await
                {
                    debug!(ivr = %self.definition.name, prompt = %path, code = ?code, "Playing prompt before hangup with code");
                    ctrl.play_audio(&path, false).await?;
                    Ok(AppAction::Continue)
                } else {
                    // No prompt — hang up immediately with the given code
                    info!(ivr = %self.definition.name, code = ?code, "IVR hanging up immediately with code (no prompt)");
                    self.state = IvrState::Done;
                    Ok(AppAction::Hangup {
                        reason: None,
                        code: *code,
                    })
                }
            }
            EntryAction::CollectExtension {
                prompt,
                prompt_text,
                prompt_voice,
                min_digits,
                max_digits,
                inter_digit_timeout_ms,
            } => {
                self.state = IvrState::CollectingExtension;
                let resolved_prompt = self
                    .resolve_audio(
                        Some(prompt),
                        prompt_text.as_deref(),
                        prompt_voice.as_deref(),
                    )
                    .await;
                debug!(
                    ivr = %self.definition.name,
                    prompt = ?resolved_prompt, min_digits, max_digits, inter_digit_timeout_ms,
                    "Collecting extension digits"
                );

                // Check if we have a pending digit from unknown_key_action
                let initial_digit = self.pending_unknown_digit.take();
                let digits = if let Some(first) = initial_digit {
                    // Already have first digit, collect more if needed
                    if first.len() >= *min_digits {
                        first.clone()
                    } else {
                        let mut combined = first;
                        let more = ctrl
                            .collect_dtmf(DtmfCollectConfig {
                                min_digits: 1,
                                max_digits: max_digits.saturating_sub(combined.len()),
                                timeout: Duration::from_millis(
                                    *inter_digit_timeout_ms * (*max_digits as u64 + 1),
                                ),
                                terminator: Some('#'),
                                play_prompt: resolved_prompt.clone(),
                                inter_digit_timeout: Some(Duration::from_millis(
                                    *inter_digit_timeout_ms,
                                )),
                                initial_digits: String::new(),
                            })
                            .await?;
                        combined.push_str(&more);
                        combined
                    }
                } else {
                    ctrl.collect_dtmf(DtmfCollectConfig {
                        min_digits: *min_digits,
                        max_digits: *max_digits,
                        timeout: Duration::from_millis(
                            *inter_digit_timeout_ms * (*max_digits as u64 + 1),
                        ),
                        terminator: Some('#'),
                        play_prompt: resolved_prompt.clone(),
                        inter_digit_timeout: Some(Duration::from_millis(*inter_digit_timeout_ms)),
                        initial_digits: String::new(),
                    })
                    .await?
                };

                if digits.is_empty() {
                    // No digits collected, go back to current menu
                    let current = self.current_menu_key().to_string();
                    self.enter_menu(&current, ctrl, ctx).await
                } else {
                    info!(ivr = %self.definition.name, extension = %digits, "Transferring to collected extension");
                    self.state = IvrState::Done;
                    Ok(AppAction::Transfer(digits))
                }
            }
            EntryAction::Collect {
                variable,
                prompt,
                prompt_text,
                prompt_voice,
                min_digits,
                max_digits,
                end_key,
                inter_digit_timeout_ms,
            } => {
                debug!(
                    ivr = %self.definition.name,
                    variable, min_digits, max_digits, inter_digit_timeout_ms,
                    "Collecting digits into variable"
                );
                let terminator = end_key.as_ref().and_then(|k| k.chars().next());
                let resolved_prompt = self
                    .resolve_audio(
                        prompt.as_deref(),
                        prompt_text.as_deref(),
                        prompt_voice.as_deref(),
                    )
                    .await;

                // Check if we have a pending digit from unknown_key_action
                let initial_digit = self.pending_unknown_digit.take();
                let digits = if let Some(first) = initial_digit {
                    // Already have first digit, collect more if needed
                    if first.len() >= *min_digits {
                        // Already have enough digits
                        first.clone()
                    } else {
                        // Collect more digits, starting with what we have
                        let mut combined = first;
                        let more = ctrl
                            .collect_dtmf(DtmfCollectConfig {
                                min_digits: 1,
                                max_digits: max_digits.saturating_sub(combined.len()),
                                timeout: Duration::from_millis(
                                    *inter_digit_timeout_ms * (*max_digits as u64 + 1),
                                ),
                                terminator,
                                play_prompt: resolved_prompt.clone(),
                                inter_digit_timeout: Some(Duration::from_millis(
                                    *inter_digit_timeout_ms,
                                )),
                                initial_digits: String::new(),
                            })
                            .await?;
                        combined.push_str(&more);
                        combined
                    }
                } else {
                    ctrl.collect_dtmf(DtmfCollectConfig {
                        min_digits: *min_digits,
                        max_digits: *max_digits,
                        timeout: Duration::from_millis(
                            *inter_digit_timeout_ms * (*max_digits as u64 + 1),
                        ),
                        terminator,
                        play_prompt: resolved_prompt.clone(),
                        inter_digit_timeout: Some(Duration::from_millis(*inter_digit_timeout_ms)),
                        initial_digits: String::new(),
                    })
                    .await?
                };

                if digits.is_empty() {
                    debug!(ivr = %self.definition.name, variable, "No digits collected for variable");
                } else {
                    info!(ivr = %self.definition.name, variable, digits, "Collected digits into variable");
                    self.collected_variables.insert(variable.clone(), digits);
                }

                // Return to current menu after collecting
                let current = self.current_menu_key().to_string();
                self.enter_menu(&current, ctrl, ctx).await
            }
            EntryAction::Webhook {
                url,
                method,
                headers,
                variables,
                timeout,
            } => {
                let method_str = method.as_deref().unwrap_or("POST");
                info!(
                    ivr = %self.definition.name,
                    url, method = method_str,
                    "IVR calling webhook"
                );

                let webhook_response = self
                    .call_webhook(
                        url,
                        method_str,
                        headers,
                        variables.as_deref(),
                        *timeout,
                        ctx,
                    )
                    .await;

                match webhook_response {
                    Ok(response) => {
                        debug!(
                            ivr = %self.definition.name,
                            url,
                            "Webhook responded successfully, executing returned command"
                        );
                        // Convert WebhookResponse into an EntryAction and execute it
                        let derived_action = response.into_entry_action();
                        // Use Box::pin to avoid recursion issues with async fn
                        Box::pin(self.execute_action(&derived_action, ctrl, ctx, None)).await
                    }
                    Err(e) => {
                        error!(
                            ivr = %self.definition.name,
                            url,
                            error = %e,
                            "Webhook call failed, continuing IVR"
                        );
                        // On error, stay in current menu (re-play greeting)
                        let current = self.current_menu_key().to_string();
                        self.enter_menu(&current, ctrl, ctx).await
                    }
                }
            }

            EntryAction::Prompt { .. }
            | EntryAction::DtmfMenu { .. }
            | EntryAction::CollectDtmf { .. }
            | EntryAction::InputPhone { .. }
            | EntryAction::InputVoice { .. }
            | EntryAction::Api { .. }
            | EntryAction::Torecord { .. }
            | EntryAction::RecordStart { .. }
            | EntryAction::RecordStop { .. }
            | EntryAction::JumpIvr { .. }
            | EntryAction::RouteToAgent { .. } => {
                error!(ivr = %self.definition.name, action = ?std::mem::discriminant(action),
                    "Tree mode IVR received unsupported step-mode action");
                Err(anyhow::anyhow!("unsupported action type for tree mode"))
            }
        }
    }

    /// Call an external webhook and return the parsed [`WebhookResponse`].
    ///
    /// The request body (for POST) or query params (for GET) include the
    /// current call context so that the webhook can make routing decisions.
    async fn call_webhook(
        &self,
        url: &str,
        method: &str,
        headers: &std::collections::HashMap<String, String>,
        variables_filter: Option<&str>,
        timeout_secs: u64,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<WebhookResponse> {
        // Filter variables if a filter is specified
        let filtered_vars = if let Some(filter) = variables_filter {
            let filter_set: std::collections::HashSet<&str> = filter
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            self.collected_variables
                .iter()
                .filter(|(k, _)| filter_set.contains(k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            self.collected_variables.clone()
        };

        // Build the request with custom headers.
        // For GET: send context as query params to avoid a JSON body.
        // For POST (and everything else): serialize the full payload as JSON.
        let req_builder = if method.eq_ignore_ascii_case("GET") {
            let mut params = vec![
                ("session_id", ctx.call_info.session_id.as_str()),
                ("caller", ctx.call_info.caller.as_str()),
                ("callee", ctx.call_info.callee.as_str()),
                ("direction", ctx.call_info.direction.as_str()),
                ("ivr_name", self.definition.name.as_str()),
                ("menu", self.current_menu_key()),
            ];
            if let Some(digit) = self.last_dtmf_digit.as_deref() {
                params.push(("digit", digit));
            }
            // Add collected variables as query params
            for (k, v) in &filtered_vars {
                params.push((k, v));
            }
            ctx.http_client.get(url).query(&params)
        } else {
            let payload = WebhookPayload {
                session_id: ctx.call_info.session_id.clone(),
                caller: ctx.call_info.caller.clone(),
                callee: ctx.call_info.callee.clone(),
                direction: ctx.call_info.direction.clone(),
                ivr_name: self.definition.name.clone(),
                menu: self.current_menu_key().to_string(),
                variables: filtered_vars,
                digit: self.last_dtmf_digit.clone(),
            };
            ctx.http_client.post(url).json(&payload)
        };

        let response = crate::http_util::execute_request(
            req_builder,
            headers,
            Some(Duration::from_secs(timeout_secs)),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Webhook request failed: {}", e))?;

        let webhook_response: WebhookResponse = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse webhook response: {}", e))?;

        Ok(webhook_response)
    }

    /// Handle timeout: either retry or execute timeout_action.
    async fn handle_timeout(
        &mut self,
        menu_key: String,
        retry_count: u32,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // Extract only what we need before any mutable borrow of self.
        let (max_retries, timeout_action, max_retries_action, menu) = {
            let menu = self
                .definition
                .get_menu(&menu_key)
                .ok_or_else(|| anyhow::anyhow!("menu '{}' not found", menu_key))?;
            (
                menu.max_retries,
                menu.timeout_action.clone(),
                menu.max_retries_action.clone(),
                menu.clone(),
            )
        };

        let new_retry = retry_count + 1;
        if new_retry > max_retries {
            if let Some(action) = max_retries_action {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    retries = new_retry,
                    "IVR max retries exceeded (timeout), executing fallback action"
                );
                return self.execute_action(&action, ctrl, ctx, None).await;
            } else {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    retries = new_retry,
                    "IVR max retries exceeded (timeout), no fallback — hanging up"
                );
                self.state = IvrState::Done;
                return Ok(AppAction::Hangup {
                    reason: None,
                    code: None,
                });
            }
        }

        // Retry: check timeout_action
        if let Some(action) = timeout_action {
            match action {
                EntryAction::Repeat => {
                    info!(
                        ivr = %self.definition.name,
                        menu = %menu_key,
                        retry = new_retry,
                        "IVR timeout: repeating menu"
                    );
                    self.state = IvrState::PlayingGreeting {
                        menu_key: menu_key.clone(),
                    };
                    self.pending_retry_count = new_retry;
                    if let Some(path) = self
                        .resolve_audio(
                            Some(&menu.greeting),
                            menu.greeting_text.as_deref(),
                            menu.greeting_voice.as_deref(),
                        )
                        .await
                    {
                        ctrl.play_audio(&path, false).await?;
                    } else {
                        self.start_waiting_dtmf(&menu_key, new_retry, ctrl);
                    }
                    Ok(AppAction::Continue)
                }
                other => self.execute_action(&other, ctrl, ctx, None).await,
            }
        } else {
            // No timeout_action defined; replay the greeting
            info!(
                ivr = %self.definition.name,
                menu = %menu_key,
                retry = new_retry,
                "IVR timeout: replaying greeting (default)"
            );
            self.state = IvrState::PlayingGreeting {
                menu_key: menu_key.clone(),
            };
            self.pending_retry_count = new_retry;
            if let Some(path) = self
                .resolve_audio(
                    Some(&menu.greeting),
                    menu.greeting_text.as_deref(),
                    menu.greeting_voice.as_deref(),
                )
                .await
            {
                ctrl.play_audio(&path, false).await?;
            } else {
                self.start_waiting_dtmf(&menu_key, new_retry, ctrl);
            }
            Ok(AppAction::Continue)
        }
    }

    /// Handle an invalid DTMF key press.
    async fn handle_invalid_key(
        &mut self,
        menu_key: &str,
        retry_count: u32,
        digit: &str,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        ctrl.cancel_timeout("ivr_dtmf_timeout");

        // Extract only what we need before any mutable borrow of self.
        let (max_retries, max_retries_action, invalid_prompt, invalid_text, invalid_voice) = {
            let menu = self
                .definition
                .get_menu(menu_key)
                .ok_or_else(|| anyhow::anyhow!("menu '{}' not found", menu_key))?;
            (
                menu.max_retries,
                menu.max_retries_action.clone(),
                menu.invalid_prompt.clone(),
                menu.invalid_text.clone(),
                menu.invalid_voice.clone(),
            )
        };

        let new_retry = retry_count + 1;
        info!(
            ivr = %self.definition.name,
            menu = menu_key,
            digit = %digit,
            retry = new_retry,
            max_retries,
            "IVR invalid DTMF key"
        );

        if new_retry > max_retries {
            if let Some(action) = max_retries_action {
                info!(
                    ivr = %self.definition.name,
                    menu = menu_key,
                    retries = new_retry,
                    "IVR max retries exceeded after invalid key, executing fallback"
                );
                return self.execute_action(&action, ctrl, ctx, None).await;
            } else {
                info!(
                    ivr = %self.definition.name,
                    menu = menu_key,
                    retries = new_retry,
                    "IVR max retries exceeded after invalid key, hanging up"
                );
                self.state = IvrState::Done;
                return Ok(AppAction::Hangup {
                    reason: None,
                    code: None,
                });
            }
        }

        if let Some(path) = self
            .resolve_audio(
                invalid_prompt.as_deref(),
                invalid_text.as_deref(),
                invalid_voice.as_deref(),
            )
            .await
        {
            info!(
                ivr = %self.definition.name,
                menu = menu_key,
                "IVR playing invalid prompt"
            );
            self.state = IvrState::PlayingInvalid {
                menu_key: menu_key.to_string(),
                retry_count: new_retry,
            };
            ctrl.play_audio(&path, false).await?;
            Ok(AppAction::Continue)
        } else {
            // No invalid prompt — just go back to waiting
            info!(
                ivr = %self.definition.name,
                menu = menu_key,
                retry = new_retry,
                "IVR no invalid prompt, returning to wait DTMF"
            );
            self.start_waiting_dtmf(menu_key, new_retry, ctrl);
            Ok(AppAction::Continue)
        }
    }
}

#[async_trait]
impl CallApp for IvrApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Ivr
    }

    fn name(&self) -> &str {
        &self.definition.name
    }

    async fn on_enter(
        &mut self,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!(ivr = %self.definition.name, "IVR application started");
        // Stash references for `on_exit`, which receives no context.
        self.runtime_vars = Some(ctx.session_vars.clone());
        self.rwi_gateway = ctx.rwi_gateway.clone();
        self.session_id = Some(ctx.call_info.session_id.clone());
        self.session_extensions = Some(ctx.session_extensions.clone());
        ctrl.answer().await?;

        // Check business hours
        let closed_action = if let Some(bh) = &self.definition.business_hours {
            if bh.enabled && !self.is_within_business_hours(bh) {
                info!(ivr = %self.definition.name, "Outside business hours");
                if let Some(path) = self
                    .resolve_audio(
                        bh.closed_greeting.as_deref(),
                        bh.closed_text.as_deref(),
                        None,
                    )
                    .await
                {
                    self.state = IvrState::PlayingHangup;
                    ctrl.play_audio(&path, false).await?;
                    // After playing, the on_audio_complete will handle closed_action
                    return Ok(AppAction::Continue);
                }
                Some(bh.closed_action.clone())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(action) = closed_action {
            if let Some(action) = action {
                return self.execute_action(&action, ctrl, ctx, None).await;
            }
            // Default: hang up
            self.state = IvrState::Done;
            return Ok(AppAction::Hangup {
                reason: Some(CallRecordHangupReason::Other("closed".to_string())),
                code: None,
            });
        }

        let start = self
            .start_menu
            .clone()
            .unwrap_or_else(|| "root".to_string());
        if self.definition.get_menu(&start).is_some() {
            self.enter_menu(&start, ctrl, ctx).await
        } else {
            self.enter_menu("root", ctrl, ctx).await
        }
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // Extract state data we need before the mutable calls below.
        // We only clone the String fields, not the full IvrState enum.
        let state_snapshot = match &self.state {
            IvrState::WaitingDtmf {
                menu_key,
                retry_count,
            } => Some((*retry_count, menu_key.clone(), false)),
            IvrState::PlayingGreeting { menu_key } => Some((0, menu_key.clone(), true)),
            _ => None,
        };

        let Some((retry_count, menu_key, is_greeting)) = state_snapshot else {
            // DTMF in other states is ignored
            info!(
                ivr = %self.definition.name,
                digit,
                state = ?self.state,
                "IVR DTMF ignored in current state"
            );
            return Ok(AppAction::Continue);
        };

        if is_greeting {
            // DTMF during greeting — barge-in if key is mapped
            let action = self
                .definition
                .get_menu(&menu_key)
                .and_then(|m| m.entries.iter().find(|e| e.key == digit))
                .map(|e| e.action.clone());

            if let Some(action) = action {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    digit = %digit,
                    "IVR DTMF barge-in during greeting"
                );
                ctrl.cancel_timeout("ivr_dtmf_timeout");
                let _ = ctrl.stop_audio().await;
                self.last_dtmf_digit = Some(digit.clone());
                self.execute_action(&action, ctrl, ctx, Some(&digit)).await
            } else {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    digit = %digit,
                    "IVR DTMF ignored during greeting (no matching entry)"
                );
                Ok(AppAction::Continue)
            }
        } else {
            // WaitingDtmf — look up the entry for this digit
            let entry_action = self
                .definition
                .get_menu(&menu_key)
                .and_then(|m| m.entries.iter().find(|e| e.key == digit))
                .map(|e| {
                    debug!(
                        ivr = %self.definition.name,
                        menu = %menu_key,
                        digit = %digit,
                        label = e.label.as_deref().unwrap_or(""),
                        "DTMF matched"
                    );
                    e.action.clone()
                });

            if let Some(action) = entry_action {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    digit = %digit,
                    "IVR DTMF matched entry, executing action"
                );
                self.last_dtmf_digit = Some(digit.clone());
                self.execute_action(&action, ctrl, ctx, Some(&digit)).await
            } else if let Some(menu) = self.definition.get_menu(&menu_key) {
                // Check for unknown_key_action (e.g., direct extension dial)
                let unknown_action = menu.unknown_key_action.clone();
                if let Some(unknown_action) = unknown_action {
                    info!(
                        ivr = %self.definition.name,
                        menu = %menu_key,
                        digit = %digit,
                        "IVR DTMF not matched, executing unknown_key_action"
                    );
                    // Store the first digit for Collect actions
                    self.pending_unknown_digit = Some(digit.to_string());
                    self.last_dtmf_digit = Some(digit.clone());
                    self.execute_action(&unknown_action, ctrl, ctx, Some(&digit))
                        .await
                } else {
                    info!(
                        ivr = %self.definition.name,
                        menu = %menu_key,
                        digit = %digit,
                        "IVR DTMF invalid key"
                    );
                    self.handle_invalid_key(&menu_key, retry_count, &digit, ctrl, ctx)
                        .await
                }
            } else {
                warn!(ivr = %self.definition.name, menu = %menu_key, "Menu not found during DTMF handling");
                // The menu is being abandoned (hangup): suppress the pending
                // ivr_dtmf_timeout so it doesn't fire against the now-dead flow.
                ctrl.cancel_timeout("ivr_dtmf_timeout");
                self.state = IvrState::Done;
                Ok(AppAction::Hangup {
                    reason: None,
                    code: None,
                })
            }
        }
    }

    async fn on_audio_complete(
        &mut self,
        _track_id: String,
        ctrl: &mut CallController,
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        // Extract string fields we need before the mutable borrows below.
        enum AudioDone {
            Greeting { menu_key: String },
            Invalid { menu_key: String, retry_count: u32 },
            Announcement { return_menu: String },
            Hangup,
            AndHangup { code: Option<u16> },
            Other,
        }

        let done = match &self.state {
            IvrState::PlayingGreeting { menu_key } => AudioDone::Greeting {
                menu_key: menu_key.clone(),
            },
            IvrState::PlayingInvalid {
                menu_key,
                retry_count,
            } => AudioDone::Invalid {
                menu_key: menu_key.clone(),
                retry_count: *retry_count,
            },
            IvrState::PlayingAnnouncement { return_menu } => AudioDone::Announcement {
                return_menu: return_menu.clone(),
            },
            IvrState::PlayingHangup => AudioDone::Hangup,
            IvrState::PlayingAndHangup { code } => AudioDone::AndHangup { code: *code },
            _ => AudioDone::Other,
        };

        match done {
            AudioDone::Greeting { menu_key } => {
                let retry_count = self.pending_retry_count;
                self.pending_retry_count = 0;
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    retry_count,
                    "IVR greeting complete, waiting DTMF"
                );
                self.start_waiting_dtmf(&menu_key, retry_count, ctrl);
                Ok(AppAction::Continue)
            }
            AudioDone::Invalid {
                menu_key,
                retry_count,
            } => {
                // Invalid prompt finished → re-play greeting
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    retry_count,
                    "IVR invalid prompt complete, replaying greeting"
                );
                let menu = self.definition.get_menu(&menu_key).cloned();
                if let Some(menu) = menu {
                    self.state = IvrState::PlayingGreeting {
                        menu_key: menu_key.clone(),
                    };
                    self.pending_retry_count = retry_count;
                    if let Some(path) = self
                        .resolve_audio(
                            Some(&menu.greeting),
                            menu.greeting_text.as_deref(),
                            menu.greeting_voice.as_deref(),
                        )
                        .await
                    {
                        ctrl.play_audio(&path, false).await?;
                    } else {
                        self.start_waiting_dtmf(&menu_key, retry_count, ctrl);
                    }
                }
                Ok(AppAction::Continue)
            }
            AudioDone::Announcement { return_menu } => {
                info!(
                    ivr = %self.definition.name,
                    return_menu = %return_menu,
                    "IVR announcement complete, returning to menu"
                );
                self.enter_menu(&return_menu, ctrl, _ctx).await
            }
            AudioDone::Hangup => {
                info!(ivr = %self.definition.name, "IVR hangup prompt complete, hanging up");
                self.state = IvrState::Done;
                Ok(AppAction::Hangup {
                    reason: None,
                    code: None,
                })
            }
            AudioDone::AndHangup { code } => {
                info!(ivr = %self.definition.name, code = ?code, "IVR prompt complete, hanging up with code");
                self.state = IvrState::Done;
                Ok(AppAction::Hangup { reason: None, code })
            }
            AudioDone::Other => Ok(AppAction::Continue),
        }
    }

    async fn on_timeout(
        &mut self,
        timeout_id: String,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if timeout_id != "ivr_dtmf_timeout" {
            return Ok(AppAction::Continue);
        }

        let waiting = match &self.state {
            IvrState::WaitingDtmf {
                menu_key,
                retry_count,
            } => Some((menu_key.clone(), *retry_count)),
            _ => None,
        };

        if let Some((menu_key, retry_count)) = waiting {
            info!(
                ivr = %self.definition.name,
                menu = %menu_key,
                retry_count,
                "IVR DTMF timeout fired"
            );
            self.handle_timeout(menu_key, retry_count, ctrl, ctx).await
        } else {
            Ok(AppAction::Continue)
        }
    }

    async fn on_exit(&mut self, reason: crate::call::app::ExitReason) -> anyhow::Result<()> {
        // A terminal action already emitted IvrFlowCompleted (transfer, queue,
        // voicemail, deliberate hangup) — nothing to report here.
        if self.flow_completed {
            return Ok(());
        }

        let end_reason_label = match reason {
            crate::call::app::ExitReason::Normal => "normal",
            crate::call::app::ExitReason::Hangup => "hangup",
            crate::call::app::ExitReason::RemoteHangup(_) => "remote_hangup",
            crate::call::app::ExitReason::Transferred => "transferred",
            crate::call::app::ExitReason::Chained => "chained",
            crate::call::app::ExitReason::Cancelled => "cancelled",
            crate::call::app::ExitReason::Error(_) => "error",
        };

        // The node the caller was on when the session terminated.
        let menu_key = match &self.state {
            IvrState::PlayingGreeting { menu_key }
            | IvrState::WaitingDtmf { menu_key, .. }
            | IvrState::PlayingInvalid { menu_key, .. } => menu_key.clone(),
            _ => self.current_menu_key().to_string(),
        };
        let duration_ms = self
            .node_entered_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let total_duration_ms = self
            .flow_started_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let call_id = self.session_id.clone().unwrap_or_default();
        let completion_time = chrono::Utc::now().to_rfc3339();

        info!(
            ivr = %self.definition.name,
            node = %menu_key,
            end_reason = end_reason_label,
            "IVR flow ended by session termination"
        );

        // Record the node the caller was on when the session terminated.
        if let Some(ref gw) = self.rwi_gateway {
            let gw = gw.clone();
            let cid = call_id.clone();
            let ev = crate::rwi::IvrNodeExited {
                call_id: cid.clone(),
                node_id: menu_key.clone(),
                node_name: menu_key.clone(),
                result_value: None,
                duration_ms: duration_ms as u32,
                exit_time: completion_time.clone(),
                next_node_id: None,
                hangup_reason: Some(end_reason_label.to_string()),
                call_result: Some("hangup".to_string()),
                extra: None,
            };
            crate::utils::spawn(async move {
                let guard = gw.read();
                guard.fan_out(&cid, &ev);
            });
        }

        // Report the whole flow as aborted (not completed via a terminal action).
        if let Some(ref gw) = self.rwi_gateway {
            let gw = gw.clone();
            let cid = call_id.clone();
            let ev = crate::rwi::IvrFlowCompleted {
                call_id: cid.clone(),
                app_id: self.definition.name.clone(),
                total_nodes_traversed: self.nodes_traversed,
                total_duration_ms: total_duration_ms as u32,
                final_result: end_reason_label.to_string(),
                completion_time,
                final_routing_target: None,
                extra: None,
            };
            crate::utils::spawn(async move {
                let guard = gw.read();
                guard.fan_out(&cid, &ev);
            });
        }

        super::exec::publish_ivr_end_reason(
            self.runtime_vars.as_ref(),
            end_reason_label,
            &self.definition.name,
        );

        // If started via ivr.exec and no terminal action already reported the result.
        if !self.flow_completed {
            if let Some(ref ext) = self.session_extensions {
                super::exec::write_ivr_exec_result(
                    ext,
                    super::exec::build_ivr_exec_result(
                        end_reason_label,
                        end_reason_label,
                        None,
                        self.collected_variables.clone(),
                        total_duration_ms,
                    ),
                );
            }
        }

        self.flow_completed = true;
        self.state = IvrState::Done;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::app::ivr::config::{MenuEntry, MenuNode};
    use crate::call::app::testing::MockCallStack;
    use crate::call::app::{CallInfo, ControllerEvent, ExitReason};
    use crate::config::Config;
    use crate::proxy::proxy_call::sip_session::SipSessionHandle;
    use sea_orm::DatabaseConnection;
    use std::collections::HashMap;

    fn test_definition() -> IvrDefinition {
        let mut def = IvrDefinition {
            name: "test-ivr".to_string(),
            description: None,
            lang: None,
            default_voice: None,
            dynamic_build: false,
            ivr_mode: None,
            provider: None,
            business_hours: None,
            tts: None,
            root: Some(MenuNode::default()),
            menus: HashMap::new(),
        };
        def.menus.insert("root".to_string(), MenuNode::default());
        def
    }

    fn test_context() -> ApplicationContext {
        ApplicationContext::new(
            DatabaseConnection::default(),
            CallInfo {
                session_id: "test-session".into(),
                caller: "1001".into(),
                callee: "2000".into(),
                direction: "inbound".into(),
                started_at: chrono::Utc::now(),
                sip_headers: HashMap::new(),
                route_name: None,
            },
            Arc::new(Config::default()),
        )
    }

    #[tokio::test]
    async fn test_session_termination_records_node_and_emits_events() {
        use crate::rwi::gateway::EventCacheEntry;

        let mut ctx = test_context();
        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        ctx.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        let mut stack =
            MockCallStack::run_with_context(Box::new(IvrApp::new(test_definition())), ctx.clone());
        stack.enter().await;

        // Simulate sip_session termination: the event loop's cancel token fires
        // and drives IvrApp::on_exit(ExitReason::Cancelled).
        stack.cancel();
        stack.join().await.expect("cancel should stop app");

        assert_eq!(
            ctx.get_var("ivr_end_reason").as_deref(),
            Some("cancelled"),
            "ivr_end_reason must be published for the session"
        );
        assert_eq!(
            ctx.get_var("ivr_status").as_deref(),
            Some("cancelled"),
            "ivr_status must be published for the session"
        );

        let mut saw_node_exited = false;
        let mut saw_flow_completed = false;
        for _ in 0..20 {
            while let Ok(entry) = rx.try_recv() {
                match entry.event.event_type {
                    "ivr_node_exited" => saw_node_exited = true,
                    "ivr_flow_completed" => saw_flow_completed = true,
                    _ => {}
                }
            }
            if saw_node_exited && saw_flow_completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert!(
            saw_node_exited,
            "session termination must emit ivr_node_exited"
        );
        assert!(
            saw_flow_completed,
            "session termination must emit ivr_flow_completed"
        );
    }

    #[tokio::test]
    async fn test_remote_hangup_reason_label() {
        let mut ctx = test_context();
        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, _rx) = tokio::sync::broadcast::channel::<crate::rwi::gateway::EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        ctx.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        let mut stack =
            MockCallStack::run_with_context(Box::new(IvrApp::new(test_definition())), ctx.clone());
        stack.enter().await;

        // A remote hangup pushes ControllerEvent::Hangup → on_exit(RemoteHangup).
        stack.remote_hangup();
        stack.join().await.expect("remote hangup should stop app");

        assert_eq!(
            ctx.get_var("ivr_end_reason").as_deref(),
            Some("remote_hangup"),
            "remote hangup must publish remote_hangup end reason"
        );
    }

    #[tokio::test]
    async fn test_flow_completed_guard_skips_duplicate_report() {
        let mut app = IvrApp::new(test_definition());
        app.flow_completed = true;
        app.state = IvrState::WaitingDtmf {
            menu_key: "root".to_string(),
            retry_count: 0,
        };
        app.session_id = Some("test-session".to_string());
        let vars = Arc::new(dashmap::DashMap::new());
        app.runtime_vars = Some(vars.clone());
        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, _rx) = tokio::sync::broadcast::channel::<crate::rwi::gateway::EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        app.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        app.on_exit(ExitReason::Cancelled)
            .await
            .expect("on_exit should succeed");

        assert!(
            vars.get("ivr_end_reason").is_none(),
            "must not re-publish end reason when flow already completed"
        );
    }

    /// DTMF arriving while in WaitingDtmf for a menu that no longer exists in
    /// the definition must hang up cleanly. The pending `ivr_dtmf_timeout` is
    /// cancelled so it can't fire into the now-dead flow (regression guard for
    /// the timeout-lifecycle bug).
    #[tokio::test]
    async fn test_dtmf_in_unknown_menu_hangs_up_cleanly() {
        let mut app = IvrApp::new(test_definition());
        app.state = IvrState::WaitingDtmf {
            menu_key: "missing".to_string(),
            retry_count: 0,
        };

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
        let handle = SipSessionHandle::new_for_test("test-session", cmd_tx);
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<ControllerEvent>();
        let (controller, _timer_rx) = CallController::new(handle, event_rx);
        let mut controller = controller;
        let ctx = test_context();

        let action = app
            .on_dtmf("1".to_string(), &mut controller, &ctx)
            .await
            .expect("on_dtmf must not error");
        assert!(
            matches!(action, AppAction::Hangup { .. }),
            "DTMF in an unknown menu must hang up, got {action:?}"
        );
        assert!(
            matches!(app.state, IvrState::Done),
            "unknown-menu hangup must set state to Done"
        );
    }

    #[tokio::test]
    async fn test_dtmf_match_emits_digit_as_result_value() {
        use crate::rwi::gateway::EventCacheEntry;

        let mut def = test_definition();
        def.root = Some(MenuNode {
            greeting: String::new(),
            greeting_text: None,
            entries: vec![MenuEntry {
                key: "1".into(),
                label: Some("agent".into()),
                action: EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                },
            }],
            ..MenuNode::default()
        });

        let mut ctx = test_context();
        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        ctx.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        let mut stack = MockCallStack::run_with_context(Box::new(IvrApp::new(def)), ctx.clone());
        stack.enter().await;
        stack
            .assert_cmd(200, "accept", |c| {
                matches!(c, crate::call::domain::CallCommand::Answer { .. })
            })
            .await;

        // Empty greeting file + failed TTS → wait for DTMF immediately.
        let _ = stack.drain_cmds();
        std::thread::sleep(std::time::Duration::from_millis(50));
        stack.dtmf("1");
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, crate::call::domain::CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let mut result_value = None;
        let mut action_type = None;
        for _ in 0..20 {
            while let Ok(entry) = rx.try_recv() {
                if entry.event.event_type == "ivr_node_exited" {
                    result_value = entry
                        .event
                        .payload
                        .get("result_value")
                        .and_then(|v| v.as_str().map(|s| s.to_string()));
                    action_type = entry
                        .event
                        .payload
                        .get("extra")
                        .and_then(|v| v.get("action_type"))
                        .and_then(|v| v.as_str().map(|s| s.to_string()));
                }
            }
            if result_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(
            result_value.as_deref(),
            Some("1"),
            "ivr_node_exited.result_value must be the pressed digit"
        );
        assert_eq!(
            action_type.as_deref(),
            Some("transfer"),
            "ivr_node_exited.extra.action_type must keep the action kind"
        );
    }

    #[tokio::test]
    async fn test_dtmf_bargein_during_greeting_emits_digit() {
        use crate::rwi::gateway::EventCacheEntry;

        let mut def = test_definition();
        def.root = Some(MenuNode {
            greeting: "sounds/welcome.wav".into(),
            greeting_text: Some("请按1转坐席".into()),
            entries: vec![MenuEntry {
                key: "1".into(),
                label: Some("agent".into()),
                action: EntryAction::Transfer {
                    target: "2001".into(),
                    params: HashMap::new(),
                    return_app: None,
                    return_target: None,
                },
            }],
            ..MenuNode::default()
        });

        let mut ctx = test_context();
        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        ctx.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        let mut stack = MockCallStack::run_with_context(Box::new(IvrApp::new(def)), ctx.clone());
        stack.enter().await;
        stack
            .assert_cmd(200, "accept", |c| {
                matches!(c, crate::call::domain::CallCommand::Answer { .. })
            })
            .await;
        stack
            .assert_cmd(200, "play", |c| {
                matches!(c, crate::call::domain::CallCommand::Play { .. })
            })
            .await;

        stack.dtmf("1");
        stack
            .assert_cmd(200, "stop", |c| {
                matches!(c, crate::call::domain::CallCommand::StopPlayback { .. })
            })
            .await;
        stack
            .assert_cmd(
                200,
                "transfer",
                |c| matches!(c, crate::call::domain::CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let mut result_value = None;
        for _ in 0..20 {
            while let Ok(entry) = rx.try_recv() {
                if entry.event.event_type == "ivr_node_exited" {
                    result_value = entry
                        .event
                        .payload
                        .get("result_value")
                        .and_then(|v| v.as_str().map(|s| s.to_string()));
                }
            }
            if result_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(
            result_value.as_deref(),
            Some("1"),
            "barge-in during TTS greeting must still emit the digit"
        );
    }

    #[tokio::test]
    async fn test_timeout_without_digit_keeps_action_type() {
        use crate::rwi::gateway::EventCacheEntry;

        let mut def = test_definition();
        def.root = Some(MenuNode {
            greeting: String::new(),
            timeout_ms: 5000,
            max_retries: 1,
            timeout_action: Some(EntryAction::Transfer {
                target: "2001".into(),
                params: HashMap::new(),
                return_app: None,
                return_target: None,
            }),
            entries: vec![],
            ..MenuNode::default()
        });

        let mut ctx = test_context();
        let mut gw = crate::rwi::gateway::RwiGateway::new();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);
        ctx.rwi_gateway = Some(Arc::new(parking_lot::RwLock::new(gw)));

        let mut stack = MockCallStack::run_with_context(Box::new(IvrApp::new(def)), ctx.clone());
        stack.enter().await;
        stack
            .assert_cmd(200, "accept", |c| {
                matches!(c, crate::call::domain::CallCommand::Answer { .. })
            })
            .await;
        stack.timeout("ivr_dtmf_timeout");
        stack
            .assert_cmd(
                500,
                "transfer",
                |c| matches!(c, crate::call::domain::CallCommand::Transfer { target, .. } if target == "2001"),
            )
            .await;

        let mut result_value = None;
        for _ in 0..20 {
            while let Ok(entry) = rx.try_recv() {
                if entry.event.event_type == "ivr_node_exited" {
                    result_value = entry
                        .event
                        .payload
                        .get("result_value")
                        .and_then(|v| v.as_str().map(|s| s.to_string()));
                }
            }
            if result_value.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(
            result_value.as_deref(),
            Some("transfer"),
            "timeout exit without a keypress must keep action type as result_value"
        );
    }
}
