//! Queue Manager for RWI CallCommand
//!
//! Manages call queue operations including enqueue, dequeue, priority management,
//! and position tracking for queued legs.

use crate::call::domain::LegId;
use crate::call::runtime::SessionId;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// Unique identifier for a queue
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueId(pub String);

impl From<String> for QueueId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for QueueId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Entry in the queue for a specific leg
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub leg_id: LegId,
    pub session_id: SessionId,
    pub priority: u32,
    pub enqueued_at: std::time::Instant,
    pub position_announcements_enabled: bool,
}

impl QueueEntry {
    pub fn new(leg_id: LegId, session_id: SessionId, priority: Option<u32>) -> Self {
        Self {
            leg_id,
            session_id,
            priority: priority.unwrap_or(5), // Default priority (1-10, lower is higher priority)
            enqueued_at: std::time::Instant::now(),
            position_announcements_enabled: true,
        }
    }
}

/// Queue state and configuration
#[derive(Debug, Clone)]
pub struct CallQueue {
    pub id: QueueId,
    pub entries: VecDeque<QueueEntry>,
    pub max_wait_time: Option<std::time::Duration>,
    pub announcement_interval: Option<std::time::Duration>,
}

impl CallQueue {
    pub fn new(id: QueueId) -> Self {
        Self {
            id,
            entries: VecDeque::new(),
            max_wait_time: None,
            announcement_interval: Some(std::time::Duration::from_secs(30)),
        }
    }

    /// Add entry to queue with priority ordering
    pub fn enqueue(&mut self, entry: QueueEntry) -> usize {
        let position = self
            .entries
            .iter()
            .position(|e| e.priority > entry.priority)
            .unwrap_or(self.entries.len());
        self.entries.insert(position, entry);
        position + 1 // Return 1-based position
    }

    /// Remove entry from queue by leg_id
    pub fn dequeue(&mut self, leg_id: &LegId) -> Option<QueueEntry> {
        if let Some(pos) = self.entries.iter().position(|e| e.leg_id == *leg_id) {
            self.entries.remove(pos)
        } else {
            None
        }
    }

    /// Get position of a leg in the queue (1-based)
    pub fn get_position(&self, leg_id: &LegId) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.leg_id == *leg_id)
            .map(|p| p + 1)
    }

    /// Get queue length
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Global queue manager
#[derive(Debug)]
pub struct QueueManager {
    queues: Arc<RwLock<HashMap<QueueId, CallQueue>>>,
}

impl QueueManager {
    pub fn new() -> Self {
        Self {
            queues: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create or get a queue
    pub async fn get_or_create_queue(&self, queue_id: QueueId) -> CallQueue {
        let mut queues = self.queues.write().await;
        queues
            .entry(queue_id.clone())
            .or_insert_with(|| CallQueue::new(queue_id))
            .clone()
    }

    /// Get a queue if it exists
    pub async fn get_queue(&self, queue_id: &QueueId) -> Option<CallQueue> {
        let queues = self.queues.read().await;
        queues.get(queue_id).cloned()
    }

    /// Enqueue a leg to a queue
    pub async fn enqueue(
        &self,
        queue_id: QueueId,
        leg_id: LegId,
        session_id: SessionId,
        priority: Option<u32>,
    ) -> Result<usize> {
        let mut queues = self.queues.write().await;
        let queue = queues
            .entry(queue_id.clone())
            .or_insert_with(|| CallQueue::new(queue_id));

        let entry = QueueEntry::new(leg_id.clone(), session_id, priority);
        let position = queue.enqueue(entry);

        info!(
            queue_id = %queue.id.0,
            leg_id = %leg_id,
            position = position,
            "Leg enqueued successfully"
        );

        Ok(position)
    }

    /// Dequeue a leg from a queue
    pub async fn dequeue(&self, queue_id: &QueueId, leg_id: &LegId) -> Result<QueueEntry> {
        let mut queues = self.queues.write().await;
        let queue = queues
            .get_mut(queue_id)
            .ok_or_else(|| anyhow!("Queue not found: {}", queue_id.0))?;

        let entry = queue
            .dequeue(leg_id)
            .ok_or_else(|| anyhow!("Leg {} not found in queue {}", leg_id, queue_id.0))?;

        info!(
            queue_id = %queue.id.0,
            leg_id = %leg_id,
            "Leg dequeued successfully"
        );

        Ok(entry)
    }

    /// Get position of a leg in a queue
    pub async fn get_position(&self, queue_id: &QueueId, leg_id: &LegId) -> Result<usize> {
        let queues = self.queues.read().await;
        let queue = queues
            .get(queue_id)
            .ok_or_else(|| anyhow!("Queue not found: {}", queue_id.0))?;

        queue
            .get_position(leg_id)
            .ok_or_else(|| anyhow!("Leg {} not found in queue {}", leg_id, queue_id.0))
    }

    /// Get queue statistics
    pub async fn get_queue_stats(&self, queue_id: &QueueId) -> Result<QueueStats> {
        let queues = self.queues.read().await;
        let queue = queues
            .get(queue_id)
            .ok_or_else(|| anyhow!("Queue not found: {}", queue_id.0))?;

        Ok(QueueStats {
            queue_id: queue_id.0.clone(),
            length: queue.len(),
            wait_times: queue
                .entries
                .iter()
                .map(|e| e.enqueued_at.elapsed())
                .collect(),
        })
    }

    /// List all queues
    pub async fn list_queues(&self) -> Vec<QueueId> {
        let queues = self.queues.read().await;
        queues.keys().cloned().collect()
    }

    /// Remove a queue if empty
    pub async fn remove_queue_if_empty(&self, queue_id: &QueueId) -> Result<bool> {
        let mut queues = self.queues.write().await;
        if let Some(queue) = queues.get(queue_id)
            && queue.is_empty() {
                queues.remove(queue_id);
                info!(queue_id = %queue_id.0, "Empty queue removed");
                return Ok(true);
            }
        Ok(false)
    }

    /// Get all entries in a queue
    pub async fn get_queue_entries(&self, queue_id: &QueueId) -> Result<Vec<QueueEntry>> {
        let queues = self.queues.read().await;
        let queue = queues
            .get(queue_id)
            .ok_or_else(|| anyhow!("Queue not found: {}", queue_id.0))?;

        Ok(queue.entries.iter().cloned().collect())
    }
}

impl Default for QueueManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Queue statistics
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub queue_id: String,
    pub length: usize,
    pub wait_times: Vec<std::time::Duration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_enqueue_dequeue() {
        let manager = QueueManager::new();
        let queue_id = QueueId::from("test-queue");
        let leg_id = LegId::from("leg-1");
        let session_id = SessionId::from("session-1");

        // Enqueue
        let position = manager
            .enqueue(queue_id.clone(), leg_id.clone(), session_id, None)
            .await
            .unwrap();
        assert_eq!(position, 1);

        // Dequeue
        let entry = manager.dequeue(&queue_id, &leg_id).await.unwrap();
        assert_eq!(entry.leg_id, leg_id);
    }

    #[tokio::test]
    async fn test_priority_ordering() {
        let manager = QueueManager::new();
        let queue_id = QueueId::from("test-queue");

        // Enqueue with different priorities (lower number = higher priority)
        manager
            .enqueue(
                queue_id.clone(),
                LegId::from("leg-low"),
                SessionId::from("session-1"),
                Some(10),
            )
            .await
            .unwrap();

        manager
            .enqueue(
                queue_id.clone(),
                LegId::from("leg-high"),
                SessionId::from("session-2"),
                Some(1),
            )
            .await
            .unwrap();

        manager
            .enqueue(
                queue_id.clone(),
                LegId::from("leg-medium"),
                SessionId::from("session-3"),
                Some(5),
            )
            .await
            .unwrap();

        // Check order - high priority should be first
        let entries = manager.get_queue_entries(&queue_id).await.unwrap();
        assert_eq!(entries[0].leg_id, LegId::from("leg-high"));
        assert_eq!(entries[1].leg_id, LegId::from("leg-medium"));
        assert_eq!(entries[2].leg_id, LegId::from("leg-low"));
    }

    #[tokio::test]
    async fn test_get_position() {
        let manager = QueueManager::new();
        let queue_id = QueueId::from("test-queue");

        let leg1 = LegId::from("leg-1");
        let leg2 = LegId::from("leg-2");

        manager
            .enqueue(
                queue_id.clone(),
                leg1.clone(),
                SessionId::from("session-1"),
                None,
            )
            .await
            .unwrap();
        manager
            .enqueue(
                queue_id.clone(),
                leg2.clone(),
                SessionId::from("session-2"),
                None,
            )
            .await
            .unwrap();

        assert_eq!(manager.get_position(&queue_id, &leg1).await.unwrap(), 1);
        assert_eq!(manager.get_position(&queue_id, &leg2).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_dequeue_nonexistent_leg() {
        let manager = QueueManager::new();
        let queue_id = QueueId::from("test-queue");

        // Create queue first
        manager
            .enqueue(
                queue_id.clone(),
                LegId::from("leg-1"),
                SessionId::from("session-1"),
                None,
            )
            .await
            .unwrap();

        // Try to dequeue non-existent leg
        let result = manager
            .dequeue(&queue_id, &LegId::from("nonexistent"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_queue_stats() {
        let manager = QueueManager::new();
        let queue_id = QueueId::from("test-queue");

        manager
            .enqueue(
                queue_id.clone(),
                LegId::from("leg-1"),
                SessionId::from("session-1"),
                None,
            )
            .await
            .unwrap();

        let stats = manager.get_queue_stats(&queue_id).await.unwrap();
        assert_eq!(stats.queue_id, "test-queue");
        assert_eq!(stats.length, 1);
        assert_eq!(stats.wait_times.len(), 1);
    }
}
