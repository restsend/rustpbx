use crate::media::recorder::{Leg, Recorder, RecorderOption};
use crate::proxy::proxy_call::media_peer::MediaPeer;
use anyhow::Result;
use audio_codec::CodecType;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rustrtc::media::{MediaKind, MediaSample, MediaStreamTrack};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

fn is_transcodable_audio(pt: Option<u8>, dtmf_pt: Option<u8>) -> bool {
    match pt {
        Some(payload_type) => {
            if let Some(dtmf) = dtmf_pt {
                if payload_type == dtmf {
                    return false;
                }
            }
            if payload_type == 101 {
                return false;
            }
            matches!(payload_type, 0 | 8 | 9 | 18 | 111) || (payload_type >= 96 && payload_type <= 127)
        }
        _ => false,
    }
}
pub struct MediaBridge {
    pub leg_a: Arc<dyn MediaPeer>,
    pub leg_b: Arc<dyn MediaPeer>,
    pub params_a: rustrtc::RtpCodecParameters,
    pub params_b: rustrtc::RtpCodecParameters,
    pub codec_a: CodecType,
    pub codec_b: CodecType,
    pub dtmf_pt_a: Option<u8>,
    pub dtmf_pt_b: Option<u8>,
    pub ssrc_a: Option<u32>,
    pub ssrc_b: Option<u32>,
    started: AtomicBool,
    recorder: Arc<Mutex<Option<Recorder>>>,
}

impl MediaBridge {
    pub fn new(
        leg_a: Arc<dyn MediaPeer>,
        leg_b: Arc<dyn MediaPeer>,
        params_a: rustrtc::RtpCodecParameters,
        params_b: rustrtc::RtpCodecParameters,
        dtmf_pt_a: Option<u8>,
        dtmf_pt_b: Option<u8>,
        codec_a: CodecType,
        codec_b: CodecType,
        ssrc_a: Option<u32>,
        ssrc_b: Option<u32>,
        recorder_option: Option<RecorderOption>,
    ) -> Self {
        let recorder = if let Some(option) = recorder_option {
            match Recorder::new(&option.recorder_file, codec_a) {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!("Failed to create recorder: {:?}", e);
                    None
                }
            }
        } else {
            None
        };

        Self {
            leg_a,
            leg_b,
            params_a,
            params_b,
            codec_a,
            codec_b,
            dtmf_pt_a,
            dtmf_pt_b,
            ssrc_a,
            ssrc_b,
            started: AtomicBool::new(false),
            recorder: Arc::new(Mutex::new(recorder)),
        }
    }

    pub async fn start(&self) -> Result<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let needs_transcoding = self.codec_a != self.codec_b;
        debug!(
            codec_a = ?self.codec_a,
            codec_b = ?self.codec_b,
            needs_transcoding,
            "Starting media bridge between Leg A and Leg B"
        );

        let tracks_a = self.leg_a.get_tracks().await;
        let tracks_b = self.leg_b.get_tracks().await;

        let pc_a = if let Some(t) = tracks_a.first() {
            t.lock().await.get_peer_connection()
        } else {
            None
        };

        let pc_b = if let Some(t) = tracks_b.first() {
            t.lock().await.get_peer_connection()
        } else {
            None
        };

        if let (Some(pc_a), Some(pc_b)) = (pc_a, pc_b) {
            let params_a = self.params_a.clone();
            let params_b = self.params_b.clone();
            let codec_a = self.codec_a;
            let codec_b = self.codec_b;
            let dtmf_pt_a = self.dtmf_pt_a;
            let dtmf_pt_b = self.dtmf_pt_b;
            let ssrc_a = self.ssrc_a;
            let ssrc_b = self.ssrc_b;
            let recorder = self.recorder.clone();
            let cancel_token = self.leg_a.cancel_token();

            crate::utils::spawn(async move {
                tokio::select! {
                    _ = cancel_token.cancelled() => {},
                    _ = Self::bridge_pcs(
                        pc_a,
                        pc_b,
                        params_a,
                        params_b,
                        codec_a,
                        codec_b,
                        dtmf_pt_a,
                        dtmf_pt_b,
                        ssrc_a,
                        ssrc_b,
                        recorder,
                    ) => {}
                }
            });
        }
        Ok(())
    }

    async fn bridge_pcs(
        pc_a: rustrtc::PeerConnection,
        pc_b: rustrtc::PeerConnection,
        params_a: rustrtc::RtpCodecParameters,
        params_b: rustrtc::RtpCodecParameters,
        codec_a: CodecType,
        codec_b: CodecType,
        dtmf_pt_a: Option<u8>,
        dtmf_pt_b: Option<u8>,
        ssrc_a: Option<u32>,
        ssrc_b: Option<u32>,
        recorder: Arc<Mutex<Option<Recorder>>>,
    ) {
        debug!(
            "bridge_pcs started: codec_a={:?} codec_b={:?} ssrc_a={:?} ssrc_b={:?}",
            codec_a, codec_b, ssrc_a, ssrc_b
        );
        let mut forwarders = FuturesUnordered::new();
        let mut started_track_ids = std::collections::HashSet::new();

        // Check for existing transceivers (important for RTP endpoints where tracks are pre-created)
        // This handles the case where tracks exist before bridging starts
        let transceivers_a = pc_a.get_transceivers();
        let transceivers_b = pc_b.get_transceivers();

        for transceiver in transceivers_a {
            if let Some(receiver) = transceiver.receiver() {
                let track = receiver.track();
                let track_id = track.id().to_string();
                let track_kind = track.kind();
                debug!(
                    "Pre-existing transceiver Leg A: track_id={} kind={:?}",
                    track_id, track_kind
                );
                if started_track_ids.insert(format!("A-{}", track_id)) {
                    debug!(
                        "Starting pre-existing track forwarder: Leg A track_id={}",
                        track_id
                    );
                    forwarders.push(Self::forward_track(
                        track,
                        pc_b.clone(),
                        params_b.clone(),
                        codec_a,
                        codec_b,
                        Leg::A,
                        dtmf_pt_a,
                        None, // Don't use remote SSRC for outgoing stream
                        recorder.clone(),
                    ));
                } else {
                    debug!("Track A {} already started, skipping", track_id);
                }
            }
        }

        for transceiver in transceivers_b {
            if let Some(receiver) = transceiver.receiver() {
                let track = receiver.track();
                let track_id = track.id().to_string();
                let track_kind = track.kind();
                debug!(
                    "Pre-existing transceiver Leg B: track_id={} kind={:?}",
                    track_id, track_kind
                );
                if started_track_ids.insert(format!("B-{}", track_id)) {
                    debug!(
                        "Starting pre-existing track forwarder: Leg B track_id={}",
                        track_id
                    );
                    forwarders.push(Self::forward_track(
                        track,
                        pc_a.clone(),
                        params_a.clone(),
                        codec_b,
                        codec_a,
                        Leg::B,
                        dtmf_pt_b,
                        None, // Don't use remote SSRC for outgoing stream
                        recorder.clone(),
                    ));
                } else {
                    debug!("Track B {} already started, skipping", track_id);
                }
            }
        }

        let mut pc_a_recv = Box::pin(pc_a.recv());
        let mut pc_b_recv = Box::pin(pc_b.recv());

        loop {
            tokio::select! {
                event_a = &mut pc_a_recv => {
                    if let Some(event) = event_a {
                        match event {
                            rustrtc::PeerConnectionEvent::Track(transceiver) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    let track_id = track.id().to_string();
                                    let track_kind = track.kind();
                                    debug!("New Track event Leg A: track_id={} kind={:?}", track_id, track_kind);
                                    if started_track_ids.insert(format!("A-{}", track_id)) {
                                        forwarders.push(Self::forward_track(
                                            track,
                                            pc_b.clone(),
                                            params_b.clone(),
                                            codec_a,
                                            codec_b,
                                            Leg::A,
                                            dtmf_pt_a,
                                            None,
                                            recorder.clone(),
                                        ));
                                    } else {
                                        debug!("Track event for already started Leg A track id={}, skipping", track_id);
                                    }
                                }
                            }
                            _ => {
                                debug!("Leg A PeerConnection received non-Track event");
                            }
                        }
                    }
                    pc_a_recv = Box::pin(pc_a.recv());
                }
                event_b = &mut pc_b_recv => {
                    if let Some(event) = event_b {
                        match event {
                            rustrtc::PeerConnectionEvent::Track(transceiver) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    let track_id = track.id().to_string();
                                    let track_kind = track.kind();
                                    info!("New Track event Leg B: track_id={} kind={:?}", track_id, track_kind);
                                    if started_track_ids.insert(format!("B-{}", track_id)) {
                                        info!("Starting new track forwarder: Leg B track_id={}", track_id);
                                        forwarders.push(Self::forward_track(
                                            track,
                                            pc_a.clone(),
                                            params_a.clone(),
                                            codec_b,
                                            codec_a,
                                            Leg::B,
                                            dtmf_pt_b,
                                            None,
                                            recorder.clone(),
                                        ));
                                    } else {
                                        debug!("Track event for already started Leg B track id={}, skipping", track_id);
                                    }
                                }
                            }
                            _ => {
                                debug!("Leg B PeerConnection received non-Track event");
                            }
                        }
                    }
                    pc_b_recv = Box::pin(pc_b.recv());
                }
                Some(_) = forwarders.next(), if !forwarders.is_empty() => {}
            }
        }
    }

    async fn forward_track(
        track: Arc<dyn MediaStreamTrack>,
        target_pc: rustrtc::PeerConnection,
        target_params: rustrtc::RtpCodecParameters,
        source_codec: CodecType,
        target_codec: CodecType,
        leg: Leg,
        dtmf_pt: Option<u8>,
        target_ssrc: Option<u32>,
        recorder: Arc<Mutex<Option<Recorder>>>,
    ) {
        let needs_transcoding = source_codec != target_codec;
        let track_id = track.id().to_string();
        debug!(
            "forward_track {:?}: track_id={} source_codec={:?} target_codec={:?} needs_transcoding={}",
            leg, track_id, source_codec, target_codec, needs_transcoding
        );
        let (source_target, track_target, _) =
            rustrtc::media::track::sample_track(MediaKind::Audio, 100);

        // Try to reuse existing transceiver first to avoid renegotiation
        let transceivers = target_pc.get_transceivers();
        let existing_transceiver = transceivers
            .iter()
            .find(|t| t.mid().is_some() && t.kind() == rustrtc::MediaKind::Audio);

        if let Some(transceiver) = existing_transceiver {
            debug!(
                "forward_track {:?}: Reusing existing transceiver mid={:?}",
                leg,
                transceiver.mid()
            );

            if let Some(old_sender) = transceiver.sender() {
                let ssrc = target_ssrc.unwrap_or(old_sender.ssrc());
                let params = target_params.clone();
                let track_arc: Arc<dyn MediaStreamTrack> = track_target.clone();

                debug!(
                    "forward_track {:?}: Transceiver direction: {:?}",
                    leg,
                    transceiver.direction()
                );

                let new_sender = rustrtc::RtpSender::builder(track_arc, ssrc)
                    .params(params)
                    .build();

                transceiver.set_sender(Some(new_sender));
                debug!(
                    "forward_track {:?}: Replaced sender on existing transceiver with ssrc={}",
                    leg, ssrc
                );
            } else {
                let ssrc = target_ssrc.unwrap_or_else(|| rand::random::<u32>());
                let track_arc: Arc<dyn MediaStreamTrack> = track_target.clone();
                // Use target_params but ensure SSRC is set correctly
                let params = target_params.clone();
                // If target_params has no SSRC, use the random one
                // Note: RtpCodecParameters doesn't have ssrc field directly usually,
                // but RtpSender builder takes ssrc.

                debug!(
                    "forward_track {:?}: Creating new sender with ssrc={} (target_ssrc={:?}) params={:?}",
                    leg, ssrc, target_ssrc, params
                );

                let new_sender = rustrtc::RtpSender::builder(track_arc, ssrc)
                    .params(params)
                    .build();
                transceiver.set_sender(Some(new_sender));
                debug!(
                    "forward_track {:?}: Created and attached new sender to existing transceiver ssrc={}",
                    leg, ssrc
                );
            }
        } else {
            match target_pc.add_track(track_target, target_params) {
                Ok(_sender) => {
                    debug!(
                        "forward_track {:?}: add_track success (new transceiver)",
                        leg
                    );
                }
                Err(e) => {
                    error!(
                        "forward_track for {:?} exiting early due to add_track failure {}",
                        leg, e
                    );
                    return;
                }
            }
        }
        let mut transcoder = if needs_transcoding {
            Some(crate::media::Transcoder::new(source_codec, target_codec))
        } else {
            None
        };

        let mut last_seq: Option<u16> = None;
        let mut packet_count: u64 = 0;

        while let Ok(mut sample) = track.recv().await {
            if let MediaSample::Audio(ref mut frame) = sample {
                // println!(
                //     "forward_track {:?}: track_id={} received audio frame: pt={:?} seq={:?} len={}",
                //     leg,
                //     track_id,
                //     frame.payload_type,
                //     frame.sequence_number,
                //     frame.data.len()
                // );
                packet_count += 1;
                if let Some(seq) = frame.sequence_number {
                    if let Some(last) = last_seq {
                        if seq == last {
                            continue;
                        }
                    }
                    last_seq = Some(seq);
                }

                // Only transcode actual audio codecs, not DTMF (telephone-event)
                if is_transcodable_audio(frame.payload_type, dtmf_pt) {
                    if let Some(ref mut t) = transcoder {
                        sample = MediaSample::Audio(t.transcode(frame))
                    }
                }
            }
            {
                if let Some(ref mut r) = *recorder.lock().unwrap() {
                    let _ = r.write_sample(leg, &sample, dtmf_pt);
                }
            }
            if let Err(e) = source_target.send(sample).await {
                error!("forward_track {:?}: source_target.send failed: {}", leg, e);
                break;
            }
        }

        debug!(
            "forward_track {:?} track={} finished: total_packets={}",
            leg, track_id, packet_count
        );
    }

    pub fn stop(&self) {
        let mut guard = self.recorder.lock().unwrap();
        if let Some(ref mut r) = *guard {
            let _ = r.finalize();
        }
        self.leg_a.stop();
        self.leg_b.stop();
    }

    pub async fn resume_forwarding(&self, track_id: &str) -> Result<()> {
        self.leg_a.resume_forwarding(track_id).await;
        self.leg_b.resume_forwarding(track_id).await;
        Ok(())
    }

    pub async fn suppress_forwarding(&self, track_id: &str) -> Result<()> {
        info!(track_id = %track_id, "Suppressing forwarding in bridge");
        self.leg_a.suppress_forwarding(track_id).await;
        self.leg_b.suppress_forwarding(track_id).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;

    #[tokio::test]
    async fn test_media_bridge_start_stop() {
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());
        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            rustrtc::RtpCodecParameters::default(),
            rustrtc::RtpCodecParameters::default(),
            None,
            None,
            CodecType::PCMU,
            CodecType::PCMU,
            None,
            None,
            None,
        );

        bridge.start().await.unwrap();
        bridge.resume_forwarding("test-track").await.unwrap();
        bridge.suppress_forwarding("test-track").await.unwrap();
        bridge.stop();
        assert!(leg_a.stop_called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(leg_b.stop_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_media_bridge_transcoding_detection() {
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());
        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            rustrtc::RtpCodecParameters::default(),
            rustrtc::RtpCodecParameters::default(),
            None,
            None,
            CodecType::PCMU,
            CodecType::PCMA,
            None,
            None,
            None,
        );

        assert_eq!(bridge.codec_a, CodecType::PCMU);
        assert_eq!(bridge.codec_b, CodecType::PCMA);

        // Should log transcoding required
        bridge.start().await.unwrap();
    }
}
