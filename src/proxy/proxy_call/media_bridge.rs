use crate::media::negotiate::CodecInfo;
use crate::media::recorder::{Leg, Recorder, RecorderOption};
use crate::media::transcoder::RtpTiming;
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
    pub dtmf_codecs_a: Vec<CodecInfo>,
    pub dtmf_codecs_b: Vec<CodecInfo>,
    pub ssrc_a: Option<u32>,
    pub ssrc_b: Option<u32>,
    started: AtomicBool,
    recorder: Arc<Mutex<Option<Recorder>>>,
    call_id: String,
    sipflow_backend: Option<Arc<dyn SipFlowBackend>>,
}

impl MediaBridge {
    fn rewrite_dtmf_duration(
        frame: &mut rustrtc::media::AudioFrame,
        source_clock_rate: u32,
        target_clock_rate: u32,
    ) {
        if frame.data.len() < 4 || source_clock_rate == target_clock_rate {
            return;
        }

        let mut payload = frame.data.to_vec();
        let duration = u16::from_be_bytes([payload[2], payload[3]]);
        let scaled = ((duration as u64 * target_clock_rate as u64) / source_clock_rate as u64)
            .min(u16::MAX as u64) as u16;
        payload[2..4].copy_from_slice(&scaled.to_be_bytes());
        frame.data = bytes::Bytes::from(payload);
    }

    fn source_dtmf_codec(source_dtmf_codecs: &[CodecInfo], payload_type: u8) -> Option<&CodecInfo> {
        source_dtmf_codecs
            .iter()
            .find(|codec| codec.payload_type == payload_type)
    }

    fn select_target_dtmf_codec<'a>(
        target_dtmf_codecs: &'a [CodecInfo],
        source_dtmf: &CodecInfo,
        target_audio_clock_rate: u32,
    ) -> Option<&'a CodecInfo> {
        target_dtmf_codecs
            .iter()
            .find(|codec| codec.clock_rate == source_dtmf.clock_rate)
            .or_else(|| {
                target_dtmf_codecs
                    .iter()
                    .find(|codec| codec.clock_rate == target_audio_clock_rate)
            })
            .or_else(|| {
                target_dtmf_codecs
                    .iter()
                    .find(|codec| codec.clock_rate == 8000)
            })
            .or_else(|| target_dtmf_codecs.first())
    }

    pub fn new(
        leg_a: Arc<dyn MediaPeer>,
        leg_b: Arc<dyn MediaPeer>,
        params_a: rustrtc::RtpCodecParameters,
        params_b: rustrtc::RtpCodecParameters,
        dtmf_codecs_a: Vec<CodecInfo>,
        dtmf_codecs_b: Vec<CodecInfo>,
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
            dtmf_codecs_a,
            dtmf_codecs_b,
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
            let dtmf_codecs_a = self.dtmf_codecs_a.clone();
            let dtmf_codecs_b = self.dtmf_codecs_b.clone();
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
                        dtmf_codecs_a,
                        dtmf_codecs_b,
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
        dtmf_codecs_a: Vec<CodecInfo>,
        dtmf_codecs_b: Vec<CodecInfo>,
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

        // Helper function to start a track forwarder
        let start_forwarder =
            |leg: Arc<dyn MediaPeer>,
             track: Arc<dyn MediaStreamTrack>,
             target_pc: rustrtc::PeerConnection,
             target_params: rustrtc::RtpCodecParameters,
             source_codec: CodecType,
             target_codec: CodecType,
             leg_enum: Leg,
             source_dtmf_codecs: Vec<CodecInfo>,
             target_dtmf_codecs: Vec<CodecInfo>,
             rec: Arc<Mutex<Option<Recorder>>>,
             cid: String,
             backend: Option<Arc<dyn SipFlowBackend>>,
             track_id: &str,
             started_ids: &mut std::collections::HashSet<String>| {
                let key = format!("{:?}-{}", leg_enum, track_id);
                if started_ids.insert(key) {
                    info!(
                        leg = ?leg_enum,
                        track_id,
                        "Starting track forwarder"
                    );
                    Some(Self::forward_track(
                        leg,
                        track,
                        target_pc,
                        target_params,
                        source_codec,
                        target_codec,
                        leg_enum,
                        source_dtmf_codecs,
                        target_dtmf_codecs,
                        None,
                        rec,
                        cid,
                        backend,
                    ))
                } else {
                    debug!(
                        leg = ?leg_enum,
                        track_id,
                        "Track already started, skipping"
                    );
                    None
                }
            };

        let mut pc_a_recv = Box::pin(pc_a.recv());
        let mut pc_b_recv = Box::pin(pc_b.recv());
        let mut pc_a_closed = false;
        let mut pc_b_closed = false;

        let transceivers_a = pc_a.get_transceivers();
        let transceivers_b = pc_b.get_transceivers();

        for transceiver in &transceivers_a {
            if let Some(receiver) = transceiver.receiver() {
                let track = receiver.track();
                let track_id = track.id().to_string();
                debug!(
                    "Pre-existing transceiver Leg A: track_id={} kind={:?}",
                    track_id,
                    track.kind()
                );
                if let Some(forwarder) = start_forwarder(
                    leg_a.clone(),
                    track,
                    pc_b.clone(),
                    params_b.clone(),
                    codec_a,
                    codec_b,
                    Leg::A,
                    dtmf_codecs_a.clone(),
                    dtmf_codecs_b.clone(),
                    recorder.clone(),
                    call_id.clone(),
                    sipflow_backend.clone(),
                    &track_id,
                    &mut started_track_ids,
                ) {
                    forwarders.push(forwarder);
                }
            }
        }

        for transceiver in &transceivers_b {
            if let Some(receiver) = transceiver.receiver() {
                let track = receiver.track();
                let track_id = track.id().to_string();
                debug!(
                    "Pre-existing transceiver Leg B: track_id={} kind={:?}",
                    track_id,
                    track.kind()
                );
                if let Some(forwarder) = start_forwarder(
                    leg_b.clone(),
                    track,
                    pc_a.clone(),
                    params_a.clone(),
                    codec_b,
                    codec_a,
                    Leg::B,
                    dtmf_codecs_b.clone(),
                    dtmf_codecs_a.clone(),
                    recorder.clone(),
                    call_id.clone(),
                    sipflow_backend.clone(),
                    &track_id,
                    &mut started_track_ids,
                ) {
                    forwarders.push(forwarder);
                }
            }
        }

        loop {
            tokio::select! {
                event_a = &mut pc_a_recv, if !pc_a_closed => {
                    if let Some(event) = event_a {
                        match event {
                            rustrtc::PeerConnectionEvent::Track(transceiver) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    let track_id = track.id().to_string();
                                    info!(
                                        "Track event Leg A: track_id={} kind={:?}",
                                        track_id,
                                        track.kind()
                                    );
                                    if let Some(forwarder) = start_forwarder(
                                        leg_a.clone(),
                                        track,
                                        pc_b.clone(),
                                        params_b.clone(),
                                        codec_a,
                                        codec_b,
                                        Leg::A,
                                        dtmf_codecs_a.clone(),
                                        dtmf_codecs_b.clone(),
                                        recorder.clone(),
                                        call_id.clone(),
                                        sipflow_backend.clone(),
                                        &track_id,
                                        &mut started_track_ids,
                                    ) {
                                        forwarders.push(forwarder);
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
                                    info!(
                                        "Track event Leg B: track_id={} kind={:?}",
                                        track_id,
                                        track.kind()
                                    );
                                    if let Some(forwarder) = start_forwarder(
                                        leg_b.clone(),
                                        track,
                                        pc_a.clone(),
                                        params_a.clone(),
                                        codec_b,
                                        codec_a,
                                        Leg::B,
                                        dtmf_codecs_b.clone(),
                                        dtmf_codecs_a.clone(),
                                        recorder.clone(),
                                        call_id.clone(),
                                        sipflow_backend.clone(),
                                        &track_id,
                                        &mut started_track_ids,
                                    ) {
                                        forwarders.push(forwarder);
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
        source_dtmf_codecs: Vec<CodecInfo>,
        target_dtmf_codecs: Vec<CodecInfo>,
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
            source_dtmf_codecs,
            target_dtmf_codecs
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
        let mut outbound_timing = RtpTiming::default();

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
                let mut drop_dtmf_for_target = false;
                let mut rewrite_source_clock_rate = source_codec.clock_rate();
                let mut rewrite_target_clock_rate = target_params.clock_rate;
                let mut rewrite_target_payload_type = target_pt;
                let source_dtmf = frame
                    .payload_type
                    .and_then(|pt| Self::source_dtmf_codec(&source_dtmf_codecs, pt))
                    .cloned();
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
                // Send RTP packet to sipflow backend if configured (only when raw_packet is available)
                if let Some(backend) = &sipflow_backend {
                    if let Some(ref rtp_packet) = frame.raw_packet {
                        if let Ok(rtp_bytes) = rtp_packet.marshal() {
                            let payload = bytes::Bytes::from(rtp_bytes);
                            let src_addr: String = if let Some(addr) = frame.source_addr {
                                format!("{:?}_{}", leg, addr)
                            } else {
                                format!("{:?}", leg)
                            };
                            let item = SipFlowItem {
                                timestamp: frame.rtp_timestamp as u64,
                                seq: frame.sequence_number.unwrap_or(0) as u64,
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
                }

                let is_dtmf = source_dtmf.is_some();

                if let Some(ref mut r) = *recorder.lock().unwrap() {
                    let recorder_dtmf_pt = if is_dtmf { frame.payload_type } else { None };
                    let recorder_dtmf_clock_rate = if is_dtmf {
                        source_dtmf.as_ref().map(|codec| codec.clock_rate)
                    } else {
                        None
                    };
                    let recorder_codec = if recorder_dtmf_pt.is_some() {
                        None
                    } else {
                        Some(source_codec)
                    };
                    let recorder_sample = MediaSample::Audio(frame.clone());
                    let _ = r.write_sample(
                        leg,
                        &recorder_sample,
                        recorder_dtmf_pt,
                        recorder_dtmf_clock_rate,
                        recorder_codec,
                    );
                }

                // Rewrite payload type to match target's expected PT
                if let Some(source_dtmf) = source_dtmf.as_ref() {
                    if let Some(target_dtmf) = Self::select_target_dtmf_codec(
                        &target_dtmf_codecs,
                        source_dtmf,
                        target_params.clock_rate,
                    ) {
                        Self::rewrite_dtmf_duration(
                            frame,
                            source_dtmf.clock_rate,
                            target_dtmf.clock_rate,
                        );
                        rewrite_source_clock_rate = source_dtmf.clock_rate;
                        rewrite_target_clock_rate = target_dtmf.clock_rate;
                        rewrite_target_payload_type = target_dtmf.payload_type;
                    } else {
                        drop_dtmf_for_target = true;
                    }
                }

                if drop_dtmf_for_target {
                    continue;
                }

                if let Some(ref mut t) = transcoder {
                    if !is_dtmf {
                        sample = MediaSample::Audio(t.transcode(frame));
                    }
                }

                if let MediaSample::Audio(ref mut outbound_frame) = sample {
                    outbound_timing.rewrite(
                        outbound_frame,
                        rewrite_source_clock_rate,
                        rewrite_target_clock_rate,
                        rewrite_target_payload_type,
                    );
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
            vec![],
            vec![],
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
            vec![],
            vec![],
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

    #[test]
    fn test_dtmf_rewriter_rescales_timestamp_duration_and_payload() {
        let mut timing = RtpTiming::default();
        let mut frame = rustrtc::media::AudioFrame {
            rtp_timestamp: 8000,
            clock_rate: 8000,
            data: bytes::Bytes::from_static(&[5, 0x80, 0x01, 0x90]),
            sequence_number: Some(320),
            payload_type: Some(101),
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        MediaBridge::rewrite_dtmf_duration(&mut frame, 8000, 48000);
        timing.rewrite(&mut frame, 8000, 48000, 110);

        assert_eq!(frame.payload_type, Some(110));
        let first_output_timestamp = frame.rtp_timestamp;
        let first_output_sequence = frame.sequence_number;
        assert_eq!(&frame.data[..], &[5, 0x80, 0x09, 0x60]);

        frame.rtp_timestamp = 8160;
        frame.clock_rate = 8000;
        frame.sequence_number = Some(321);
        frame.data = bytes::Bytes::from_static(&[5, 0x80, 0x03, 0x20]);

        MediaBridge::rewrite_dtmf_duration(&mut frame, 8000, 48000);
        timing.rewrite(&mut frame, 8000, 48000, 110);

        assert_eq!(frame.rtp_timestamp, first_output_timestamp + 960);
        assert_eq!(
            frame.sequence_number,
            Some(first_output_sequence.unwrap().wrapping_add(1))
        );
        assert_eq!(&frame.data[..], &[5, 0x80, 0x12, 0xC0]);
    }

    #[test]
    fn test_passthrough_audio_and_dtmf_share_rtp_timeline() {
        let mut timing = RtpTiming::default();
        let mut audio = rustrtc::media::AudioFrame {
            rtp_timestamp: 48_000,
            clock_rate: 48_000,
            data: bytes::Bytes::from_static(&[0; 8]),
            sequence_number: Some(1000),
            payload_type: Some(96),
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        timing.rewrite(&mut audio, 48_000, 8_000, 0);

        let first_output_timestamp = audio.rtp_timestamp;
        let first_output_sequence = audio.sequence_number.unwrap();

        let mut dtmf = rustrtc::media::AudioFrame {
            rtp_timestamp: 48_960,
            clock_rate: 48_000,
            data: bytes::Bytes::from_static(&[1, 0x80, 0x03, 0xC0]),
            sequence_number: Some(1001),
            payload_type: Some(110),
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        timing.rewrite(&mut dtmf, 48_000, 8_000, 101);

        assert_eq!(dtmf.payload_type, Some(101));
        assert_eq!(dtmf.sequence_number, Some(first_output_sequence.wrapping_add(1)));
        assert_eq!(dtmf.rtp_timestamp, first_output_timestamp + 160);
    }

}
