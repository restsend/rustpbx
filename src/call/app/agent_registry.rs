//! Agent Registry - Unified Agent Registry and Presence Management
//!
//! Provides a centralized registry for agent management with multiple backend implementations:
//! - MemoryRegistry: In-memory storage (single node, testing)
//! - DbRegistry: SeaORM database persistence
//! - HttpRegistry: External HTTP API integration
//!
//! All implementations share the same AgentRegistry trait for consistent behavior.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;

// Re-export submodules
pub mod memory;
pub mod db;
pub mod http;

// Re-export types
pub use memory::MemoryRegistry;
pub use db::DbRegistry;
pub use http::HttpRegistry;

// ===================================================================
// Presence State Machine
// ===================================================================

/// Standard presence states based on RFC 3856 + CC extensions
/// Supports custom states for flexible agent status management
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PresenceState {
    /// Agent is offline and cannot receive calls
    Offline,
    /// Agent is online but not ready (e.g., logged in but on break)
    Away,
    /// Agent is online and ready to receive calls
    Available,
    /// Agent is being rang for a call
    Ringing,
    /// Agent is on an active call
    Busy,
    /// Agent is in wrap-up after a call
    Wrapup,
    /// Agent is in a meeting/training (Do Not Disturb)
    Dnd,
    /// Custom state for extended use cases (e.g., "training", "lunch", "meeting")
    Custom(String),
}

impl PresenceState {
    /// Check if agent can receive calls
    pub fn can_receive_calls(&self) -> bool {
        matches!(self, PresenceState::Available)
    }

    /// Check if agent is in a call-related state
    pub fn is_call_active(&self) -> bool {
        matches!(self, PresenceState::Ringing | PresenceState::Busy)
    }

    /// Check if state is a custom state
    pub fn is_custom(&self) -> bool {
        matches!(self, PresenceState::Custom(_))
    }

    /// Get the custom state name if it's a custom state
    pub fn custom_name(&self) -> Option<&str> {
        match self {
            PresenceState::Custom(name) => Some(name),
            _ => None,
        }
    }

    /// Get state name for API/DB storage
    pub fn as_str(&self) -> String {
        match self {
            PresenceState::Offline => "offline".to_string(),
            PresenceState::Away => "away".to_string(),
            PresenceState::Available => "available".to_string(),
            PresenceState::Ringing => "ringing".to_string(),
            PresenceState::Busy => "busy".to_string(),
            PresenceState::Wrapup => "wrapup".to_string(),
            PresenceState::Dnd => "dnd".to_string(),
            PresenceState::Custom(name) => format!("custom:{}", name),
        }
    }

    /// Parse state from string
    /// Supports custom states in format "custom:state_name"
    pub fn parse_state(s: &str) -> Option<Self> {
        match s {
            "offline" => Some(PresenceState::Offline),
            "away" => Some(PresenceState::Away),
            "available" => Some(PresenceState::Available),
            "ringing" => Some(PresenceState::Ringing),
            "busy" => Some(PresenceState::Busy),
            "wrapup" => Some(PresenceState::Wrapup),
            "dnd" => Some(PresenceState::Dnd),
            _ => {
                // Check for custom state format: "custom:state_name"
                if let Some(custom_name) = s.strip_prefix("custom:") {
                    if !custom_name.is_empty() {
                        Some(PresenceState::Custom(custom_name.to_string()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    }

    /// Get display name for UI
    pub fn display_name(&self) -> String {
        match self {
            PresenceState::Offline => "Offline".to_string(),
            PresenceState::Away => "Away".to_string(),
            PresenceState::Available => "Available".to_string(),
            PresenceState::Ringing => "Ringing".to_string(),
            PresenceState::Busy => "Busy".to_string(),
            PresenceState::Wrapup => "Wrap-up".to_string(),
            PresenceState::Dnd => "Do Not Disturb".to_string(),
            PresenceState::Custom(name) => name.clone(),
        }
    }
}

// ===================================================================
// Agent Record
// ===================================================================

/// Complete agent information stored in registry
#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub agent_id: String,
    pub display_name: String,
    pub uri: String, // SIP URI for calling
    pub skills: Vec<String>,
    pub max_concurrency: u32,
    pub current_calls: u32,
    pub presence: PresenceState,
    pub last_state_change: Instant,
    pub total_calls_handled: u64,
    pub total_talk_time_secs: u64,
    pub last_call_end: Option<Instant>,
    pub custom_data: HashMap<String, String>,
}

pub type AgentEventHandler = Box<dyn Fn(&AgentRecord) + Send + Sync>;

impl AgentRecord {
    /// Check if agent has capacity for new call
    pub fn has_capacity(&self) -> bool {
        self.current_calls < self.max_concurrency
            && self.presence.can_receive_calls()
    }

    /// Check if agent matches required skills
    pub fn has_skills(&self, required: &[String]) -> bool {
        if required.is_empty() {
            return true;
        }
        required.iter().all(|skill| self.skills.contains(skill))
    }

    /// Calculate idle time since last call
    pub fn idle_duration(&self) -> Duration {
        match self.last_call_end {
            Some(t) => t.elapsed(),
            None => Duration::from_secs(u64::MAX), // Never handled call = very idle
        }
    }
}

// ===================================================================
// Routing Strategy
// ===================================================================

/// Agent selection strategy for call distribution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum RoutingStrategy {
    /// Longest Idle Time - agent idle longest gets call first
    #[default]
    LongestIdle,
    /// Round Robin - distribute evenly across agents
    RoundRobin,
    /// Skill-based with priority (most skilled first)
    SkillBased,
    /// Least Calls Handled - agent with fewest calls gets priority
    LeastCalls,
    /// External - use RWI or Webhook for decision
    External,
}


impl RoutingStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingStrategy::LongestIdle => "longest_idle",
            RoutingStrategy::RoundRobin => "round_robin",
            RoutingStrategy::SkillBased => "skill_based",
            RoutingStrategy::LeastCalls => "least_calls",
            RoutingStrategy::External => "external",
        }
    }
}

// ===================================================================
// Agent Registry Trait
// ===================================================================

/// Core trait for agent registry implementations.
/// 
/// This trait abstracts over different storage backends (memory, database, HTTP API)
/// allowing the Queue and other components to work with any implementation.
#[async_trait]
pub trait AgentRegistry: Send + Sync {
    /// Register a new agent
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()>;

    /// Unregister an agent
    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()>;

    /// Get agent by ID
    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord>;

    /// List all agents
    async fn list_agents(&self) -> Vec<AgentRecord>;

    /// Update agent presence state
    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()>;

    /// Increment call count when agent receives call
    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()>;

    /// Decrement call count and update stats when call ends
    async fn end_call(&self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()>;

    /// Find available agents matching criteria
    async fn find_available_agents(
        &self,
        required_skills: &[String],
    ) -> Vec<AgentRecord>;

    /// Select best agent using specified strategy
    async fn select_agent(
        &self,
        required_skills: &[String],
        strategy: RoutingStrategy,
    ) -> Option<AgentRecord>;

    /// Resolve a target URI to a list of agent URIs.
    /// This is a hook for addons (like CC) to implement custom routing logic.
    /// For example, a skill-group addon can resolve "skill-group:sales" to
    /// actual agent SIP URIs.
    /// 
    /// Returns empty Vec if the URI is not recognized or cannot be resolved.
    async fn resolve_target(&self, target_uri: &str) -> Vec<String>;

    /// Check if state transition is valid
    fn is_valid_transition(from: &PresenceState, to: &PresenceState) -> bool where Self: Sized {
        match (from, to) {
            // Any state can go to Offline
            (_, PresenceState::Offline) => true,
            
            // Can go to Available from any non-active state
            (PresenceState::Away | PresenceState::Wrapup | PresenceState::Dnd | PresenceState::Custom(_), PresenceState::Available) => true,
            
            // Can go to Ringing only from Available
            (PresenceState::Available, PresenceState::Ringing) => true,
            
            // Can go to Busy only from Ringing
            (PresenceState::Ringing, PresenceState::Busy) => true,
            
            // Can go to Wrapup only from Busy
            (PresenceState::Busy, PresenceState::Wrapup) => true,
            
            // Can go to Away/Dnd from Available or Custom
            (PresenceState::Available | PresenceState::Custom(_), PresenceState::Away | PresenceState::Dnd) => true,
            
            // Can go to any Custom state from Available, Away, Dnd, or Wrapup
            (PresenceState::Available | PresenceState::Away | PresenceState::Dnd | PresenceState::Wrapup, PresenceState::Custom(_)) => true,
            
            // Same state is valid (no-op)
            (a, b) if a == b => true,
            
            // Everything else is invalid
            _ => false,
        }
    }

    /// Register event handler for agent state changes
    async fn on_state_change(
        &self,
        handler: Box<dyn Fn(&AgentRecord) + Send + Sync>,
    );
}

/// Factory for creating registry instances
pub enum RegistryType {
    /// In-memory registry (single node, testing)
    Memory,
    /// Database-backed registry (persistent)
    Db { connection_string: String },
    /// HTTP API registry (external system)
    Http { base_url: String, api_key: Option<String> },
}

impl RegistryType {
    /// Create a registry instance based on configuration
    pub async fn create(&self) -> anyhow::Result<Arc<dyn AgentRegistry>> {
        match self {
            RegistryType::Memory => {
                Ok(Arc::new(MemoryRegistry::new()))
            }
            RegistryType::Db { connection_string } => {
                let db = sea_orm::Database::connect(connection_string).await?;
                Ok(Arc::new(DbRegistry::new(db)))
            }
            RegistryType::Http { base_url, api_key } => {
                Ok(Arc::new(HttpRegistry::new(base_url.clone(), api_key.clone())))
            }
        }
    }
}

// ===================================================================
// Common helper functions
// ===================================================================

/// Select best agent from candidates using specified strategy
pub fn select_best_agent(
    mut candidates: Vec<AgentRecord>,
    strategy: RoutingStrategy,
    rr_counter: &mut u64,
) -> Option<AgentRecord> {
    if candidates.is_empty() {
        return None;
    }

    match strategy {
        RoutingStrategy::LongestIdle => {
            // Sort by idle duration (longest first)
            candidates.sort_by(|a, b| {
                b.idle_duration().cmp(&a.idle_duration())
            });
            candidates.into_iter().next()
        }
        RoutingStrategy::RoundRobin => {
            let idx = (*rr_counter as usize) % candidates.len();
            *rr_counter += 1;
            Some(candidates.remove(idx))
        }
        RoutingStrategy::SkillBased => {
            // Sort by number of matching skills (most first)
            candidates.sort_by(|a, b| {
                let a_matches = a.skills.len();
                let b_matches = b.skills.len();
                b_matches.cmp(&a_matches)
            });
            candidates.into_iter().next()
        }
        RoutingStrategy::LeastCalls => {
            // Sort by total calls handled (least first)
            candidates.sort_by(|a, b| {
                a.total_calls_handled.cmp(&b.total_calls_handled)
            });
            candidates.into_iter().next()
        }
        RoutingStrategy::External => {
            // For external routing, return all candidates
            // External system will make the decision
            candidates.into_iter().next()
        }
    }
}
