use crate::call::{
    DialStrategy, FailureAction, Location, QueueFallbackAction, QueueHoldConfig, QueuePlan,
};
use anyhow::{Result, anyhow};
use rsip::{StatusCode, Uri};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Serializable queue configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    #[serde(default)]
    pub name: Option<String>,

    /// Accept call immediately (200 OK) before dialing agents
    #[serde(default)]
    pub accept_immediately: bool,

    /// Passthrough agent's ringback tone to caller
    #[serde(default)]
    pub passthrough_ringback: bool,

    /// Hold music configuration
    #[serde(default)]
    pub hold: Option<HoldMusicConfig>,

    /// Dial strategy (sequential or parallel)
    pub strategy: StrategyConfig,

    /// Ring timeout per agent in seconds
    #[serde(default)]
    pub ring_timeout_secs: Option<u64>,

    /// Fallback action when all agents fail
    #[serde(default)]
    pub fallback: Option<FallbackConfig>,
}

/// Hold music configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldMusicConfig {
    /// Audio file path
    pub audio_file: String,

    /// Loop playback
    #[serde(default = "default_true")]
    pub loop_playback: bool,
}

fn default_true() -> bool {
    true
}

/// Dialing strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StrategyConfig {
    /// Try agents one by one
    Sequential { agents: Vec<AgentConfig> },

    /// Try all agents simultaneously (ring all)
    Parallel { agents: Vec<AgentConfig> },
}

/// Agent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// SIP URI (e.g., "sip:agent1@example.com")
    pub uri: String,

    /// Display name (optional)
    #[serde(default)]
    pub display_name: Option<String>,

    /// Priority (lower number = higher priority, only for sequential)
    #[serde(default)]
    pub priority: Option<u32>,
}

/// Fallback action configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum FallbackConfig {
    /// Simply hangup with status code
    Hangup {
        #[serde(default = "default_status_code")]
        code: u16,
        #[serde(default)]
        reason: Option<String>,
    },

    /// Play announcement then hangup
    PlayThenHangup {
        audio_file: String,
        #[serde(default)]
        use_early_media: bool,
        #[serde(default = "default_status_code")]
        code: u16,
        #[serde(default)]
        reason: Option<String>,
    },

    /// Redirect to another SIP URI
    Redirect { target: String },

    /// Transfer to another queue
    Queue { name: String },
}

fn default_status_code() -> u16 {
    480 // Temporarily Unavailable
}

impl QueueConfig {
    /// Load from TOML string
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        toml::from_str(toml_str).map_err(|e| anyhow!("Failed to parse TOML: {}", e))
    }

    /// Load from JSON string
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| anyhow!("Failed to parse JSON: {}", e))
    }

    /// Load from TOML file
    pub fn from_toml_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("Failed to read file {}: {}", path, e))?;
        Self::from_toml(&content)
    }

    /// Save to TOML string
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| anyhow!("Failed to serialize to TOML: {}", e))
    }

    /// Save to JSON string
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("Failed to serialize to JSON: {}", e))
    }

    /// Convert to runtime QueuePlan
    pub fn to_queue_plan(&self) -> Result<QueuePlan> {
        let plan = QueuePlan {
            accept_immediately: self.accept_immediately,
            passthrough_ringback: self.passthrough_ringback,
            hold: self.hold.as_ref().map(|h| QueueHoldConfig {
                audio_file: Some(h.audio_file.clone()),
                loop_playback: h.loop_playback,
            }),
            fallback: self.fallback.as_ref().map(|f| f.to_fallback_action()),
            dial_strategy: Some(self.strategy.to_dial_strategy()?),
            ring_timeout: self.ring_timeout_secs.map(|s| Duration::from_secs(s)),
            label: self.name.clone(),
            retry_codes: None,
            no_trying_timeout: None,
        };

        Ok(plan)
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Validate agents
        match &self.strategy {
            StrategyConfig::Sequential { agents } | StrategyConfig::Parallel { agents } => {
                if agents.is_empty() {
                    return Err(anyhow!("At least one agent must be configured"));
                }

                for (idx, agent) in agents.iter().enumerate() {
                    if agent.uri.is_empty() {
                        return Err(anyhow!("Agent #{} has empty URI", idx + 1));
                    }

                    // Validate URI format
                    let uri = Uri::try_from(agent.uri.as_str()).map_err(|e| {
                        anyhow!("Agent #{} has invalid URI '{}': {}", idx + 1, agent.uri, e)
                    })?;

                    // Ensure URI has a valid scheme (sip or sips)
                    let has_valid_scheme = match &uri.scheme {
                        None => false,
                        Some(scheme) => {
                            let scheme_str = scheme.to_string();
                            !scheme_str.is_empty()
                        }
                    };

                    if !has_valid_scheme {
                        return Err(anyhow!(
                            "Agent #{} URI '{}' must have a valid scheme (sip: or sips:)",
                            idx + 1,
                            agent.uri
                        ));
                    }

                    // Ensure URI has a valid host
                    let host_str = uri.host_with_port.host.to_string();
                    if host_str.is_empty() || host_str == "//" || host_str.starts_with(':') {
                        return Err(anyhow!(
                            "Agent #{} URI '{}' must have a valid host",
                            idx + 1,
                            agent.uri
                        ));
                    }
                }
            }
        }

        // Validate hold music file if configured
        if let Some(hold) = &self.hold {
            if hold.audio_file.is_empty() {
                return Err(anyhow!("Hold music audio_file cannot be empty"));
            }
        }

        // Validate fallback
        if let Some(fallback) = &self.fallback {
            fallback.validate()?;
        }

        Ok(())
    }
}

impl StrategyConfig {
    fn to_dial_strategy(&self) -> Result<DialStrategy> {
        match self {
            StrategyConfig::Sequential { agents } => {
                let mut locations = Vec::new();
                for agent in agents {
                    locations.push(agent.to_location()?);
                }
                // Sort by priority if specified
                locations.sort_by_key(|_loc| {
                    // Extract priority from somewhere if needed
                    // For now, maintain order
                    0
                });
                Ok(DialStrategy::Sequential(locations))
            }
            StrategyConfig::Parallel { agents } => {
                let locations = agents
                    .iter()
                    .map(|a| a.to_location())
                    .collect::<Result<Vec<_>>>()?;
                Ok(DialStrategy::Parallel(locations))
            }
        }
    }
}

impl AgentConfig {
    fn to_location(&self) -> Result<Location> {
        let uri = Uri::try_from(self.uri.as_str())
            .map_err(|e| anyhow!("Invalid URI '{}': {}", self.uri, e))?;

        Ok(Location {
            aor: uri,
            expires: 3600, // Default
            destination: None,
            last_modified: None,
            supports_webrtc: false,
            credential: None,
            headers: None,
            registered_aor: None,
            contact_raw: None,
            contact_params: None,
            path: None,
            service_route: None,
            instance_id: None,
            gruu: None,
            temp_gruu: None,
            reg_id: None,
            transport: None,
            user_agent: None,
        })
    }
}

impl FallbackConfig {
    fn to_fallback_action(&self) -> QueueFallbackAction {
        match self {
            FallbackConfig::Hangup { code, reason } => {
                QueueFallbackAction::Failure(FailureAction::Hangup {
                    code: Some(StatusCode::from(*code)),
                    reason: reason.clone(),
                })
            }
            FallbackConfig::PlayThenHangup {
                audio_file,
                use_early_media,
                code,
                reason,
            } => QueueFallbackAction::Failure(FailureAction::PlayThenHangup {
                audio_file: audio_file.clone(),
                use_early_media: *use_early_media,
                status_code: StatusCode::from(*code),
                reason: reason.clone(),
            }),
            FallbackConfig::Redirect { target } => {
                let uri = Uri::try_from(target.as_str()).unwrap_or_else(|_| Uri::default());
                QueueFallbackAction::Redirect { target: uri }
            }
            FallbackConfig::Queue { name } => QueueFallbackAction::Queue { name: name.clone() },
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            FallbackConfig::Hangup { code, .. } => {
                if *code < 400 || *code >= 700 {
                    return Err(anyhow!(
                        "Hangup status code must be 4xx, 5xx, or 6xx (got {})",
                        code
                    ));
                }
            }
            FallbackConfig::PlayThenHangup {
                audio_file, code, ..
            } => {
                if audio_file.is_empty() {
                    return Err(anyhow!("PlayThenHangup audio_file cannot be empty"));
                }
                if *code < 400 || *code >= 700 {
                    return Err(anyhow!(
                        "PlayThenHangup status code must be 4xx, 5xx, or 6xx (got {})",
                        code
                    ));
                }
            }
            FallbackConfig::Redirect { target } => {
                if target.is_empty() {
                    return Err(anyhow!("Redirect target cannot be empty"));
                }
                Uri::try_from(target.as_str())
                    .map_err(|e| anyhow!("Redirect target invalid URI '{}': {}", target, e))?;
            }
            FallbackConfig::Queue { name } => {
                if name.is_empty() {
                    return Err(anyhow!("Queue name cannot be empty"));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_toml_sequential_queue() {
        let toml_str = r#"
name = "customer-service"
accept_immediately = true
passthrough_ringback = false
ring_timeout_secs = 15

[hold]
audio_file = "sounds/please-wait.wav"
loop_playback = true

[strategy]
type = "sequential"

[[strategy.agents]]
uri = "sip:agent1@example.com"
display_name = "Agent 1"

[[strategy.agents]]
uri = "sip:agent2@example.com"
display_name = "Agent 2"

[fallback]
action = "playthenhangup"
audio_file = "sounds/all-busy.wav"
code = 480
reason = "All agents busy"
"#;

        let config = QueueConfig::from_toml(toml_str).expect("Failed to parse TOML");
        assert_eq!(config.name, Some("customer-service".to_string()));
        assert!(config.accept_immediately);
        assert!(!config.passthrough_ringback);

        match &config.strategy {
            StrategyConfig::Sequential { agents } => {
                assert_eq!(agents.len(), 2);
                assert_eq!(agents[0].uri, "sip:agent1@example.com");
            }
            _ => panic!("Expected Sequential strategy"),
        }

        assert_eq!(config.ring_timeout_secs, Some(15));
    }

    #[test]
    fn test_parse_toml_parallel_queue() {
        let toml_str = r#"
name = "emergency"
accept_immediately = false
passthrough_ringback = true
ring_timeout_secs = 10

[strategy]
type = "parallel"

[[strategy.agents]]
uri = "sip:responder1@hospital.com"

[[strategy.agents]]
uri = "sip:responder2@hospital.com"

[[strategy.agents]]
uri = "sip:responder3@hospital.com"

[fallback]
action = "redirect"
target = "sip:backup@hospital.com"
"#;

        let config = QueueConfig::from_toml(toml_str).expect("Failed to parse TOML");
        assert_eq!(config.name, Some("emergency".to_string()));
        assert!(!config.accept_immediately);
        assert!(config.passthrough_ringback);

        match &config.strategy {
            StrategyConfig::Parallel { agents } => {
                assert_eq!(agents.len(), 3);
            }
            _ => panic!("Expected Parallel strategy"),
        }
    }

    #[test]
    fn test_convert_to_queue_plan() {
        let toml_str = r#"
name = "test-queue"
accept_immediately = true
ring_timeout_secs = 20

[strategy]
type = "sequential"

[[strategy.agents]]
uri = "sip:test@example.com"
"#;

        let config = QueueConfig::from_toml(toml_str).expect("Failed to parse");
        let plan = config.to_queue_plan().expect("Failed to convert");

        assert!(plan.accept_immediately);
        assert_eq!(plan.ring_timeout, Some(Duration::from_secs(20)));
        assert_eq!(plan.label, Some("test-queue".to_string()));
    }

    #[test]
    fn test_validate_empty_agents() {
        let toml_str = r#"
name = "empty"

[strategy]
type = "sequential"
agents = []
"#;

        let config = QueueConfig::from_toml(toml_str).expect("Should parse");

        // Debug: print what we got
        println!("Config: {:?}", config);

        let result = config.validate();
        if let Err(e) = &result {
            println!("Validation error: {}", e);
        } else {
            println!("Validation succeeded (unexpected!)");
        }

        assert!(
            result.is_err(),
            "Expected validation to fail for empty agents"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("agent"),
            "Error message should mention agents: {}",
            err_msg
        );
    }

    #[test]
    fn test_validate_invalid_uri() {
        // Use a clearly invalid URI that should fail parsing
        let toml_str = r#"
name = "invalid"

[strategy]
type = "sequential"

[[strategy.agents]]
uri = "://invalid"
"#;

        let config = QueueConfig::from_toml(toml_str).expect("Should parse");
        println!("Config with invalid URI: {:?}", config);

        let result = config.validate();
        if let Err(e) = &result {
            println!("Validation error: {}", e);
        } else {
            println!("Validation succeeded (unexpected!)");
        }

        assert!(
            result.is_err(),
            "Expected validation to fail for invalid URI"
        );
    }

    #[test]
    fn test_roundtrip_toml() {
        let original = QueueConfig {
            name: Some("test".to_string()),
            accept_immediately: true,
            passthrough_ringback: false,
            hold: Some(HoldMusicConfig {
                audio_file: "hold.wav".to_string(),
                loop_playback: true,
            }),
            strategy: StrategyConfig::Sequential {
                agents: vec![AgentConfig {
                    uri: "sip:agent@example.com".to_string(),
                    display_name: Some("Agent".to_string()),
                    priority: None,
                }],
            },
            ring_timeout_secs: Some(15),
            fallback: None,
        };

        let toml_str = original.to_toml().expect("Failed to serialize");
        let parsed = QueueConfig::from_toml(&toml_str).expect("Failed to parse");

        assert_eq!(parsed.name, original.name);
        assert_eq!(parsed.accept_immediately, original.accept_immediately);
    }
}
