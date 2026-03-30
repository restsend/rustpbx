use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EventPriority {
    Realtime = 1,
    LocalRule = 2,
    Application = 3,
}

impl Default for EventPriority {
    fn default() -> Self {
        EventPriority::LocalRule
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DtmfAction {
    Passthrough,
    ExecuteRule(String),
    NotifyApp { priority: EventPriority },
    PassthroughAndNotify { priority: EventPriority },
    Consume { priority: EventPriority },
}

impl Default for DtmfAction {
    fn default() -> Self {
        DtmfAction::Passthrough
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LocalAction {
    TransferToQueue(String),
    TransferToNumber(String),
    PlayAnnouncement(String),
    StartRecording,
    StopRecording,
    Voicemail,
    Hangup,
    Hold,
    Unhold,
    SendToPeer { content_type: String, body: String },
    NotifyRwi { event: String },
    Sequence(Vec<Box<LocalAction>>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DtmfRule {
    pub name: String,
    pub pattern: String,
    pub match_mode: DtmfMatchMode,
    pub context: DtmfContext,
    pub action: DtmfAction,
    pub enabled: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DtmfMatchMode {
    Exact,
    Prefix,
    Regex(String),
}

impl Default for DtmfMatchMode {
    fn default() -> Self {
        DtmfMatchMode::Exact
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DtmfContext {
    Any,
    AgentOnly,
    CustomerOnly,
    CallState(String),
}

impl Default for DtmfContext {
    fn default() -> Self {
        DtmfContext::Any
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubscriptionLevel {
    BusinessOnly,
    MediaAware,
    FullControl,
}

impl Default for SubscriptionLevel {
    fn default() -> Self {
        SubscriptionLevel::BusinessOnly
    }
}

impl SubscriptionLevel {
    pub fn should_report(&self, priority: EventPriority) -> bool {
        match self {
            SubscriptionLevel::BusinessOnly => {
                matches!(priority, EventPriority::Application)
            }
            SubscriptionLevel::MediaAware => {
                matches!(priority, EventPriority::LocalRule | EventPriority::Application)
            }
            SubscriptionLevel::FullControl => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartRoutingConfig {
    pub default_priority: EventPriority,
    pub dtmf_rules: Vec<DtmfRule>,
    pub message_rules: Vec<MessageRule>,
    pub fallback_rules: FallbackRules,
    pub enable_local_engine: bool,
    pub rwi_timeout_ms: u64,
}

impl Default for SmartRoutingConfig {
    fn default() -> Self {
        Self {
            default_priority: EventPriority::LocalRule,
            dtmf_rules: default_dtmf_rules(),
            message_rules: default_message_rules(),
            fallback_rules: FallbackRules::default_callcenter(),
            enable_local_engine: true,
            rwi_timeout_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRule {
    pub name: String,
    pub conditions: MessageMatchConditions,
    pub action: MessageAction,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMatchConditions {
    pub content_type: Option<String>,
    pub json_path: Option<String>,
    pub json_values: Option<Vec<String>>,
    pub body_regex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageAction {
    AutoReply { code: u16, body: Option<String> },
    ForwardToRwi { 
        wait_response: bool,
        timeout_ms: u64,
    },
    ExecuteLocal(LocalAction),
    ForwardToPeer,
    ForwardBoth { 
        to_peer: bool,
        to_rwi: bool,
        rwi_async: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackRules {
    pub dtmf_handlers: HashMap<String, LocalAction>,
    pub default_action: LocalAction,
}

impl FallbackRules {
    pub fn default_callcenter() -> Self {
        let mut handlers = HashMap::new();
        handlers.insert("0".to_string(), LocalAction::TransferToQueue("operator".to_string()));
        handlers.insert("1".to_string(), LocalAction::TransferToQueue("sales".to_string()));
        handlers.insert("2".to_string(), LocalAction::TransferToQueue("support".to_string()));
        handlers.insert("*".to_string(), LocalAction::Hold);
        handlers.insert("#".to_string(), LocalAction::Unhold);
        handlers.insert("*9".to_string(), LocalAction::Voicemail);
        handlers.insert("00".to_string(), LocalAction::Hangup);
        
        Self {
            dtmf_handlers: handlers,
            default_action: LocalAction::PlayAnnouncement("invalid_option.wav".to_string()),
        }
    }
}

fn default_dtmf_rules() -> Vec<DtmfRule> {
    vec![
        DtmfRule {
            name: "conference_invite".to_string(),
            pattern: "*9".to_string(),
            match_mode: DtmfMatchMode::Exact,
            context: DtmfContext::AgentOnly,
            action: DtmfAction::Consume { priority: EventPriority::Application },
            enabled: true,
            description: None,
        },
        DtmfRule {
            name: "supervisor_help".to_string(),
            pattern: "*0".to_string(),
            match_mode: DtmfMatchMode::Exact,
            context: DtmfContext::AgentOnly,
            action: DtmfAction::NotifyApp { priority: EventPriority::Application },
            enabled: true,
            description: None,
        },
        DtmfRule {
            name: "hold".to_string(),
            pattern: "*".to_string(),
            match_mode: DtmfMatchMode::Exact,
            context: DtmfContext::Any,
            action: DtmfAction::ExecuteRule("hold_toggle".to_string()),
            enabled: true,
            description: None,
        },
        DtmfRule {
            name: "default_digits".to_string(),
            pattern: "[0-9]".to_string(),
            match_mode: DtmfMatchMode::Regex("^[0-9]$".to_string()),
            context: DtmfContext::Any,
            action: DtmfAction::Passthrough,
            enabled: true,
            description: None,
        },
    ]
}

fn default_message_rules() -> Vec<MessageRule> {
    vec![
        MessageRule {
            name: "conference_control".to_string(),
            conditions: MessageMatchConditions {
                content_type: Some("application/json".to_string()),
                json_path: Some("$.action".to_string()),
                json_values: Some(vec![
                    "invite_participant".to_string(),
                    "kick_participant".to_string(),
                    "mute_participant".to_string(),
                    "unmute_participant".to_string(),
                    "start_recording".to_string(),
                    "stop_recording".to_string(),
                ]),
                body_regex: None,
            },
            action: MessageAction::ForwardToRwi {
                wait_response: false,
                timeout_ms: 5000,
            },
            enabled: true,
        },
        MessageRule {
            name: "status_query".to_string(),
            conditions: MessageMatchConditions {
                content_type: Some("application/json".to_string()),
                json_path: Some("$.action".to_string()),
                json_values: Some(vec![
                    "get_call_status".to_string(),
                    "get_queue_position".to_string(),
                    "get_participant_list".to_string(),
                ]),
                body_regex: None,
            },
            action: MessageAction::ForwardToRwi {
                wait_response: true,
                timeout_ms: 3000,
            },
            enabled: true,
        },
    ]
}

#[derive(Debug, Clone)]
pub enum RoutingDecision {
    AutoProcess,
    ExecuteLocal(LocalAction),
    NotifyRwi { priority: EventPriority },
    Hybrid {
        local: Option<LocalAction>,
        notify: Option<EventPriority>,
    },
}

pub struct SmartRouter {
    config: SmartRoutingConfig,
    rwi_connected: bool,
    subscription_level: SubscriptionLevel,
}

impl SmartRouter {
    pub fn new(config: SmartRoutingConfig) -> Self {
        Self {
            config,
            rwi_connected: true,
            subscription_level: SubscriptionLevel::BusinessOnly,
        }
    }

    pub fn set_rwi_connected(&mut self, connected: bool) {
        self.rwi_connected = connected;
    }

    pub fn set_subscription_level(&mut self, level: SubscriptionLevel) {
        self.subscription_level = level;
    }

    pub fn route_dtmf(&self, digit: &str, is_agent: bool) -> RoutingDecision {
        if !self.rwi_connected {
            return self.fallback_dtmf(digit);
        }

        for rule in &self.config.dtmf_rules {
            if !rule.enabled {
                continue;
            }
            if !self.matches_context(&rule.context, is_agent) {
                continue;
            }
            if !self.matches_dtmf_pattern(digit, &rule.pattern, &rule.match_mode) {
                continue;
            }

            return self.dtmf_action_to_decision(&rule.action);
        }

        RoutingDecision::AutoProcess
    }

    pub fn route_message(&self, content_type: &str, body: &str) -> RoutingDecision {
        if !self.rwi_connected {
            return RoutingDecision::AutoProcess;
        }

        for rule in &self.config.message_rules {
            if !rule.enabled {
                continue;
            }
            if !self.matches_message_conditions(content_type, body, &rule.conditions) {
                continue;
            }

            return self.message_action_to_decision(&rule.action);
        }

        RoutingDecision::NotifyRwi {
            priority: self.config.default_priority,
        }
    }

    fn matches_context(&self, context: &DtmfContext, is_agent: bool) -> bool {
        match context {
            DtmfContext::Any => true,
            DtmfContext::AgentOnly => is_agent,
            DtmfContext::CustomerOnly => !is_agent,
            DtmfContext::CallState(_) => true,
        }
    }

    fn matches_dtmf_pattern(&self, digit: &str, pattern: &str, mode: &DtmfMatchMode) -> bool {
        match mode {
            DtmfMatchMode::Exact => digit == pattern,
            DtmfMatchMode::Prefix => digit.starts_with(pattern),
            DtmfMatchMode::Regex(_regex) => {
                digit == pattern || (pattern == "[0-9]" && digit.chars().all(|c| c.is_ascii_digit()))
            }
        }
    }

    fn matches_message_conditions(
        &self,
        content_type: &str,
        body: &str,
        conditions: &MessageMatchConditions,
    ) -> bool {
        if let Some(ref ct) = conditions.content_type {
            if content_type != ct {
                return false;
            }
        }

        if let (Some(path), Some(values)) = (&conditions.json_path, &conditions.json_values) {
            if path == "$.action" {
                for value in values {
                    if body.contains(&format!("\"action\":\"{}\"", value))
                        || body.contains(&format!("\"action\": \"{}\"", value))
                    {
                        return true;
                    }
                }
                return false;
            }
        }

        true
    }

    fn dtmf_action_to_decision(&self, action: &DtmfAction) -> RoutingDecision {
        match action {
            DtmfAction::Passthrough => RoutingDecision::AutoProcess,
            DtmfAction::ExecuteRule(rule) => {
                RoutingDecision::ExecuteLocal(LocalAction::NotifyRwi { event: rule.clone() })
            }
            DtmfAction::NotifyApp { priority } => {
                if self.subscription_level.should_report(*priority) {
                    RoutingDecision::NotifyRwi { priority: *priority }
                } else {
                    RoutingDecision::AutoProcess
                }
            }
            DtmfAction::PassthroughAndNotify { priority } => {
                if self.subscription_level.should_report(*priority) {
                    RoutingDecision::Hybrid {
                        local: None,
                        notify: Some(*priority),
                    }
                } else {
                    RoutingDecision::AutoProcess
                }
            }
            DtmfAction::Consume { priority } => {
                if self.subscription_level.should_report(*priority) {
                    RoutingDecision::NotifyRwi { priority: *priority }
                } else {
                    RoutingDecision::AutoProcess
                }
            }
        }
    }

    fn message_action_to_decision(&self, action: &MessageAction) -> RoutingDecision {
        match action {
            MessageAction::AutoReply { .. } => RoutingDecision::AutoProcess,
            MessageAction::ForwardToRwi { wait_response: _, timeout_ms: _ } => {
                RoutingDecision::NotifyRwi {
                    priority: EventPriority::Application,
                }
            }
            MessageAction::ExecuteLocal(local_action) => {
                RoutingDecision::ExecuteLocal(local_action.clone())
            }
            MessageAction::ForwardToPeer => {
                RoutingDecision::Hybrid {
                    local: Some(LocalAction::SendToPeer {
                        content_type: "application/json".to_string(),
                        body: String::new(),
                    }),
                    notify: None,
                }
            }
            MessageAction::ForwardBoth { to_peer, to_rwi, rwi_async: _ } => {
                RoutingDecision::Hybrid {
                    local: if *to_peer {
                        Some(LocalAction::SendToPeer {
                            content_type: "application/json".to_string(),
                            body: String::new(),
                        })
                    } else {
                        None
                    },
                    notify: if *to_rwi {
                        Some(EventPriority::Application)
                    } else {
                        None
                    },
                }
            }
        }
    }

    fn fallback_dtmf(&self, digit: &str) -> RoutingDecision {
        if let Some(action) = self.config.fallback_rules.dtmf_handlers.get(digit) {
            RoutingDecision::ExecuteLocal(action.clone())
        } else {
            RoutingDecision::ExecuteLocal(self.config.fallback_rules.default_action.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dtmf_exact_match() {
        let config = SmartRoutingConfig::default();
        let router = SmartRouter::new(config);

        let decision = router.route_dtmf("*9", true);
        match decision {
            RoutingDecision::NotifyRwi { priority } => {
                assert_eq!(priority, EventPriority::Application);
            }
            _ => panic!("Expected NotifyRwi for *9"),
        }
    }

    #[test]
    fn test_dtmf_passthrough() {
        let config = SmartRoutingConfig::default();
        let router = SmartRouter::new(config);

        let decision = router.route_dtmf("5", false);
        match decision {
            RoutingDecision::AutoProcess => {}
            _ => panic!("Expected AutoProcess for regular digit"),
        }
    }

    #[test]
    fn test_fallback_mode() {
        let config = SmartRoutingConfig::default();
        let mut router = SmartRouter::new(config);
        
        router.set_rwi_connected(false);

        let decision = router.route_dtmf("0", false);
        match decision {
            RoutingDecision::ExecuteLocal(LocalAction::TransferToQueue(queue)) => {
                assert_eq!(queue, "operator");
            }
            _ => panic!("Expected fallback transfer"),
        }
    }

    #[test]
    fn test_subscription_level() {
        let level = SubscriptionLevel::BusinessOnly;
        
        assert!(!level.should_report(EventPriority::Realtime));
        assert!(!level.should_report(EventPriority::LocalRule));
        assert!(level.should_report(EventPriority::Application));

        let level = SubscriptionLevel::MediaAware;
        assert!(!level.should_report(EventPriority::Realtime));
        assert!(level.should_report(EventPriority::LocalRule));
        assert!(level.should_report(EventPriority::Application));

        let level = SubscriptionLevel::FullControl;
        assert!(level.should_report(EventPriority::Realtime));
        assert!(level.should_report(EventPriority::LocalRule));
        assert!(level.should_report(EventPriority::Application));
    }

    #[test]
    fn test_dtmf_all_fallback_handlers() {
        let config = SmartRoutingConfig::default();
        let mut router = SmartRouter::new(config);
        router.set_rwi_connected(false);

        let test_cases = vec![
            ("0", "TransferToQueue"),
            ("1", "TransferToQueue"),
            ("2", "TransferToQueue"),
            ("*", "Hold"),
            ("#", "Unhold"),
            ("*9", "Voicemail"),
            ("00", "Hangup"),
        ];

        for (digit, expected_variant) in test_cases {
            let decision = router.route_dtmf(digit, false);
            match decision {
                RoutingDecision::ExecuteLocal(action) => {
                    let actual = format!("{:?}", action);
                    assert!(actual.starts_with(expected_variant),
                        "DTMF {} should trigger {}, got {}", digit, expected_variant, actual);
                }
                _ => panic!("Expected ExecuteLocal for DTMF {}", digit),
            }
        }
    }

    #[test]
    fn test_dtmf_fallback_default_action() {
        let config = SmartRoutingConfig::default();
        let mut router = SmartRouter::new(config);
        router.set_rwi_connected(false);

        let decision = router.route_dtmf("9", false);
        match decision {
            RoutingDecision::ExecuteLocal(LocalAction::PlayAnnouncement(file)) => {
                assert_eq!(file, "invalid_option.wav");
            }
            _ => panic!("Expected default PlayAnnouncement for unknown DTMF"),
        }
    }

    #[test]
    fn test_dtmf_agent_only_context() {
        let config = SmartRoutingConfig::default();
        let router = SmartRouter::new(config);

        let decision = router.route_dtmf("*9", false);
        assert!(matches!(decision, RoutingDecision::AutoProcess),
            "Agent-only DTMF should not match for customer");
    }

    #[test]
    fn test_message_routing_default() {
        let config = SmartRoutingConfig::default();
        let router = SmartRouter::new(config);

        let decision = router.route_message("text/plain", "Hello");
        match decision {
            RoutingDecision::NotifyRwi { priority } => {
                assert_eq!(priority, EventPriority::LocalRule);
            }
            _ => panic!("Expected NotifyRwi for unknown message"),
        }
    }

    #[test]
    fn test_message_routing_when_disconnected() {
        let config = SmartRoutingConfig::default();
        let mut router = SmartRouter::new(config);
        router.set_rwi_connected(false);

        let decision = router.route_message("application/json", r#"{"action":"test"}"#);
        assert!(matches!(decision, RoutingDecision::AutoProcess),
            "Messages should AutoProcess when RWI disconnected");
    }

    #[test]
    fn test_message_conference_control_routing() {
        let config = SmartRoutingConfig::default();
        let router = SmartRouter::new(config);

        let test_cases = vec![
            "invite_participant",
            "kick_participant",
            "mute_participant",
            "start_recording",
        ];

        for action in test_cases {
            let body = format!(r#"{{"action":"{}"}}"#, action);
            let decision = router.route_message("application/json", &body);
            assert!(matches!(decision, RoutingDecision::NotifyRwi { .. }),
                "Conference action {} should be forwarded to RWI", action);
        }
    }

    #[test]
    fn test_message_status_query_routing() {
        let config = SmartRoutingConfig::default();
        let router = SmartRouter::new(config);

        let body = r#"{"action":"get_call_status"}"#;
        let decision = router.route_message("application/json", body);
        assert!(matches!(decision, RoutingDecision::NotifyRwi { .. }),
            "Status queries should be forwarded to RWI");
    }

    #[test]
    fn test_smart_routing_config_default() {
        let config = SmartRoutingConfig::default();
        
        assert!(config.enable_local_engine);
        assert_eq!(config.default_priority, EventPriority::LocalRule);
        assert!(!config.dtmf_rules.is_empty());
        assert!(!config.message_rules.is_empty());
        assert!(!config.fallback_rules.dtmf_handlers.is_empty());
        assert_eq!(config.rwi_timeout_ms, 5000);
    }

    #[test]
    fn test_dtmf_rule_disabled() {
        let mut config = SmartRoutingConfig::default();
        for rule in &mut config.dtmf_rules {
            rule.enabled = false;
        }
        
        let router = SmartRouter::new(config);
        let decision = router.route_dtmf("*9", true);
        assert!(matches!(decision, RoutingDecision::AutoProcess),
            "Disabled rules should fall through to default");
    }

    #[test]
    fn test_event_priority_ordering() {
        assert!((EventPriority::Realtime as i32) < (EventPriority::LocalRule as i32));
        assert!((EventPriority::LocalRule as i32) < (EventPriority::Application as i32));
    }

    #[test]
    fn test_smart_router_state_management() {
        let config = SmartRoutingConfig::default();
        let mut router = SmartRouter::new(config);

        router.set_rwi_connected(false);
        router.set_subscription_level(SubscriptionLevel::FullControl);
        
        assert!(router.subscription_level.should_report(EventPriority::Realtime));
        assert!(router.subscription_level.should_report(EventPriority::LocalRule));
        assert!(router.subscription_level.should_report(EventPriority::Application));
    }

    #[test]
    fn test_dtmf_pattern_matching_prefix() {
        let router = SmartRouter::new(SmartRoutingConfig::default());

        let matches = router.matches_dtmf_pattern("123", "12", &DtmfMatchMode::Prefix);
        assert!(matches, "Prefix pattern should match");

        let matches = router.matches_dtmf_pattern("123", "23", &DtmfMatchMode::Prefix);
        assert!(!matches, "Non-prefix should not match");
    }

    #[test]
    fn test_dtmf_pattern_matching_exact() {
        let router = SmartRouter::new(SmartRoutingConfig::default());

        let matches = router.matches_dtmf_pattern("*9", "*9", &DtmfMatchMode::Exact);
        assert!(matches, "Exact pattern should match");

        let matches = router.matches_dtmf_pattern("*9", "*0", &DtmfMatchMode::Exact);
        assert!(!matches, "Different pattern should not match");
    }

    #[test]
    fn test_dtmf_pattern_matching_regex() {
        let router = SmartRouter::new(SmartRoutingConfig::default());

        let matches = router.matches_dtmf_pattern("5", "[0-9]", &DtmfMatchMode::Regex("^[0-9]$".to_string()));
        assert!(matches, "Digit regex should match single digit");

        let matches = router.matches_dtmf_pattern("a", "[0-9]", &DtmfMatchMode::Regex("^[0-9]$".to_string()));
        assert!(!matches, "Non-digit should not match digit regex");
    }

    #[test]
    fn test_message_conditions_content_type() {
        let router = SmartRouter::new(SmartRoutingConfig::default());

        let conditions = MessageMatchConditions {
            content_type: Some("application/json".to_string()),
            json_path: None,
            json_values: None,
            body_regex: None,
        };

        assert!(router.matches_message_conditions("application/json", "{}", &conditions),
            "Should match exact content type");
        assert!(!router.matches_message_conditions("text/plain", "{}", &conditions),
            "Should not match different content type");
    }

    #[test]
    fn test_message_conditions_json_path() {
        let router = SmartRouter::new(SmartRoutingConfig::default());

        let conditions = MessageMatchConditions {
            content_type: Some("application/json".to_string()),
            json_path: Some("$.action".to_string()),
            json_values: Some(vec!["invite_participant".to_string(), "kick_participant".to_string()]),
            body_regex: None,
        };

        assert!(router.matches_message_conditions(
            "application/json", 
            r#"{"action":"invite_participant"}"#, 
            &conditions
        ), "Should match JSON action");

        assert!(!router.matches_message_conditions(
            "application/json", 
            r#"{"action":"unknown_action"}"#, 
            &conditions
        ), "Should not match unknown action");
    }

    #[test]
    fn test_local_action_variants() {
        let actions = vec![
            LocalAction::TransferToQueue("test".to_string()),
            LocalAction::TransferToNumber("1234".to_string()),
            LocalAction::PlayAnnouncement("test.wav".to_string()),
            LocalAction::StartRecording,
            LocalAction::StopRecording,
            LocalAction::Hold,
            LocalAction::Unhold,
            LocalAction::Hangup,
            LocalAction::Voicemail,
            LocalAction::NotifyRwi { event: "test".to_string() },
            LocalAction::SendToPeer { 
                content_type: "text/plain".to_string(), 
                body: "hello".to_string() 
            },
            LocalAction::Sequence(vec![]),
        ];

        for action in actions {
            let cloned = action.clone();
            assert_eq!(format!("{:?}", action), format!("{:?}", cloned));
        }
    }

    #[test]
    fn test_dtmf_action_variants() {
        let _actions = vec![
            DtmfAction::Passthrough,
            DtmfAction::ExecuteRule("rule1".to_string()),
            DtmfAction::NotifyApp { priority: EventPriority::Application },
            DtmfAction::PassthroughAndNotify { priority: EventPriority::Realtime },
            DtmfAction::Consume { priority: EventPriority::LocalRule },
        ];
    }

    #[test]
    fn test_dtmf_context_variants() {
        let router = SmartRouter::new(SmartRoutingConfig::default());
        
        assert!(router.matches_context(&DtmfContext::Any, true));
        assert!(router.matches_context(&DtmfContext::Any, false));

        assert!(router.matches_context(&DtmfContext::AgentOnly, true));
        assert!(!router.matches_context(&DtmfContext::AgentOnly, false));

        assert!(!router.matches_context(&DtmfContext::CustomerOnly, true));
        assert!(router.matches_context(&DtmfContext::CustomerOnly, false));

        assert!(router.matches_context(&DtmfContext::CallState("any".to_string()), true));
        assert!(router.matches_context(&DtmfContext::CallState("any".to_string()), false));
    }

    #[test]
    fn test_message_action_variants() {
        let _actions = vec![
            MessageAction::AutoReply { code: 200, body: None },
            MessageAction::AutoReply { code: 486, body: Some("Busy".to_string()) },
            MessageAction::ForwardToRwi { wait_response: true, timeout_ms: 3000 },
            MessageAction::ForwardToRwi { wait_response: false, timeout_ms: 5000 },
            MessageAction::ExecuteLocal(LocalAction::Hold),
            MessageAction::ForwardToPeer,
            MessageAction::ForwardBoth { to_peer: true, to_rwi: true, rwi_async: false },
        ];
    }

    #[test]
    fn test_routing_decision_variants() {
        let _decisions = vec![
            RoutingDecision::AutoProcess,
            RoutingDecision::ExecuteLocal(LocalAction::Hold),
            RoutingDecision::NotifyRwi { priority: EventPriority::Application },
            RoutingDecision::Hybrid { local: None, notify: None },
            RoutingDecision::Hybrid { 
                local: Some(LocalAction::Hold), 
                notify: Some(EventPriority::Application) 
            },
        ];
    }

    #[test]
    fn test_default_trait_implementations() {
        let priority: EventPriority = Default::default();
        assert_eq!(priority, EventPriority::LocalRule);

        let match_mode: DtmfMatchMode = Default::default();
        assert!(matches!(match_mode, DtmfMatchMode::Exact));

        let context: DtmfContext = Default::default();
        assert!(matches!(context, DtmfContext::Any));

        let level: SubscriptionLevel = Default::default();
        assert!(matches!(level, SubscriptionLevel::BusinessOnly));

        let dtmf_action: DtmfAction = Default::default();
        assert!(matches!(dtmf_action, DtmfAction::Passthrough));
    }
}
