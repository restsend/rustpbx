//! Memory Registry - In-memory agent registry implementation
//!
//! Suitable for:
//! - Single-node deployments
//! - Testing and development
//! - Scenarios where persistence is not required

use super::{AgentRecord, AgentRegistry, PresenceState, RoutingStrategy, select_best_agent};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::info;
use async_trait::async_trait;

/// In-memory agent registry implementation
/// 
/// All data is stored in memory and lost on restart.
/// Suitable for single-node deployments and testing.
pub struct MemoryRegistry {
    agents: RwLock<HashMap<String, AgentRecord>>,
    /// Round-robin counter
    rr_counter: RwLock<u64>,
    /// Event callbacks for state changes
    event_handlers: RwLock<Vec<super::AgentEventHandler>>,
}

impl MemoryRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            rr_counter: RwLock::new(0),
            event_handlers: RwLock::new(Vec::new()),
        }
    }

    async fn notify_handlers(&self, record: &AgentRecord) {
        let handlers = self.event_handlers.read().await;
        for handler in handlers.iter() {
            handler(record);
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
        let mut agents = self.agents.write().await;
        if agents.contains_key(&agent_id) {
            anyhow::bail!("Agent {} already registered", agent_id);
        }

        let record = AgentRecord {
            agent_id: agent_id.clone(),
            display_name,
            uri,
            skills,
            max_concurrency,
            current_calls: 0,
            presence: PresenceState::Available,
            last_state_change: Instant::now(),
            total_calls_handled: 0,
            total_talk_time_secs: 0,
            last_call_end: None,
            custom_data: HashMap::new(),
        };

        agents.insert(agent_id.clone(), record.clone());
        info!(agent_id = %agent_id, "Agent registered in memory");

        // Notify handlers
        drop(agents);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        let mut agents = self.agents.write().await;
        if agents.remove(agent_id).is_some() {
            info!(agent_id = %agent_id, "Agent unregistered from memory");
            Ok(())
        } else {
            anyhow::bail!("Agent {} not found", agent_id)
        }
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        let agents = self.agents.read().await;
        agents.get(agent_id).cloned()
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        let agents = self.agents.read().await;
        agents.values().cloned().collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        let mut agents = self.agents.write().await;
        let agent = agents
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

        let record = agent.clone();
        drop(agents);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        let mut agents = self.agents.write().await;
        let agent = agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent {} not found", agent_id))?;

        agent.current_calls += 1;
        agent.presence = PresenceState::Busy;
        agent.last_state_change = Instant::now();

        let record = agent.clone();
        drop(agents);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn end_call(
        &self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
        let mut agents = self.agents.write().await;
        let agent = agents
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
            agent.presence = PresenceState::Wrapup;
        }

        let record = agent.clone();
        drop(agents);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn find_available_agents(
        &self,
        required_skills: &[String],
    ) -> Vec<AgentRecord> {
        let agents = self.agents.read().await;
        agents
            .values()
            .filter(|a| a.has_capacity() && a.has_skills(required_skills))
            .cloned()
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

    async fn on_state_change(
        &self, handler: Box<dyn Fn(&AgentRecord) + Send + Sync>) {
        let mut handlers = self.event_handlers.write().await;
        let handler: Box<dyn Fn(&AgentRecord) + Send + Sync + 'static> = unsafe { std::mem::transmute(handler) };
        handlers.push(handler);
    }

    async fn resolve_target(&self, _target_uri: &str) -> Vec<String> {
        // Memory registry doesn't support custom targets by default.
        // CC addon should provide a custom registry implementation.
        vec![]
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
            .update_presence("agent-001", PresenceState::Busy)
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
}
