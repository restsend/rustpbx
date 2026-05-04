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
        let conf_id_str = format!("conf-{}", self.id.0);
        let conf_id = crate::call::runtime::ConferenceId::from(conf_id_str.as_str());

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
                let peer = self.peers.get(id).cloned();
                (id.clone(), peer)
            })
            .collect();

        for (leg_id, peer) in active_legs {
            let participant_leg = LegId::new(format!("{}-{}", self.id.0, leg_id));

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
                if let Err(e) = self
                    .start_conference_media_bridge_for_peer(&conf_id_str, &leg_id, &peer)
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

        let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaSample>(100);

        if let Some(sender) = audio_sender {
            info!(
                session_id = %self.id,
                conf_id = %conf_id,
                leg_id = %leg_id,
                "Using existing track sender for conference media bridge"
            );

            let cancel_token = self.cancel_token.child_token();
            let forwarder_handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel_token.cancelled() => {
                            break;
                        }
                        sample = rx.recv() => {
                            match sample {
                                Some(s) => {
                                    if sender.send(s).await.is_err() {
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                }
            });
            self.leg_tasks
                .entry(leg_id.clone())
                .or_default()
                .push(forwarder_handle);
        } else {
            warn!(
                session_id = %self.id,
                conf_id = %conf_id,
                leg_id = %leg_id,
                "No track sender found, conference audio will not be sent to this leg"
            );
        }

        let audio_receiver = self
            .create_audio_receiver_from_peer(peer)
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
            (self.callee_peer.clone(), Self::CALLEE_TRACK_ID)
        } else {
            (self.caller_peer.clone(), Self::CALLER_TRACK_ID)
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

        let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaSample>(100);

        let cancel_token = self.cancel_token.child_token();
        let forwarder_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    sample = rx.recv() => {
                        match sample {
                            Some(s) => {
                                if audio_sender.send(s).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });
        self.leg_tasks
            .entry(leg_id.clone())
            .or_default()
            .push(forwarder_handle);

        let audio_receiver = if is_callee {
            self.create_audio_receiver_from_peer(&peer).await
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
        let mut pc = None;
        for _ in 0..100 {
            if let Some(found_pc) = self.get_caller_peer_connection().await {
                pc = Some(found_pc);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pc = pc.ok_or_else(|| anyhow!("No peer connection found for conference input"))?;

        let decoder = self
            .create_audio_decoder()
            .ok_or_else(|| anyhow!("Failed to create audio decoder"))?;

        Ok(Box::new(
            crate::proxy::proxy_call::sip_session::PeerConnectionAudioReceiver::new(pc, decoder),
        ))
    }

    pub(super) async fn create_audio_receiver_from_peer(
        &self,
        peer: &Arc<dyn MediaPeer>,
    ) -> Result<Box<dyn crate::call::runtime::conference_media_bridge::AudioReceiver>> {
        let mut pc = None;
        for _ in 0..150 {
            let tracks = peer.get_tracks().await;
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
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let pc = pc.ok_or_else(|| anyhow!("No peer connection found for conference input"))?;

        let decoder = self
            .create_audio_decoder()
            .ok_or_else(|| anyhow!("Failed to create audio decoder"))?;

        Ok(Box::new(
            crate::proxy::proxy_call::sip_session::PeerConnectionAudioReceiver::new(pc, decoder),
        ))
    }

    pub(super) async fn get_caller_peer_connection(&self) -> Option<rustrtc::PeerConnection> {
        let tracks = self.caller_peer.get_tracks().await;
        for t in tracks.iter() {
            let guard = t.lock().await;
            if let Some(pc) = guard.get_peer_connection().await {
                return Some(pc);
            }
        }
        None
    }

    pub(super) fn leg_negotiated_codec(&self, leg_id: &LegId) -> audio_codec::CodecType {
        use crate::media::negotiate::MediaNegotiator;

        let sdp = self
            .leg_answers
            .get(leg_id)
            .map(|s| s.as_str())
            .or_else(|| {
                if leg_id.as_str() == "caller" {
                    self.answer.as_deref()
                } else if leg_id.as_str() == "callee" {
                    self.callee_answer_sdp.as_deref()
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

        let codec = if let Some(ref answer_sdp) = self.answer {
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

        if !self.legs.contains_key(&leg_id) {
            return Err(anyhow!("Leg not found: {}", leg_id));
        }

        self.server
            .conference_manager
            .add_participant(&conf_id.into(), leg_id)
            .await?;

        Ok(())
    }

    pub(super) async fn handle_conference_remove(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        info!(%conf_id, %leg_id, "Removing leg from conference");

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
        info!(%conf_id, %leg_id, "Muting leg in conference");

        self.server
            .conference_manager
            .mute_participant(&conf_id.into(), &leg_id)
            .await?;

        Ok(())
    }

    pub(super) async fn handle_conference_unmute(
        &mut self,
        conf_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        info!(%conf_id, %leg_id, "Unmuting leg in conference");

        self.server
            .conference_manager
            .unmute_participant(&conf_id.into(), &leg_id)
            .await?;

        Ok(())
    }

    pub(super) async fn handle_conference_destroy(&mut self, conf_id: String) -> Result<()> {
        info!(%conf_id, "Destroying conference");

        self.server
            .conference_manager
            .destroy_conference(&conf_id.into())
            .await?;

        Ok(())
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
        match self
            .start_conference_media_bridge(&mixer_id, &participant_leg)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %self.id,
                    conf_id = %mixer_id,
                    "Supervisor conference media bridge started"
                );
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(mixer_id.clone());
            }
            Err(e) => {
                warn!(
                    session_id = %self.id,
                    conf_id = %mixer_id,
                    error = %e,
                    "Failed to start supervisor conference media bridge"
                );
            }
        }

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
