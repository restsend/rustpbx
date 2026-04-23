//! HTTP Registry - External HTTP API agent registry integration
//!
//! Suitable for:
//! - Integration with existing CRM/HR systems
//! - Microservices architectures
//! - Scenarios where agent data is managed externally

use super::{AgentRecord, AgentRegistry, PresenceState, RoutingStrategy};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, error};
use async_trait::async_trait;

/// HTTP API-backed agent registry implementation
/// 
/// Communicates with an external HTTP API for agent management.
/// Suitable for integration with existing systems.
pub struct HttpRegistry {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    /// Local cache for fast reads
    cache: RwLock<HashMap<String, (AgentRecord, Instant)>>,
    /// Round-robin counter
    rr_counter: RwLock<u64>,
    /// Event callbacks for state changes
    event_handlers: RwLock<Vec<super::AgentEventHandler>>,

    /// Cache TTL
    cache_ttl: Duration,
}

impl HttpRegistry {
    pub fn new(base_url: String, api_key: Option<String>) -> Self {
        Self {
            base_url,
            api_key,
            client: reqwest::Client::new(),
            cache: RwLock::new(HashMap::new()),
            rr_counter: RwLock::new(0),
            event_handlers: RwLock::new(Vec::new()),
            cache_ttl: Duration::from_secs(30),
        }
    }

    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// Build HTTP headers with API key if present
    fn build_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        if let Some(ref key) = self.api_key {
            headers.insert(
                "X-API-Key",
                reqwest::header::HeaderValue::from_str(key).unwrap(),
            );
        }
        headers
    }

    /// Check if cache entry is still valid
    fn is_cache_valid(&self, timestamp: Instant) -> bool {
        timestamp.elapsed() < self.cache_ttl
    }

    /// Fetch agent from HTTP API
    async fn fetch_agent(
        &self, agent_id: &str) -> anyhow::Result<Option<AgentRecord>> {
        let url = format!("{}/agents/{}", self.base_url, agent_id);
        
        let response = self.client
            .get(&url)
            .headers(self.build_headers())
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            anyhow::bail!("HTTP error: {}", response.status());
        }

        let data: serde_json::Value = response.json().await?;
        
        // Parse agent record from JSON
        let record = Self::parse_agent_from_json(&data)?;
        
        // Update cache
        let mut cache = self.cache.write().await;
        cache.insert(agent_id.to_string(), (record.clone(), Instant::now()));
        
        Ok(Some(record))
    }

    /// Update agent via HTTP API
    async fn update_agent_api(
        &self, agent_id: &str, updates: serde_json::Value
    ) -> anyhow::Result<()> {
        let url = format!("{}/agents/{}", self.base_url, agent_id);
        
        let response = self.client
            .patch(&url)
            .headers(self.build_headers())
            .json(&updates)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("HTTP error: {}", response.status());
        }

        Ok(())
    }

    /// Parse agent record from JSON
    pub fn parse_agent_from_json(data: &serde_json::Value) -> anyhow::Result<AgentRecord> {
        let agent_id = data["agent_id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing agent_id"))?;
        let display_name = data["display_name"].as_str()
            .unwrap_or(agent_id);
        let uri = data["uri"].as_str()
            .unwrap_or("");
        let skills: Vec<String> = data["skills"].as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let max_concurrency = data["max_concurrency"].as_u64()
            .unwrap_or(1) as u32;
        let current_calls = data["current_calls"].as_u64()
            .unwrap_or(0) as u32;
        let presence = data["presence"].as_str()
            .and_then(PresenceState::parse_state)
            .unwrap_or(PresenceState::Offline);
        let total_calls_handled = data["total_calls_handled"].as_u64()
            .unwrap_or(0);
        let total_talk_time_secs = data["total_talk_time_secs"].as_u64()
            .unwrap_or(0);

        Ok(AgentRecord {
            agent_id: agent_id.to_string(),
            display_name: display_name.to_string(),
            uri: uri.to_string(),
            skills,
            max_concurrency,
            current_calls,
            presence,
            last_state_change: Instant::now(),
            total_calls_handled,
            total_talk_time_secs,
            last_call_end: None,
            custom_data: HashMap::new(),
        })
    }
}

#[async_trait]
impl AgentRegistry for HttpRegistry {
    async fn register(
        &self,
        agent_id: String,
        display_name: String,
        uri: String,
        skills: Vec<String>,
        max_concurrency: u32,
    ) -> anyhow::Result<()> {
        let url = format!("{}/agents", self.base_url);
        
        let payload = serde_json::json!({
            "agent_id": agent_id,
            "display_name": display_name,
            "uri": uri,
            "skills": skills,
            "max_concurrency": max_concurrency,
            "presence": "available",
        });

        let response = self.client
            .post(&url)
            .headers(self.build_headers())
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("HTTP error: {}", response.status());
        }

        info!(agent_id = %agent_id, "Agent registered via HTTP API");
        Ok(())
    }

    async fn unregister(&self, agent_id: &str) -> anyhow::Result<()> {
        let url = format!("{}/agents/{}", self.base_url, agent_id);
        
        let response = self.client
            .delete(&url)
            .headers(self.build_headers())
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("HTTP error: {}", response.status());
        }

        // Remove from cache
        let mut cache = self.cache.write().await;
        cache.remove(agent_id);

        info!(agent_id = %agent_id, "Agent unregistered via HTTP API");
        Ok(())
    }

    async fn get_agent(&self, agent_id: &str) -> Option<AgentRecord> {
        // Try cache first
        let cache = self.cache.read().await;
        if let Some((record, timestamp)) = cache.get(agent_id)
            && self.is_cache_valid(*timestamp) {
                return Some(record.clone());
            }
        drop(cache);

        // Fetch from API
        match self.fetch_agent(agent_id).await {
            Ok(agent) => agent,
            Err(e) => {
                error!(agent_id = %agent_id, error = %e, "Failed to fetch agent from HTTP API");
                None
            }
        }
    }

    async fn list_agents(&self) -> Vec<AgentRecord> {
        let url = format!("{}/agents", self.base_url);
        
        match self.client
            .get(&url)
            .headers(self.build_headers())
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                match response.json::<Vec<serde_json::Value>>().await {
                    Ok(data) => {
                        data.iter()
                            .filter_map(|v| Self::parse_agent_from_json(v).ok())
                            .collect()
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to parse agents list");
                        Vec::new()
                    }
                }
            }
            _ => {
                // Fallback to cache
                let cache = self.cache.read().await;
                cache.values()
                    .filter(|(_, ts)| self.is_cache_valid(*ts))
                    .map(|(record, _)| record.clone())
                    .collect()
            }
        }
    }

    async fn update_presence(
        &self,
        agent_id: &str,
        new_state: PresenceState,
    ) -> anyhow::Result<()> {
        let updates = serde_json::json!({
            "presence": new_state.as_str(),
        });

        self.update_agent_api(agent_id, updates).await?;

        // Update cache
        let mut cache = self.cache.write().await;
        if let Some((record, _)) = cache.get_mut(agent_id) {
            record.presence = new_state;
            record.last_state_change = Instant::now();
        }

        info!(agent_id = %agent_id, "Presence updated via HTTP API");
        Ok(())
    }

    async fn start_call(&self, agent_id: &str) -> anyhow::Result<()> {
        let updates = serde_json::json!({
            "current_calls": 1,
            "presence": "busy",
        });

        self.update_agent_api(agent_id, updates).await?;

        // Update cache
        let mut cache = self.cache.write().await;
        if let Some((record, _)) = cache.get_mut(agent_id) {
            record.current_calls += 1;
            record.presence = PresenceState::Busy;
            record.last_state_change = Instant::now();
        }

        Ok(())
    }

    async fn end_call(
        &self, agent_id: &str, talk_time_secs: u64) -> anyhow::Result<()> {
        let updates = serde_json::json!({
            "talk_time_secs": talk_time_secs,
        });

        self.update_agent_api(agent_id, updates).await?;

        // Update cache
        let mut cache = self.cache.write().await;
        if let Some((record, _)) = cache.get_mut(agent_id) {
            if record.current_calls > 0 {
                record.current_calls -= 1;
            }
            record.total_calls_handled += 1;
            record.total_talk_time_secs += talk_time_secs;
            record.last_call_end = Some(Instant::now());
            
            if record.current_calls == 0 {
                record.presence = PresenceState::Wrapup;
            }
        }

        Ok(())
    }

    async fn find_available_agents(
        &self,
        required_skills: &[String],
    ) -> Vec<AgentRecord> {
        let agents = self.list_agents().await;
        agents.into_iter()
            .filter(|a| a.has_capacity() && a.has_skills(required_skills))
            .collect()
    }

    async fn select_agent(
        &self,
        required_skills: &[String],
        strategy: RoutingStrategy,
    ) -> Option<AgentRecord> {
        let candidates = self.find_available_agents(required_skills).await;
        let mut rr_counter = self.rr_counter.write().await;
        super::select_best_agent(candidates, strategy, &mut rr_counter)
    }

    async fn on_state_change(
        &self, handler: Box<dyn Fn(&AgentRecord) + Send + Sync>) {
        let mut handlers = self.event_handlers.write().await;
        let handler: Box<dyn Fn(&AgentRecord) + Send + Sync + 'static> = unsafe { std::mem::transmute(handler) };
        handlers.push(handler);
    }

    async fn resolve_target(
        &self, _target_uri: &str) -> Vec<String> {
        // HTTP registry could query external API for target resolution
        // For now, return empty list - CC addon should override if needed
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http_registry_creation() {
        let registry = HttpRegistry::new(
            "https://api.example.com".to_string(),
            Some("test-api-key".to_string()),
        );

        assert_eq!(registry.base_url, "https://api.example.com");
        assert_eq!(registry.api_key, Some("test-api-key".to_string()));
    }

    #[test]
    fn test_parse_agent_from_json() {
        let json = serde_json::json!({
            "agent_id": "agent-001",
            "display_name": "Alice",
            "uri": "sip:1001@localhost",
            "skills": ["support", "sales"],
            "max_concurrency": 2,
            "current_calls": 0,
            "presence": "available",
            "total_calls_handled": 10,
            "total_talk_time_secs": 3600,
        });

        let agent = HttpRegistry::parse_agent_from_json(&json).unwrap();
        assert_eq!(agent.agent_id, "agent-001");
        assert_eq!(agent.display_name, "Alice");
        assert_eq!(agent.skills, vec!["support", "sales"]);
        assert_eq!(agent.max_concurrency, 2);
        assert!(matches!(agent.presence, PresenceState::Available));
    }
}
