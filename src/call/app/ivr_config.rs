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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level wrapper for the TOML file (`[ivr]` table).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrFileConfig {
    pub ivr: IvrDefinition,
}

/// Complete IVR definition: metadata + root menu + sub-menus.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    pub root: MenuNode,
    #[serde(default)]
    pub menus: HashMap<String, MenuNode>,
}

/// A single menu level in the IVR tree.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MenuNode {
    /// Audio file / URL / TTS URI to play as greeting.
    pub greeting: String,
    /// How long to wait for DTMF input (milliseconds).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum number of retries before executing `max_retries_action`.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Audio to play when an invalid key is pressed.
    #[serde(default)]
    pub invalid_prompt: Option<String>,
    /// Action to take when timeout occurs.
    #[serde(default)]
    pub timeout_action: Option<EntryAction>,
    /// Action to take when max retries exceeded.
    #[serde(default)]
    pub max_retries_action: Option<EntryAction>,
    /// DTMF key → action mappings.
    #[serde(default)]
    pub entries: Vec<MenuEntry>,
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
    Play { prompt: String },
    /// Replay the current menu greeting.
    Repeat,
    /// Hang up the call, optionally playing a goodbye prompt.
    Hangup {
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Collect digits for extension dialling.
    CollectExtension {
        prompt: String,
        #[serde(default = "default_min_digits")]
        min_digits: usize,
        #[serde(default = "default_max_digits")]
        max_digits: usize,
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
    },
    /// Play an audio prompt then hang up with a specific SIP response code.
    /// Playback completes regardless of DTMF barge-in (interrupted flag is ignored).
    PlayAndHangup {
        /// Audio file to play before hanging up.
        #[serde(default)]
        prompt: Option<String>,
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
    Play { prompt: String, #[serde(default)] data: Option<String> },
    /// Replay the current menu greeting.
    Repeat,
    /// Hang up the call.
    Hangup { #[serde(default)] prompt: Option<String> },
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
            WebhookResponse::Play { prompt, .. } => EntryAction::Play { prompt },
            WebhookResponse::Repeat => EntryAction::Repeat,
            WebhookResponse::Hangup { prompt } => EntryAction::Hangup { prompt },
            WebhookResponse::CollectExtension {
                prompt,
                min_digits,
                max_digits,
                inter_digit_timeout_ms,
            } => EntryAction::CollectExtension {
                prompt,
                min_digits: min_digits.unwrap_or_else(default_min_digits),
                max_digits: max_digits.unwrap_or_else(default_max_digits),
                inter_digit_timeout_ms: inter_digit_timeout_ms
                    .unwrap_or_else(default_inter_digit_timeout_ms),
            },
            WebhookResponse::PlayAndHangup { prompt, code } => {
                EntryAction::PlayAndHangup { prompt, code }
            }
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
            if let EntryAction::Menu { menu: ref target } = entry.action {
                if target != "root" && !menus.contains_key(target) {
                    return Err(format!(
                        "menu '{}' entry key '{}' references unknown menu '{}'",
                        menu_key, entry.key, target
                    ));
                }
            }
        }
        // Also check timeout_action and max_retries_action
        if let Some(EntryAction::Menu { menu: ref target }) = menu.timeout_action {
            if target != "root" && !menus.contains_key(target) {
                return Err(format!(
                    "menu '{}' timeout_action references unknown menu '{}'",
                    menu_key, target
                ));
            }
        }
        if let Some(EntryAction::Menu { menu: ref target }) = menu.max_retries_action {
            if target != "root" && !menus.contains_key(target) {
                return Err(format!(
                    "menu '{}' max_retries_action references unknown menu '{}'",
                    menu_key, target
                ));
            }
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
}
