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

use super::{
    AppAction, ApplicationContext, CallApp, CallAppType, CallController, DtmfCollectConfig,
};
use crate::call::app::ivr_config::{EntryAction, IvrDefinition, WebhookResponse};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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
}

impl IvrApp {
    /// Create a new `IvrApp` from a parsed [`IvrDefinition`].
    pub fn new(definition: IvrDefinition) -> Self {
        Self {
            definition,
            state: IvrState::Init,
            menu_stack: vec!["root".to_string()],
            pending_retry_count: 0,
        }
    }

    /// Load an `IvrApp` from a TOML file path.
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read IVR config '{}': {}", path, e))?;
        let file_config: crate::call::app::ivr_config::IvrFileConfig = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse IVR config '{}': {}", path, e))?;
        file_config
            .ivr
            .validate()
            .map_err(|e| anyhow::anyhow!("IVR config validation failed '{}': {}", path, e))?;
        Ok(Self::new(file_config.ivr))
    }

    /// Get the current menu key (top of stack).
    fn current_menu_key(&self) -> &str {
        self.menu_stack.last().map(|s| s.as_str()).unwrap_or("root")
    }

    /// Navigate to a menu. If `"root"`, reset the stack. Otherwise push only
    /// if the menu is not already the current top (avoids unbounded growth on Repeat).
    fn navigate_to_menu(&mut self, menu_key: &str) {
        if menu_key == "root" {
            self.menu_stack.clear();
            self.menu_stack.push("root".to_string());
        } else if self.current_menu_key() != menu_key {
            self.menu_stack.push(menu_key.to_string());
        }
        // If already on this menu (e.g. Repeat), keep the stack as-is.
    }

    /// Start playing the greeting for the specified menu.
    async fn enter_menu(
        &mut self,
        menu_key: &str,
        ctrl: &mut CallController,
    ) -> anyhow::Result<AppAction> {
        self.navigate_to_menu(menu_key);
        let menu = self
            .definition
            .get_menu(menu_key)
            .ok_or_else(|| anyhow::anyhow!("IVR menu '{}' not found", menu_key))?;
        let greeting = menu.greeting.clone();
        self.state = IvrState::PlayingGreeting {
            menu_key: menu_key.to_string(),
        };
        debug!(ivr = %self.definition.name, menu = menu_key, "Playing greeting: {}", greeting);
        ctrl.play_audio(&greeting, false).await?;
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
        debug!(
            ivr = %self.definition.name,
            menu = menu_key,
            retry_count,
            timeout_ms,
            "Waiting for DTMF input"
        );
    }

    /// Execute an action from a DTMF press or timeout/max-retries fallback.
    async fn execute_action(
        &mut self,
        action: &EntryAction,
        ctrl: &mut CallController,
        ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        ctrl.cancel_timeout("ivr_dtmf_timeout");
        match action {
            EntryAction::Transfer { target } => {
                info!(ivr = %self.definition.name, target, "IVR transferring call");
                self.state = IvrState::Done;
                Ok(AppAction::Transfer(target.clone()))
            }
            EntryAction::Queue { target } => {
                info!(ivr = %self.definition.name, queue = target, "IVR sending to queue");
                self.state = IvrState::Done;
                // Queue is handled as a transfer to the queue endpoint
                Ok(AppAction::Transfer(format!("queue:{}", target)))
            }
            EntryAction::Menu { menu } => {
                debug!(ivr = %self.definition.name, menu, "IVR navigating to menu");
                self.enter_menu(menu, ctrl).await
            }
            EntryAction::Voicemail { target } => {
                info!(ivr = %self.definition.name, target, "IVR transferring to voicemail");
                self.state = IvrState::Done;
                Ok(AppAction::Transfer(format!("voicemail:{}", target)))
            }
            EntryAction::Play { prompt } => {
                let return_menu = self.current_menu_key().to_string();
                self.state = IvrState::PlayingAnnouncement {
                    return_menu: return_menu.clone(),
                };
                debug!(ivr = %self.definition.name, prompt, return_menu, "Playing announcement");
                ctrl.play_audio(prompt, false).await?;
                Ok(AppAction::Continue)
            }
            EntryAction::Repeat => {
                let current = self.current_menu_key().to_string();
                debug!(ivr = %self.definition.name, menu = %current, "Repeating menu");
                self.enter_menu(&current, ctrl).await
            }
            EntryAction::Hangup { prompt } => {
                if let Some(prompt) = prompt {
                    self.state = IvrState::PlayingHangup;
                    debug!(ivr = %self.definition.name, prompt, "Playing hangup prompt");
                    ctrl.play_audio(prompt, false).await?;
                    Ok(AppAction::Continue)
                } else {
                    info!(ivr = %self.definition.name, "IVR hanging up");
                    self.state = IvrState::Done;
                    Ok(AppAction::Hangup { reason: None, code: None })
                }
            }
            EntryAction::PlayAndHangup { prompt, code } => {
                self.state = IvrState::PlayingAndHangup { code: *code };
                if let Some(prompt) = prompt {
                    debug!(ivr = %self.definition.name, prompt, code = ?code, "Playing prompt before hangup with code");
                    ctrl.play_audio(prompt, false).await?;
                    Ok(AppAction::Continue)
                } else {
                    // No prompt — hang up immediately with the given code
                    info!(ivr = %self.definition.name, code = ?code, "IVR hanging up immediately with code (no prompt)");
                    self.state = IvrState::Done;
                    Ok(AppAction::Hangup { reason: None, code: *code })
                }
            }
            EntryAction::CollectExtension {
                prompt,
                min_digits,
                max_digits,
                inter_digit_timeout_ms,
            } => {
                self.state = IvrState::CollectingExtension;
                debug!(
                    ivr = %self.definition.name,
                    prompt, min_digits, max_digits, inter_digit_timeout_ms,
                    "Collecting extension digits"
                );
                let digits = ctrl
                    .collect_dtmf(DtmfCollectConfig {
                        min_digits: *min_digits,
                        max_digits: *max_digits,
                        timeout: Duration::from_millis(
                            *inter_digit_timeout_ms * (*max_digits as u64 + 1),
                        ),
                        terminator: Some('#'),
                        play_prompt: Some(prompt.clone()),
                        inter_digit_timeout: Some(Duration::from_millis(*inter_digit_timeout_ms)),
                    })
                    .await?;

                if digits.is_empty() {
                    // No digits collected, go back to current menu
                    let current = self.current_menu_key().to_string();
                    self.enter_menu(&current, ctrl).await
                } else {
                    info!(ivr = %self.definition.name, extension = %digits, "Transferring to collected extension");
                    self.state = IvrState::Done;
                    Ok(AppAction::Transfer(digits))
                }
            }
            EntryAction::Webhook { url, method, headers } => {
                let method_str = method.as_deref().unwrap_or("POST");
                info!(
                    ivr = %self.definition.name,
                    url, method = method_str,
                    "IVR calling webhook"
                );

                let webhook_response = self
                    .call_webhook(url, method_str, headers, ctx)
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
                        Box::pin(self.execute_action(&derived_action, ctrl, ctx)).await
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
                        self.enter_menu(&current, ctrl).await
                    }
                }
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
        ctx: &ApplicationContext,
    ) -> anyhow::Result<WebhookResponse> {
        // Build the request with custom headers.
        // For GET: send context as query params to avoid a JSON body.
        // For POST (and everything else): serialize the full payload as JSON.
        let mut req_builder = if method.eq_ignore_ascii_case("GET") {
            let params = [
                ("session_id", ctx.call_info.session_id.as_str()),
                ("caller", ctx.call_info.caller.as_str()),
                ("callee", ctx.call_info.callee.as_str()),
                ("direction", ctx.call_info.direction.as_str()),
                ("ivr_name", self.definition.name.as_str()),
                ("menu", self.current_menu_key()),
            ];
            ctx.http_client.get(url).query(&params)
        } else {
            let payload = WebhookPayload {
                session_id: ctx.call_info.session_id.clone(),
                caller: ctx.call_info.caller.clone(),
                callee: ctx.call_info.callee.clone(),
                direction: ctx.call_info.direction.clone(),
                ivr_name: self.definition.name.clone(),
                menu: self.current_menu_key().to_string(),
            };
            ctx.http_client.post(url).json(&payload)
        };

        for (key, value) in headers {
            req_builder = req_builder.header(key, value);
        }

        let response = tokio::time::timeout(
            Duration::from_secs(10),
            req_builder.send(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Webhook request timed out after 10 seconds"))?
        .map_err(|e| anyhow::anyhow!("Webhook request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Webhook returned non-success status: {}",
                status
            ));
        }

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
        let (max_retries, timeout_action, max_retries_action, greeting) = {
            let menu = self
                .definition
                .get_menu(&menu_key)
                .ok_or_else(|| anyhow::anyhow!("menu '{}' not found", menu_key))?;
            (
                menu.max_retries,
                menu.timeout_action.clone(),
                menu.max_retries_action.clone(),
                menu.greeting.clone(),
            )
        };

        let new_retry = retry_count + 1;
        if new_retry > max_retries {
            if let Some(action) = max_retries_action {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    retries = new_retry,
                    "Max retries exceeded, executing fallback action"
                );
                return self.execute_action(&action, ctrl, ctx).await;
            } else {
                info!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    retries = new_retry,
                    "Max retries exceeded, no fallback — hanging up"
                );
                self.state = IvrState::Done;
                return Ok(AppAction::Hangup { reason: None, code: None });
            }
        }

        // Retry: check timeout_action
        if let Some(action) = timeout_action {
            match action {
                EntryAction::Repeat => {
                    debug!(
                        ivr = %self.definition.name,
                        menu = %menu_key,
                        retry = new_retry,
                        "Timeout: repeating menu"
                    );
                    self.state = IvrState::PlayingGreeting {
                        menu_key: menu_key.clone(),
                    };
                    self.pending_retry_count = new_retry;
                    ctrl.play_audio(&greeting, false).await?;
                    Ok(AppAction::Continue)
                }
                other => self.execute_action(&other, ctrl, ctx).await,
            }
        } else {
            // No timeout_action defined; replay the greeting
            debug!(
                ivr = %self.definition.name,
                menu = %menu_key,
                retry = new_retry,
                "Timeout: replaying greeting (default)"
            );
            self.state = IvrState::PlayingGreeting {
                menu_key: menu_key.clone(),
            };
            self.pending_retry_count = new_retry;
            ctrl.play_audio(&greeting, false).await?;
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
        let (max_retries, max_retries_action, invalid_prompt) = {
            let menu = self
                .definition
                .get_menu(menu_key)
                .ok_or_else(|| anyhow::anyhow!("menu '{}' not found", menu_key))?;
            (
                menu.max_retries,
                menu.max_retries_action.clone(),
                menu.invalid_prompt.clone(),
            )
        };

        let new_retry = retry_count + 1;
        debug!(
            ivr = %self.definition.name,
            menu = menu_key,
            digit,
            retry = new_retry,
            "Invalid DTMF key"
        );

        if new_retry > max_retries {
            if let Some(action) = max_retries_action {
                return self.execute_action(&action, ctrl, ctx).await;
            } else {
                self.state = IvrState::Done;
                return Ok(AppAction::Hangup { reason: None, code: None });
            }
        }

        if let Some(ref invalid_prompt) = invalid_prompt {
            self.state = IvrState::PlayingInvalid {
                menu_key: menu_key.to_string(),
                retry_count: new_retry,
            };
            ctrl.play_audio(invalid_prompt, false).await?;
            Ok(AppAction::Continue)
        } else {
            // No invalid prompt — just go back to waiting
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
        _ctx: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        info!(ivr = %self.definition.name, "IVR application started");
        ctrl.answer().await?;
        self.enter_menu("root", ctrl).await
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
            IvrState::WaitingDtmf { menu_key, retry_count } => {
                Some((*retry_count, menu_key.clone(), false))
            }
            IvrState::PlayingGreeting { menu_key } => Some((0, menu_key.clone(), true)),
            _ => None,
        };

        let Some((retry_count, menu_key, is_greeting)) = state_snapshot else {
            // DTMF in other states is ignored
            debug!(
                ivr = %self.definition.name,
                digit,
                state = ?self.state,
                "DTMF ignored in current state"
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
                debug!(
                    ivr = %self.definition.name,
                    menu = %menu_key,
                    digit = %digit,
                    "DTMF barge-in during greeting"
                );
                ctrl.cancel_timeout("ivr_dtmf_timeout");
                let _ = ctrl.stop_audio().await;
                self.execute_action(&action, ctrl, ctx).await
            } else {
                // Ignore invalid DTMF during greeting
                Ok(AppAction::Continue)
            }
        } else {
            // WaitingDtmf — look up the entry for this digit
            let action = self
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

            if let Some(action) = action {
                self.execute_action(&action, ctrl, ctx).await
            } else if self.definition.get_menu(&menu_key).is_some() {
                self.handle_invalid_key(&menu_key, retry_count, &digit, ctrl, ctx)
                    .await
            } else {
                warn!(ivr = %self.definition.name, menu = %menu_key, "Menu not found during DTMF handling");
                self.state = IvrState::Done;
                Ok(AppAction::Hangup { reason: None, code: None })
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
            IvrState::PlayingGreeting { menu_key } => {
                AudioDone::Greeting { menu_key: menu_key.clone() }
            }
            IvrState::PlayingInvalid { menu_key, retry_count } => AudioDone::Invalid {
                menu_key: menu_key.clone(),
                retry_count: *retry_count,
            },
            IvrState::PlayingAnnouncement { return_menu } => {
                AudioDone::Announcement { return_menu: return_menu.clone() }
            }
            IvrState::PlayingHangup => AudioDone::Hangup,
            IvrState::PlayingAndHangup { code } => AudioDone::AndHangup { code: *code },
            _ => AudioDone::Other,
        };

        match done {
            AudioDone::Greeting { menu_key } => {
                let retry_count = self.pending_retry_count;
                self.pending_retry_count = 0;
                self.start_waiting_dtmf(&menu_key, retry_count, ctrl);
                Ok(AppAction::Continue)
            }
            AudioDone::Invalid { menu_key, retry_count } => {
                // Invalid prompt finished → re-play greeting
                let greeting = self
                    .definition
                    .get_menu(&menu_key)
                    .map(|m| m.greeting.clone());
                if let Some(greeting) = greeting {
                    self.state = IvrState::PlayingGreeting {
                        menu_key: menu_key.clone(),
                    };
                    self.pending_retry_count = retry_count;
                    ctrl.play_audio(&greeting, false).await?;
                }
                Ok(AppAction::Continue)
            }
            AudioDone::Announcement { return_menu } => {
                self.enter_menu(&return_menu, ctrl).await
            }
            AudioDone::Hangup => {
                self.state = IvrState::Done;
                Ok(AppAction::Hangup { reason: None, code: None })
            }
            AudioDone::AndHangup { code } => {
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
            IvrState::WaitingDtmf { menu_key, retry_count } => {
                Some((menu_key.clone(), *retry_count))
            }
            _ => None,
        };

        if let Some((menu_key, retry_count)) = waiting {
            debug!(
                ivr = %self.definition.name,
                menu = %menu_key,
                retry_count,
                "DTMF timeout"
            );
            self.handle_timeout(menu_key, retry_count, ctrl, ctx).await
        } else {
            Ok(AppAction::Continue)
        }
    }
}
