use super::SipSession;
use crate::call::domain::LegId;
use anyhow::{Result, anyhow};
use tracing::{debug, info, warn};

impl SipSession {
    /// Join all active legs into the given conference room.
    /// Creates the room if it does not already exist.
    pub(super) async fn join_conference_mixer(&mut self, conf_id_str: &str) {
        let conf_id = crate::call::runtime::ConferenceId::from(conf_id_str);

        if self
            .server
            .conference_server
            .get_conference(&conf_id)
            .await
            .is_none()
        {
            if let Err(e) = self
                .server
                .conference_server
                .create_conference(conf_id.clone(), None)
                .await
            {
                warn!(session_id = %self.id, error = %e, "Failed to create conference");
                return;
            }
            info!(session_id = %self.id, conf_id = %conf_id_str, "Conference created");
        }

        let mut active_legs: Vec<LegId> = self
            .legs
            .iter()
            .filter(|(_, leg)| leg.is_active())
            .map(|(id, _)| id.clone())
            .collect();

        // Room dial-in (app=conference): join_conference_mixer runs right
        // after the app's ctrl.answer() is INITIATED — the caller leg is
        // typically still negotiating (not yet Connected), so the is_active()
        // filter alone yields an empty list and the participant never joins.
        // The caller leg IS the participant for dial-in: always include it.
        {
            let caller_leg = LegId::from("caller");
            if self.legs.get(&caller_leg).is_some() && !active_legs.contains(&caller_leg) {
                active_legs.insert(0, caller_leg);
            }
        }

        for leg_id in active_legs {
            let join_leg = self.participant_leg(&leg_id);
            let joined = self
                .try_start_and_store_bridge(conf_id_str, &leg_id, "conference media bridge")
                .await;
            match joined {
                Ok(()) => {
                    info!(session_id = %self.id, %join_leg, conf_id = %conf_id_str, "Leg joined conference");
                    self.emit_typed_rwi_event(&crate::rwi::ConferenceJoined {
                        conf_id: conf_id_str.to_string(),
                        call_id: self.context.session_id.to_string(),
                        leg_id: join_leg.0.clone(),
                    });
                }
                Err(e) => {
                    warn!(session_id = %self.id, %leg_id, "Failed to start conference media bridge: {}", e);
                }
            }
        }
    }

    pub(super) async fn start_conference_media_bridge(
        &mut self,
        conf_id: &str,
        leg_id: &LegId,
    ) -> Result<crate::call::runtime::ConferenceBridgeHandle> {
        let prefix = format!("{}-", self.id);
        let local_leg = LegId::from(
            leg_id
                .as_str()
                .strip_prefix(&prefix)
                .unwrap_or(leg_id.as_str()),
        );
        if let Some(peer) = self.media_leg(&local_leg) {
            let audio = crate::media::app_ingress::LegPcmStream::attach(
                peer.pc(), peer.negotiated().ok_or_else(|| anyhow!("Leg is not negotiated"))?,
                local_leg.clone(), self.cancel_token.child_token(),
            )?;
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            peer.set_egress_source(crate::media::egress::EgressSource::Inject {
                rx: parking_lot::Mutex::new(rx),
            }).await?;
            let bridge = crate::call::runtime::ConferenceMediaBridge::new(
                self.server.conference_server.manager_raw().clone(),
            );
            return bridge.start_bridge_full_duplex(
                conf_id, leg_id, tx, Box::new(MediaBridgeLegAudioReceiver::new(audio)),
                self.leg_negotiated_codec(leg_id),
            ).await;
        }
        Err(anyhow!("Missing media endpoint for {}", local_leg))
    }

    pub(super) fn leg_negotiated_codec(&self, leg_id: &LegId) -> audio_codec::CodecType {
        use crate::media::negotiate::MediaNegotiator;

        let prefix = format!("{}-", self.id);
        let local_leg = LegId::from(
            leg_id
                .as_str()
                .strip_prefix(&prefix)
                .unwrap_or(leg_id.as_str()),
        );
        let leg_id = &local_leg;
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
                info!(session_id = %self.id,
                    leg_id = %leg_id,
                    codec = ?audio.codec,
                    "Resolved per-leg codec from SDP"
                );
                audio.codec
            }
            None => {
                debug!(session_id = %self.id,
                    leg_id = %leg_id,
                    "No negotiated codec found, defaulting to PCMU"
                );
                audio_codec::CodecType::PCMU
            }
        }
    }

    pub(super) async fn handle_join_mixer(&mut self, mixer_id: String) -> Result<()> {
        info!(session_id = %self.id, %mixer_id, "Joining mixer/conference");

        let conf_id_obj = crate::call::runtime::ConferenceId::from(mixer_id.as_str());

        if self
            .server
            .conference_server
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
        .await
    }

    /// Join a specific leg of this session into a conference mixer.
    ///
    /// Mirrors `handle_join_mixer` but bridges a chosen leg (caller/callee)
    /// instead of the hard-coded `{session}-callee`. Used by the consult-
    /// transfer merge flow (`ConsultTransferManager::merge_to_conference`)
    /// to put session_a's customer leg (A) and session_b's expert leg (C)
    /// into the same mixer so they continue talking after B exits.
    ///
    /// `start_bridge_full_duplex` registers the participant exactly once
    /// with the composite leg id `{session}-{leg}` returned by
    /// `participant_leg()`, so the caller must NOT pre-register via
    /// `add_participant` (would create orphan/duplicate entries).
    pub(super) async fn handle_join_mixer_leg(
        &mut self,
        mixer_id: String,
        leg_id: LegId,
    ) -> Result<()> {
        info!(session_id = %self.id, %mixer_id, %leg_id, "Joining mixer/conference (specific leg)");

        let conf_id_obj = crate::call::runtime::ConferenceId::from(mixer_id.as_str());
        if self
            .server
            .conference_server
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            return Err(anyhow!("Conference {} not found", mixer_id));
        }

        self.require_leg(&leg_id)?;
        self.try_start_and_store_bridge(&mixer_id, &leg_id, "consult-transfer 3-way merge")
            .await?;
        if self
            .legs
            .get(&leg_id)
            .is_some_and(|leg| leg.state == crate::call::domain::LegState::Hold)
        {
            self.handle_unhold(leg_id).await?;
        }
        Ok(())
    }

    pub(super) async fn handle_leave_mixer(&mut self) -> Result<()> {
        info!(session_id = %self.id, "Leaving mixer/conference");

        // The takeover flag deliberately suppressed the B-leg-disconnect
        // cascade while the customer was parked in the takeover conference.
        // Leaving the mixer ends that state: if the flag stayed set it would
        // permanently disable the cascade and strand the caller on a dead
        // call, so it must expire here.
        if self.meta.supervisor_takeover_active {
            self.meta.supervisor_takeover_active = false;
            info!(session_id = %self.id, "Left takeover mixer; disconnect cascade re-enabled");
        }

        if let Some(conf_id) = self.conference_bridge.conf_id.take() {
            let conf_id = crate::call::runtime::ConferenceId::from(conf_id.as_str());
            for leg in self.legs.keys() {
                let _ = self
                    .server
                    .conference_server
                    .remove_participant(&conf_id, &self.participant_leg(leg))
                    .await;
            }
        }
        self.legs.stop_all_conference_bridge_handles();
        // The session-level handle is still used by the separate direct
        // cross-session bridge path; conference participants live on legs.
        self.conference_bridge.stop_bridge();
        Ok(())
    }
}

/// `AudioReceiver` backed by a MediaBridge leg's decoded PCM stream (P2.4).
///
/// This replaces the legacy `PeerConnectionAudioReceiver` (which read RTP from
/// an independent VoiceEnginePeer PC) so the conference / supervisor mixer's
/// data source is the same MediaBridge leg that carries the call's media.
struct MediaBridgeLegAudioReceiver {
    stream: crate::media::app_ingress::LegPcmStream,
}

impl MediaBridgeLegAudioReceiver {
    fn new(stream: crate::media::app_ingress::LegPcmStream) -> Self {
        Self { stream }
    }
}

impl crate::call::runtime::conference_media_bridge::AudioReceiver for MediaBridgeLegAudioReceiver {
    fn recv(
        &mut self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Option<crate::call::runtime::conference_media_bridge::PcmAudioFrame>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            loop {
                let frame = self.stream.recv().await?;
                if frame.silence {
                    continue;
                }
                return Some(
                    crate::call::runtime::conference_media_bridge::PcmAudioFrame::new(
                        frame.frame.samples,
                        frame.frame.sample_rate,
                    ),
                );
            }
        })
    }
}
