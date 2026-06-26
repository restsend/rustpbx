use super::SipSession;
use crate::call::domain::LegId;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

impl SipSession {
    pub(super) async fn setup_conference_mixer(&mut self) {
        self.join_conference_mixer(&format!("conf-{}", self.id.0))
            .await;
    }

    /// Join all active legs into the given conference room.
    /// Creates the room if it does not already exist.
    pub(super) async fn join_conference_mixer(&mut self, conf_id_str: &str) {
        let conf_id = crate::call::runtime::ConferenceId::from(conf_id_str);

        if self
            .server
            .conference_manager
            .get_conference(&conf_id)
            .await
            .is_none()
        {
            if let Err(e) = self
                .server
                .conference_manager
                .create_conference(conf_id.clone(), None)
                .await
            {
                warn!("Failed to create conference: {}", e);
                return;
            }
            info!(conf_id = %conf_id_str, "Conference created");
        }

        let active_legs: Vec<(LegId, Option<Arc<dyn MediaPeer>>)> = self
            .legs
            .iter()
            .filter(|(_, leg)| leg.is_active())
            .map(|(id, _)| {
                let peer = self.legs.peers.get(id).cloned();
                (id.clone(), peer)
            })
            .collect();

        for (leg_id, peer) in active_legs {
            let participant_leg = self.participant_leg(&leg_id);

            if let Err(e) = self
                .server
                .conference_manager
                .add_participant(&conf_id, participant_leg.clone())
                .await
            {
                warn!(%leg_id, "Failed to add participant: {}", e);
                continue;
            }

            info!(%leg_id, "Added participant to conference");

            if let Some(peer) = peer {
                let fallback_pc = if leg_id.0 == "caller" {
                    self.media
                        .media_bridge
                        .as_ref()
                        .map(|b| b.caller_pc().clone())
                } else if leg_id.0 == "callee" {
                    self.media
                        .media_bridge
                        .as_ref()
                        .map(|b| b.callee_pc().clone())
                } else {
                    None
                };

                let fallback_sender = if leg_id.0 == "caller" {
                    if let Some(b) = self.media.media_bridge.as_ref() {
                        b.get_caller_sender().await
                    } else {
                        None
                    }
                } else if leg_id.0 == "callee" {
                    if let Some(b) = self.media.media_bridge.as_ref() {
                        b.get_callee_sender().await
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Err(e) = self
                    .start_conference_media_bridge_for_peer(
                        &conf_id_str,
                        &leg_id,
                        &peer,
                        fallback_pc,
                        fallback_sender,
                    )
                    .await
                {
                    warn!(%leg_id, "Failed to start conference media bridge for dynamic leg: {}", e);
                }
            } else {
                if let Err(e) = self
                    .start_conference_media_bridge(&conf_id_str, &leg_id)
                    .await
                {
                    warn!(%leg_id, "Failed to start conference media bridge: {}", e);
                }
            }
        }
    }

    pub(super) async fn start_conference_media_bridge_for_peer(
        &mut self,
        conf_id: &str,
        leg_id: &LegId,
        peer: &Arc<dyn MediaPeer>,
        fallback_pc: Option<rustrtc::PeerConnection>,
        fallback_sender: Option<rustrtc::media::SampleStreamSource>,
    ) -> Result<crate::call::runtime::ConferenceBridgeHandle> {
        use rustrtc::media::MediaSample;

        let tracks = peer.get_tracks().await;
        let mut audio_sender = None;
        for t in &tracks {
            let guard = t.lock().await;
            if let Some(sender) = guard.get_sender() {
                audio_sender = Some(sender);
                break;
            }
        }

        if audio_sender.is_none() {
            audio_sender = fallback_sender;
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<MediaSample>(100);

        if let Some(sender) = audio_sender {
            info!(
                session_id = %self.id,
                conf_id = %conf_id,
                leg_id = %leg_id,
                "Using existing track sender for conference media bridge"
            );

            let cancel = self.cancel_token.child_token();
            self.spawn_forwarder(leg_id, cancel, sender, rx);
        } else {
            warn!(
                session_id = %self.id,
                conf_id = %conf_id,
                leg_id = %leg_id,
                "No track sender found, conference audio will not be sent to this leg"
            );
        }

        let audio_receiver = self
            .create_audio_receiver_from_peer(peer, fallback_pc)
            .await
            .map_err(|e| anyhow!("Failed to create audio receiver for dynamic leg: {}", e))?;

        let bridge = crate::call::runtime::ConferenceMediaBridge::new(
            self.server.conference_manager.clone(),
        );
        let leg_codec = self.leg_negotiated_codec(leg_id);
        bridge
            .start_bridge_full_duplex(conf_id, leg_id, tx, audio_receiver, leg_codec)
            .await
            .map_err(|e| anyhow!("Failed to start conference media bridge: {}", e))
    }

    pub(super) async fn start_conference_media_bridge(
        &mut self,
        conf_id: &str,
        leg_id: &LegId,
    ) -> Result<crate::call::runtime::ConferenceBridgeHandle> {
        use rustrtc::RtpCodecParameters;
        use rustrtc::media::MediaKind;
        use rustrtc::media::MediaSample;
        use rustrtc::media::track::sample_track;

        let is_callee = leg_id.0.ends_with("-callee") || leg_id.0 == "callee";
        if !is_callee {
            self.stop_caller_ingress_monitor().await;
        }
        let (peer, track_id) = if is_callee {
            (self.callee_peer().cloned().ok_or_else(|| anyhow!("Missing callee peer"))?, Self::CALLEE_TRACK_ID)
        } else {
            (self.caller_peer().cloned().ok_or_else(|| anyhow!("Missing caller peer"))?, Self::CALLER_TRACK_ID)
        };

        let (audio_sender, track, _feedback_rx) = sample_track(MediaKind::Audio, 100);

        let mut pc = None;
        for attempt in 0..150 {
            let tracks = peer.get_tracks().await;
            for t in &tracks {
                let guard = t.lock().await;
                if guard.id() == track_id {
                    if let Some(found_pc) = guard.get_peer_connection().await {
                        pc = Some(found_pc);
                        break;
                    }
                }
            }
            if pc.is_some() {
                break;
            }
            for t in &tracks {
                let guard = t.lock().await;
                if let Some(found_pc) = guard.get_peer_connection().await {
                    pc = Some(found_pc);
                    break;
                }
            }
            if pc.is_some() {
                break;
            }
            if attempt % 25 == 0 {
                let track_ids: Vec<_> = {
                    let mut ids = Vec::new();
                    for t in &tracks {
                        ids.push(t.lock().await.id().to_string());
                    }
                    ids
                };
                tracing::debug!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    wanted_track_id = %track_id,
                    available_tracks = ?track_ids,
                    attempt = attempt,
                    "Waiting for peer connection on conference media bridge"
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pc = pc.ok_or_else(|| {
            anyhow!(
                "No peer connection found for conference audio injection (leg={}, track={}, session={})",
                leg_id,
                track_id,
                self.id
            )
        })?;

        let params = RtpCodecParameters {
            payload_type: 0,
            clock_rate: 8000,
            channels: 1,
        };

        pc.add_track(track, params)
            .map_err(|e| anyhow!("Failed to add conference track to peer connection: {}", e))?;

        info!(
            session_id = %self.id,
            conf_id = %conf_id,
            leg_id = %leg_id,
            "Conference sample track added to existing peer connection"
        );

        let (tx, rx) = tokio::sync::mpsc::channel::<MediaSample>(100);
        let cancel = self.cancel_token.child_token();
        self.spawn_forwarder(leg_id, cancel, audio_sender, rx);

        let audio_receiver = if is_callee {
            self.create_audio_receiver_from_peer(&peer, None).await
        } else {
            self.create_audio_receiver().await
        }
        .map_err(|e| anyhow!("Failed to create audio receiver: {}", e))?;

        let bridge = crate::call::runtime::ConferenceMediaBridge::new(
            self.server.conference_manager.clone(),
        );
        let leg_codec = self.leg_negotiated_codec(leg_id);
        bridge
            .start_bridge_full_duplex(conf_id, leg_id, tx, audio_receiver, leg_codec)
            .await
            .map_err(|e| anyhow!("Failed to start conference media bridge: {}", e))
    }

    pub(super) async fn create_audio_receiver(
        &self,
    ) -> Result<Box<dyn crate::call::runtime::conference_media_bridge::AudioReceiver>> {
        let Some(peer) = self.caller_peer() else {
            return Err(anyhow!("No caller peer for conference input"));
        };
        let pc = Self::wait_for_peer_connection(peer, 100)
            .await
            .ok_or_else(|| anyhow!("No peer connection found for conference input"))?;
        self.build_audio_receiver(pc)
    }

    pub(super) async fn create_audio_receiver_from_peer(
        &self,
        peer: &Arc<dyn MediaPeer>,
        fallback_pc: Option<rustrtc::PeerConnection>,
    ) -> Result<Box<dyn crate::call::runtime::conference_media_bridge::AudioReceiver>> {
        let pc = Self::wait_for_peer_connection(peer, 150)
            .await
            .or(fallback_pc)
            .ok_or_else(|| anyhow!("No peer connection found for conference input"))?;
        self.build_audio_receiver(pc)
    }

    pub(super) fn leg_negotiated_codec(&self, leg_id: &LegId) -> audio_codec::CodecType {
        use crate::media::negotiate::MediaNegotiator;

        let sdp = self.legs.get_answer(leg_id).or_else(|| {
            if leg_id.as_str() == "caller" {
                self.media.answer.as_deref()
            } else if leg_id.as_str() == "callee" {
                self.media.callee_answer_sdp.as_deref()
            } else {
                None
            }
        });

        match sdp.and_then(|s| MediaNegotiator::extract_leg_profile(s).audio) {
            Some(audio) => {
                info!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    codec = ?audio.codec,
                    "Resolved per-leg codec from SDP"
                );
                audio.codec
            }
            None => {
                debug!(
                    session_id = %self.id,
                    leg_id = %leg_id,
                    "No negotiated codec found, defaulting to PCMU"
                );
                audio_codec::CodecType::PCMU
            }
        }
    }

    pub(super) fn create_audio_decoder(&self) -> Option<Box<dyn audio_codec::Decoder>> {
        use crate::media::negotiate::MediaNegotiator;
        use audio_codec::create_decoder;

        let codec = if let Some(ref answer_sdp) = self.media.answer {
            let profile = MediaNegotiator::extract_leg_profile(answer_sdp);
            if let Some(audio) = profile.audio {
                info!(
                    session_id = %self.id,
                    codec = ?audio.codec,
                    payload_type = audio.payload_type,
                    "Using negotiated codec for conference decoder"
                );
                audio.codec
            } else {
                CodecType::PCMU
            }
        } else {
            CodecType::PCMU
        };

        Some(create_decoder(codec))
    }

    pub(super) async fn handle_conference_create(
        &mut self,
        conf_id: String,
        options: crate::call::domain::ConferenceOptions,
    ) -> Result<()> {
        info!(%conf_id, "Creating conference");

        let max_participants = options.max_participants.map(|m| m as usize);
        self.server
            .conference_manager
            .create_conference(conf_id.into(), max_participants)
            .await?;

        Ok(())
    }

    pub(super) async fn handle_conference_add(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        info!(%conf_id, %leg_id, "Adding leg to conference");

        self.require_leg(&leg_id)?;

        let peer = self.legs.peers.get(&leg_id).cloned();
        let bridge_result = if let Some(peer) = peer {
            self.start_conference_media_bridge_for_peer(&conf_id, &leg_id, &peer, None, None)
                .await
        } else {
            self.start_conference_media_bridge(&conf_id, &leg_id).await
        };

        match bridge_result {
            Ok(handle) => {
                info!(%conf_id, %leg_id, "Conference media bridge started for added leg");
                self.legs
                    .set_conference_bridge_handle(leg_id.clone(), handle);
                Ok(())
            }
            Err(e) => {
                warn!(%conf_id, %leg_id, error = %e, "Failed to start conference media bridge, cleaning up participant");
                let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());
                let _ = self
                    .server
                    .conference_manager
                    .remove_participant(&conf_id_obj, &leg_id)
                    .await;
                Err(e)
            }
        }
    }

    pub(super) async fn handle_conference_remove(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        info!(%conf_id, %leg_id, "Removing leg from conference");

        if let Some(handle) = self.legs.remove_conference_bridge_handle(&leg_id) {
            handle.stop();
            info!(%leg_id, "Stopped conference media bridge for removed leg");
        }

        self.server
            .conference_manager
            .remove_participant(&conf_id.into(), &leg_id)
            .await?;

        Ok(())
    }

    pub(super) async fn handle_conference_mute(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        self.set_conference_mute_state(conf_id, leg_id, true).await
    }

    pub(super) async fn handle_conference_unmute(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        self.set_conference_mute_state(conf_id, leg_id, false).await
    }

    async fn set_conference_mute_state(
        &mut self,
        conf_id: String,
        leg_id: LegId,
        mute: bool,
    ) -> Result<()> {
        let action = if mute { "Muting" } else { "Unmuting" };
        info!(%conf_id, %leg_id, "{} leg in conference", action);
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());
        if mute {
            self.server
                .conference_manager
                .mute_participant(&conf_id_obj, &leg_id)
                .await?;
        } else {
            self.server
                .conference_manager
                .unmute_participant(&conf_id_obj, &leg_id)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn handle_conference_destroy(&mut self, conf_id: String) -> Result<()> {
        info!(%conf_id, "Destroying conference");

        self.legs.stop_all_conference_bridge_handles();

        self.server
            .conference_manager
            .destroy_conference(&conf_id.into())
            .await?;

        Ok(())
    }

    pub(super) async fn handle_conference_end(
        &mut self,
        conf_id: String,
        host_leg_id: LegId,
    ) -> Result<()> {
        info!(%conf_id, %host_leg_id, "Host ending conference");

        self.legs.stop_all_conference_bridge_handles();

        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());

        let removed = self
            .server
            .conference_manager
            .end_by_host(&conf_id_obj, &host_leg_id)
            .await?;

        info!(
            %conf_id,
            %host_leg_id,
            removed_count = removed.len(),
            "Conference ended by host, all participants removed"
        );

        Ok(())
    }

    pub(super) async fn handle_conference_kick(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        info!(%conf_id, %leg_id, "Kicking leg from conference");
        self.handle_conference_remove(conf_id, leg_id).await
    }

    pub(super) async fn handle_conference_mute_all(&mut self, conf_id: String) -> Result<()> {
        info!(%conf_id, "Muting all participants in conference");

        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());
        let conf = self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .ok_or_else(|| anyhow!("Conference {} not found", conf_id))?;

        for leg_id in conf.participant_ids() {
            let _ = self
                .server
                .conference_manager
                .mute_participant(&conf_id_obj, &leg_id)
                .await;
        }

        Ok(())
    }

    pub(super) async fn handle_conference_info(
        &self,
        conf_id: String,
    ) -> Result<crate::call::runtime::ConferenceRoom> {
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());
        self.server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .ok_or_else(|| anyhow!("Conference {} not found", conf_id))
    }

    pub(super) async fn handle_conference_list(&self) -> Vec<crate::call::runtime::ConferenceRoom> {
        self.server
            .conference_manager
            .list_conferences_detail()
            .await
    }

    pub(super) async fn handle_join_mixer(&mut self, mixer_id: String) -> Result<()> {
        info!(%mixer_id, "Joining mixer/conference");

        let conf_id_obj = crate::call::runtime::ConferenceId::from(mixer_id.as_str());

        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            return Err(anyhow!("Conference {} not found", mixer_id));
        }

        let participant_leg = LegId::new(format!("{}-callee", self.id.0));
        self.try_start_and_store_bridge(
            &mixer_id,
            &participant_leg,
            "supervisor conference media bridge",
        )
        .await;

        Ok(())
    }

    pub(super) async fn handle_leave_mixer(&mut self) -> Result<()> {
        info!("Leaving mixer/conference");

        if let Some(conf_id) = self.conference_bridge.conf_id.take() {
            let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());
            let participant_leg = LegId::new(format!("{}-callee", self.id.0));

            let _ = self
                .server
                .conference_manager
                .remove_participant(&conf_id_obj, &participant_leg)
                .await;

            if let Some(ref handle) = self.conference_bridge.bridge_handle {
                handle.stop();
            }
            self.conference_bridge.bridge_handle = None;
            info!(session_id = %self.id, conf_id = %conf_id, "Left conference");
        }

        Ok(())
    }
}
