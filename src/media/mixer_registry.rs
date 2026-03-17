// src/media/mixer_registry.rs
//
// MixerRegistry - Central registry for all audio mixers (supervisor and conference)
//
// This module provides a unified interface for managing audio mixers:
// - SupervisorMixer: for listen/whisper/barge operations
// - ConferenceMixer: for MCU-style conferences
//
// Architecture:
// ┌─────────────────────────────────────────────────────────┐
// │                  MixerRegistry                          │
// │  - mixer_id -> MediaMixer                               │
// │  - session_id -> mixer_id (participant tracking)         │
// └─────────────────────────────────────────────────────────┘

use crate::media::mixer::{MediaMixer, SupervisorMixerMode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Participant role in a mixer
#[derive(Clone, Debug, PartialEq)]
pub enum MixerParticipantRole {
    /// Supervisor in a call
    Supervisor,
    /// Agent (caller) in a call
    Agent,
    /// Customer (callee) in a call
    Customer,
    /// Conference participant
    ConferenceParticipant,
}

/// Information about a participant in a mixer
#[derive(Clone, Debug)]
pub struct MixerParticipant {
    pub session_id: String,
    pub role: MixerParticipantRole,
    pub input_enabled: bool,
    pub output_enabled: bool,
}

/// Registry state for a single mixer
#[derive(Clone)]
pub struct MixerRegistryEntry {
    pub mixer: Arc<MediaMixer>,
    pub participants: Vec<MixerParticipant>,
    pub mode: MixerMode,
    pub created_at: std::time::Instant,
}

/// Mixer mode - either supervisor or conference
#[derive(Clone, Debug, PartialEq)]
pub enum MixerMode {
    Supervisor {
        supervisor_session_id: String,
        target_session_id: String,
        mode: SupervisorMixerMode,
    },
    Conference {
        room_id: String,
    },
}

/// Main registry for all active mixers
pub struct MixerRegistry {
    /// mixer_id -> mixer entry
    mixers: Arc<Mutex<HashMap<String, MixerRegistryEntry>>>,
    /// session_id -> mixer_id (for quick lookup)
    participants: Arc<Mutex<HashMap<String, String>>>,
    /// Cancellation token for shutdown
    #[allow(dead_code)]
    cancel_token: CancellationToken,
}

impl MixerRegistry {
    /// Create a new MixerRegistry
    pub fn new() -> Self {
        Self {
            mixers: Arc::new(Mutex::new(HashMap::new())),
            participants: Arc::new(Mutex::new(HashMap::new())),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Create a supervisor mixer
    ///
    /// # Arguments
    /// * `mixer_id` - Unique identifier for this mixer
    /// * `supervisor_session_id` - Session ID of the supervisor
    /// * `target_session_id` - Session ID of the target call
    /// * `mode` - Supervisor mode (Listen/Whisper/Barge)
    ///
    /// # Returns
    /// The created MediaMixer instance
    pub fn create_supervisor_mixer(
        &self,
        mixer_id: String,
        supervisor_session_id: String,
        target_session_id: String,
        mode: SupervisorMixerMode,
    ) -> Arc<MediaMixer> {
        let mixer = Arc::new(MediaMixer::new(mixer_id.clone(), 8000));

        // Configure supervisor mode
        mixer.set_mode(mode.clone());

        // Create participants for supervisor and target
        let participants = vec![
            MixerParticipant {
                session_id: supervisor_session_id.clone(),
                role: MixerParticipantRole::Supervisor,
                input_enabled: true,
                output_enabled: true,
            },
            MixerParticipant {
                session_id: target_session_id.clone(),
                role: MixerParticipantRole::Agent, // target is the agent/customer
                input_enabled: true,
                output_enabled: true,
            },
        ];

        let entry = MixerRegistryEntry {
            mixer: mixer.clone(),
            participants,
            mode: MixerMode::Supervisor {
                supervisor_session_id: supervisor_session_id.clone(),
                target_session_id: target_session_id.clone(),
                mode: mode.clone(),
            },
            created_at: std::time::Instant::now(),
        };

        // Register mixer
        {
            let mut mixers = self.mixers.lock().unwrap();
            mixers.insert(mixer_id.clone(), entry);
        }

        // Register participants for quick lookup
        {
            let mut participants = self.participants.lock().unwrap();
            participants.insert(supervisor_session_id.clone(), mixer_id.clone());
            participants.insert(target_session_id.clone(), mixer_id.clone());
        }

        info!(
            mixer_id = %mixer_id,
            supervisor = %supervisor_session_id,
            target = %target_session_id,
            mode = ?mode,
            "Created supervisor mixer"
        );

        mixer
    }

    /// Create a conference mixer
    ///
    /// # Arguments
    /// * `room_id` - Unique room/conference identifier
    /// * `sample_rate` - Audio sample rate (default 8000)
    ///
    /// # Returns
    /// The created MediaMixer instance
    pub fn create_conference_mixer(
        &self,
        room_id: String,
        sample_rate: u32,
    ) -> Arc<MediaMixer> {
        let mixer = Arc::new(MediaMixer::new(room_id.clone(), sample_rate));

        let entry = MixerRegistryEntry {
            mixer: mixer.clone(),
            participants: vec![],
            mode: MixerMode::Conference {
                room_id: room_id.clone(),
            },
            created_at: std::time::Instant::now(),
        };

        {
            let mut mixers = self.mixers.lock().unwrap();
            mixers.insert(room_id.clone(), entry);
        }

        info!(room_id = %room_id, "Created conference mixer");

        mixer
    }

    /// Add a participant to a mixer
    ///
    /// # Arguments
    /// * `mixer_id` - ID of the mixer to add the participant to
    /// * `session_id` - Session ID of the participant
    /// * `role` - Role of the participant
    ///
    /// # Returns
    /// true if participant was added successfully
    pub fn add_participant(
        &self,
        mixer_id: &str,
        session_id: String,
        role: MixerParticipantRole,
    ) -> bool {
        let participant = MixerParticipant {
            session_id: session_id.clone(),
            role,
            input_enabled: true,
            output_enabled: true,
        };

        let mut success = false;

        {
            let mut mixers = self.mixers.lock().unwrap();
            if let Some(entry) = mixers.get_mut(mixer_id) {
                entry.participants.push(participant);
                success = true;
            }
        }

        if success {
            let mut participants = self.participants.lock().unwrap();
            participants.insert(session_id.clone(), mixer_id.to_string());

            info!(
                mixer_id = %mixer_id,
                session_id = %session_id,
                "Added participant to mixer"
            );
        } else {
            warn!(
                mixer_id = %mixer_id,
                session_id = %session_id,
                "Failed to add participant - mixer not found"
            );
        }

        success
    }

    /// Remove a participant from a mixer
    ///
    /// # Arguments
    /// * `session_id` - Session ID of the participant to remove
    ///
    /// # Returns
    /// true if participant was removed successfully
    pub fn remove_participant(&self, session_id: &str) -> bool {
        let mixer_id = {
            let participants = self.participants.lock().unwrap();
            participants.get(session_id).cloned()
        };

        if let Some(mixer_id) = mixer_id {
            let mut removed = false;

            {
                let mut mixers = self.mixers.lock().unwrap();
                if let Some(entry) = mixers.get_mut(&mixer_id) {
                    entry.participants.retain(|p| p.session_id != session_id);
                    removed = true;
                }
            }

            if removed {
                let mut participants = self.participants.lock().unwrap();
                participants.remove(session_id);

                info!(
                    mixer_id = %mixer_id,
                    session_id = %session_id,
                    "Removed participant from mixer"
                );
            }

            removed
        } else {
            false
        }
    }

    /// Get a mixer by ID
    ///
    /// # Arguments
    /// * `mixer_id` - ID of the mixer to retrieve
    ///
    /// # Returns
    /// Option containing the MediaMixer if found
    pub fn get_mixer(&self, mixer_id: &str) -> Option<Arc<MediaMixer>> {
        let mixers = self.mixers.lock().unwrap();
        mixers.get(mixer_id).map(|e| e.mixer.clone())
    }

    /// Get a mixer by participant session ID
    ///
    /// # Arguments
    /// * `session_id` - Session ID of a participant
    ///
    /// # Returns
    /// Option containing the MediaMixer if the participant is in a mixer
    pub fn get_mixer_by_session(&self, session_id: &str) -> Option<Arc<MediaMixer>> {
        let mixer_id = {
            let participants = self.participants.lock().unwrap();
            participants.get(session_id).cloned()
        };

        if let Some(id) = mixer_id {
            let mixers = self.mixers.lock().unwrap();
            mixers.get(&id).map(|e| e.mixer.clone())
        } else {
            None
        }
    }

    /// Get mixer info by session ID
    pub fn get_mixer_info(&self, session_id: &str) -> Option<MixerRegistryEntry> {
        let mixer_id = {
            let participants = self.participants.lock().unwrap();
            participants.get(session_id).cloned()
        };

        if let Some(id) = mixer_id {
            let mixers = self.mixers.lock().unwrap();
            mixers.get(&id).cloned()
        } else {
            None
        }
    }

    /// Stop and remove a mixer
    ///
    /// # Arguments
    /// * `mixer_id` - ID of the mixer to remove
    ///
    /// # Returns
    /// true if mixer was stopped and removed
    pub fn remove_mixer(&self, mixer_id: &str) -> bool {
        // Stop the mixer first
        if let Some(mixer) = self.get_mixer(mixer_id) {
            mixer.stop();
        }

        // Get participants to clean up
        let participant_ids: Vec<String> = {
            let mixers = self.mixers.lock().unwrap();
            if let Some(entry) = mixers.get(mixer_id) {
                entry.participants.iter().map(|p| p.session_id.clone()).collect()
            } else {
                return false;
            }
        };

        // Remove mixer
        let removed = {
            let mut mixers = self.mixers.lock().unwrap();
            mixers.remove(mixer_id).is_some()
        };

        // Clean up participant mappings
        {
            let mut participants = self.participants.lock().unwrap();
            for session_id in participant_ids {
                participants.remove(&session_id);
            }
        }

        if removed {
            info!(mixer_id = %mixer_id, "Removed mixer");
        }

        removed
    }

    /// Get all active mixers
    pub fn list_mixers(&self) -> Vec<(String, MixerRegistryEntry)> {
        let mixers = self.mixers.lock().unwrap();
        mixers.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Get participant count for a mixer
    pub fn participant_count(&self, mixer_id: &str) -> usize {
        let mixers = self.mixers.lock().unwrap();
        mixers.get(mixer_id).map(|e| e.participants.len()).unwrap_or(0)
    }

    /// Check if a session is in any mixer
    pub fn is_in_mixer(&self, session_id: &str) -> bool {
        let participants = self.participants.lock().unwrap();
        participants.contains_key(session_id)
    }
}

impl Default for MixerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_supervisor_mixer() {
        let registry = MixerRegistry::new();

        let _mixer = registry.create_supervisor_mixer(
            "test-mixer".to_string(),
            "supervisor-1".to_string(),
            "agent-1".to_string(),
            SupervisorMixerMode::Listen,
        );

        assert!(registry.get_mixer("test-mixer").is_some());
        assert!(registry.is_in_mixer("supervisor-1"));
        assert!(registry.is_in_mixer("agent-1"));
    }

    #[test]
    fn test_add_remove_participant() {
        let registry = MixerRegistry::new();

        let _mixer = registry.create_supervisor_mixer(
            "test-mixer".to_string(),
            "supervisor-1".to_string(),
            "agent-1".to_string(),
            SupervisorMixerMode::Barge,
        );

        // Add another participant
        let result = registry.add_participant(
            "test-mixer",
            "customer-1".to_string(),
            MixerParticipantRole::Customer,
        );
        assert!(result);

        assert_eq!(registry.participant_count("test-mixer"), 3);

        // Remove participant
        let result = registry.remove_participant("customer-1");
        assert!(result);
        assert_eq!(registry.participant_count("test-mixer"), 2);
    }

    #[test]
    fn test_remove_mixer() {
        let registry = MixerRegistry::new();

        let _mixer = registry.create_supervisor_mixer(
            "test-mixer".to_string(),
            "supervisor-1".to_string(),
            "agent-1".to_string(),
            SupervisorMixerMode::Whisper,
        );

        assert!(registry.get_mixer("test-mixer").is_some());

        let result = registry.remove_mixer("test-mixer");
        assert!(result);
        assert!(registry.get_mixer("test-mixer").is_none());
        assert!(!registry.is_in_mixer("supervisor-1"));
    }

    #[test]
    fn test_conference_mixer() {
        let registry = MixerRegistry::new();

        let _mixer = registry.create_conference_mixer("room-1".to_string(), 8000);

        assert!(registry.get_mixer("room-1").is_some());

        // Add participants
        registry.add_participant(
            "room-1",
            "user-1".to_string(),
            MixerParticipantRole::ConferenceParticipant,
        );
        registry.add_participant(
            "room-1",
            "user-2".to_string(),
            MixerParticipantRole::ConferenceParticipant,
        );

        assert_eq!(registry.participant_count("room-1"), 2);

        // Get mixer by session
        let found = registry.get_mixer_by_session("user-1");
        assert!(found.is_some());
    }
}
