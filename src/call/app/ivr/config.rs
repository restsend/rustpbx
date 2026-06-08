use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level wrapper for the TOML file (`[ivr]` table).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrFileConfig {
    pub ivr: IvrDefinition,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BusinessHoursSchedule {
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BusinessHours {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub work_days: Option<Vec<u8>>,
    #[serde(default)]
    pub work_start: Option<String>,
    #[serde(default)]
    pub work_end: Option<String>,
    #[serde(default)]
    pub break_start: Option<String>,
    #[serde(default)]
    pub break_end: Option<String>,
    #[serde(default)]
    pub closed_text: Option<String>,
    #[serde(default)]
    pub schedules: Vec<BusinessHoursSchedule>,
    #[serde(default)]
    pub closed_action: Option<EntryAction>,
    #[serde(default)]
    pub closed_greeting: Option<String>,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IvrDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub default_voice: Option<String>,
    #[serde(default)]
    pub dynamic_build: bool,
    #[serde(default)]
    pub ivr_mode: Option<String>,
    #[serde(default)]
    pub provider: Option<IvrProviderConfig>,
    #[serde(default)]
    pub business_hours: Option<BusinessHours>,
    #[serde(default)]
    pub tts: Option<crate::tts::TtsConfig>,
    #[serde(default)]
    pub root: Option<MenuNode>,
    #[serde(default)]
    pub menus: HashMap<String, MenuNode>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IvrProviderConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_provider_retries")]
    pub max_retries: u32,
    #[serde(default = "default_provider_delay")]
    pub retry_delay_ms: u64,
    #[serde(default = "default_provider_timeout")]
    pub timeout_secs: u64,
}

fn default_provider_retries() -> u32 {
    3
}
fn default_provider_delay() -> u64 {
    1000
}
fn default_provider_timeout() -> u64 {
    10
}

impl IvrDefinition {
    pub fn is_step_mode(&self) -> bool {
        self.ivr_mode.as_deref() == Some("step")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MenuNode {
    #[serde(default)]
    pub greeting: String,
    #[serde(default)]
    pub greeting_text: Option<String>,
    #[serde(default)]
    pub greeting_voice: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub invalid_prompt: Option<String>,
    #[serde(default)]
    pub invalid_text: Option<String>,
    #[serde(default)]
    pub invalid_voice: Option<String>,
    #[serde(default)]
    pub timeout_action: Option<EntryAction>,
    #[serde(default)]
    pub max_retries_action: Option<EntryAction>,
    #[serde(default)]
    pub unknown_key_action: Option<EntryAction>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MenuEntry {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
    pub action: EntryAction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryAction {
    Transfer {
        target: String,
    },
    Queue {
        target: String,
        #[serde(default)]
        return_to_ivr: Option<bool>,
    },
    Menu {
        menu: String,
    },
    Voicemail {
        target: String,
    },
    Play {
        prompt: String,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
    },
    Repeat,
    Hangup {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
    },
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
    Collect {
        variable: String,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
        #[serde(default = "default_min_digits")]
        min_digits: usize,
        #[serde(default = "default_max_digits")]
        max_digits: usize,
        #[serde(default)]
        end_key: Option<String>,
        #[serde(default = "default_inter_digit_timeout_ms")]
        inter_digit_timeout_ms: u64,
    },
    Webhook {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        variables: Option<String>,
        #[serde(default = "default_webhook_timeout")]
        timeout: u64,
    },
    PlayAndHangup {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
        #[serde(default)]
        code: Option<u16>,
    },
    Back,

    Prompt {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        tts_text: Option<String>,
        #[serde(default)]
        tts_voice: Option<String>,
        #[serde(default)]
        record_name_list: Option<String>,
        #[serde(default)]
        interruptible: bool,
        #[serde(default)]
        tts_api_url: Option<String>,
    },

    DtmfMenu {
        #[serde(default)]
        greeting: Option<String>,
        #[serde(default)]
        greeting_text: Option<String>,
        #[serde(default)]
        greeting_record_list: Option<String>,
        #[serde(default)]
        greeting_voice: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_max_retries")]
        max_retries: u32,
        #[serde(default)]
        entries: HashMap<String, ActionNode>,
        #[serde(default)]
        timeout_action: Option<Box<ActionNode>>,
        #[serde(default)]
        invalid_action: Option<Box<ActionNode>>,
        #[serde(default)]
        greeting_api_url: Option<String>,
    },

    CollectDtmf {
        #[serde(default = "default_min_digits")]
        min_digits: usize,
        #[serde(default = "default_max_digits")]
        max_digits: usize,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        #[serde(default)]
        terminator: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
    },

    InputPhone {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        prompt_text: Option<String>,
        #[serde(default)]
        prompt_voice: Option<String>,
        #[serde(default = "default_phone_digits")]
        min_digits: usize,
        #[serde(default = "default_phone_digits")]
        max_digits: usize,
    },

    InputVoice {
        scene: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },

    Api {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        variables: Option<String>,
        #[serde(default = "default_webhook_timeout")]
        timeout: u64,
        #[serde(default)]
        get_dynamic_tree: Option<bool>,
    },

    Torecord {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        beep: bool,
        #[serde(default)]
        max_duration_secs: Option<u32>,
    },

    JumpIvr {
        route_point: String,
        #[serde(default)]
        params: HashMap<String, String>,
    },

    RouteToAgent {
        target: String,
        #[serde(default)]
        skill_group_id: Option<String>,
        #[serde(default)]
        key_id: Option<String>,
        #[serde(default)]
        channel_code: Option<String>,
    },
    VoipBridge {
        create_room_uri: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        success: Option<Box<ActionNode>>,
        #[serde(default)]
        failure: Option<Box<ActionNode>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionNode {
    #[serde(flatten)]
    pub action: EntryAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Box<ActionNode>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

impl ActionNode {
    pub fn new(action: EntryAction) -> Self {
        Self {
            action,
            next: None,
            step_id: None,
            step_name: None,
            extra: None,
        }
    }

    pub fn with_next(action: EntryAction, next: ActionNode) -> Self {
        Self {
            action,
            next: Some(Box::new(next)),
            step_id: None,
            step_name: None,
            extra: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
pub enum WebhookResponse {
    Transfer {
        target: String,
    },
    Queue {
        target: String,
        #[serde(default)]
        return_to_ivr: Option<bool>,
    },
    Menu {
        menu: String,
    },
    Voicemail {
        target: String,
    },
    Play {
        prompt: String,
        #[serde(default)]
        data: Option<String>,
    },
    Repeat,
    Hangup {
        #[serde(default)]
        prompt: Option<String>,
    },
    CollectExtension {
        prompt: String,
        #[serde(default)]
        min_digits: Option<usize>,
        #[serde(default)]
        max_digits: Option<usize>,
        #[serde(default)]
        inter_digit_timeout_ms: Option<u64>,
    },
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
    PlayAndHangup {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        code: Option<u16>,
    },
    Back,
}

impl WebhookResponse {
    pub fn into_entry_action(self) -> EntryAction {
        match self {
            WebhookResponse::Transfer { target } => EntryAction::Transfer { target },
            WebhookResponse::Queue {
                target,
                return_to_ivr,
            } => EntryAction::Queue {
                target,
                return_to_ivr,
            },
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
            WebhookResponse::Back => EntryAction::Back,
        }
    }
}

impl IvrDefinition {
    pub fn get_menu(&self, key: &str) -> Option<&MenuNode> {
        if key == "root" {
            self.root.as_ref()
        } else {
            self.menus.get(key)
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.is_step_mode() {
            return Ok(());
        }
        if let Some(ref root) = self.root {
            Self::validate_menu_refs(root, "root", &self.menus)?;
        }
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
                && target != "root"
                && !menus.contains_key(target)
            {
                return Err(format!(
                    "menu '{}' entry key '{}' references unknown menu '{}'",
                    menu_key, entry.key, target
                ));
            }
        }
        if let Some(EntryAction::Menu { menu: ref target }) = menu.timeout_action
            && target != "root"
            && !menus.contains_key(target)
        {
            return Err(format!(
                "menu '{}' timeout_action references unknown menu '{}'",
                menu_key, target
            ));
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
fn default_phone_digits() -> usize {
    11
}

impl EntryAction {
    pub fn is_dtmf_menu(&self) -> bool {
        matches!(self, EntryAction::DtmfMenu { .. })
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
description = "main menu"
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
action = { type = "menu", menu = "support" }

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

[ivr.menus.support]
greeting = "sounds/ivr/support_menu.wav"
timeout_ms = 5000
max_retries = 3

[[ivr.menus.support.entries]]
key = "0"
label = "Back"
action = { type = "menu", menu = "root" }
"##;
        let config: IvrFileConfig = toml::from_str(toml_str).expect("parse TOML");
        let ivr = &config.ivr;
        assert_eq!(ivr.name, "main");
        let root = ivr.root.as_ref().expect("root menu");
        assert_eq!(root.greeting, "sounds/ivr/welcome.wav");
        assert_eq!(root.entries.len(), 2);
        assert_eq!(root.timeout_ms, 5000);
        assert_eq!(root.max_retries, 3);
        let sales = ivr.menus.get("sales").expect("sales menu");
        assert_eq!(sales.entries.len(), 3);
        ivr.validate().expect("validation should pass");
    }

    #[test]
    fn test_action_node_serialize_roundtrip() {
        let node = ActionNode::with_next(
            EntryAction::Prompt {
                file: Some("welcome.wav".into()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: true,
                tts_api_url: None,
            },
            ActionNode::new(EntryAction::Transfer {
                target: "2001".into(),
            }),
        );
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["type"], "prompt");
        assert_eq!(json["file"], "welcome.wav");
        assert_eq!(json["interruptible"], true);
        assert!(json.get("next").is_some());
        assert_eq!(json["next"]["type"], "transfer");
        assert_eq!(json["next"]["target"], "2001");
    }

    #[test]
    fn test_action_node_deserialize_prompt_chain() {
        let json = serde_json::json!({
            "type": "prompt",
            "file": "welcome.wav",
            "interruptible": true,
            "next": {
                "type": "transfer",
                "target": "2001"
            }
        });
        let node: ActionNode = serde_json::from_value(json).unwrap();
        match &node.action {
            EntryAction::Prompt {
                file,
                interruptible,
                ..
            } => {
                assert_eq!(file.as_deref(), Some("welcome.wav"));
                assert_eq!(*interruptible, true);
            }
            _ => panic!("expected Prompt"),
        }
        let next = node.next.expect("expected next");
        match &next.action {
            EntryAction::Transfer { target } => assert_eq!(target, "2001"),
            _ => panic!("expected Transfer"),
        }
    }

    #[test]
    fn test_action_node_no_next() {
        let json = serde_json::json!({
            "type": "hangup",
            "prompt": "goodbye.wav"
        });
        let node: ActionNode = serde_json::from_value(json).unwrap();
        assert!(node.next.is_none());
        match &node.action {
            EntryAction::Hangup { prompt, .. } => {
                assert_eq!(prompt.as_deref(), Some("goodbye.wav"))
            }
            _ => panic!("expected Hangup"),
        }
    }

    #[test]
    fn test_dtmf_menu_serialize() {
        let menu = ActionNode::new(EntryAction::DtmfMenu {
            greeting: Some("menu.wav".into()),
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: 5000,
            max_retries: 3,
            entries: HashMap::from([
                (
                    "1".into(),
                    ActionNode::new(EntryAction::Transfer {
                        target: "2001".into(),
                    }),
                ),
                (
                    "2".into(),
                    ActionNode::new(EntryAction::Queue {
                        target: "support".into(),
                        return_to_ivr: None,
                    }),
                ),
            ]),
            timeout_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            invalid_action: Some(Box::new(ActionNode::new(EntryAction::Repeat))),
            greeting_api_url: None,
        });
        let json = serde_json::to_value(&menu).unwrap();
        assert_eq!(json["type"], "dtmf_menu");
        assert_eq!(json["greeting"], "menu.wav");
        assert_eq!(json["entries"]["1"]["type"], "transfer");
        assert_eq!(json["timeout_action"]["type"], "repeat");
    }

    #[test]
    fn test_voip_bridge_serialize() {
        let node = ActionNode::new(EntryAction::VoipBridge {
            create_room_uri: "https://voip.example.com/rooms".into(),
            headers: HashMap::from([("Authorization".into(), "Bearer token123".into())]),
            timeout_ms: Some(30000),
            success: None,
            failure: None,
        });
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["type"], "voip_bridge");
        assert_eq!(json["create_room_uri"], "https://voip.example.com/rooms");
        assert_eq!(json["headers"]["Authorization"], "Bearer token123");
    }

    #[test]
    fn test_all_entry_action_types_parse() {
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
        let root = config.ivr.root.as_ref().expect("root menu");
        assert_eq!(root.entries.len(), 9);
        config.ivr.validate().expect("all refs valid");
    }

    #[test]
    fn test_new_action_types_json() {
        // prompt
        let node: ActionNode =
            serde_json::from_str(r#"{"type":"prompt","file":"hello.wav","interruptible":true}"#)
                .unwrap();
        assert!(matches!(
            node.action,
            EntryAction::Prompt {
                interruptible: true,
                ..
            }
        ));

        // api
        let node: ActionNode = serde_json::from_str(
            r#"{"type":"api","url":"https://api.example.com","method":"POST"}"#,
        )
        .unwrap();
        assert!(
            matches!(node.action, EntryAction::Api { url, .. } if url == "https://api.example.com")
        );

        // dtmf_menu
        let node: ActionNode = serde_json::from_str(r#"{"type":"dtmf_menu","greeting":"m.wav","entries":{"1":{"type":"transfer","target":"2001"}}}"#).unwrap();
        assert!(matches!(node.action, EntryAction::DtmfMenu { entries, .. } if entries.len() == 1));

        // route_to_agent
        let node: ActionNode = serde_json::from_str(
            r#"{"type":"route_to_agent","target":"9200","skill_group_id":"sales"}"#,
        )
        .unwrap();
        assert!(
            matches!(node.action, EntryAction::RouteToAgent { target, skill_group_id, .. }
            if target == "9200" && skill_group_id.as_deref() == Some("sales"))
        );

        // jump_ivr
        let node: ActionNode = serde_json::from_str(
            r#"{"type":"jump_ivr","route_point":"39290","params":{"businessType":"7"}}"#,
        )
        .unwrap();
        assert!(
            matches!(node.action, EntryAction::JumpIvr { route_point, params, .. }
            if route_point == "39290" && params.get("businessType") == Some(&"7".into()))
        );

        // input_voice
        let node: ActionNode = serde_json::from_str(
            r#"{"type":"input_voice","scene":"order_scene","timeout_ms":8000}"#,
        )
        .unwrap();
        assert!(
            matches!(node.action, EntryAction::InputVoice { scene, .. } if scene == "order_scene")
        );

        // torecord
        let node: ActionNode =
            serde_json::from_str(r#"{"type":"torecord","prompt":"leave_msg.wav","beep":true}"#)
                .unwrap();
        assert!(matches!(
            node.action,
            EntryAction::Torecord { beep: true, .. }
        ));

        // voip_bridge
        let node: ActionNode = serde_json::from_str(r#"{"type":"voip_bridge","create_room_uri":"https://voip.example.com/room","headers":{"Authorization":"Bearer x"}}"#).unwrap();
        assert!(
            matches!(node.action, EntryAction::VoipBridge { create_room_uri, headers, .. }
            if create_room_uri == "https://voip.example.com/room"
            && headers.get("Authorization") == Some(&"Bearer x".into()))
        );

        // collect_dtmf
        let node: ActionNode =
            serde_json::from_str(r#"{"type":"collect_dtmf","min_digits":1,"max_digits":4}"#)
                .unwrap();
        assert!(matches!(
            node.action,
            EntryAction::CollectDtmf { min_digits: 1, .. }
        ));

        // input_phone
        let node: ActionNode =
            serde_json::from_str(r#"{"type":"input_phone","prompt":"input_phone.wav"}"#).unwrap();
        assert!(matches!(node.action, EntryAction::InputPhone { .. }));
    }
}
