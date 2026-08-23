use super::prelude::*;

/// Reads incoming RTP from a peer connection, decodes it to PCM, and feeds it
/// to the conference mixer (full-duplex conference bridge input).
pub(crate) struct PeerConnectionAudioReceiver {
    pc: rustrtc::PeerConnection,
    decoder: Box<dyn audio_codec::Decoder>,
    audio_track: Option<Arc<dyn rustrtc::media::MediaStreamTrack>>,
}

impl PeerConnectionAudioReceiver {
    pub(crate) fn new(pc: rustrtc::PeerConnection, decoder: Box<dyn audio_codec::Decoder>) -> Self {
        Self {
            pc,
            decoder,
            audio_track: None,
        }
    }

    /// Wait for and capture the first audio track from the peer connection.
    async fn capture_audio_track(&mut self) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
        // First, check pre-existing transceivers for a receiver track.
        for transceiver in self.pc.get_transceivers() {
            if transceiver.kind() == rustrtc::MediaKind::Audio
                && let Some(receiver) = transceiver.receiver()
            {
                let track = receiver.track();
                info!("Conference audio receiver using pre-existing audio track");
                return Some(track);
            }
        }

        // If no pre-existing track, wait for a Track event.
        let mut pc_recv = Box::pin(self.pc.recv());
        loop {
            match pc_recv.await {
                Some(rustrtc::PeerConnectionEvent::Track(transceiver)) => {
                    if transceiver.kind() == rustrtc::MediaKind::Audio
                        && let Some(receiver) = transceiver.receiver()
                    {
                        let track = receiver.track();
                        info!("Conference audio receiver captured audio track");
                        return Some(track);
                    }
                    pc_recv = Box::pin(self.pc.recv());
                }
                Some(_) => {
                    pc_recv = Box::pin(self.pc.recv());
                }
                None => {
                    warn!("PeerConnection closed before audio track was captured");
                    return None;
                }
            }
        }
    }
}

impl crate::call::runtime::conference_media_bridge::AudioReceiver for PeerConnectionAudioReceiver {
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
                // Capture audio track if not already captured.
                // Track availability can be racy during re-INVITE / transfer windows,
                // so keep retrying until cancellation closes the bridge.
                if self.audio_track.is_none() {
                    self.audio_track = self.capture_audio_track().await;
                    if self.audio_track.is_none() {
                        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
                        continue;
                    }
                }

                let Some(track) = self.audio_track.as_ref().cloned() else {
                    continue;
                };

                match track.recv().await {
                    Ok(rustrtc::media::MediaSample::Audio(audio_frame)) => {
                        // Decode RTP payload to PCM
                        let pcm = self.decoder.decode(&audio_frame.data);

                        return Some(
                            crate::call::runtime::conference_media_bridge::PcmAudioFrame::new(
                                pcm,
                                self.decoder.sample_rate(),
                            ),
                        );
                    }
                    Ok(_) => {
                        // Ignore non-audio samples and keep waiting for PCM payload.
                        continue;
                    }
                    Err(e) => {
                        tracing::debug!("Track recv failed, re-capturing audio track: {}", e);
                        self.audio_track = None;
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        continue;
                    }
                }
            }
        })
    }
}
