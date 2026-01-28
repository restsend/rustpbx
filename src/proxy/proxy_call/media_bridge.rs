use crate::media::recorder::{Leg, Recorder, RecorderOption};
use crate::proxy::proxy_call::media_peer::MediaPeer;
use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use anyhow::Result;
use audio_codec::CodecType;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use rustrtc::media::{MediaKind, MediaSample, MediaStreamTrack};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

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
    call_id: String,
    sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
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
        call_id: String,
        sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
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
            call_id,
            sipflow_backend,
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
            t.lock().await.get_peer_connection().await
        } else {
            None
        };

        let pc_b = if let Some(t) = tracks_b.first() {
            t.lock().await.get_peer_connection().await
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
            let leg_a = self.leg_a.clone();
            let leg_b = self.leg_b.clone();
            let call_id = self.call_id.clone();
            let sipflow_backend = self.sipflow_backend.clone();

            crate::utils::spawn(async move {
                tokio::select! {
                    _ = cancel_token.cancelled() => {},
                    _ = Self::bridge_pcs(
                        leg_a,
                        leg_b,
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
                        call_id,
                        sipflow_backend,
                    ) => {}
                }
            });
        }
        Ok(())
    }

    async fn bridge_pcs(
        leg_a: Arc<dyn MediaPeer>,
        leg_b: Arc<dyn MediaPeer>,
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
        call_id: String,
        sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
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
                        leg_a.clone(),
                        track,
                        pc_b.clone(),
                        params_b.clone(),
                        codec_a,
                        codec_b,
                        Leg::A,
                        dtmf_pt_a,
                        dtmf_pt_b,
                        None, // Don't use remote SSRC for outgoing stream
                        recorder.clone(),
                        call_id.clone(),
                        sipflow_backend.clone(),
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
                        leg_b.clone(),
                        track,
                        pc_a.clone(),
                        params_a.clone(),
                        codec_b,
                        codec_a,
                        Leg::B,
                        dtmf_pt_b,
                        dtmf_pt_a,
                        None, // Don't use remote SSRC for outgoing stream
                        recorder.clone(),
                        call_id.clone(),
                        sipflow_backend.clone(),
                    ));
                } else {
                    debug!("Track B {} already started, skipping", track_id);
                }
            }
        }

        let mut pc_a_recv = Box::pin(pc_a.recv());
        let mut pc_b_recv = Box::pin(pc_b.recv());
        let mut pc_a_closed = false;
        let mut pc_b_closed = false;

        loop {
            tokio::select! {
                event_a = &mut pc_a_recv, if !pc_a_closed => {
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
                                            leg_a.clone(),
                                            track,
                                            pc_b.clone(),
                                            params_b.clone(),
                                            codec_a,
                                            codec_b,
                                            Leg::A,
                                            dtmf_pt_a,
                                            dtmf_pt_b,
                                            None,
                                            recorder.clone(),
                                            call_id.clone(),
                                            sipflow_backend.clone(),
                                        ));
                                    } else {
                                        debug!("Track event for already started Leg A track id={}, skipping", track_id);
                                    }
                                }
                            }
                            _ => {
                            }
                        }
                        pc_a_recv = Box::pin(pc_a.recv());
                    } else {
                        debug!("Leg A PeerConnection closed");
                        pc_a_closed = true;
                    }
                }
                event_b = &mut pc_b_recv, if !pc_b_closed => {
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
                                            leg_b.clone(),
                                            track,
                                            pc_a.clone(),
                                            params_a.clone(),
                                            codec_b,
                                            codec_a,
                                            Leg::B,
                                            dtmf_pt_b,
                                            dtmf_pt_a,
                                            None,
                                            recorder.clone(),
                                            call_id.clone(),
                                            sipflow_backend.clone(),
                                        ));
                                    } else {
                                        debug!("Track event for already started Leg B track id={}, skipping", track_id);
                                    }
                                }
                            }
                            _ => {}
                        }
                        pc_b_recv = Box::pin(pc_b.recv());
                    } else {
                        debug!("Leg B PeerConnection closed");
                        pc_b_closed = true;
                    }
                }
                Some(_) = forwarders.next(), if !forwarders.is_empty() => {}
            }
            if pc_a_closed && pc_b_closed && forwarders.is_empty() {
                break;
            }
        }
    }

    async fn forward_track(
        source_peer: Arc<dyn MediaPeer>,
        track: Arc<dyn MediaStreamTrack>,
        target_pc: rustrtc::PeerConnection,
        target_params: rustrtc::RtpCodecParameters,
        source_codec: CodecType,
        target_codec: CodecType,
        leg: Leg,
        source_dtmf_pt: Option<u8>,
        target_dtmf_pt: Option<u8>,
        target_ssrc: Option<u32>,
        recorder: Arc<Mutex<Option<Recorder>>>,
        call_id: String,
        sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
    ) {
        let needs_transcoding = source_codec != target_codec;
        let track_id = track.id().to_string();
        debug!(
            call_id,
            track_id,
            ?leg,
            "forward_track source_codec={:?} target_codec={:?} needs_transcoding={} source_dtmf={:?} target_dtmf={:?}",
            source_codec,
            target_codec,
            needs_transcoding,
            source_dtmf_pt,
            target_dtmf_pt
        );
        let (source_target, track_target, _) =
            rustrtc::media::track::sample_track(MediaKind::Audio, 100);

        // Try to reuse existing transceiver first to avoid renegotiation
        let transceivers = target_pc.get_transceivers();
        let existing_transceiver = transceivers
            .iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Audio);

        if let Some(transceiver) = existing_transceiver {
            debug!(
                call_id,
                track_id,
                ?leg,
                "forward_track reusing existing transceiver",
            );

            if let Some(old_sender) = transceiver.sender() {
                let ssrc = target_ssrc.unwrap_or(old_sender.ssrc());
                let params = target_params.clone();
                let track_arc: Arc<dyn MediaStreamTrack> = track_target.clone();
                debug!(
                    call_id,
                    track_id,
                    ssrc,
                    ?leg,
                    ?params,
                    "forward_track replacing sender on existing transceiver",
                );

                let new_sender = rustrtc::RtpSender::builder(track_arc, ssrc)
                    .params(params)
                    .build();

                transceiver.set_sender(Some(new_sender));
            } else {
                let ssrc = target_ssrc.unwrap_or_else(|| rand::random::<u32>());
                let track_arc: Arc<dyn MediaStreamTrack> = track_target.clone();
                let params = target_params.clone();
                debug!(
                    call_id,
                    track_id,
                    ?leg,
                    ?params,
                    "forward_track creating new sender on existing transceiver",
                );

                let new_sender = rustrtc::RtpSender::builder(track_arc, ssrc)
                    .params(params)
                    .build();
                transceiver.set_sender(Some(new_sender));
            }
        } else {
            match target_pc.add_track(track_target, target_params.clone()) {
                Ok(_sender) => {
                    debug!(call_id, track_id, ?leg, "forward_track add_track success");
                }
                Err(e) => {
                    warn!(
                        call_id,
                        track_id,
                        ?leg,
                        "forward_track add_track failed: {}",
                        e
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
        let target_pt = target_params.payload_type;

        // Stats tracking
        let mut last_stats_time = std::time::Instant::now();
        let mut packets_since_last_stat = 0;
        let mut bytes_since_last_stat = 0;

        while let Ok(mut sample) = track.recv().await {
            if source_peer.is_suppressed(&track_id) {
                continue;
            }

            let mut is_dtmf = false;
            let mut pt_for_recorder = None;

            packets_since_last_stat += 1;
            if let MediaSample::Audio(ref f) = sample {
                bytes_since_last_stat += f.data.len();
            }

            // Periodic stats logging (simulated RTCP report)
            if last_stats_time.elapsed().as_secs() >= 5 {
                let duration = last_stats_time.elapsed().as_secs_f64();
                let bitrate_kbps = (bytes_since_last_stat as f64 * 8.0) / duration / 1000.0;
                let pps = packets_since_last_stat as f64 / duration;

                info!(
                   ?leg,
                   %track_id,
                   pps,
                   bitrate_kbps,
                   total_packets = packet_count,
                   "Media Stream Stats"
                );
                last_stats_time = std::time::Instant::now();
                packets_since_last_stat = 0;
                bytes_since_last_stat = 0;
            }

            if let MediaSample::Audio(ref mut frame) = sample {
                packet_count += 1;
                if packet_count % 250 == 1 {
                    // 5 seconds at 50pps
                    debug!(
                        call_id,
                        track_id,
                        ?leg,
                        packet_count,
                        "forward_track received"
                    );
                    // debug!(
                    //     "forward_track {:?} {} received packet #{} pt={:?}",
                    //     leg, track_id, packet_count, frame.payload_type
                    // );
                }

                if let Some(seq) = frame.sequence_number {
                    if let Some(last) = last_seq {
                        if seq == last {
                            continue;
                        }
                    }
                    last_seq = Some(seq);
                }

                // Rewrite payload type to match target's expected PT
                if let Some(pt) = frame.payload_type {
                    if Some(pt) == source_dtmf_pt {
                        is_dtmf = true;
                        pt_for_recorder = target_dtmf_pt;
                        if let Some(t_dtmf) = target_dtmf_pt {
                            frame.payload_type = Some(t_dtmf);
                        }
                    } else if !needs_transcoding {
                        // If not transcoding, rewrite audio PT to target PT
                        frame.payload_type = Some(target_pt);
                    }
                }

                if let Some(ref mut t) = transcoder {
                    if !is_dtmf {
                        sample = MediaSample::Audio(t.transcode(frame));
                        // After transcoding, ensure PT matches the target's negotiated PT
                        if let MediaSample::Audio(ref mut new_frame) = sample {
                            if new_frame.payload_type != Some(target_pt) {
                                new_frame.payload_type = Some(target_pt);
                            }
                        }
                    } else {
                        t.update_dtmf_timestamp(frame);
                    }
                }
            }

            // Send to recorder if configured
            {
                if let Some(ref mut r) = *recorder.lock().unwrap() {
                    let _ = r.write_sample(leg, &sample, pt_for_recorder);
                }
            }

            // Send RTP packet to sipflow backend if configured (only when raw_packet is available)
            if let (Some(backend), MediaSample::Audio(audio_frame)) = (&sipflow_backend, &sample) {
                if let Some(ref rtp_packet) = audio_frame.raw_packet {
                    let payload = bytes::Bytes::copy_from_slice(&rtp_packet.payload);

                    let src_addr: String = if let Some(addr) = audio_frame.source_addr {
                        format!("{:?}_{}", leg, addr)
                    } else {
                        format!("{:?}", leg)
                    };

                    let item = SipFlowItem {
                        timestamp: audio_frame.rtp_timestamp as u64,
                        seq: audio_frame.sequence_number.unwrap_or(0) as u64,
                        msg_type: SipFlowMsgType::Rtp,
                        src_addr,
                        dst_addr: format!("bridge"),
                        payload,
                    };

                    if let Err(e) = backend.record(&call_id, item) {
                        debug!("Failed to record RTP to sipflow: {}", e);
                    }
                }
            }

            if let Err(e) = source_target.send(sample).await {
                warn!(
                    call_id,
                    track_id,
                    ?leg,
                    "forward_track source_target.send failed: {}",
                    e
                );
                break;
            }
        }

        debug!(
            call_id,
            track_id,
            ?leg,
            packet_count,
            "forward_track finished",
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
            "test-call-id".to_string(),
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
            "test-call-id".to_string(),
            None,
        );

        assert_eq!(bridge.codec_a, CodecType::PCMU);
        assert_eq!(bridge.codec_b, CodecType::PCMA);

        // Should log transcoding required
        bridge.start().await.unwrap();
    }
}
