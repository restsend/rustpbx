use crate::media::recorder::{Leg, Recorder};
use crate::proxy::proxy_call::media_peer::MediaPeer;
use anyhow::Result;
use audio_codec::CodecType;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rustrtc::media::{AudioFrame, MediaKind, MediaSample, MediaStreamTrack};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info};

pub trait IsAudioPayload {
    fn is_audio(&self) -> bool;
}

impl IsAudioPayload for AudioFrame {
    fn is_audio(&self) -> bool {
        match &self.payload_type {
            Some(pt) => {
                matches!(pt, 0 | 8 | 9 | 18 | 111) // Common audio payload types
            }
            _ => false,
        }
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
    ) -> Self {
        let recorder_option = leg_a.get_recorder_option();
        let recorder = if let Some(option) = recorder_option {
            let sample_rate = params_a.clock_rate;
            let channels = 2; // Always 2 channels for call recording
            match Recorder::new(
                &option.recorder_file,
                codec_a,
                codec_a,
                codec_b,
                sample_rate,
                channels,
            ) {
                Ok(r) => Some(r),
                Err(e) => {
                    error!("Failed to create recorder: {:?}", e);
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
            started: AtomicBool::new(false),
            recorder: Arc::new(Mutex::new(recorder)),
        }
    }

    pub async fn start(&self) -> Result<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let needs_transcoding = self.codec_a != self.codec_b;
        info!(
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
            let recorder = self.recorder.clone();
            let cancel_token = self.leg_a.cancel_token();

            tokio::spawn(async move {
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
        recorder: Arc<Mutex<Option<Recorder>>>,
    ) {
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
                debug!("Pre-existing transceiver Leg A: track_id={} kind={:?}", track_id, track_kind);
                if started_track_ids.insert(format!("A-{}", track_id)) {
                    info!("Starting pre-existing track forwarder: Leg A track_id={}", track_id);
                    forwarders.push(Self::forward_track(
                        track,
                        pc_b.clone(),
                        params_b.clone(),
                        codec_a,
                        codec_b,
                        Leg::A,
                        dtmf_pt_a,
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
                debug!("Pre-existing transceiver Leg B: track_id={} kind={:?}", track_id, track_kind);
                if started_track_ids.insert(format!("B-{}", track_id)) {
                    info!("Starting pre-existing track forwarder: Leg B track_id={}", track_id);
                    forwarders.push(Self::forward_track(
                        track,
                        pc_a.clone(),
                        params_a.clone(),
                        codec_b,
                        codec_a,
                        Leg::B,
                        dtmf_pt_b,
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
                                    info!("New Track event Leg A: track_id={} kind={:?}", track_id, track_kind);
                                    if started_track_ids.insert(format!("A-{}", track_id)) {
                                        info!("Starting new track forwarder: Leg A track_id={}", track_id);
                                        forwarders.push(Self::forward_track(
                                            track,
                                            pc_b.clone(),
                                            params_b.clone(),
                                            codec_a,
                                            codec_b,
                                            Leg::A,
                                            dtmf_pt_a,
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
        recorder: Arc<Mutex<Option<Recorder>>>,
    ) {
        let needs_transcoding = source_codec != target_codec;
        let track_id = track.id().to_string();
        info!(
            "forward_track {:?}: track_id={} source_codec={:?} target_codec={:?} needs_transcoding={}",
            leg, track_id, source_codec, target_codec, needs_transcoding
        );
        let (source_target, track_target, _) =
            rustrtc::media::track::sample_track(MediaKind::Audio, 100);
        if let Err(e) = target_pc.add_track(track_target, target_params) {
            error!(
                "forward_track for {:?} exiting early due to add_track failure {}",
                leg, e
            );
            return;
        }
        let mut transcoder = if needs_transcoding {
            Some(crate::media::Transcoder::new(source_codec, target_codec))
        } else {
            None
        };

        let mut last_seq: Option<u16> = None;
        let mut last_timestamp: Option<u32> = None;
        let mut packet_count: u64 = 0;
        let mut discontinuity_count: u64 = 0;

        while let Ok(mut sample) = track.recv().await {
            if let MediaSample::Audio(ref mut frame) = sample {
                packet_count += 1;

                // Log detailed packet info for debugging
                if packet_count <= 10 || packet_count % 100 == 0 {
                    debug!(
                        "Leg {:?} track={} pkt#{}: seq={:?} ts={} pt={:?} size={}",
                        leg, track_id, packet_count, frame.sequence_number,
                        frame.rtp_timestamp, frame.payload_type, frame.data.len()
                    );
                }

                // Validate timestamp continuity and rewrite if needed to fix interleaved streams
                if let Some(last_ts) = last_timestamp {
                    let expected_ts = last_ts.wrapping_add(frame.samples);
                    let ts_diff = frame.rtp_timestamp.wrapping_sub(expected_ts);

                    // Allow up to 10 seconds of jump to handle legitimate gaps
                    let max_reasonable_jump: u32 = frame.sample_rate * 10;

                    // Rewrite packets with large forward jumps (>10 seconds)
                    if ts_diff > max_reasonable_jump && ts_diff < (u32::MAX / 2) {
                        discontinuity_count += 1;
                        debug!(
                            "Leg {:?} track={} REWRITING timestamp #{}: seq={:?} original_ts={} -> expected_ts={} jump={} samples ({:.2}s)",
                            leg, track_id, discontinuity_count, frame.sequence_number,
                            frame.rtp_timestamp, expected_ts, ts_diff, ts_diff as f32 / frame.sample_rate as f32
                        );
                        // Rewrite the timestamp to maintain continuity
                        frame.rtp_timestamp = expected_ts;
                    }
                    // Rewrite packets with large backward jumps (>10 seconds)
                    else if ts_diff > (u32::MAX / 2) {
                        let backward_jump = last_ts.wrapping_sub(frame.rtp_timestamp);
                        if backward_jump > max_reasonable_jump {
                            discontinuity_count += 1;
                            debug!(
                                "Leg {:?} track={} REWRITING timestamp (backward) #{}: seq={:?} original_ts={} -> expected_ts={} jump=-{} samples ({:.2}s)",
                                leg, track_id, discontinuity_count, frame.sequence_number,
                                frame.rtp_timestamp, expected_ts, backward_jump, backward_jump as f32 / frame.sample_rate as f32
                            );
                            // Rewrite the timestamp to maintain continuity
                            frame.rtp_timestamp = expected_ts;
                        }
                    }
                    // Log smaller discontinuities for debugging
                    else if ts_diff > 8000 && ts_diff < max_reasonable_jump {
                        debug!(
                            "Leg {:?} track={} timestamp discontinuity (acceptable): last_ts={} expected={} actual={} jump={} samples ({:.2}s)",
                            leg, track_id, last_ts, expected_ts,
                            frame.rtp_timestamp, ts_diff, ts_diff as f32 / frame.sample_rate as f32
                        );
                    }
                }

                // Update last_timestamp with the potentially rewritten value
                last_timestamp = Some(frame.rtp_timestamp);

                // Deduplication: only skip exact duplicates (same sequence number)
                // Don't try to handle out-of-order packets - let downstream jitter buffer handle it
                if let Some(seq) = frame.sequence_number {
                    if let Some(last) = last_seq {
                        if seq == last {
                            // Exact duplicate packet, skip it
                            debug!("Skipping duplicate packet for {:?} track={} with seq={}", leg, track_id, seq);
                            continue;
                        }

                        // Detect sequence number discontinuities
                        let expected_seq = last.wrapping_add(1);
                        if seq != expected_seq {
                            let seq_diff = seq.wrapping_sub(expected_seq);
                            if seq_diff < 100 {
                                // Forward jump (packet loss)
                                debug!(
                                    "Leg {:?} track={} SEQ JUMP: expected={} actual={} gap={} (possible packet loss)",
                                    leg, track_id, expected_seq, seq, seq_diff
                                );
                            } else if seq_diff > 65435 {
                                // Backward jump (out-of-order or very late)
                                let backward = expected_seq.wrapping_sub(seq);
                                debug!(
                                    "Leg {:?} track={} SEQ BACKWARDS: expected={} actual={} (out-of-order by {})",
                                    leg, track_id, expected_seq, seq, backward
                                );
                            }
                        }
                    }
                    last_seq = Some(seq);
                }

                if frame.is_audio() {
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
            if let Err(_) = source_target.send(sample).await {
                break;
            }
        }

        info!(
            "forward_track {:?} track={} finished: total_packets={} discontinuities={}",
            leg, track_id, packet_count, discontinuity_count
        );
    }

    pub fn stop(&self) {
        info!("Stopping media bridge");
        let mut guard = self.recorder.lock().unwrap();
        if let Some(ref mut r) = *guard {
            let _ = r.finalize();
        }
        self.leg_a.stop();
        self.leg_b.stop();
    }

    pub async fn resume_forwarding(&self, track_id: &str) -> Result<()> {
        info!(track_id = %track_id, "Resuming forwarding in bridge");
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
        );

        assert_eq!(bridge.codec_a, CodecType::PCMU);
        assert_eq!(bridge.codec_b, CodecType::PCMA);

        // Should log transcoding required
        bridge.start().await.unwrap();
    }
}
