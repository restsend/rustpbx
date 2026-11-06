use async_trait::async_trait;
use rustpbx::call::app::agent_registry::{
    AgentRecord, AgentRegistry, PresenceState, RoutingStrategy,
};
use std::sync::Arc;

/// Minimal static agent registry (the in-core `MemoryRegistry` is
/// `#[cfg(test)]`-only): just enough for attribution lookups — `register`
/// fills a map, `get_agent` reads it, everything queue/ACD-related is inert.
#[derive(Default)]
pub struct StaticRegistry {
    agents: std::sync::Mutex<std::collections::HashMap<String, AgentRecord>>,
}

impl StaticRegistry {
    pub fn with_agents(agents: &[(&str, &str)]) -> Arc<Self> {
        let reg = Self::default();
        for (id, name) in agents {
            reg.agents.lock().unwrap().insert(
                (*id).to_string(),
                AgentRecord {
                    agent_id: (*id).to_string(),
                    display_name: (*name).to_string(),
                    uri: format!("sip:{id}@127.0.0.1"),
                    skills: vec![],
                    max_concurrency: 1,
                    current_calls: 0,
                    presence: PresenceState::Idle,
                    last_state_change: std::time::Instant::now(),
                    total_calls_handled: 0,
                    total_talk_time_secs: 0,
                    last_call_end: None,
                },
            );
        }
        Arc::new(reg)
    }
}

#[async_trait]
impl AgentRegistry for StaticRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()> {
        self.agents.lock().unwrap().insert(
            agent_id.clone(),
            AgentRecord {
                agent_id,
                display_name,
                uri,
                skills,
                max_concurrency,
                current_calls: 0,
                presence: PresenceState::Idle,
                last_state_change: std::time::Instant::now(),
                total_calls_handled: 0,
                total_talk_time_secs: 0,
                last_call_end: None,
            },
        );
        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        self.agents
            .lock()
            .unwrap()
            .remove(agent_id)
            .map(|_| ())
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} not found"))
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        self.agents.lock().unwrap().get(agent_id).cloned()
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        self.agents.lock().unwrap().values().cloned().collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        let mut agents = self.agents.lock().unwrap();
        let agent = agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} not found"))?;
        agent.presence = new_state;
        agent.last_state_change = std::time::Instant::now();
        Ok(())
    }

    async fn find_available_agents(&self, _required_skills: &[String]) -> Vec<AgentRecord> {
        Vec::new()
    }

    async fn select_agent(
        &self,
        _required_skills: &[String],
        _strategy: RoutingStrategy,
    ) -> Option<AgentRecord> {
        None
    }

    async fn resolve_target(&self, _target_uri: &str) -> Vec<String> {
        Vec::new()
    }
}
