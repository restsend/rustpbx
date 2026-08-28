//! Memory Registry - In-memory agent registry implementation
//!
//! Suitable for:
//! - Single-node deployments
//! - Testing and development
//! - Scenarios where persistence is not required

use super::{AgentRecord, AgentRegistry, PresenceState, RoutingStrategy, select_best_agent};
use async_trait::async_trait;
use dashmap::DashMap;
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::info;

/// In-memory agent registry implementation
///
/// All data is stored in memory and lost on restart.
/// Suitable for single-node deployments and testing.
pub struct MemoryRegistry {
    agents: DashMap<String, AgentRecord>,
    /// Round-robin counter
    rr_counter: RwLock<u64>,
}

impl MemoryRegistry {
    pub fn new() -> Self {
        Self {
            agents: DashMap::new(),
            rr_counter: RwLock::new(0),
        }
    }
}

impl Default for MemoryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentRegistry for MemoryRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()> {
        if self.agents.contains_key(&agent_id) {
            anyhow::bail!("Agent {} already registered", agent_id);
        }

        let record = AgentRecord {
            agent_id: agent_id.clone(),
            display_name,
            uri,
            skills,
            max_concurrency,
            current_calls: 0,
            presence: PresenceState::Idle,
            last_state_change: Instant::now(),
            total_calls_handled: 0,
            total_talk_time_secs: 0,
            last_call_end: None,
            custom_data: HashMap::new(),
        };

        self.agents.insert(agent_id.clone(), record.clone());
        info!(agent_id = %agent_id, "Agent registered in memory");

        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        if self.agents.remove(agent_id).is_some() {
            info!(agent_id = %agent_id, "Agent unregistered from memory");
            Ok(())
        } else {
            anyhow::bail!("Agent {} not found", agent_id)
        }
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        self.agents.get(agent_id).map(|v| v.clone())
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        self.agents.iter().map(|e| e.value().clone()).collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        let mut agent = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent {} not found", agent_id))?;

        let old_state = agent.presence.clone();
        agent.presence = new_state;
        agent.last_state_change = Instant::now();

        info!(
            agent_id = %agent_id,
            old = %old_state.as_str(),
            new = %agent.presence.as_str(),
            "Presence updated in memory"
        );

        drop(agent);

        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        let mut agent = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent {} not found", agent_id))?;

        agent.current_calls += 1;
        agent.presence = PresenceState::Busy { call_id: None };
        agent.last_state_change = Instant::now();

        drop(agent);

        Ok(())
    }

    async fn end_call(&self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
        let mut agent = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent {} not found", agent_id))?;

        if agent.current_calls > 0 {
            agent.current_calls -= 1;
        }
        agent.total_calls_handled += 1;
        agent.total_talk_time_secs += talk_time_secs;
        agent.last_call_end = Some(Instant::now());

        // Auto-transition to Available if no more calls
        if agent.current_calls == 0 {
            agent.presence = PresenceState::Wrapup { call_id: None };
        }

        drop(agent);

        Ok(())
    }

    async fn find_available_agents(&self, required_skills: &[String]) -> Vec<AgentRecord> {
        self.agents
            .iter()
            .filter(|e| e.value().has_capacity() && e.value().has_skills(required_skills))
            .map(|e| e.value().clone())
            .collect()
    }

    async fn select_agent(
        &self,
        required_skills: &[String],
        strategy: RoutingStrategy,
    ) -> Option<AgentRecord> {
        let candidates = self.find_available_agents(required_skills).await;
        let mut rr_counter = self.rr_counter.write().await;
        select_best_agent(candidates, strategy, &mut rr_counter)
    }

    async fn resolve_target(&self, _target_uri: &str) -> Vec<String> {
        // Memory registry doesn't support custom targets by default.
        // CC addon should provide a custom registry implementation.
        vec![]
    }

    async fn release_call(&self, agent_id: &str, call_id: &str) -> bool {
        let bound = |presence: &PresenceState| {
            matches!(
                presence,
                PresenceState::Ringing {
                    call_id: Some(cid),
                }
                | PresenceState::Busy {
                    call_id: Some(cid),
                } if cid == call_id
            )
        };
        let matches = self
            .agents
            .get(agent_id)
            .map(|a| bound(&a.presence))
            .unwrap_or(false);
        if !matches {
            return false;
        }
        let mut agent = match self.agents.get_mut(agent_id) {
            Some(a) => a,
            None => return false,
        };
        // No wrapup machinery in the memory backend (no timer to end the
        // wrapup), so phantom states release straight back to Idle.
        agent.presence = PresenceState::Idle;
        agent.last_state_change = Instant::now();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_registry_lifecycle() {
        let registry = MemoryRegistry::new();

        // Register
        registry
            .register(
                "agent-001".to_string(),
                "Alice".to_string(),
                "sip:1001@localhost".to_string(),
                vec!["support".to_string()],
                2,
            )
            .await
            .unwrap();

        // Verify
        let agent = registry.get_agent("agent-001").await.unwrap();
        assert_eq!(agent.display_name, "Alice");
        assert!(agent.has_capacity());

        // Update presence
        registry
            .update_presence("agent-001", PresenceState::Busy { call_id: None })
            .await
            .unwrap();
        let agent = registry.get_agent("agent-001").await.unwrap();
        assert!(!agent.has_capacity());

        // Unregister
        registry.unregister("agent-001").await.unwrap();
        assert!(registry.get_agent("agent-001").await.is_none());
    }

    #[tokio::test]
    async fn test_memory_registry_routing() {
        let registry = MemoryRegistry::new();

        // Register multiple agents
        for i in 1..=3 {
            registry
                .register(
                    format!("agent-00{}", i),
                    format!("Agent {}", i),
                    format!("sip:100{}@localhost", i),
                    vec!["support".to_string()],
                    1,
                )
                .await
                .unwrap();
        }

        // Test LongestIdle
        let agent = registry
            .select_agent(&["support".to_string()], RoutingStrategy::LongestIdle)
            .await;
        assert!(agent.is_some());

        // Test RoundRobin
        let a1 = registry
            .select_agent(&["support".to_string()], RoutingStrategy::RoundRobin)
            .await;
        let a2 = registry
            .select_agent(&["support".to_string()], RoutingStrategy::RoundRobin)
            .await;
        assert_ne!(a1.unwrap().agent_id, a2.unwrap().agent_id);
    }

    #[tokio::test]
    async fn test_release_call_releases_bound_states_only() {
        let registry = MemoryRegistry::new();
        registry
            .register(
                "agent-rel".to_string(),
                "Rel".to_string(),
                "sip:2001@localhost".to_string(),
                vec![],
                1,
            )
            .await
            .unwrap();

        // Unrelated state: not released.
        registry
            .update_presence(
                "agent-rel",
                PresenceState::Busy {
                    call_id: Some("call-other".to_string()),
                },
            )
            .await
            .unwrap();
        assert!(!registry.release_call("agent-rel", "call-mine").await);

        // Bound Ringing: released to Idle.
        registry
            .update_presence(
                "agent-rel",
                PresenceState::Ringing {
                    call_id: Some("call-mine".to_string()),
                },
            )
            .await
            .unwrap();
        assert!(registry.release_call("agent-rel", "call-mine").await);
        let agent = registry.get_agent("agent-rel").await.unwrap();
        assert!(matches!(agent.presence, PresenceState::Idle));

        // Bound Busy: released (memory backend has no wrapup timer, so
        // straight to Idle).
        registry
            .update_presence(
                "agent-rel",
                PresenceState::Busy {
                    call_id: Some("call-mine".to_string()),
                },
            )
            .await
            .unwrap();
        assert!(registry.release_call("agent-rel", "call-mine").await);
        let agent = registry.get_agent("agent-rel").await.unwrap();
        assert!(matches!(agent.presence, PresenceState::Idle));
    }
}
