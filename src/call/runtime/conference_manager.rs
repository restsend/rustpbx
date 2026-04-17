//! Conference Manager for RWI CallCommand
//!
//! Manages conference rooms including create, destroy, participant management,
//! and mute/unmute functionality with real-time audio mixing.

use crate::call::domain::LegId;
use crate::media::conference_mixer::{AudioFrame, ConferenceAudioMixer};
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn};

/// Unique identifier for a conference
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConferenceId(pub String);

impl From<String> for ConferenceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ConferenceId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Participant in a conference
#[derive(Debug, Clone)]
pub struct ConferenceParticipant {
    pub leg_id: LegId,
    pub muted: bool,
    pub joined_at: std::time::Instant,
}

impl ConferenceParticipant {
    pub fn new(leg_id: LegId) -> Self {
        Self {
            leg_id,
            muted: false,
            joined_at: std::time::Instant::now(),
        }
    }
}

/// Conference room state with audio mixing
#[derive(Debug, Clone)]
pub struct ConferenceRoom {
    pub id: ConferenceId,
    pub participants: HashMap<LegId, ConferenceParticipant>,
    pub created_at: std::time::Instant,
    pub max_participants: Option<usize>,
    pub locked: bool,
}

impl ConferenceRoom {
    pub fn new(id: ConferenceId, max_participants: Option<usize>) -> Self {
        Self {
            id,
            participants: HashMap::new(),
            created_at: std::time::Instant::now(),
            max_participants,
            locked: false,
        }
    }

    /// Add a participant to the conference
    pub fn add_participant(&mut self, leg_id: LegId) -> Result<()> {
        if let Some(max) = self.max_participants {
            if self.participants.len() >= max {
                return Err(anyhow!("Conference is at maximum capacity"));
            }
        }

        if self.participants.contains_key(&leg_id) {
            warn!(leg_id = %leg_id, "Leg already in conference");
            return Ok(());
        }

        let participant = ConferenceParticipant::new(leg_id.clone());
        self.participants.insert(leg_id.clone(), participant);
        info!(conf_id = %self.id.0, leg_id = %leg_id, "Participant added to conference");
        Ok(())
    }

    /// Remove a participant from the conference
    pub fn remove_participant(&mut self, leg_id: &LegId) -> Result<()> {
        if self.participants.remove(leg_id).is_none() {
            return Err(anyhow!("Leg {} is not in conference", leg_id));
        }
        info!(conf_id = %self.id.0, leg_id = %leg_id, "Participant removed from conference");
        Ok(())
    }

    /// Mute a participant
    pub fn mute_participant(&mut self, leg_id: &LegId) -> Result<()> {
        let participant = self
            .participants
            .get_mut(leg_id)
            .ok_or_else(|| anyhow!("Leg {} is not in conference", leg_id))?;
        participant.muted = true;
        info!(conf_id = %self.id.0, leg_id = %leg_id, "Participant muted");
        Ok(())
    }

    /// Unmute a participant
    pub fn unmute_participant(&mut self, leg_id: &LegId) -> Result<()> {
        let participant = self
            .participants
            .get_mut(leg_id)
            .ok_or_else(|| anyhow!("Leg {} is not in conference", leg_id))?;
        participant.muted = false;
        info!(conf_id = %self.id.0, leg_id = %leg_id, "Participant unmuted");
        Ok(())
    }

    /// Get participant count
    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    /// Check if conference is empty
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    /// Get all participant IDs
    pub fn participant_ids(&self) -> Vec<LegId> {
        self.participants.keys().cloned().collect()
    }

    /// Lock the conference (prevent new participants)
    pub fn lock(&mut self) {
        self.locked = true;
    }

    /// Unlock the conference
    pub fn unlock(&mut self) {
        self.locked = false;
    }
}

/// Audio channels for a conference participant
/// Note: This struct should not be cloned for output_rx; use reference instead
#[derive(Clone)]
pub struct ParticipantChannels {
    /// Channel to send audio to the conference (from participant)
    pub input_tx: mpsc::Sender<AudioFrame>,
}

/// Global conference manager with audio mixing
#[derive(Clone)]
pub struct ConferenceManager {
    conferences: Arc<RwLock<HashMap<ConferenceId, ConferenceRoom>>>,
    /// Track which conference a leg belongs to
    leg_to_conference: Arc<RwLock<HashMap<LegId, ConferenceId>>>,
    /// Audio mixers for conferences
    audio_mixers: Arc<RwLock<HashMap<ConferenceId, Arc<ConferenceAudioMixer>>>>,
    /// Audio channels for participants
    participant_channels: Arc<RwLock<HashMap<LegId, ParticipantChannels>>>,
}

impl ConferenceManager {
    pub fn new() -> Self {
        Self {
            conferences: Arc::new(RwLock::new(HashMap::new())),
            leg_to_conference: Arc::new(RwLock::new(HashMap::new())),
            audio_mixers: Arc::new(RwLock::new(HashMap::new())),
            participant_channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new conference with audio mixing
    pub async fn create_conference(
        &self,
        conf_id: ConferenceId,
        max_participants: Option<usize>,
    ) -> Result<ConferenceRoom> {
        let mut conferences = self.conferences.write().await;
        let mut audio_mixers = self.audio_mixers.write().await;

        if conferences.contains_key(&conf_id) {
            return Err(anyhow!("Conference {} already exists", conf_id.0));
        }

        let conference = ConferenceRoom::new(conf_id.clone(), max_participants);
        conferences.insert(conf_id.clone(), conference.clone());

        // Create audio mixer for this conference
        let mixer = Arc::new(ConferenceAudioMixer::new(conf_id.0.clone(), 8000));
        mixer.start();
        audio_mixers.insert(conf_id.clone(), mixer);

        info!(conf_id = %conf_id.0, "Conference created with audio mixing");
        Ok(conference)
    }

    /// Get a conference if it exists
    pub async fn get_conference(&self, conf_id: &ConferenceId) -> Option<ConferenceRoom> {
        let conferences = self.conferences.read().await;
        conferences.get(conf_id).cloned()
    }

    /// Destroy a conference and stop audio mixing
    pub async fn destroy_conference(&self, conf_id: &ConferenceId) -> Result<()> {
        // Stop and remove audio mixer
        {
            let mut audio_mixers = self.audio_mixers.write().await;
            if let Some(mixer) = audio_mixers.remove(conf_id) {
                mixer.stop().await;
            }
        }

        let mut conferences = self.conferences.write().await;
        let mut leg_map = self.leg_to_conference.write().await;
        let mut participant_channels = self.participant_channels.write().await;

        if let Some(conf) = conferences.get(conf_id) {
            // Remove all leg mappings and channels
            for leg_id in conf.participant_ids() {
                leg_map.remove(&leg_id);
                participant_channels.remove(&leg_id);
            }
        }

        conferences
            .remove(conf_id)
            .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

        info!(conf_id = %conf_id.0, "Conference destroyed");
        Ok(())
    }

    /// Add a participant to a conference with audio channels
    pub async fn add_participant(
        &self,
        conf_id: &ConferenceId,
        leg_id: LegId,
    ) -> Result<ParticipantChannels> {
        // Check if leg is already in another conference
        {
            let leg_map = self.leg_to_conference.read().await;
            if let Some(existing_conf) = leg_map.get(&leg_id) {
                if existing_conf != conf_id {
                    return Err(anyhow!(
                        "Leg {} is already in conference {}",
                        leg_id,
                        existing_conf.0
                    ));
                }
            }
        }

        // Add to conference room
        {
            let mut conferences = self.conferences.write().await;
            let conference = conferences
                .get_mut(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

            conference.add_participant(leg_id.clone())?;
        }

        // Add to audio mixer
        let channels = {
            let audio_mixers = self.audio_mixers.read().await;
            let mixer = audio_mixers
                .get(conf_id)
                .ok_or_else(|| anyhow!("Audio mixer not found for conference {}", conf_id.0))?;

            let (input_tx, _output_rx) = mixer
                .add_participant(leg_id.clone(), CodecType::PCMU)
                .await?;

            ParticipantChannels { input_tx }
        };

        // Store channels and mapping
        {
            let mut participant_channels = self.participant_channels.write().await;
            participant_channels.insert(leg_id.clone(), channels.clone());

            let mut leg_map = self.leg_to_conference.write().await;
            leg_map.insert(leg_id.clone(), conf_id.clone());
        }

        Ok(channels)
    }

    /// Remove a participant from a conference
    pub async fn remove_participant(&self, conf_id: &ConferenceId, leg_id: &LegId) -> Result<()> {
        // Remove from conference room
        {
            let mut conferences = self.conferences.write().await;
            let conference = conferences
                .get_mut(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

            conference.remove_participant(leg_id)?;
        }

        // Remove from audio mixer
        {
            let audio_mixers = self.audio_mixers.read().await;
            if let Some(mixer) = audio_mixers.get(conf_id) {
                mixer.remove_participant(leg_id).await?;
            }
        }

        // Remove channels and mapping
        {
            let mut participant_channels = self.participant_channels.write().await;
            participant_channels.remove(leg_id);

            let mut leg_map = self.leg_to_conference.write().await;
            leg_map.remove(leg_id);
        }

        Ok(())
    }

    /// Mute a participant
    pub async fn mute_participant(&self, conf_id: &ConferenceId, leg_id: &LegId) -> Result<()> {
        // Update conference room state
        {
            let mut conferences = self.conferences.write().await;
            let conference = conferences
                .get_mut(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

            conference.mute_participant(leg_id)?;
        }

        // Update audio mixer
        {
            let audio_mixers = self.audio_mixers.read().await;
            if let Some(mixer) = audio_mixers.get(conf_id) {
                mixer.set_muted(leg_id, true).await?;
            }
        }

        Ok(())
    }

    /// Unmute a participant
    pub async fn unmute_participant(&self, conf_id: &ConferenceId, leg_id: &LegId) -> Result<()> {
        // Update conference room state
        {
            let mut conferences = self.conferences.write().await;
            let conference = conferences
                .get_mut(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

            conference.unmute_participant(leg_id)?;
        }

        // Update audio mixer
        {
            let audio_mixers = self.audio_mixers.read().await;
            if let Some(mixer) = audio_mixers.get(conf_id) {
                mixer.set_muted(leg_id, false).await?;
            }
        }

        Ok(())
    }

    /// Get conference ID for a leg
    pub async fn get_conference_id_for_leg(&self, leg_id: &LegId) -> Option<ConferenceId> {
        let leg_map = self.leg_to_conference.read().await;
        leg_map.get(leg_id).cloned()
    }

    /// Get participant channels for audio streaming
    pub async fn get_participant_channels(&self, leg_id: &LegId) -> Option<ParticipantChannels> {
        let participant_channels = self.participant_channels.read().await;
        participant_channels.get(leg_id).cloned()
    }

    /// List all conferences
    pub async fn list_conferences(&self) -> Vec<ConferenceId> {
        let conferences = self.conferences.read().await;
        conferences.keys().cloned().collect()
    }

    /// Get conference statistics
    pub async fn get_conference_stats(&self, conf_id: &ConferenceId) -> Result<ConferenceStats> {
        let conferences = self.conferences.read().await;
        let conference = conferences
            .get(conf_id)
            .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

        Ok(ConferenceStats {
            conference_id: conf_id.0.clone(),
            participant_count: conference.participant_count(),
            muted_count: conference.participants.values().filter(|p| p.muted).count(),
            duration: conference.created_at.elapsed(),
        })
    }

    /// Remove a leg from any conference (called when leg hangs up)
    pub async fn remove_leg_from_all(&self, leg_id: &LegId) -> Result<()> {
        let conf_id = {
            let leg_map = self.leg_to_conference.read().await;
            leg_map.get(leg_id).cloned()
        };

        if let Some(conf_id) = conf_id {
            let _ = self.remove_participant(&conf_id, leg_id).await;
        }

        Ok(())
    }
}

impl Default for ConferenceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Conference statistics
#[derive(Debug, Clone)]
pub struct ConferenceStats {
    pub conference_id: String,
    pub participant_count: usize,
    pub muted_count: usize,
    pub duration: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_conference_manager_creation() {
        let manager = ConferenceManager::new();
        let conferences = manager.list_conferences().await;
        assert!(conferences.is_empty());
    }

    #[tokio::test]
    async fn test_create_destroy_conference() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf");

        let conf = manager
            .create_conference(conf_id.clone(), Some(10))
            .await
            .unwrap();
        assert_eq!(conf.participant_count(), 0);

        manager.destroy_conference(&conf_id).await.unwrap();
        assert!(manager.get_conference(&conf_id).await.is_none());
    }

    #[tokio::test]
    async fn test_add_remove_participant_with_audio() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_id = LegId::new("leg1");
        let channels = manager
            .add_participant(&conf_id, leg_id.clone())
            .await
            .unwrap();

        // Test sending audio
        let frame = crate::media::conference_mixer::AudioFrame::new(vec![1000i16; 160], 8000);
        channels.input_tx.send(frame).await.unwrap();

        manager.remove_participant(&conf_id, &leg_id).await.unwrap();

        let conf = manager.get_conference(&conf_id).await.unwrap();
        assert_eq!(conf.participant_count(), 0);

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_mute_unmute() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_id = LegId::new("leg1");
        manager
            .add_participant(&conf_id, leg_id.clone())
            .await
            .unwrap();

        manager.mute_participant(&conf_id, &leg_id).await.unwrap();

        let conf = manager.get_conference(&conf_id).await.unwrap();
        let participant = conf.participants.get(&leg_id).unwrap();
        assert!(participant.muted);

        manager.unmute_participant(&conf_id, &leg_id).await.unwrap();

        let conf = manager.get_conference(&conf_id).await.unwrap();
        let participant = conf.participants.get(&leg_id).unwrap();
        assert!(!participant.muted);

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_max_participants_limit() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf-max");

        // Create conference with max 2 participants
        manager
            .create_conference(conf_id.clone(), Some(2))
            .await
            .unwrap();

        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");
        let leg3 = LegId::new("leg3");

        // Add first two participants (should succeed)
        manager.add_participant(&conf_id, leg1).await.unwrap();
        manager.add_participant(&conf_id, leg2).await.unwrap();

        // Add third participant (should fail due to limit)
        let result = manager.add_participant(&conf_id, leg3).await;
        assert!(
            result.is_err(),
            "Should fail when exceeding max participants"
        );

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_duplicate_participant() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf-dup");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_id = LegId::new("leg1");

        // Add participant first time
        manager
            .add_participant(&conf_id, leg_id.clone())
            .await
            .unwrap();

        // Add same participant again (should succeed but be a no-op)
        manager
            .add_participant(&conf_id, leg_id.clone())
            .await
            .unwrap();

        let conf = manager.get_conference(&conf_id).await.unwrap();
        assert_eq!(conf.participant_count(), 1);

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_conference_stats() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf-stats");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");

        manager
            .add_participant(&conf_id, leg1.clone())
            .await
            .unwrap();
        manager
            .add_participant(&conf_id, leg2.clone())
            .await
            .unwrap();
        manager.mute_participant(&conf_id, &leg1).await.unwrap();

        let stats = manager.get_conference_stats(&conf_id).await.unwrap();
        assert_eq!(stats.participant_count, 2);
        assert_eq!(stats.muted_count, 1);
        assert_eq!(stats.conference_id, "test-conf-stats");

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_list_conferences() {
        let manager = ConferenceManager::new();

        let conf1 = ConferenceId::from("conf1");
        let conf2 = ConferenceId::from("conf2");

        manager
            .create_conference(conf1.clone(), None)
            .await
            .unwrap();
        manager
            .create_conference(conf2.clone(), None)
            .await
            .unwrap();

        let conferences = manager.list_conferences().await;
        assert_eq!(conferences.len(), 2);
        assert!(conferences.contains(&conf1));
        assert!(conferences.contains(&conf2));

        manager.destroy_conference(&conf1).await.unwrap();
        manager.destroy_conference(&conf2).await.unwrap();
    }

    #[tokio::test]
    async fn test_remove_leg_from_all() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf-remove-all");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_id = LegId::new("leg1");
        manager
            .add_participant(&conf_id, leg_id.clone())
            .await
            .unwrap();

        // Remove from all conferences
        manager.remove_leg_from_all(&leg_id).await.unwrap();

        let conf = manager.get_conference(&conf_id).await.unwrap();
        assert_eq!(conf.participant_count(), 0);

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_participants() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf-concurrent");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        // Add multiple participants concurrently
        let mut handles = vec![];
        for i in 0..5 {
            let manager = manager.clone();
            let conf_id = conf_id.clone();
            let handle = tokio::spawn(async move {
                let leg_id = LegId::new(format!("leg{}", i));
                manager.add_participant(&conf_id, leg_id).await.unwrap();
            });
            handles.push(handle);
        }

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        let conf = manager.get_conference(&conf_id).await.unwrap();
        assert_eq!(conf.participant_count(), 5);

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_get_conference_for_leg() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-conf-lookup");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_id = LegId::new("leg1");
        manager
            .add_participant(&conf_id, leg_id.clone())
            .await
            .unwrap();

        let found_conf = manager.get_conference_id_for_leg(&leg_id).await;
        assert!(found_conf.is_some());
        assert_eq!(found_conf.unwrap().0, "test-conf-lookup");

        // Non-existent leg
        let not_found = manager
            .get_conference_id_for_leg(&LegId::new("nonexistent"))
            .await;
        assert!(not_found.is_none());

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_cross_conference_isolation() {
        let manager = ConferenceManager::new();
        let conf1 = ConferenceId::from("conf1");
        let conf2 = ConferenceId::from("conf2");

        manager
            .create_conference(conf1.clone(), None)
            .await
            .unwrap();
        manager
            .create_conference(conf2.clone(), None)
            .await
            .unwrap();

        let leg = LegId::new("shared-leg");

        // Add to first conference
        manager.add_participant(&conf1, leg.clone()).await.unwrap();

        // Try to add to second conference (should fail)
        let result = manager.add_participant(&conf2, leg.clone()).await;
        assert!(
            result.is_err(),
            "Leg should not be able to join multiple conferences"
        );

        manager.destroy_conference(&conf1).await.unwrap();
        manager.destroy_conference(&conf2).await.unwrap();
    }
}
