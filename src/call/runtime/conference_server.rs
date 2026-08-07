//! Conference Server
//!
//! Standalone MCU (Multipoint Control Unit) service that wraps a
//! [`ConferenceManager`] and manages per-participant media bridging via
//! [`ConferenceMediaBridge`]. It is deliberately decoupled from `SipSession`:
//! callers supply generic [`AudioSender`] / [`AudioReceiver`] primitives that
//! any media path (SIP RTP, WebRTC, mock) can implement.
//!
//! This is the single entry point for:
//! - Conference room lifecycle (create / destroy / list / end-by-host)
//! - Participant management (join / leave / mute / unmute)
//! - Per-participant media bridge lifecycle (forward + reverse audio loops)

use std::sync::Arc;

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

use crate::call::domain::LegId;
use crate::call::runtime::{
    AudioReceiver, AudioSender, ConferenceBridgeHandle, ConferenceId, ConferenceManager,
    ConferenceMediaBridge, ConferenceRoom, ConferenceStats, ParticipantChannels,
    ParticipantRole,
};

/// Standalone conference / MCU service.
#[derive(Clone)]
pub struct ConferenceServer {
    manager: Arc<ConferenceManager>,
    /// Per (conference, leg) media bridge handles for cleanup.
    bridges: Arc<DashMap<(ConferenceId, LegId), ConferenceBridgeHandle>>,
}

impl ConferenceServer {
    pub fn new(manager: Arc<ConferenceManager>) -> Self {
        Self {
            manager,
            bridges: Arc::new(DashMap::new()),
        }
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

    /// Destroy a conference, stopping all media bridges first.
    pub async fn destroy_conference(&self, conf_id: &ConferenceId) -> Result<()> {
        self.stop_all_bridges_for(conf_id);
        self.manager.destroy_conference(conf_id).await
    }

    /// Host ends the conference for all participants, stopping all media bridges first.
    pub async fn end_by_host(
        &self,
        conf_id: &ConferenceId,
        host_leg_id: &LegId,
    ) -> Result<Vec<LegId>> {
        self.stop_all_bridges_for(conf_id);
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

    /// Remove a participant, stopping its media bridge first.
    /// Returns the number of remaining participants.
    pub async fn remove_participant(
        &self,
        conf_id: &ConferenceId,
        leg_id: &LegId,
    ) -> Result<usize> {
        self.stop_leg_bridge(conf_id, leg_id);
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

    // ---------------------------------------------------------------------
    // Media bridging
    // ---------------------------------------------------------------------

    /// Join a leg into a conference as a member with full-duplex media bridging.
    ///
    /// This registers the participant (via `start_bridge_full_duplex`) and starts
    /// both the forward loop (mixed conference audio → participant) and the
    /// reverse loop (participant audio → conference mixer).
    pub async fn join_conference_with_media<S>(
        &self,
        conf_id: &str,
        leg_id: &LegId,
        audio_sender: S,
        audio_receiver: Box<dyn AudioReceiver>,
        codec: audio_codec::CodecType,
    ) -> Result<()>
    where
        S: AudioSender + Send + Sync + 'static,
    {
        self.join_conference_with_media_ex(
            conf_id,
            leg_id,
            ParticipantRole::Member,
            audio_sender,
            audio_receiver,
            codec,
        )
        .await
    }

    /// Join a leg into a conference with an explicit role and full-duplex media bridging.
    pub async fn join_conference_with_media_ex<S>(
        &self,
        conf_id: &str,
        leg_id: &LegId,
        role: ParticipantRole,
        audio_sender: S,
        audio_receiver: Box<dyn AudioReceiver>,
        codec: audio_codec::CodecType,
    ) -> Result<()>
    where
        S: AudioSender + Send + Sync + 'static,
    {
        let conf_id_obj = ConferenceId::from(conf_id);
        let key = (conf_id_obj.clone(), leg_id.clone());
        if self.bridges.contains_key(&key) {
            return Err(anyhow!(
                "Leg {} already bridged into conference {}",
                leg_id,
                conf_id
            ));
        }

        if role == ParticipantRole::Member {
            let bridge = ConferenceMediaBridge::new(self.manager.clone());
            let handle = bridge
                .start_bridge_full_duplex(conf_id, leg_id, audio_sender, audio_receiver, codec)
                .await?;
            self.bridges.insert(key, handle);
            return Ok(());
        }

        // Role-aware path (e.g. Host): register explicitly with the role, then
        // build the forward/reverse loops manually since `start_bridge_full_duplex`
        // always registers as a plain member.
        let channels = self
            .manager
            .add_participant_ex(&conf_id_obj, leg_id.clone(), role)
            .await?;
        let input_tx = channels.input_tx;

        let output_rx = self
            .manager
            .take_participant_output_rx(leg_id)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "No output_rx found for leg {} in conference {}",
                    leg_id,
                    conf_id
                )
            })?;

        let cancel_token = CancellationToken::new();

        let forward_cancel = cancel_token.child_token();
        let leg_forward = leg_id.clone();
        let conf_forward = conf_id.to_string();
        let forward_handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                output_rx,
                audio_sender,
                leg_forward,
                conf_forward,
                forward_cancel,
                codec,
            )
            .await;
        });

        let reverse_cancel = cancel_token.child_token();
        let leg_reverse = leg_id.clone();
        let conf_reverse = conf_id.to_string();
        let reverse_handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::reverse_loop(
                audio_receiver,
                input_tx,
                leg_reverse,
                conf_reverse,
                reverse_cancel,
                8000,
            )
            .await;
        });

        let handle = ConferenceBridgeHandle {
            _tasks: vec![forward_handle, reverse_handle],
            cancel_token,
        };
        self.bridges.insert(key, handle);
        Ok(())
    }

    /// Leave a conference: stop the media bridge and remove the participant.
    pub async fn leave_conference(&self, conf_id: &str, leg_id: &LegId) -> Result<()> {
        let conf_id_obj = ConferenceId::from(conf_id);
        self.stop_leg_bridge(&conf_id_obj, leg_id);
        self.manager
            .remove_participant(&conf_id_obj, leg_id)
            .await
            .map(|_| ())
    }

    /// Stop a single leg's media bridge (does not remove the participant).
    pub fn stop_leg_bridge(&self, conf_id: &ConferenceId, leg_id: &LegId) {
        let key = (conf_id.clone(), leg_id.clone());
        if let Some((_, handle)) = self.bridges.remove(&key) {
            handle.stop();
        }
    }

    /// Whether a leg currently has an active media bridge in the given conference.
    pub fn is_leg_bridged(&self, conf_id: &ConferenceId, leg_id: &LegId) -> bool {
        self.bridges.contains_key(&(conf_id.clone(), leg_id.clone()))
    }

    /// Number of active media bridges managed by this server.
    pub fn active_bridge_count(&self) -> usize {
        self.bridges.len()
    }

    // ---------------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------------

    fn stop_all_bridges_for(&self, conf_id: &ConferenceId) {
        let keys: Vec<(ConferenceId, LegId)> = self
            .bridges
            .iter()
            .filter(|e| &e.key().0 == conf_id)
            .map(|e| e.key().clone())
            .collect();
        for key in keys {
            if let Some((_, handle)) = self.bridges.remove(&key) {
                handle.stop();
            }
        }
    }
}
