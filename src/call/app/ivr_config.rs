//! IVR configuration structures.
//!
//! Parsed from `config/ivr/{name}.toml` files. Each file defines a complete
//! IVR tree with a root menu and optional sub-menus.
//!
//! # Example
//!
//! ```toml
//! [ivr]
//! name = "main"
//! description = "Main menu"
//!
//! [ivr.root]
//! greeting = "sounds/ivr/welcome.wav"
//! timeout_ms = 5000
//! max_retries = 3
//!
//! [[ivr.root.entries]]
//! key = "1"
//! label = "Sales"
//! action = { type = "transfer", target = "2001" }
//! ```

use crate::tts::TtsConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level wrapper for the TOML file (`[ivr]` table).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrFileConfig {
    pub ivr: IvrDefinition,
}

/// Business hours schedule for a single day or day range.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BusinessHoursSchedule {
    /// Days of the week: "mon", "tue", "wed", "thu", "fri", "sat", "sun"
    pub days: Vec<String>,
    /// Start time in HH:MM format (24-hour).
    pub start: String,
    /// End time in HH:MM format (24-hour).
    pub end: String,
}

/// Business hours configuration for the IVR.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BusinessHours {
    /// Whether business hours checking is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Timezone (e.g. "America/New_York", "Asia/Shanghai"). Defaults to "UTC".
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Work days: 0=Mon, 1=Tue, 2=Wed, 3=Thu, 4=Fri, 5=Sat, 6=Sun
    #[serde(default)]
    pub work_days: Option<Vec<u8>>,
    /// Work start time in HH:MM format.
    #[serde(default)]
    pub work_start: Option<String>,
    /// Work end time in HH:MM format.
    #[serde(default)]
    pub work_end: Option<String>,
    /// Break start time in HH:MM format (optional).
    #[serde(default)]
    pub break_start: Option<String>,
    /// Break end time in HH:MM format (optional).
    #[serde(default)]
    pub break_end: Option<String>,
    /// Closed prompt text (for TTS) - editor UI stores this.
    #[serde(default)]
    pub closed_text: Option<String>,
    /// Schedule entries defining open hours (legacy format).
    #[serde(default)]
    pub schedules: Vec<BusinessHoursSchedule>,
    /// Action to take when outside business hours.
    #[serde(default)]
    pub closed_action: Option<EntryAction>,
    /// Optional greeting to play when closed (before executing closed_action).
    #[serde(default)]
    pub closed_greeting: Option<String>,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

/// Complete IVR definition: metadata + root menu + sub-menus.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    /// Business hours configuration. When enabled, calls outside business
    /// hours are routed to `closed_action` instead of the normal IVR menu.
    #[serde(default)]
    pub business_hours: Option<BusinessHours>,
    /// Optional TTS configuration for this IVR.
    #[serde(default)]
    pub tts: Option<TtsConfig>,
    pub root: MenuNode,
    #[serde(default)]
    pub menus: HashMap<String, MenuNode>,
}

/// A single menu level in the IVR tree.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MenuNode {
    /// Audio file / URL / TTS URI to play as greeting.
    #[serde(default)]
    pub greeting: String,
    /// TTS text for greeting (editor stores this, converted to greeting on publish).
    #[serde(default)]
    pub greeting_text: Option<String>,
    /// TTS voice for greeting.
    #[serde(default)]
    pub greeting_voice: Option<String>,
    /// How long to wait for DTMF input (milliseconds).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum number of retries before executing `max_retries_action`.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Audio to play when an invalid key is pressed.
    #[serde(default)]
    pub invalid_prompt: Option<String>,
    /// TTS text for invalid prompt (editor stores this).
    #[serde(default)]
    pub invalid_text: Option<String>,
    /// TTS voice for invalid prompt.
    #[serde(default)]
    pub invalid_voice: Option<String>,
    /// Action to take when timeout occurs.
    #[serde(default)]
    pub timeout_action: Option<EntryAction>,
    /// Action to take when max retries exceeded.
    #[serde(default)]
    pub max_retries_action: Option<EntryAction>,
    /// Action to take when an unmapped DTMF key is pressed.
    /// Useful for "direct extension dial" - set to CollectExtension or Collect.
    #[serde(default)]
    pub unknown_key_action: Option<EntryAction>,
    /// DTMF key → action mappings.
    #[serde(default)]
    pub entries: Vec<MenuEntry>,
}

impl Default for MenuNode {
    fn default() -> Self {
        Self {
            greeting: String::new(),
            greeting_text: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            invalid_prompt: None,
            invalid_text: None,
            invalid_voice: None,
            timeout_action: None,
            max_retries_action: None,
            unknown_key_action: None,
            entries: Vec::new(),
        }
    }
}

/// A single DTMF key mapping in a menu.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MenuEntry {
    /// DTMF key: "0"-"9", "*", "#"
    pub key: String,
    /// Human-readable label (for editor UI).
    #[serde(default)]
    pub label: Option<String>,
    /// Action to execute when this key is pressed.
    pub action: EntryAction,
}

/// Actions that can be triggered by a DTMF key, timeout, or max-retries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryAction {
    /// Transfer the call to a SIP extension or URI.
    Transfer { target: String },
    /// Send the call to a queue.
    Queue { target: String },
    /// Navigate to a sub-menu (or "root" for top level).
    Menu { menu: String },
    /// Transfer to voicemail for the given extension.
    Voicemail { target: String },
    /// Play an announcement and return to the current menu.
    Play {
        prompt: String,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
    },
    /// Replay the current menu greeting.
    Repeat,
    /// Hang up the call, optionally playing a goodbye prompt.
    Hangup {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
    },
    /// Collect digits for extension dialling.
    CollectExtension {
        prompt: String,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
        #[serde(default = "default_min_digits")]
        min_digits: usize,
        #[serde(default = "default_max_digits")]
        max_digits: usize,
        #[serde(default = "default_inter_digit_timeout_ms")]
        inter_digit_timeout_ms: u64,
    },
    /// Collect digits into a named variable (not directly dialled).
    Collect {
        /// Variable name to store collected digits.
        variable: String,
        /// Prompt to play before collecting (TTS or file path).
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
        /// Minimum digits to collect.
        #[serde(default = "default_min_digits")]
        min_digits: usize,
        /// Maximum digits to collect.
        #[serde(default = "default_max_digits")]
        max_digits: usize,
        /// Key that terminates input (e.g., "#").
        #[serde(default)]
        end_key: Option<String>,
        /// Inter-digit timeout in milliseconds.
        #[serde(default = "default_inter_digit_timeout_ms")]
        inter_digit_timeout_ms: u64,
    },
    /// Call an external webhook to determine next action.
    Webhook {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        /// Variables to include in the webhook payload (comma-separated names).
        #[serde(default)]
        variables: Option<String>,
        /// Timeout in seconds.
        #[serde(default = "default_webhook_timeout")]
        timeout: u64,
    },
    /// Play an audio prompt then hang up with a specific SIP response code.
    /// Playback completes regardless of DTMF barge-in (interrupted flag is ignored).
    PlayAndHangup {
        /// Audio file to play before hanging up.
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
        /// SIP status code sent in the BYE/response (e.g. 486, 603, 503).
        /// Defaults to 200 if absent.
        #[serde(default)]
        code: Option<u16>,
    },
}

/// Response from a webhook, determining the next IVR action.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
pub enum WebhookResponse {
    /// Transfer the call to a SIP extension or URI.
    Transfer { target: String },
    /// Send the call to a queue.
    Queue { target: String },
    /// Navigate to a sub-menu.
    Menu { menu: String },
    /// Transfer to voicemail.
    Voicemail { target: String },
    /// Play an announcement (optionally with TTS data).
    Play {
        prompt: String,
        #[serde(default)]
        data: Option<String>,
    },
    /// Replay the current menu greeting.
    Repeat,
    /// Hang up the call.
    Hangup {
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Collect more digits.
    CollectExtension {
        prompt: String,
        #[serde(default)]
        min_digits: Option<usize>,
        #[serde(default)]
        max_digits: Option<usize>,
        #[serde(default)]
        inter_digit_timeout_ms: Option<u64>,
    },
    /// Collect digits into a variable.
    Collect {
        variable: String,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        min_digits: Option<usize>,
        #[serde(default)]
        max_digits: Option<usize>,
        #[serde(default)]
        end_key: Option<String>,
        #[serde(default)]
        inter_digit_timeout_ms: Option<u64>,
    },
    /// Play an audio prompt then hang up with a specific SIP response code.
    PlayAndHangup {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        code: Option<u16>,
    },
}

impl WebhookResponse {
    /// Convert a [`WebhookResponse`] into an [`EntryAction`] that can be
    /// directly executed by the IVR state machine.
    pub fn into_entry_action(self) -> EntryAction {
        match self {
            WebhookResponse::Transfer { target } => EntryAction::Transfer { target },
            WebhookResponse::Queue { target } => EntryAction::Queue { target },
            WebhookResponse::Menu { menu } => EntryAction::Menu { menu },
            WebhookResponse::Voicemail { target } => EntryAction::Voicemail { target },
            WebhookResponse::Play { prompt, .. } => EntryAction::Play {
                prompt,
                prompt_text: None,
                prompt_voice: None,
            },
            WebhookResponse::Repeat => EntryAction::Repeat,
            WebhookResponse::Hangup { prompt } => EntryAction::Hangup {
                prompt,
                prompt_text: None,
                prompt_voice: None,
            },
            WebhookResponse::CollectExtension {
                prompt,
                min_digits,
                max_digits,
                inter_digit_timeout_ms,
            } => EntryAction::CollectExtension {
                prompt,
                prompt_text: None,
                prompt_voice: None,
                min_digits: min_digits.unwrap_or_else(default_min_digits),
                max_digits: max_digits.unwrap_or_else(default_max_digits),
                inter_digit_timeout_ms: inter_digit_timeout_ms
                    .unwrap_or_else(default_inter_digit_timeout_ms),
            },
            WebhookResponse::Collect {
                variable,
                prompt,
                min_digits,
                max_digits,
                end_key,
                inter_digit_timeout_ms,
            } => EntryAction::Collect {
                variable,
                prompt,
                prompt_text: None,
                prompt_voice: None,
                min_digits: min_digits.unwrap_or_else(default_min_digits),
                max_digits: max_digits.unwrap_or_else(default_max_digits),
                end_key,
                inter_digit_timeout_ms: inter_digit_timeout_ms
                    .unwrap_or_else(default_inter_digit_timeout_ms),
            },
            WebhookResponse::PlayAndHangup { prompt, code } => EntryAction::PlayAndHangup {
                prompt,
                prompt_text: None,
                prompt_voice: None,
                code,
            },
        }
    }
}

fn default_timeout_ms() -> u64 {
    5000
}

fn default_max_retries() -> u32 {
    3
}

fn default_min_digits() -> usize {
    3
}

fn default_max_digits() -> usize {
    4
}

fn default_inter_digit_timeout_ms() -> u64 {
    3000
}

fn default_webhook_timeout() -> u64 {
    10
}

impl IvrDefinition {
    /// Look up a menu by key. `"root"` returns the root menu.
    pub fn get_menu(&self, key: &str) -> Option<&MenuNode> {
        if key == "root" {
            Some(&self.root)
        } else {
            self.menus.get(key)
        }
    }

    /// Validate the IVR definition: check that all menu references are valid.
    pub fn validate(&self) -> Result<(), String> {
        // Validate root entries
        Self::validate_menu_refs(&self.root, "root", &self.menus)?;
        // Validate sub-menu entries
        for (key, menu) in &self.menus {
            Self::validate_menu_refs(menu, key, &self.menus)?;
        }
        Ok(())
    }

    fn validate_menu_refs(
        menu: &MenuNode,
        menu_key: &str,
        menus: &HashMap<String, MenuNode>,
    ) -> Result<(), String> {
        for entry in &menu.entries {
            if let EntryAction::Menu { menu: ref target } = entry.action
                && target != "root" && !menus.contains_key(target) {
                    return Err(format!(
                        "menu '{}' entry key '{}' references unknown menu '{}'",
                        menu_key, entry.key, target
                    ));
                }
        }
        // Also check timeout_action and max_retries_action
        if let Some(EntryAction::Menu { menu: ref target }) = menu.timeout_action
            && target != "root" && !menus.contains_key(target) {
                return Err(format!(
                    "menu '{}' timeout_action references unknown menu '{}'",
                    menu_key, target
                ));
            }
        if let Some(EntryAction::Menu { menu: ref target }) = menu.max_retries_action
            && target != "root" && !menus.contains_key(target) {
                return Err(format!(
                    "menu '{}' max_retries_action references unknown menu '{}'",
                    menu_key, target
                ));
            }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_ivr_toml() {
        let toml_str = r##"
[ivr]
name = "main"
description = "主菜单"
lang = "zh"

[ivr.root]
greeting = "sounds/ivr/welcome.wav"
timeout_ms = 5000
max_retries = 3
invalid_prompt = "sounds/ivr/invalid.wav"
timeout_action = { type = "repeat" }
max_retries_action = { type = "transfer", target = "operator" }

[[ivr.root.entries]]
key = "1"
label = "Sales"
action = { type = "menu", menu = "sales" }

[[ivr.root.entries]]
key = "2"
label = "Support"
action = { type = "queue", target = "support_queue" }

[[ivr.root.entries]]
key = "0"
label = "Operator"
action = { type = "transfer", target = "1000" }

[[ivr.root.entries]]
key = "#"
label = "Enter extension"
action = { type = "collect_extension", prompt = "sounds/ivr/enter_ext.wav", min_digits = 3, max_digits = 4, inter_digit_timeout_ms = 3000 }

[[ivr.root.entries]]
key = "*"
label = "Repeat"
action = { type = "repeat" }

[ivr.menus.sales]
greeting = "sounds/ivr/sales_menu.wav"
timeout_ms = 5000
max_retries = 2
timeout_action = { type = "transfer", target = "2000" }

[[ivr.menus.sales.entries]]
key = "1"
label = "East"
action = { type = "transfer", target = "2001" }

[[ivr.menus.sales.entries]]
key = "2"
label = "South"
action = { type = "transfer", target = "2002" }

[[ivr.menus.sales.entries]]
key = "9"
label = "Back"
action = { type = "menu", menu = "root" }
"##;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        let ivr = &config.ivr;

        assert_eq!(ivr.name, "main");
        assert_eq!(ivr.root.greeting, "sounds/ivr/welcome.wav");
        assert_eq!(ivr.root.entries.len(), 5);
        assert_eq!(ivr.root.timeout_ms, 5000);
        assert_eq!(ivr.root.max_retries, 3);

        // Validate sub-menu
        let sales = ivr.menus.get("sales").expect("sales menu");
        assert_eq!(sales.entries.len(), 3);
        assert_eq!(sales.timeout_ms, 5000);

        // Validate references
        ivr.validate().expect("validation should pass");
    }

    #[test]
    fn test_invalid_menu_reference() {
        let toml_str = r#"
[ivr]
name = "bad"

[ivr.root]
greeting = "hello.wav"

[[ivr.root.entries]]
key = "1"
action = { type = "menu", menu = "nonexistent" }
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        let result = config.ivr.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nonexistent"));
    }

    #[test]
    fn test_all_action_types_parse() {
        let toml_str = r#"
[ivr]
name = "all-actions"

[ivr.root]
greeting = "hello.wav"

[[ivr.root.entries]]
key = "1"
action = { type = "transfer", target = "1001" }

[[ivr.root.entries]]
key = "2"
action = { type = "queue", target = "q1" }

[[ivr.root.entries]]
key = "3"
action = { type = "voicemail", target = "1001" }

[[ivr.root.entries]]
key = "4"
action = { type = "play", prompt = "address.wav" }

[[ivr.root.entries]]
key = "5"
action = { type = "repeat" }

[[ivr.root.entries]]
key = "6"
action = { type = "hangup", prompt = "goodbye.wav" }

[[ivr.root.entries]]
key = "7"
action = { type = "collect_extension", prompt = "ext.wav" }

[[ivr.root.entries]]
key = "8"
action = { type = "webhook", url = "https://example.com/hook", method = "POST" }

[[ivr.root.entries]]
key = "9"
action = { type = "menu", menu = "sub" }

[ivr.menus.sub]
greeting = "sub.wav"

[[ivr.menus.sub.entries]]
key = "0"
action = { type = "menu", menu = "root" }
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        assert_eq!(config.ivr.root.entries.len(), 9);
        config.ivr.validate().expect("all refs valid");
    }

    #[test]
    fn test_defaults() {
        let toml_str = r#"
[ivr]
name = "minimal"

[ivr.root]
greeting = "hello.wav"
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        assert_eq!(config.ivr.root.timeout_ms, 5000);
        assert_eq!(config.ivr.root.max_retries, 3);
        assert!(config.ivr.root.entries.is_empty());
        assert!(config.ivr.root.invalid_prompt.is_none());
    }

    #[test]
    fn test_collect_action_parse() {
        let toml_str = r##"
[ivr]
name = "test-collect"

[ivr.root]
greeting = "hello.wav"

[[ivr.root.entries]]
key = "1"
label = "Enter account"
action = { type = "collect", variable = "account_id", prompt = "请输入账号", min_digits = 4, max_digits = 6, end_key = "#", inter_digit_timeout_ms = 5000 }
"##;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        assert_eq!(config.ivr.root.entries.len(), 1);

        let entry = &config.ivr.root.entries[0];
        assert_eq!(entry.key, "1");
        assert_eq!(entry.label.as_deref(), Some("Enter account"));

        match &entry.action {
            EntryAction::Collect {
                variable,
                prompt,
                prompt_text: _,
                prompt_voice: _,
                min_digits,
                max_digits,
                end_key,
                inter_digit_timeout_ms,
            } => {
                assert_eq!(variable, "account_id");
                assert_eq!(prompt.as_deref(), Some("请输入账号"));
                assert_eq!(*min_digits, 4);
                assert_eq!(*max_digits, 6);
                assert_eq!(end_key.as_deref(), Some("#"));
                assert_eq!(*inter_digit_timeout_ms, 5000);
            }
            _ => panic!("Expected Collect action"),
        }
    }

    #[test]
    fn test_webhook_with_variables() {
        let toml_str = r#"
[ivr]
name = "test-webhook"

[ivr.root]
greeting = "hello.wav"

[[ivr.root.entries]]
key = "1"
action = { type = "webhook", url = "https://api.example.com/ivr", method = "POST", variables = "account_id,caller_id", timeout = 15 }
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        assert_eq!(config.ivr.root.entries.len(), 1);

        let entry = &config.ivr.root.entries[0];
        match &entry.action {
            EntryAction::Webhook {
                url,
                method,
                variables,
                timeout,
                ..
            } => {
                assert_eq!(url, "https://api.example.com/ivr");
                assert_eq!(method.as_deref(), Some("POST"));
                assert_eq!(variables.as_deref(), Some("account_id,caller_id"));
                assert_eq!(*timeout, 15);
            }
            _ => panic!("Expected Webhook action"),
        }
    }

    #[test]
    fn test_unknown_key_action() {
        let toml_str = r#"
[ivr]
name = "direct-dial"

[ivr.root]
greeting = "hello.wav"
unknown_key_action = { type = "collect_extension", prompt = "", min_digits = 3, max_digits = 4 }

[[ivr.root.entries]]
key = "0"
action = { type = "transfer", target = "operator" }
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");

        // Check unknown_key_action is parsed
        let unknown_action = config
            .ivr
            .root
            .unknown_key_action
            .as_ref()
            .expect("unknown_key_action");
        match unknown_action {
            EntryAction::CollectExtension {
                min_digits,
                max_digits,
                ..
            } => {
                assert_eq!(*min_digits, 3);
                assert_eq!(*max_digits, 4);
            }
            _ => panic!("Expected CollectExtension action"),
        }

        // Only the explicit entry should be in entries
        assert_eq!(config.ivr.root.entries.len(), 1);
        assert_eq!(config.ivr.root.entries[0].key, "0");
    }

    #[test]
    fn test_webhook_response_collect() {
        // Test WebhookResponse::Collect conversion to EntryAction
        let response = WebhookResponse::Collect {
            variable: "code".to_string(),
            prompt: Some("请输入验证码".to_string()),
            min_digits: Some(4),
            max_digits: Some(6),
            end_key: Some("#".to_string()),
            inter_digit_timeout_ms: Some(3000),
        };

        let action = response.into_entry_action();
        match action {
            EntryAction::Collect {
                variable,
                prompt,
                prompt_text: _,
                prompt_voice: _,
                min_digits,
                max_digits,
                end_key,
                inter_digit_timeout_ms,
            } => {
                assert_eq!(variable, "code");
                assert_eq!(prompt.as_deref(), Some("请输入验证码"));
                assert_eq!(min_digits, 4);
                assert_eq!(max_digits, 6);
                assert_eq!(end_key.as_deref(), Some("#"));
                assert_eq!(inter_digit_timeout_ms, 3000);
            }
            _ => panic!("Expected Collect action"),
        }
    }

    #[test]
    fn test_full_ivr_with_all_features() {
        let toml_str = r#"
[ivr]
name = "full-featured"
description = "Complete IVR with all features"

[ivr.root]
greeting = "sounds/welcome.wav"
timeout_ms = 8000
max_retries = 2
unknown_key_action = { type = "collect_extension", prompt = "", min_digits = 3, max_digits = 4, inter_digit_timeout_ms = 3000 }

[[ivr.root.entries]]
key = "0"
label = "Operator"
action = { type = "transfer", target = "9000" }

[[ivr.root.entries]]
key = "1"
label = "Enter account"
action = { type = "collect", variable = "account", min_digits = 6, max_digits = 8 }

[[ivr.root.entries]]
key = "2"
label = "API lookup"
action = { type = "webhook", url = "https://api.example.com/lookup", variables = "account,caller_id", timeout = 10 }

[ivr.menus.support]
greeting = "sounds/support.wav"
unknown_key_action = { type = "collect_extension", prompt = "", min_digits = 3, max_digits = 4 }

[[ivr.menus.support.entries]]
key = "9"
label = "Back"
action = { type = "menu", menu = "root" }
"#;

        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        let ivr = &config.ivr;

        // Root menu checks
        assert_eq!(ivr.root.entries.len(), 3);
        assert!(ivr.root.unknown_key_action.is_some());

        // Verify 0 -> transfer to operator
        let op_entry = ivr
            .root
            .entries
            .iter()
            .find(|e| e.key == "0")
            .expect("0 entry");
        match &op_entry.action {
            EntryAction::Transfer { target } => assert_eq!(target, "9000"),
            _ => panic!("Expected Transfer"),
        }

        // Verify 1 -> collect
        let collect_entry = ivr
            .root
            .entries
            .iter()
            .find(|e| e.key == "1")
            .expect("1 entry");
        match &collect_entry.action {
            EntryAction::Collect { variable, .. } => assert_eq!(variable, "account"),
            _ => panic!("Expected Collect"),
        }

        // Verify 2 -> webhook with variables
        let webhook_entry = ivr
            .root
            .entries
            .iter()
            .find(|e| e.key == "2")
            .expect("2 entry");
        match &webhook_entry.action {
            EntryAction::Webhook { url, variables, .. } => {
                assert_eq!(url, "https://api.example.com/lookup");
                assert_eq!(variables.as_deref(), Some("account,caller_id"));
            }
            _ => panic!("Expected Webhook"),
        }

        // Support menu checks
        let support = ivr.menus.get("support").expect("support menu");
        assert!(support.unknown_key_action.is_some());

        // Validate
        ivr.validate().expect("validation should pass");
    }
}
