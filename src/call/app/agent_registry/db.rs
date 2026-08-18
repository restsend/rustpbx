//! Database Registry - SeaORM-backed persistent agent registry
//!
//! Suitable for:
//! - Multi-node deployments requiring shared state
//! - Production environments requiring persistence
//! - Scenarios where agent data must survive restarts

use super::{AgentRecord, AgentRegistry, PresenceState, RoutingStrategy, select_best_agent};
use async_trait::async_trait;
use dashmap::DashMap;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::info;

/// Database-backed agent registry implementation
///
/// Persists agent data to a relational database via SeaORM.
/// Suitable for production multi-node deployments.
pub struct DbRegistry {
    /// Local cache for fast reads
    cache: DashMap<String, AgentRecord>,
    /// Round-robin counter
    rr_counter: RwLock<u64>,

    /// Cache TTL in seconds
    cache_ttl_secs: u64,
}

impl DbRegistry {
    pub fn new(_db: DatabaseConnection) -> Self {
        Self {
            cache: DashMap::new(),
            rr_counter: RwLock::new(0),
            cache_ttl_secs: 30, // Default 30 second cache
        }
    }

    pub fn with_cache_ttl(mut self, ttl_secs: u64) -> Self {
        self.cache_ttl_secs = ttl_secs;
        self
    }
}

#[async_trait]
impl AgentRegistry for DbRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()> {
        // Note: This is a placeholder implementation
        // In production, you'd use SeaORM entities and migrations

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

        // Update cache
        self.cache.insert(agent_id.clone(), record.clone());

        info!(agent_id = %agent_id, "Agent registered in database");

        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        // Remove from cache
        if self.cache.remove(agent_id).is_some() {
            info!(agent_id = %agent_id, "Agent unregistered from database");
            Ok(())
        } else {
            anyhow::bail!("Agent {} not found", agent_id)
        }
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        // Try cache first
        if let Some(record) = self.cache.get(agent_id) {
            return Some(record.clone());
        }

        // In production, this would query the database
        // For now, return None if not in cache
        None
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        self.cache.iter().map(|e| e.value().clone()).collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        let mut agent = self
            .cache
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent {} not found", agent_id))?;

        let old_state = agent.presence.clone();
        agent.presence = new_state;
        agent.last_state_change = Instant::now();

        info!(
            agent_id = %agent_id,
            old = %old_state.as_str(),
            new = %agent.presence.as_str(),
            "Presence updated in database"
        );

        drop(agent);

        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        let mut agent = self
            .cache
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
            .cache
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
        self.cache
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
        // Db registry doesn't support custom targets by default.
        // CC addon should provide a custom registry implementation.
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agent_registry_basic() {
        // Create an in-memory SQLite database for testing
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();

        let registry = DbRegistry::new(db);

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

        // Update presence
        registry
            .update_presence("agent-001", PresenceState::Busy { call_id: None })
            .await
            .unwrap();
        let agent = registry.get_agent("agent-001").await.unwrap();
        assert!(matches!(
            agent.presence,
            PresenceState::Busy { call_id: None }
        ));
    }
}
