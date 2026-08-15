//! Conference Server
//!
//! Standalone MCU (Multipoint Control Unit) service that wraps a
//! [`ConferenceManager`]. It is deliberately decoupled from `SipSession`.
//!
//! This is the single entry point for:
//! - Conference room lifecycle (create / destroy / list / end-by-host)
//! - Participant management (join / leave / mute / unmute)
//!
//! Per-leg audio bridging is owned by the session (`SipSession` keeps each
//! [`ConferenceBridgeHandle`] on its leg registry via
//! `start_conference_media_bridge`); this server only tracks participant
//! lifecycle in the shared [`ConferenceManager`].

use std::sync::Arc;

use anyhow::Result;

use crate::call::domain::LegId;
use crate::call::runtime::{
    ConferenceId, ConferenceManager, ConferenceRoom, ConferenceStats, ParticipantChannels,
    ParticipantRole,
};

/// Standalone conference / MCU service.
#[derive(Clone)]
pub struct ConferenceServer {
    manager: Arc<ConferenceManager>,
}

impl ConferenceServer {
    pub fn new(manager: Arc<ConferenceManager>) -> Self {
        Self { manager }
    }

    /// Low-level accessor for the wrapped [`ConferenceManager`].
    ///
    /// **Use with care**: direct `ConferenceManager` calls bypass this server's
    /// media-bridge lifecycle management (a raw `remove_participant`/`destroy`
    /// will not stop the corresponding per-leg audio bridges). Prefer the
    /// [`ConferenceServer`] methods instead.
    pub(crate) fn manager_raw(&self) -> &Arc<ConferenceManager> {
        &self.manager
    }

    // ---------------------------------------------------------------------
    // Room management (delegated to ConferenceManager)
    // ---------------------------------------------------------------------

    pub async fn create_conference(
        &self,
        conf_id: ConferenceId,
        max_participants: Option<usize>,
    ) -> Result<ConferenceRoom> {
        self.manager.create_conference(conf_id, max_participants).await
    }

    pub async fn create_conference_ex(
        &self,
        conf_id: ConferenceId,
        max_participants: Option<usize>,
        host_leg_id: Option<LegId>,
        max_duration_secs: Option<u64>,
    ) -> Result<ConferenceRoom> {
        self.manager
            .create_conference_ex(conf_id, max_participants, host_leg_id, max_duration_secs)
            .await
    }

    pub async fn get_conference(&self, conf_id: &ConferenceId) -> Option<ConferenceRoom> {
        self.manager.get_conference(conf_id).await
    }

    pub async fn list_conferences(&self) -> Vec<ConferenceId> {
        self.manager.list_conferences().await
    }

    pub async fn list_conferences_detail(&self) -> Vec<ConferenceRoom> {
        self.manager.list_conferences_detail().await
    }

    pub async fn get_conference_stats(&self, conf_id: &ConferenceId) -> Result<ConferenceStats> {
        self.manager.get_conference_stats(conf_id).await
    }

    /// Destroy a conference and all its participants.
    pub async fn destroy_conference(&self, conf_id: &ConferenceId) -> Result<()> {
        self.manager.destroy_conference(conf_id).await
    }

    /// Host ends the conference for all participants.
    pub async fn end_by_host(
        &self,
        conf_id: &ConferenceId,
        host_leg_id: &LegId,
    ) -> Result<Vec<LegId>> {
        self.manager.end_by_host(conf_id, host_leg_id).await
    }

    // ---------------------------------------------------------------------
    // Participant management (delegated to ConferenceManager)
    // ---------------------------------------------------------------------

    pub async fn add_participant(
        &self,
        conf_id: &ConferenceId,
        leg_id: LegId,
    ) -> Result<ParticipantChannels> {
        self.manager.add_participant(conf_id, leg_id).await
    }

    pub async fn add_participant_ex(
        &self,
        conf_id: &ConferenceId,
        leg_id: LegId,
        role: ParticipantRole,
    ) -> Result<ParticipantChannels> {
        self.manager.add_participant_ex(conf_id, leg_id, role).await
    }

    /// Remove a participant from its conference.
    /// Returns the number of remaining participants.
    pub async fn remove_participant(
        &self,
        conf_id: &ConferenceId,
        leg_id: &LegId,
    ) -> Result<usize> {
        self.manager.remove_participant(conf_id, leg_id).await
    }

    pub async fn mute_participant(&self, conf_id: &ConferenceId, leg_id: &LegId) -> Result<()> {
        self.manager.mute_participant(conf_id, leg_id).await
    }

    pub async fn unmute_participant(&self, conf_id: &ConferenceId, leg_id: &LegId) -> Result<()> {
        self.manager.unmute_participant(conf_id, leg_id).await
    }

    pub async fn get_conference_id_for_leg(&self, leg_id: &LegId) -> Option<ConferenceId> {
        self.manager.get_conference_id_for_leg(leg_id).await
    }

    /// Leave a conference: remove the participant (its media bridge is owned
    /// and stopped by the session that started it).
    pub async fn leave_conference(&self, conf_id: &str, leg_id: &LegId) -> Result<()> {
        let conf_id_obj = ConferenceId::from(conf_id);
        self.manager
            .remove_participant(&conf_id_obj, leg_id)
            .await
            .map(|_| ())
    }
}
