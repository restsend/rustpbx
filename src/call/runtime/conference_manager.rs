//! Conference Manager for RWI CallCommand
//!
//! Manages conference rooms including create, destroy, participant management,
//! and mute/unmute functionality with real-time audio mixing.
//!
//! Supports host/moderator role: the participant who creates or merges the
//! conference becomes the host. The host can end the entire conference for
//! all participants. When only 0-1 participants remain the conference is
//! auto-destroyed. An optional `max_duration_secs` triggers auto-destroy
//! on timeout (default 1 hour when used from the CC addon).

use crate::call::domain::LegId;
use crate::media::conference_mixer::{AudioFrame, ConferenceAudioMixer};
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::info;

pub const DEFAULT_CONFERENCE_TIMEOUT_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipantRole {
    Host,
    Member,
}

impl Default for ParticipantRole {
    fn default() -> Self {
        Self::Member
    }
}

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
    pub role: ParticipantRole,
    pub joined_at: std::time::Instant,
}

impl ConferenceParticipant {
    pub fn new(leg_id: LegId) -> Self {
        Self {
            leg_id,
            muted: false,
            role: ParticipantRole::Member,
            joined_at: std::time::Instant::now(),
        }
    }

    pub fn with_role(leg_id: LegId, role: ParticipantRole) -> Self {
        Self {
            leg_id,
            muted: false,
            role,
            joined_at: std::time::Instant::now(),
        }
    }
}

/// Conference room state with audio mixing
#[derive(Debug, Clone)]
pub struct ConferenceRoom {
    pub id: ConferenceId,
    pub participants: HashMap<LegId, ConferenceParticipant>,
    pub host_leg_id: Option<LegId>,
    pub max_duration_secs: Option<u64>,
    pub created_at: std::time::Instant,
    pub max_participants: Option<usize>,
    pub locked: bool,
}

impl ConferenceRoom {
    pub fn new(id: ConferenceId, max_participants: Option<usize>) -> Self {
        Self {
            id,
            participants: HashMap::new(),
            host_leg_id: None,
            max_duration_secs: None,
            created_at: std::time::Instant::now(),
            max_participants,
            locked: false,
        }
    }

    pub fn with_host(mut self, host_leg_id: LegId) -> Self {
        self.host_leg_id = Some(host_leg_id);
        self
    }

    pub fn with_max_duration(mut self, secs: u64) -> Self {
        self.max_duration_secs = Some(secs);
        self
    }

    /// Add a participant to the conference
    pub fn add_participant(&mut self, leg_id: LegId) -> Result<()> {
        self.add_participant_with_role(leg_id, ParticipantRole::Member)
    }

    /// Add a participant with an explicit role
    pub fn add_participant_with_role(&mut self, leg_id: LegId, role: ParticipantRole) -> Result<()> {
        if let Some(max) = self.max_participants
            && self.participants.len() >= max
        {
            return Err(anyhow!("Conference is at maximum capacity"));
        }

        if self.participants.contains_key(&leg_id) {
            return Err(anyhow!("Leg {} already in conference", leg_id));
        }

        let participant = ConferenceParticipant::with_role(leg_id.clone(), role);
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

    /// Check if a leg is the host of this conference
    pub fn is_host(&self, leg_id: &LegId) -> bool {
        self.host_leg_id.as_ref() == Some(leg_id)
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
/// Note: Only input_tx is cloneable; output_rx must be accessed via get_participant_output_rx
#[derive(Clone)]
pub struct ParticipantChannels {
    /// Channel to send audio to the conference (from participant)
    pub input_tx: mpsc::Sender<AudioFrame>,
}

impl ParticipantChannels {
    /// Create a new participant channels pair with only input_tx
    pub fn new(input_tx: mpsc::Sender<AudioFrame>) -> Self {
        Self { input_tx }
    }
}

/// Global conference manager with in-server audio mixing
#[derive(Clone)]
pub struct ConferenceManager {
    conferences: Arc<RwLock<HashMap<ConferenceId, ConferenceRoom>>>,
    leg_to_conference: Arc<RwLock<HashMap<LegId, ConferenceId>>>,
    audio_mixers: Arc<RwLock<HashMap<ConferenceId, Arc<ConferenceAudioMixer>>>>,
    participant_channels: Arc<RwLock<HashMap<LegId, ParticipantChannels>>>,
    participant_output_rxs: Arc<RwLock<HashMap<LegId, mpsc::Receiver<AudioFrame>>>>,
    timeout_tokens: Arc<RwLock<HashMap<ConferenceId, tokio_util::sync::CancellationToken>>>,
}

impl ConferenceManager {
    pub fn new() -> Self {
        Self {
            conferences: Arc::new(RwLock::new(HashMap::new())),
            leg_to_conference: Arc::new(RwLock::new(HashMap::new())),
            audio_mixers: Arc::new(RwLock::new(HashMap::new())),
            participant_channels: Arc::new(RwLock::new(HashMap::new())),
            participant_output_rxs: Arc::new(RwLock::new(HashMap::new())),
            timeout_tokens: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new conference with in-server audio mixing
    pub async fn create_conference(
        &self,
        conf_id: ConferenceId,
        max_participants: Option<usize>,
    ) -> Result<ConferenceRoom> {
        self.create_conference_ex(conf_id, max_participants, None, None).await
    }

    /// Create a new conference with extended options (host, timeout)
    pub async fn create_conference_ex(
        &self,
        conf_id: ConferenceId,
        max_participants: Option<usize>,
        host_leg_id: Option<LegId>,
        max_duration_secs: Option<u64>,
    ) -> Result<ConferenceRoom> {
        let mut conferences = self.conferences.write().await;

        if conferences.contains_key(&conf_id) {
            return Err(anyhow!("Conference {} already exists", conf_id.0));
        }

        let mut conference = ConferenceRoom::new(conf_id.clone(), max_participants);
        if let Some(ref host) = host_leg_id {
            conference = conference.with_host(host.clone());
        }
        if let Some(dur) = max_duration_secs {
            conference = conference.with_max_duration(dur);
        }
        conferences.insert(conf_id.clone(), conference.clone());

        let mut audio_mixers = self.audio_mixers.write().await;
        let mixer = Arc::new(ConferenceAudioMixer::new(conf_id.0.clone(), 8000));
        mixer.start();
        audio_mixers.insert(conf_id.clone(), mixer);
        info!(conf_id = %conf_id.0, "Conference created with local audio mixing");

        drop(audio_mixers);
        drop(conferences);

        if let Some(dur) = max_duration_secs {
            self.spawn_timeout(conf_id.clone(), dur).await;
        }

        Ok(conference)
    }

    async fn spawn_timeout(&self, conf_id: ConferenceId, dur_secs: u64) {
        let manager = self.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let child = cancel.child_token();

        {
            let mut tokens = self.timeout_tokens.write().await;
            tokens.insert(conf_id.clone(), cancel);
        }

        crate::utils::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(dur_secs)) => {
                    info!(conf_id = %conf_id.0, "Conference timed out, auto-destroying");
                    let _ = manager.destroy_conference(&conf_id).await;
                }
                _ = child.cancelled() => {}
            }
        });
    }

    /// Get a conference if it exists
    pub async fn get_conference(&self, conf_id: &ConferenceId) -> Option<ConferenceRoom> {
        let conferences = self.conferences.read().await;
        conferences.get(conf_id).cloned()
    }

    /// Destroy a conference
    pub async fn destroy_conference(&self, conf_id: &ConferenceId) -> Result<()> {
        {
            let mut tokens = self.timeout_tokens.write().await;
            if let Some(token) = tokens.remove(conf_id) {
                token.cancel();
            }
        }

        let mut audio_mixers = self.audio_mixers.write().await;
        if let Some(mixer) = audio_mixers.remove(conf_id) {
            mixer.stop().await;
        }

        let mut conferences = self.conferences.write().await;
        let mut leg_map = self.leg_to_conference.write().await;
        let mut participant_channels = self.participant_channels.write().await;
        let mut participant_output_rxs = self.participant_output_rxs.write().await;

        if let Some(conf) = conferences.get(conf_id) {
            // Remove all leg mappings and channels
            for leg_id in conf.participant_ids() {
                leg_map.remove(&leg_id);
                participant_channels.remove(&leg_id);
                participant_output_rxs.remove(&leg_id);
            }
        }

        conferences
            .remove(conf_id)
            .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

        info!(conf_id = %conf_id.0, "Conference destroyed");
        Ok(())
    }

    /// Add a participant to a conference
    pub async fn add_participant(
        &self,
        conf_id: &ConferenceId,
        leg_id: LegId,
    ) -> Result<ParticipantChannels> {
        self.add_participant_ex(conf_id, leg_id, ParticipantRole::Member).await
    }

    /// Add a participant with an explicit role
    pub async fn add_participant_ex(
        &self,
        conf_id: &ConferenceId,
        leg_id: LegId,
        role: ParticipantRole,
    ) -> Result<ParticipantChannels> {
        // Check if leg is already in another conference
        {
            let leg_map = self.leg_to_conference.read().await;
            if let Some(existing_conf) = leg_map.get(&leg_id)
                && existing_conf != conf_id
            {
                return Err(anyhow!(
                    "Leg {} is already in conference {}",
                    leg_id,
                    existing_conf.0
                ));
            }
        }

        // Add to conference room
        {
            let mut conferences = self.conferences.write().await;
            let conference = conferences
                .get_mut(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

            conference.add_participant_with_role(leg_id.clone(), role)?;
        }

        // Add to local audio mixer
        let (input_tx, output_rx) = {
            let audio_mixers = self.audio_mixers.read().await;
            let mixer = audio_mixers
                .get(conf_id)
                .ok_or_else(|| anyhow!("Audio mixer not found for conference {}", conf_id.0))?;

            mixer
                .add_participant(leg_id.clone(), CodecType::PCMU)
                .await?
        };

        let channels = ParticipantChannels::new(input_tx);

        // Store channels and mapping
        {
            let mut participant_channels = self.participant_channels.write().await;
            participant_channels.insert(leg_id.clone(), channels.clone());

            let mut leg_map = self.leg_to_conference.write().await;
            leg_map.insert(leg_id.clone(), conf_id.clone());
        }

        // Store output_rx separately for media path integration
        {
            let mut output_rxs = self.participant_output_rxs.write().await;
            output_rxs.insert(leg_id.clone(), output_rx);
        }

        Ok(channels)
    }

    /// Remove a participant from a conference.
    /// Returns the number of remaining participants.
    /// If 0 or 1 remain, the conference is auto-destroyed.
    pub async fn remove_participant(&self, conf_id: &ConferenceId, leg_id: &LegId) -> Result<usize> {
        // Remove from conference room
        let remaining;
        {
            let mut conferences = self.conferences.write().await;
            let conference = conferences
                .get_mut(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;

            conference.remove_participant(leg_id)?;
            remaining = conference.participant_count();
        }

        // Remove from local audio mixer
        let audio_mixers = self.audio_mixers.read().await;
        if let Some(mixer) = audio_mixers.get(conf_id) {
            mixer.remove_participant(leg_id).await?;
        }

        // Remove channels and mapping
        {
            let mut participant_channels = self.participant_channels.write().await;
            participant_channels.remove(leg_id);

            let mut participant_output_rxs = self.participant_output_rxs.write().await;
            participant_output_rxs.remove(leg_id);

            let mut leg_map = self.leg_to_conference.write().await;
            leg_map.remove(leg_id);
        }

        if remaining == 0 {
            info!(
                conf_id = %conf_id.0,
                "Auto-destroying empty conference"
            );
            let mgr = self.clone();
            let cid = conf_id.clone();
            crate::utils::spawn(async move {
                let _ = mgr.destroy_conference(&cid).await;
            });
            return Ok(0);
        }

        Ok(remaining)
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

        // Update local audio mixer
        let audio_mixers = self.audio_mixers.read().await;
        if let Some(mixer) = audio_mixers.get(conf_id) {
            mixer.set_muted(leg_id, true).await?;
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

        // Update local audio mixer
        let audio_mixers = self.audio_mixers.read().await;
        if let Some(mixer) = audio_mixers.get(conf_id) {
            mixer.set_muted(leg_id, false).await?;
        }

        Ok(())
    }

    /// Get conference ID for a leg
    pub async fn get_conference_id_for_leg(&self, leg_id: &LegId) -> Option<ConferenceId> {
        let leg_map = self.leg_to_conference.read().await;
        leg_map.get(leg_id).cloned()
    }

    /// Get participant channels for audio streaming (input only)
    pub async fn get_participant_channels(&self, leg_id: &LegId) -> Option<ParticipantChannels> {
        let participant_channels = self.participant_channels.read().await;
        participant_channels.get(leg_id).cloned()
    }

    /// Get participant output receiver for mixed audio (to participant)
    /// Returns the output_rx for a leg, removing it from internal storage.
    /// Caller is responsible for polling this receiver to receive mixed audio.
    pub async fn take_participant_output_rx(
        &self,
        leg_id: &LegId,
    ) -> Option<mpsc::Receiver<AudioFrame>> {
        let mut participant_output_rxs = self.participant_output_rxs.write().await;
        participant_output_rxs.remove(leg_id)
    }

    /// List all conferences
    pub async fn list_conferences(&self) -> Vec<ConferenceId> {
        let conferences = self.conferences.read().await;
        conferences.keys().cloned().collect()
    }

    /// List all conferences with full details
    pub async fn list_conferences_detail(&self) -> Vec<ConferenceRoom> {
        let conferences = self.conferences.read().await;
        conferences.values().cloned().collect()
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

    /// Remove a leg from any conference (called when leg hangs up).
    /// Triggers auto-destroy if too few participants remain.
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

    /// Host ends the entire conference for all participants.
    /// Validates that the caller is the host before destroying.
    pub async fn end_by_host(&self, conf_id: &ConferenceId, host_leg_id: &LegId) -> Result<Vec<LegId>> {
        let (is_host, participant_ids) = {
            let conferences = self.conferences.read().await;
            let conf = conferences
                .get(conf_id)
                .ok_or_else(|| anyhow!("Conference {} not found", conf_id.0))?;
            (conf.is_host(host_leg_id), conf.participant_ids())
        };

        if !is_host {
            return Err(anyhow!(
                "Leg {} is not the host of conference {}",
                host_leg_id,
                conf_id.0
            ));
        }

        let removed = participant_ids.clone();
        self.destroy_conference(conf_id).await?;
        info!(
            conf_id = %conf_id.0,
            host_leg_id = %host_leg_id,
            removed_count = removed.len(),
            "Host ended conference"
        );
        Ok(removed)
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

        // Add same participant again (should return error)
        assert!(
            manager
                .add_participant(&conf_id, leg_id.clone())
                .await
                .is_err()
        );

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
            let handle = crate::utils::spawn(async move {
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

    #[tokio::test]
    async fn test_list_conferences_detail() {
        let manager = ConferenceManager::new();

        let conf1 = ConferenceId::from("detail-conf1");
        let conf2 = ConferenceId::from("detail-conf2");

        manager
            .create_conference(conf1.clone(), Some(5))
            .await
            .unwrap();
        manager
            .create_conference(conf2.clone(), None)
            .await
            .unwrap();

        manager
            .add_participant(&conf1, LegId::new("leg-a"))
            .await
            .unwrap();
        manager
            .add_participant(&conf1, LegId::new("leg-b"))
            .await
            .unwrap();

        let details = manager.list_conferences_detail().await;
        assert_eq!(details.len(), 2);

        let conf1_detail = details.iter().find(|c| c.id == conf1).unwrap();
        assert_eq!(conf1_detail.participant_count(), 2);
        assert_eq!(conf1_detail.max_participants, Some(5));

        let conf2_detail = details.iter().find(|c| c.id == conf2).unwrap();
        assert_eq!(conf2_detail.participant_count(), 0);

        manager.destroy_conference(&conf1).await.unwrap();
        manager.destroy_conference(&conf2).await.unwrap();
    }

    #[tokio::test]
    async fn test_mute_all_participants() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-mute-all");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");
        let leg3 = LegId::new("leg3");

        manager
            .add_participant(&conf_id, leg1.clone())
            .await
            .unwrap();
        manager
            .add_participant(&conf_id, leg2.clone())
            .await
            .unwrap();
        manager
            .add_participant(&conf_id, leg3.clone())
            .await
            .unwrap();

        manager.mute_participant(&conf_id, &leg1).await.unwrap();

        // Mute all remaining unmuted participants
        let conf = manager.get_conference(&conf_id).await.unwrap();
        for (lid, p) in &conf.participants {
            if !p.muted {
                manager.mute_participant(&conf_id, lid).await.unwrap();
            }
        }

        let conf = manager.get_conference(&conf_id).await.unwrap();
        assert_eq!(conf.participants.values().filter(|p| p.muted).count(), 3);

        manager.destroy_conference(&conf_id).await.unwrap();
    }

    #[tokio::test]
    async fn test_host_role_and_end_by_host() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-host");

        let host_leg = LegId::new("agent-b");
        manager
            .create_conference_ex(
                conf_id.clone(),
                None,
                Some(host_leg.clone()),
                None,
            )
            .await
            .unwrap();

        let leg_a = LegId::new("customer-a");
        let leg_c = LegId::new("expert-c");

        manager
            .add_participant(&conf_id, leg_a.clone())
            .await
            .unwrap();
        manager
            .add_participant_ex(&conf_id, host_leg.clone(), ParticipantRole::Host)
            .await
            .unwrap();
        manager
            .add_participant(&conf_id, leg_c.clone())
            .await
            .unwrap();

        let conf = manager.get_conference(&conf_id).await.unwrap();
        assert_eq!(conf.participant_count(), 3);
        assert!(conf.is_host(&host_leg));
        assert!(!conf.is_host(&leg_a));

        // Non-host cannot end
        let result = manager.end_by_host(&conf_id, &leg_a).await;
        assert!(result.is_err());

        // Host ends conference
        let removed = manager.end_by_host(&conf_id, &host_leg).await.unwrap();
        assert_eq!(removed.len(), 3);

        assert!(manager.get_conference(&conf_id).await.is_none());
    }

    #[tokio::test]
    async fn test_auto_destroy_on_last_participant() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-auto-destroy");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_a = LegId::new("leg-a");
        let leg_b = LegId::new("leg-b");
        let leg_c = LegId::new("leg-c");

        manager.add_participant(&conf_id, leg_a.clone()).await.unwrap();
        manager.add_participant(&conf_id, leg_b.clone()).await.unwrap();
        manager.add_participant(&conf_id, leg_c.clone()).await.unwrap();

        assert_eq!(
            manager.get_conference(&conf_id).await.unwrap().participant_count(),
            3
        );

        // Remove leg_a -> 2 remain, conference stays
        let remaining = manager.remove_participant(&conf_id, &leg_a).await.unwrap();
        assert_eq!(remaining, 2);
        assert!(manager.get_conference(&conf_id).await.is_some());

        // Remove leg_b -> 1 remain (C still in conference), stays alive
        let remaining = manager.remove_participant(&conf_id, &leg_b).await.unwrap();
        assert_eq!(remaining, 1);
        assert!(manager.get_conference(&conf_id).await.is_some(),
            "Conference should stay alive with 1 participant remaining"
        );

        // Remove leg_c -> 0 remain, auto-destroy
        let remaining = manager.remove_participant(&conf_id, &leg_c).await.unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn test_auto_destroy_on_all_leave() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-all-leave");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_a = LegId::new("leg-a");
        manager.add_participant(&conf_id, leg_a.clone()).await.unwrap();

        // Remove the only participant -> auto-destroy
        let remaining = manager.remove_participant(&conf_id, &leg_a).await.unwrap();
        assert_eq!(remaining, 0);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(manager.get_conference(&conf_id).await.is_none());
    }

    #[tokio::test]
    async fn test_conference_timeout_auto_destroy() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-timeout");

        // Create with 1 second timeout
        manager
            .create_conference_ex(
                conf_id.clone(),
                None,
                None,
                Some(1),
            )
            .await
            .unwrap();

        let leg_a = LegId::new("leg-a");
        manager.add_participant(&conf_id, leg_a.clone()).await.unwrap();

        assert!(manager.get_conference(&conf_id).await.is_some());

        // Wait for timeout
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        assert!(
            manager.get_conference(&conf_id).await.is_none(),
            "Conference should be auto-destroyed after timeout"
        );
    }

    #[tokio::test]
    async fn test_remove_leg_from_all_triggers_auto_destroy() {
        let manager = ConferenceManager::new();
        let conf_id = ConferenceId::from("test-remove-all-auto");

        manager
            .create_conference(conf_id.clone(), None)
            .await
            .unwrap();

        let leg_a = LegId::new("leg-a");
        let leg_b = LegId::new("leg-b");

        manager.add_participant(&conf_id, leg_a.clone()).await.unwrap();
        manager.add_participant(&conf_id, leg_b.clone()).await.unwrap();

        // Remove leg_a via remove_leg_from_all -> 1 remains (leg_b), conference stays alive
        manager.remove_leg_from_all(&leg_a).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            manager.get_conference(&conf_id).await.is_some(),
            "Conference should stay alive with 1 participant remaining"
        );

        // Remove leg_b -> 0 remain, auto-destroy
        manager.remove_leg_from_all(&leg_b).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            manager.get_conference(&conf_id).await.is_none(),
            "Conference should auto-destroy when empty"
        );
    }
}
