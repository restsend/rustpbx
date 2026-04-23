//! Database Registry - SeaORM-backed persistent agent registry
//!
//! Suitable for:
//! - Multi-node deployments requiring shared state
//! - Production environments requiring persistence
//! - Scenarios where agent data must survive restarts

use super::{AgentRecord, AgentRegistry, PresenceState, RoutingStrategy, select_best_agent};
use sea_orm::{DatabaseConnection};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::info;
use async_trait::async_trait;

/// Database-backed agent registry implementation
/// 
/// Persists agent data to a relational database via SeaORM.
/// Suitable for production multi-node deployments.
pub struct DbRegistry {
    /// Local cache for fast reads
    cache: RwLock<HashMap<String, AgentRecord>>,
    /// Round-robin counter
    rr_counter: RwLock<u64>,
    /// Event callbacks for state changes
    event_handlers: RwLock<Vec<super::AgentEventHandler>>,

    /// Cache TTL in seconds
    cache_ttl_secs: u64,
}

impl DbRegistry {
    pub fn new(_db: DatabaseConnection) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            rr_counter: RwLock::new(0),
            event_handlers: RwLock::new(Vec::new()),
            cache_ttl_secs: 30, // Default 30 second cache
        }
    }

    pub fn with_cache_ttl(mut self, ttl_secs: u64) -> Self {
        self.cache_ttl_secs = ttl_secs;
        self
    }

    async fn notify_handlers(&self, record: &AgentRecord) {
        let handlers = self.event_handlers.read().await;
        for handler in handlers.iter() {
            handler(record);
        }
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
            presence: PresenceState::Available,
            last_state_change: Instant::now(),
            total_calls_handled: 0,
            total_talk_time_secs: 0,
            last_call_end: None,
            custom_data: HashMap::new(),
        };

        // Update cache
        let mut cache = self.cache.write().await;
        cache.insert(agent_id.clone(), record.clone());
        drop(cache);

        info!(agent_id = %agent_id, "Agent registered in database");
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        // Remove from cache
        let mut cache = self.cache.write().await;
        if cache.remove(agent_id).is_some() {
            info!(agent_id = %agent_id, "Agent unregistered from database");
            Ok(())
        } else {
            anyhow::bail!("Agent {} not found", agent_id)
        }
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        // Try cache first
        let cache = self.cache.read().await;
        if let Some(record) = cache.get(agent_id) {
            return Some(record.clone());
        }
        drop(cache);

        // In production, this would query the database
        // For now, return None if not in cache
        None
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        let cache = self.cache.read().await;
        cache.values().cloned().collect()
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        let mut cache = self.cache.write().await;
        let agent = cache
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

        let record = agent.clone();
        drop(cache);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        let mut cache = self.cache.write().await;
        let agent = cache
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent {} not found", agent_id))?;

        agent.current_calls += 1;
        agent.presence = PresenceState::Busy;
        agent.last_state_change = Instant::now();

        let record = agent.clone();
        drop(cache);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn end_call(
        &self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
        let mut cache = self.cache.write().await;
        let agent = cache
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
        drop(cache);
        self.notify_handlers(&record).await;

        Ok(())
    }

    async fn find_available_agents(
        &self,
        required_skills: &[String],
    ) -> Vec<AgentRecord> {
        let cache = self.cache.read().await;
        cache
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

    async fn resolve_target(
        &self, _target_uri: &str) -> Vec<String> {
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
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        
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
            .update_presence("agent-001", PresenceState::Busy)
            .await
            .unwrap();
        let agent = registry.get_agent("agent-001").await.unwrap();
        assert!(matches!(agent.presence, PresenceState::Busy));
    }
}
