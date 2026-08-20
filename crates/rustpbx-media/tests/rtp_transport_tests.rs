//! RTP transport end-to-end tests (TestMediaHarness).
//! Validates the core media capabilities across all transport modes
//! (RTP / SRTP / WebRTC host-only) and both relay modes:
//!
//! - fast-path: `MediaBridge::bridge()` → `EgressSource::RewriteRelay`
//!   (transport-level zero-copy relay, same codec)
//! - transcoding: `MediaBridge::bridge()` → `EgressSource::TranscodePeer`
//!   (different codec, decode → auto-resample → re-encode)
//!
//! Topology (B2BUA):
//!
//! ```text
//! test_a ←SDP→ leg_a      leg_b ←SDP→ test_b
//!                   ↕ bridge
//! ```
//!
//! test_a sends audio → leg_a receives → bridge forwards → leg_b → test_b
//! receives. For transcoding the payload type changes to the target codec.

use std::time::Duration;

use audio_codec::CodecType;
use rustpbx_media::leg::LegInner;
use rustpbx_media::media_bridge::{LegSide, MediaBridge};
use rustpbx_media::negotiate::{CodecInfo, MediaNegotiator};
use rustrtc::config::{BufferDropStrategy, SdpCompatibilityMode};
use rustrtc::media::MediaStreamTrack;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::sample_track;
use rustrtc::peer_connection::RtpObserver;
use rustrtc::{PeerConnection, RtcConfiguration, SdpType, SessionDescription, TransportMode};

/// A standalone peer that faces one leg of the MediaBridge. Sends audio via a
/// track source; receives via its receiver track.
struct TestPeer {
    pc: PeerConnection,
    tx: rustrtc::media::track::SampleStreamSource,
    codec: CodecInfo,
    video_tx: Option<rustrtc::media::track::SampleStreamSource>,
}

impl TestPeer {
    fn new(transport: TransportMode, codec: CodecInfo) -> Self {
        let cfg = rtc_config(transport, &codec);
        let pc = PeerConnection::new(cfg);
        let (tx, track, _) = sample_track(rustrtc::media::MediaKind::Audio, 500);
        pc.add_track(track, codec.to_params()).unwrap();
        Self {
            pc,
            tx,
            codec,
            video_tx: None,
        }
    }

    /// Build a peer with an explicit media capability list (e.g. to also offer
    /// telephone-event so a DTMF payload type can be negotiated).
    fn with_caps(
        transport: TransportMode,
        codec: CodecInfo,
        caps: rustrtc::config::MediaCapabilities,
    ) -> Self {
        let mut cfg = rtc_config(transport, &codec);
        cfg.media_capabilities = Some(caps);
        let pc = PeerConnection::new(cfg);
        let (tx, track, _) = sample_track(rustrtc::media::MediaKind::Audio, 500);
        pc.add_track(track, codec.to_params()).unwrap();
        Self {
            pc,
            tx,
            codec,
            video_tx: None,
        }
    }

    /// Add a video sender track (so the peer can relay video end-to-end). The
    /// PC is rebuilt with video capabilities so the offer carries a video m-line.
    fn with_video(
        mut self,
        caps: &[rustrtc::config::VideoCapability],
        sdp_compatibility: SdpCompatibilityMode,
    ) -> Self {
        let mut cfg = rtc_config(self.pc.config().transport_mode.clone(), &self.codec);
        cfg.media_capabilities.as_mut().unwrap().video = caps.to_vec();
        cfg.sdp_compatibility = sdp_compatibility;
        self.pc.close();
        self.pc = PeerConnection::new(cfg);
        let (tx, track, _) = sample_track(rustrtc::media::MediaKind::Audio, 500);
        self.pc.add_track(track, self.codec.to_params()).unwrap();
        self.tx = tx;
        if let Some(first) = caps.first() {
            let (vtx, vtrack, _) = sample_track(rustrtc::media::MediaKind::Video, 100);
            let vparams = rustrtc::RtpCodecParameters {
                payload_type: first.payload_type,
                name: first.codec_name.clone(),
                clock_rate: first.clock_rate,
                channels: 0,
            };
            self.pc.add_track(vtrack, vparams).unwrap();
            self.video_tx = Some(vtx);
        }
        self
    }

    /// Push an audio frame (already encoded in `codec`) to the remote peer.
    fn send_audio(&self, payload: Vec<u8>, ts: u32) {
        self.tx
            .send(MediaSample::Audio(AudioFrame {
                data: bytes::Bytes::from(payload),
                payload_type: Some(self.codec.payload_type),
                rtp_timestamp: ts,
                clock_rate: self.codec.clock_rate,
                ..Default::default()
            }))
            .unwrap();
    }

    /// Receive one audio frame from the remote peer (via the bridge).
    async fn recv_audio(&self, timeout_ms: u64) -> Option<AudioFrame> {
        let track = self
            .pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Audio)?
            .receiver()?
            .track();
        tokio::time::timeout(Duration::from_millis(timeout_ms), track.recv())
            .await
            .ok()?
            .ok()
            .and_then(|s| match s {
                MediaSample::Audio(f) => Some(f),
                _ => None,
            })
    }

    /// Push a raw H264 access-unit frame to the remote peer (via the bridge).
    fn send_video(&self, payload: Vec<u8>, ts: u32) {
        if let Some(vtx) = &self.video_tx {
            vtx.send(MediaSample::Video(rustrtc::media::frame::VideoFrame {
                data: bytes::Bytes::from(payload),
                payload_type: Some(96),
                rtp_timestamp: ts,
                width: 640,
                height: 480,
                format: rustrtc::media::frame::VideoPixelFormat::I420,
                rotation_deg: 0,
                is_last_packet: true,
                header_extension: None,
                csrcs: Vec::new(),
                sequence_number: None,
                source_addr: None,
                raw_packet: None,
            }))
            .unwrap();
        }
    }

    /// Receive one video frame from the remote peer (via the bridge).
    async fn recv_video(&self, timeout_ms: u64) -> Option<rustrtc::media::frame::VideoFrame> {
        let track = self
            .pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Video)?
            .receiver()?
            .track();
        tokio::time::timeout(Duration::from_millis(timeout_ms), track.recv())
            .await
            .ok()?
            .ok()
            .and_then(|s| match s {
                MediaSample::Video(f) => Some(f),
                _ => None,
            })
    }
}

/// Observer that records inbound RTP packets (post-SRTP-unprotect) — used to
/// assert RFC 2833 telephone-event packets arrive from a leg's `send_dtmf`.
#[derive(Default)]
struct DtmfCapture {
    /// (payload_type, digit_code, end_flag, ssrc) of each telephone-event.
    events: std::sync::Mutex<Vec<(u8, u8, bool, u32)>>,
}

impl RtpObserver for DtmfCapture {
    fn on_ingress(&self, packet: &rustrtc::rtp::RtpPacket, _src: std::net::SocketAddr) {
        let payload = packet.payload.as_ref();
        if payload.len() == 4 && matches!(payload[0], 0..=15) {
            let end = payload[1] & 0x80 != 0;
            self.events.lock().unwrap().push((
                packet.header.payload_type,
                payload[0],
                end,
                packet.header.ssrc,
            ));
        }
    }

    fn on_egress(&self, packet: &rustrtc::rtp::RtpPacket, _dst: std::net::SocketAddr) {
        let payload = packet.payload.as_ref();
        if payload.len() == 4 && matches!(payload[0], 0..=15) {
            let end = payload[1] & 0x80 != 0;
            self.events.lock().unwrap().push((
                packet.header.payload_type,
                payload[0],
                end,
                packet.header.ssrc,
            ));
        }
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        // Best-effort close, guarded against runtime-shutdown panics so a test
        // failure doesn't cascade into a double-panic → SIGABRT.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if tokio::runtime::Handle::try_current().is_ok() {
                self.pc.close();
            }
        }));
    }
}

/// B2BUA test harness: two legs in a MediaBridge, each faced by a TestPeer.
struct TestMediaHarness {
    mb: MediaBridge,
    test_a: TestPeer,
    test_b: TestPeer,
}

/// Build an RtcConfiguration restricted to a single audio codec so both sides
/// negotiate exactly that codec (rustrtc's default capabilities otherwise
/// include Opus + PCMU, causing an unpredictable first-choice codec).
fn rtc_config(transport: TransportMode, codec: &CodecInfo) -> RtcConfiguration {
    let mut media_caps = rustrtc::config::MediaCapabilities::default();
    if let Some(audio_cap) = codec.to_audio_capability() {
        media_caps.audio = vec![audio_cap];
    }
    RtcConfiguration {
        transport_mode: transport,
        bind_ip: Some("127.0.0.1".into()),
        // host-only ICE: no STUN/TURN → fast localhost gathering
        ice_servers: vec![],
        enable_latching: true,
        buffer_drop_strategy: BufferDropStrategy::DropOldest,
        rtp_buffer_capacity: 500,
        sdp_compatibility: SdpCompatibilityMode::Standard,
        media_capabilities: Some(media_caps),
        runtime_handle: Some(tokio::runtime::Handle::current()),
        ..Default::default()
    }
}

/// SDP exchange between a TestPeer (UAC offerer) and a Leg (UAS answerer).
/// Uses the leg's own SDP methods so its `negotiated()` profile is populated.
/// Follows rustrtc's gathering pattern: prime → wait → re-create → set local.
async fn negotiate(test_peer: &TestPeer, leg: &LegInner) {
    // ── Offer side (TestPeer) ──
    let _ = test_peer.pc.create_offer().await.unwrap();
    test_peer.pc.wait_for_gathering_complete().await;
    let offer = test_peer.pc.create_offer().await.unwrap();
    test_peer.pc.set_local_description(offer.clone()).unwrap();
    let offer_sdp = offer.to_sdp_string();

    // ── Answer side (Leg) — leg.answer() does its own gathering ──
    let answer_sdp = leg.answer(&offer_sdp).await.unwrap();
    let answer = SessionDescription::parse(SdpType::Answer, &answer_sdp).unwrap();
    test_peer.pc.set_remote_description(answer).await.unwrap();

    // RTP / SRTP use a direct transport (no ICE) → wait for the RTP transport
    // to be ready. WebRTC needs ICE+DTLS → wait for Connected.
    let transport = test_peer.pc.config().transport_mode.clone();
    match transport {
        rustrtc::TransportMode::WebRtc => {
            match tokio::time::timeout(Duration::from_secs(10), test_peer.pc.wait_for_connected())
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => panic!("test peer failed to connect: {e}"),
                Err(_) => panic!(
                    "test peer connection timed out (codec {:?})",
                    test_peer.codec
                ),
            }
        }
        _ => {
            tokio::time::timeout(
                Duration::from_secs(10),
                test_peer
                    .pc
                    .wait_for_rtp_transport_ready(Duration::from_secs(10)),
            )
            .await
            .expect("transport ready timeout")
            .expect("transport ready error");
        }
    }
}

impl TestMediaHarness {
    async fn create(
        transport_a: TransportMode,
        codec_a: CodecType,
        transport_b: TransportMode,
        codec_b: CodecType,
    ) -> Self {
        Self::create_inner(transport_a, codec_a, transport_b, codec_b, None).await
    }

    async fn create_with_recorder(
        transport_a: TransportMode,
        codec_a: CodecType,
        transport_b: TransportMode,
        codec_b: CodecType,
        recorder: Box<dyn rustpbx_media::media_recorder::MediaRecorder>,
    ) -> Self {
        Self::create_inner(transport_a, codec_a, transport_b, codec_b, Some(recorder)).await
    }

    async fn create_inner(
        transport_a: TransportMode,
        codec_a: CodecType,
        transport_b: TransportMode,
        codec_b: CodecType,
        recorder: Option<Box<dyn rustpbx_media::media_recorder::MediaRecorder>>,
    ) -> Self {
        let codec_a = MediaNegotiator::codec_info_for_type(codec_a);
        let codec_b = MediaNegotiator::codec_info_for_type(codec_b);
        let mut mb = MediaBridge::new("rtp-test");
        let recorder_sender = if recorder.is_some() {
            Some(mb.setup_recorder_task().unwrap())
        } else {
            None
        };

        let leg_a = LegInner::from_rtc_config(
            "a",
            rtc_config(transport_a.clone(), &codec_a),
            vec![codec_a.clone()],
            true,
            -35.0,
            recorder_sender,
        )
        .unwrap();
        let leg_b = LegInner::from_rtc_config(
            "b",
            rtc_config(transport_b.clone(), &codec_b),
            vec![codec_b.clone()],
            true,
            -35.0,
            None,
        )
        .unwrap();

        // test_a ↔ leg_a, test_b ↔ leg_b
        let test_a = TestPeer::new(transport_a.clone(), codec_a.clone());
        let test_b = TestPeer::new(transport_b.clone(), codec_b.clone());
        negotiate(&test_a, &leg_a).await;
        negotiate(&test_b, &leg_b).await;

        // Ensure both legs' RTP transports (and their SRTP sessions) are ready
        // before bridging — otherwise the rewrite bridge drops packets while
        // DTLS/SRTP setup is still in flight (flaky under parallel load).
        for leg in [&leg_a, &leg_b] {
            let _ = leg
                .pc()
                .wait_for_rtp_transport_ready(Duration::from_secs(10))
                .await;
            // WebRTC: also wait for the full DTLS handshake (SRTP keys ready).
            if leg.pc().config().transport_mode == rustrtc::TransportMode::WebRtc {
                let _ =
                    tokio::time::timeout(Duration::from_secs(10), leg.pc().wait_for_connected())
                        .await;
            }
        }

        mb.replace_leg(LegSide::A, leg_a.clone()).await;
        mb.replace_leg(LegSide::B, leg_b.clone()).await;
        if let Some(recorder) = recorder {
            mb.set_recorder(recorder, None).await.unwrap();
        }

        Self { mb, test_a, test_b }
    }

    /// Explicitly tear down the bridge (stops legs + PCs) before the test ends.
    fn close(&mut self) {
        self.mb.close();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if tokio::runtime::Handle::try_current().is_ok() {
                self.test_a.pc.close();
                self.test_b.pc.close();
            }
        }));
    }

    /// Bridge + accept both legs → activates fast-path or transcoding.
    async fn bridge_and_accept(&mut self) {
        self.mb.accept(LegSide::A).await;
        self.mb.accept(LegSide::B).await;
        assert!(
            self.mb.is_bridged(),
            "route must be active after both accept"
        );
    }

    /// Assert both legs are (or are not) on the zero-copy fast-path relay.
    fn assert_relay(&self, expected: bool) {
        for side in [LegSide::A, LegSide::B] {
            let leg = self.mb.leg(side).expect("leg");
            assert_eq!(
                leg.egress_is_relay(),
                expected,
                "leg {side:?} relay mode mismatch (expected relay={expected})"
            );
        }
    }

    /// Continuously send real codec frames from test_a and poll test_b for a
    /// non-empty frame until the timeout. WebRTC loopback SRTP/DTLS can take a
    /// moment to stabilize under load, so frames are fed for the whole window.
    async fn send_and_receive(
        &mut self,
        send_codec: CodecType,
        recv_timeout_ms: u64,
    ) -> Option<AudioFrame> {
        let frames = encode_codec_frames(send_codec, 20);
        assert!(
            !frames.is_empty(),
            "must have encoded frames for {send_codec:?}"
        );
        let rate = send_codec.samplerate();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(recv_timeout_ms);
        let mut idx = 0usize;
        let mut ts = 0u32;
        let mut last_rebridge = tokio::time::Instant::now();

        loop {
            // Feed a few frames each ~30ms for the whole window.
            for _ in 0..2 {
                self.test_a
                    .send_audio(frames[idx % frames.len()].clone(), ts);
                ts = ts.wrapping_add(rate / 50);
                idx += 1;
            }
            if let Some(frame) = self.test_b.recv_audio(25).await {
                if !frame.data.is_empty() {
                    return Some(frame);
                }
            }
            // Under parallel load the WebRTC relay may take a moment to fully
            // establish; re-run bridge() periodically to re-arm it.
            if tokio::time::Instant::now() - last_rebridge >= Duration::from_secs(2) {
                let _ = self.mb.bridge().await;
                last_rebridge = tokio::time::Instant::now();
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Mirror of [`Self::send_and_receive`] for the B→A direction: test_b
    /// (baresip / agent) sends encoded frames → relayed through the bridge
    /// → test_a (caller) receives.
    async fn send_b_to_a_receive(
        &mut self,
        send_codec: CodecType,
        recv_timeout_ms: u64,
    ) -> Option<AudioFrame> {
        let frames = encode_codec_frames(send_codec, 20);
        assert!(
            !frames.is_empty(),
            "must have encoded frames for {send_codec:?}"
        );
        let rate = send_codec.samplerate();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(recv_timeout_ms);
        let mut ts = 0u32;
        let mut last_rebridge = tokio::time::Instant::now();

        loop {
            for _ in 0..2 {
                self.test_b.send_audio(frames[0].clone(), ts);
                ts = ts.wrapping_add(rate / 50);
            }
            if let Some(frame) = self.test_a.recv_audio(25).await {
                if !frame.data.is_empty() {
                    return Some(frame);
                }
            }
            if tokio::time::Instant::now() - last_rebridge >= Duration::from_secs(2) {
                let _ = self.mb.bridge().await;
                last_rebridge = tokio::time::Instant::now();
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

// ── Real audio fixtures (from fixtures/sample.wav) ───────────────────────

/// Load `fixtures/sample.wav` (16 kHz mono s16) into PCM i16 samples.
/// Embedded at compile time so the path is relative to this source file.
const SAMPLE_WAV: &[u8] = include_bytes!("../../../fixtures/sample.wav");

fn load_sample_pcm() -> Vec<i16> {
    let bytes = SAMPLE_WAV;
    // RIFF/WAVE header: data chunk starts after 44 bytes for this file.
    let pcm: Vec<i16> = bytes[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    pcm
}

/// Resample mono PCM from 16 kHz to `codec`'s sample rate, then encode into
/// `frame_ms`-sized frames of the given codec. Returns a list of encoded
/// payloads (one per ptime frame).
fn encode_codec_frames(codec: CodecType, frame_ms: u64) -> Vec<Vec<u8>> {
    let pcm = load_sample_pcm();
    let src_rate = 16000u32;
    let dst_rate = codec.samplerate();
    // Resample 16k → codec rate (audio_codec::resample is a free fn).
    let resampled = audio_codec::resample(&pcm, src_rate, dst_rate);
    let samples_per_frame = (dst_rate as u64 * frame_ms / 1000) as usize;

    let mut encoder = audio_codec::create_encoder(codec);
    resampled
        .chunks(samples_per_frame)
        .map(|chunk| {
            let mut frame = vec![0i16; samples_per_frame];
            frame[..chunk.len()].copy_from_slice(chunk);
            encoder.encode(&frame)
        })
        .filter(|f| !f.is_empty())
        .collect()
}

// ── C1: Fast-path (same codec, RewriteRelay) ─────────────────────────────

#[tokio::test]
async fn fast_path_rtp_pcmu_rtp_pcmu() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::PCMU, 5000)
        .await
        .expect("B must receive audio");
    assert_eq!(frame.payload_type, Some(0), "same codec → PT 0");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_rtp_g722_rtp_g722() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::G722,
        TransportMode::Rtp,
        CodecType::G722,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::G722, 5000)
        .await
        .expect("B must receive audio");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_srtp_pcmu_srtp_pcmu() {
    let mut h = TestMediaHarness::create(
        TransportMode::Srtp,
        CodecType::PCMU,
        TransportMode::Srtp,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::PCMU, 5000)
        .await
        .expect("B must receive audio over SDES-SRTP");
    assert_eq!(frame.payload_type, Some(0));
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_webrtc_pcmu_webrtc_pcmu() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::PCMU,
        TransportMode::WebRtc,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::PCMU, 8000)
        .await
        .expect("B must receive audio over DTLS-SRTP");
    assert_eq!(frame.payload_type, Some(0));
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_cross_transport_rtp_to_webrtc() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::WebRtc,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::PCMU, 8000)
        .await
        .expect("B must receive audio across RTP→WebRTC");
    assert_eq!(frame.payload_type, Some(0));
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

/// Local playback frames sent to a WebRTC leg must carry the SDES-MID
/// header extension (rustrtc's sender stamps it on every outbound RTP packet).
/// Without MID, Chrome cannot attribute even local playback to the audio track.
#[tokio::test]
async fn local_playback_to_webrtc_carries_mid() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    let playback_ssrc = playback_ssrc(&h, LegSide::A);
    let relay_ssrc = relay_ssrc(&h, LegSide::A);
    assert_ne!(playback_ssrc, relay_ssrc);

    h.mb.leg(LegSide::A)
        .unwrap()
        .play(Box::new(TestBeep::new(8000)), true, None)
        .await
        .expect("play beep on WebRTC leg");
    let control_track = h
        .test_a
        .pc
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == rustrtc::MediaKind::Audio)
        .and_then(|t| t.receiver())
        .map(|r| r.track())
        .expect("test_a audio receiver track");
    let frame = tokio::time::timeout(Duration::from_secs(2), control_track.recv())
        .await
        .expect("playback frame timeout")
        .expect("playback frame");
    let MediaSample::Audio(frame) = frame else {
        panic!("expected audio");
    };
    let raw = frame.raw_packet.as_ref().expect("raw packet");
    assert_eq!(
        raw.header.ssrc, playback_ssrc,
        "local playback must stay on the sender SSRC"
    );
    assert_ne!(raw.header.ssrc, relay_ssrc);
    assert!(
        raw.header.extension.is_some(),
        "local playback to WebRTC must carry the MID header extension for browser attribution"
    );

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_webrtc_opus_webrtc_opus() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::Opus,
        TransportMode::WebRtc,
        CodecType::Opus,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::Opus, 8000)
        .await
        .expect("B must receive Opus audio");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_webrtc_opus_rtp_opus() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::Opus,
        TransportMode::Rtp,
        CodecType::Opus,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::Opus, 8000)
        .await
        .expect("B must receive Opus audio across WebRTC→RTP");
    assert!(!frame.data.is_empty());
    h.close();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

// ── Video fast-path (relay-only) ─────────────────────────────────────────

fn h264_caps(pt: u8) -> Vec<rustrtc::config::VideoCapability> {
    vec![rustrtc::config::VideoCapability {
        payload_type: pt,
        codec_name: "H264".to_string(),
        clock_rate: 90000,
        fmtp: Some("packetization-mode=1;profile-level-id=42e01f".to_string()),
        rtcp_fbs: vec![],
        rtx_payload_type: None,
    }]
}

fn vp8_caps(pt: u8) -> Vec<rustrtc::config::VideoCapability> {
    vec![rustrtc::config::VideoCapability {
        payload_type: pt,
        codec_name: "VP8".to_string(),
        clock_rate: 90000,
        fmtp: None,
        rtcp_fbs: vec![],
        rtx_payload_type: None,
    }]
}

/// B2BUA harness for video: two legs (each with audio PCMU + the given video
/// caps) bridged in a MediaBridge, each faced by a video-capable TestPeer.
struct VideoTestHarness {
    mb: MediaBridge,
    test_a: TestPeer,
    test_b: TestPeer,
}

impl VideoTestHarness {
    /// Assert both legs are on the zero-copy fast-path relay.
    fn assert_relay(&self, expected: bool) {
        for side in [LegSide::A, LegSide::B] {
            let leg = self.mb.leg(side).expect("leg");
            assert_eq!(
                leg.egress_is_relay(),
                expected,
                "leg {side:?} relay mode mismatch (expected relay={expected})"
            );
        }
    }
}

fn media_transports_are_bundled(pc: &PeerConnection) -> bool {
    let transport_for = |kind| {
        pc.get_transceivers()
            .into_iter()
            .find(|transceiver| transceiver.kind() == kind)
            .and_then(|transceiver| transceiver.sender())
            .and_then(|sender| sender.transport())
            .expect("negotiated media transport")
    };
    let audio = transport_for(rustrtc::MediaKind::Audio);
    let video = transport_for(rustrtc::MediaKind::Video);
    std::sync::Arc::ptr_eq(&audio, &video)
}

async fn create_video_harness(
    transport_a: TransportMode,
    transport_b: TransportMode,
    caps_a: Vec<rustrtc::config::VideoCapability>,
    caps_b: Vec<rustrtc::config::VideoCapability>,
    sdp_compatibility_a: SdpCompatibilityMode,
    sdp_compatibility_b: SdpCompatibilityMode,
) -> VideoTestHarness {
    let codec = MediaNegotiator::codec_info_for_type(CodecType::PCMU);
    let mk_leg = |name: &str,
                  transport: TransportMode,
                  caps: Vec<rustrtc::config::VideoCapability>,
                  sdp_compatibility: SdpCompatibilityMode| {
        let mut cfg = rtc_config(transport, &codec);
        cfg.media_capabilities.as_mut().unwrap().video = caps;
        cfg.sdp_compatibility = sdp_compatibility;
        LegInner::from_rtc_config(name, cfg, vec![codec.clone()], true, -35.0, None).unwrap()
    };
    let leg_a = mk_leg(
        "a",
        transport_a.clone(),
        caps_a.clone(),
        sdp_compatibility_a.clone(),
    );
    let leg_b = mk_leg(
        "b",
        transport_b.clone(),
        caps_b.clone(),
        sdp_compatibility_b.clone(),
    );

    let test_a = TestPeer::new(transport_a.clone(), codec.clone())
        .with_video(&caps_a, sdp_compatibility_a);
    let test_b = TestPeer::new(transport_b.clone(), codec.clone())
        .with_video(&caps_b, sdp_compatibility_b);
    negotiate(&test_a, &leg_a).await;
    negotiate(&test_b, &leg_b).await;

    // Both legs must have negotiated video (a common codec) for relay to arm.
    assert!(
        !leg_a
            .negotiated()
            .map(|p| p.video.is_empty())
            .unwrap_or(true)
    );
    assert!(
        !leg_b
            .negotiated()
            .map(|p| p.video.is_empty())
            .unwrap_or(true)
    );

    // Ensure both legs' RTP transports (and DTLS for WebRTC) are ready before
    // bridging so the deferred relay arming succeeds promptly.
    for leg in [&leg_a, &leg_b] {
        let _ = leg
            .pc()
            .wait_for_rtp_transport_ready(Duration::from_secs(10))
            .await;
        if leg.pc().config().transport_mode == rustrtc::TransportMode::WebRtc {
            let _ =
                tokio::time::timeout(Duration::from_secs(10), leg.pc().wait_for_connected()).await;
        }
    }

    let mut mb = MediaBridge::new("video-harness");
    mb.replace_leg(LegSide::A, leg_a.clone()).await;
    mb.replace_leg(LegSide::B, leg_b.clone()).await;
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged(), "route must be active after both accept");

    VideoTestHarness { mb, test_a, test_b }
}

impl VideoTestHarness {
    /// Feed video frames from `sender` until `receiver` gets one, or timeout.
    async fn relay_video(&self, sender: &TestPeer, receiver: &TestPeer) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(8000);
        let mut ts = 1000u32;
        while tokio::time::Instant::now() < deadline {
            sender.send_video(
                vec![
                    0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x00, 0x00, 0x00,
                ],
                ts,
            );
            ts = ts.wrapping_add(3000);
            if let Some(frame) = receiver.recv_video(100).await {
                return !frame.data.is_empty();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// Assert video does NOT cross the bridge within the window (codec mismatch
    /// → relay-only degradation), while audio still does.
    async fn assert_video_not_relayed(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        let mut ts = 5000u32;
        while tokio::time::Instant::now() < deadline {
            self.test_a
                .send_video(vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88], ts);
            ts = ts.wrapping_add(3000);
            if self.test_b.recv_video(50).await.is_some() {
                panic!("video must NOT cross the bridge when codecs mismatch");
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    /// Feed audio frames from test_a until test_b receives one (verifies the
    /// audio path stays up in the degraded / transcoded mode).
    async fn relay_audio_a_to_b(&mut self, timeout_ms: u64) -> Option<AudioFrame> {
        let frames = encode_codec_frames(CodecType::PCMU, 20);
        assert!(!frames.is_empty(), "must have PCMU frames");
        let rate = CodecType::PCMU.samplerate();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut ts = 0u32;
        loop {
            for _ in 0..2 {
                self.test_a.send_audio(frames[0].clone(), ts);
                ts = ts.wrapping_add(rate / 50);
            }
            if let Some(frame) = self.test_b.recv_audio(25).await {
                if !frame.data.is_empty() {
                    return Some(frame);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn close(&mut self) {
        self.mb.close();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if tokio::runtime::Handle::try_current().is_ok() {
                self.test_a.pc.close();
                self.test_b.pc.close();
            }
        }));
    }
}

/// H264 video relay over non-BUNDLE RTP: audio and video use separate sockets,
/// so each source leg must install independent audio and video bridges.
#[tokio::test]
async fn fast_path_rtp_h264_rtp_h264_video_relay() {
    let mut h = create_video_harness(
        TransportMode::Rtp,
        TransportMode::Rtp,
        h264_caps(96),
        h264_caps(96),
        SdpCompatibilityMode::LegacySip,
        SdpCompatibilityMode::LegacySip,
    )
    .await;
    assert!(
        !media_transports_are_bundled(&h.test_a.pc),
        "RTP source A must exercise separate audio/video transports"
    );
    assert!(
        !media_transports_are_bundled(&h.test_b.pc),
        "RTP source B must exercise separate audio/video transports"
    );
    h.assert_relay(true);
    assert!(
        h.relay_video(&h.test_a, &h.test_b).await,
        "B must receive A's video"
    );
    assert!(
        h.relay_video(&h.test_b, &h.test_a).await,
        "A must receive B's video"
    );
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// VP8 is also a pass-through codec: when both RTP legs offered it, the
/// non-BUNDLE video sockets must be bridged exactly like H264.
#[tokio::test]
async fn fast_path_rtp_vp8_rtp_vp8_video_relay() {
    let mut h = create_video_harness(
        TransportMode::Rtp,
        TransportMode::Rtp,
        vp8_caps(96),
        vp8_caps(110),
        SdpCompatibilityMode::LegacySip,
        SdpCompatibilityMode::LegacySip,
    )
    .await;
    assert!(!media_transports_are_bundled(&h.test_a.pc));
    assert!(!media_transports_are_bundled(&h.test_b.pc));
    h.assert_relay(true);
    assert!(
        h.relay_video(&h.test_a, &h.test_b).await,
        "B must receive A's VP8 video"
    );
    assert!(
        h.relay_video(&h.test_b, &h.test_a).await,
        "A must receive B's VP8 video"
    );
    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// WebRTC (DTLS-SRTP) video relay: the same payload-type-aware rewrite relay
/// must carry video over SRTP once ICE+DTLS complete, and the deferred relay
/// arming (background task waiting for the SRTP transport) must come up.
#[tokio::test]
async fn fast_path_webrtc_h264_webrtc_h264_video_relay() {
    let mut h = create_video_harness(
        TransportMode::WebRtc,
        TransportMode::WebRtc,
        h264_caps(96),
        h264_caps(96),
        SdpCompatibilityMode::Standard,
        SdpCompatibilityMode::Standard,
    )
    .await;
    assert!(
        media_transports_are_bundled(&h.test_a.pc),
        "WebRTC source must exercise a bundled audio/video transport"
    );
    assert!(
        media_transports_are_bundled(&h.test_b.pc),
        "WebRTC source B must exercise a bundled audio/video transport"
    );
    h.assert_relay(true);
    assert!(
        h.relay_video(&h.test_a, &h.test_b).await,
        "B must receive A's WebRTC video"
    );
    assert!(
        h.relay_video(&h.test_b, &h.test_a).await,
        "A must receive B's WebRTC video"
    );
    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// Cross-transport BUNDLE ↔ non-BUNDLE video relay (browser ↔ SIP phone).
/// Exercises optional video-target dispatch from the bundled WebRTC source,
/// and two independent source bridges in the reverse RTP direction.
#[tokio::test]
async fn fast_path_webrtc_h264_rtp_h264_video_relay() {
    let mut h = create_video_harness(
        TransportMode::WebRtc,
        TransportMode::Rtp,
        h264_caps(96),
        h264_caps(96),
        SdpCompatibilityMode::Standard,
        SdpCompatibilityMode::LegacySip,
    )
    .await;
    assert!(
        media_transports_are_bundled(&h.test_a.pc),
        "WebRTC source must exercise a bundled audio/video transport"
    );
    assert!(
        !media_transports_are_bundled(&h.test_b.pc),
        "Legacy SIP RTP source must exercise separate audio/video transports"
    );
    h.assert_relay(true);
    assert!(
        h.relay_video(&h.test_a, &h.test_b).await,
        "RTP peer must receive WebRTC peer's video"
    );
    assert!(
        h.relay_video(&h.test_b, &h.test_a).await,
        "WebRTC peer must receive RTP peer's video"
    );
    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// Relay-only degradation: when the legs share no video codec (H264 vs VP8),
/// video must NOT cross the bridge while audio still flows.
#[tokio::test]
async fn video_codec_mismatch_degrades_to_audio_only() {
    let mut h = create_video_harness(
        TransportMode::Rtp,
        TransportMode::Rtp,
        h264_caps(96),
        vp8_caps(98),
        SdpCompatibilityMode::LegacySip,
        SdpCompatibilityMode::LegacySip,
    )
    .await;
    h.assert_video_not_relayed().await;
    // Audio must keep flowing after the degradation: the bridge falls back to
    // the transcoding path (PCMU → decode → PCMU), so B still receives audio.
    let audio = h.relay_audio_a_to_b(3000).await;
    assert!(audio.is_some(), "audio must survive the video degradation");
    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

// ── C2: Transcoding (different codec, TranscodePeer) ─────────────────────

#[tokio::test]
async fn transcode_rtp_pcmu_to_rtp_pcma() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMA,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(false);
    let frame = h
        .send_and_receive(CodecType::PCMU, 8000)
        .await
        .expect("B must receive transcoded audio");
    assert_eq!(frame.payload_type, Some(8), "PCMU→PCMA must emit PT 8");
    assert!(
        !frame.data.is_empty(),
        "transcoded payload must be non-empty"
    );
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn transcode_rtp_pcmu_to_webrtc_opus() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::WebRtc,
        CodecType::Opus,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(false);
    let frame = h
        .send_and_receive(CodecType::PCMU, 8000)
        .await
        .expect("B must receive PCMU→Opus transcoded audio");
    let pt = frame.payload_type.expect("frame must carry a payload type");
    assert!(
        pt != 0,
        "transcoded audio must NOT be PCMU (PT 0); got PT {pt}"
    );
    assert!(!frame.data.is_empty(), "Opus payload must be non-empty");
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn transcode_webrtc_opus_to_rtp_pcmu() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::Opus,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(false);
    let frame = h
        .send_and_receive(CodecType::Opus, 8000)
        .await
        .expect("B must receive Opus→PCMU transcoded audio");
    assert_eq!(frame.payload_type, Some(0), "Opus→PCMU must emit PT 0");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn transcode_rtp_g722_to_webrtc_opus() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::G722,
        TransportMode::WebRtc,
        CodecType::Opus,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(false);
    let frame = h
        .send_and_receive(CodecType::G722, 8000)
        .await
        .expect("B must receive G722→Opus transcoded audio");
    let pt = frame.payload_type.expect("frame must carry a payload type");
    assert_ne!(pt, 9, "G722→Opus must not keep G722 PT 9; got PT {pt}");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

// ── G729 (RTP-only; WebRTC does not support G729) ─────────────────────────

/// G729 fast-path: same codec on both RTP legs → transport-level rewrite relay.
#[tokio::test]
async fn fast_path_rtp_g729_rtp_g729() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::G729,
        TransportMode::Rtp,
        CodecType::G729,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::G729, 8000)
        .await
        .expect("B must receive G729 audio");
    assert_eq!(frame.payload_type, Some(18), "G729 must keep PT 18");
    assert!(!frame.data.is_empty());
    h.close();
}

/// G729 → PCMU transcoding (different codec → TranscodePeer).
#[tokio::test]
async fn transcode_rtp_g729_to_rtp_pcmu() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::G729,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(false);
    let frame = h
        .send_and_receive(CodecType::G729, 8000)
        .await
        .expect("B must receive G729→PCMU transcoded audio");
    assert_eq!(frame.payload_type, Some(0), "G729→PCMU must emit PT 0");
    assert!(!frame.data.is_empty());
    h.close();
}

/// G729 → Opus transcoding across transport (RTP G729 → WebRTC Opus).
#[tokio::test]
async fn transcode_rtp_g729_to_webrtc_opus() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::G729,
        TransportMode::WebRtc,
        CodecType::Opus,
    )
    .await;
    h.bridge_and_accept().await;
    h.assert_relay(false);
    let frame = h
        .send_and_receive(CodecType::G729, 8000)
        .await
        .expect("B must receive G729→Opus transcoded audio");
    let pt = frame.payload_type.expect("frame must carry a payload type");
    assert_ne!(pt, 18, "G729→Opus must not keep G729 PT 18; got PT {pt}");
    assert!(!frame.data.is_empty());
    h.close();
}

/// Same-codec fast-path uses an SSRC distinct from local playback on leg B.
#[tokio::test]
async fn fast_path_rtp_pcmu_uses_separate_relay_ssrc() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    let playback_ssrc =
        h.mb.leg(LegSide::B)
            .unwrap()
            .pc()
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == rustrtc::MediaKind::Audio)
            .and_then(|t| t.sender())
            .map(|s| s.ssrc())
            .expect("leg B audio sender");
    h.bridge_and_accept().await;
    h.assert_relay(true);
    let frame = h
        .send_and_receive(CodecType::PCMU, 8000)
        .await
        .expect("B must receive audio");
    let raw = frame
        .raw_packet
        .as_ref()
        .expect("received frame must carry the raw RTP packet");
    assert_ne!(
        raw.header.ssrc, playback_ssrc,
        "playback uses a separate SSRC"
    );
    assert_eq!(raw.header.payload_type, 0);
    h.close();
}

// ── Full-duplex relay connectivity (all 4 transport-mode combos) ──────────
//
// Each test verifies audio flow in BOTH directions (caller→agent and
// agent→caller) and asserts the SSRC attribution rule:
//   - relay to a WebRTC destination uses the leg's separate relay SSRC and MID.
//   - relay to a plain RTP destination uses a distinct random relay SSRC
//     (RTP peers are SSRC-tolerant and don't need MID attribution).

/// RTP ↔ RTP: both legs are plain RTP. Relay SSRC is distinct from the
/// destination's playback SSRC in both directions.
#[tokio::test]
async fn relay_full_duplex_rtp_rtp() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    let a_playback = playback_ssrc(&h, LegSide::A);
    let b_playback = playback_ssrc(&h, LegSide::B);
    h.bridge_and_accept().await;
    h.assert_relay(true);

    // A→B: caller voice → agent
    let a_to_b = h
        .send_and_receive(CodecType::PCMU, 8000)
        .await
        .expect("A→B");
    assert!(!a_to_b.data.is_empty(), "A→B: agent must receive audio");
    let raw_a_b = a_to_b.raw_packet.as_ref().expect("raw packet");
    assert_eq!(raw_a_b.header.payload_type, 0, "A→B: PT must be PCMU");
    assert_ne!(
        raw_a_b.header.ssrc, b_playback,
        "RTP destination: relay SSRC must be distinct from playback SSRC"
    );
    assert_no_mid(raw_a_b);

    // B→A: agent voice → caller
    let b_to_a = h
        .send_b_to_a_receive(CodecType::PCMU, 8000)
        .await
        .expect("B→A");
    assert!(!b_to_a.data.is_empty(), "B→A: caller must receive audio");
    let raw_b_a = b_to_a.raw_packet.as_ref().expect("raw packet");
    assert_eq!(raw_b_a.header.payload_type, 0, "B→A: PT must be PCMU");
    assert_ne!(
        raw_b_a.header.ssrc, a_playback,
        "RTP destination: relay SSRC must be distinct from playback SSRC"
    );
    assert_no_mid(raw_b_a);

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// WebRTC ↔ WebRTC: relayed packets use each destination leg's separate audio
/// SSRC and carry its SDES-MID.
#[tokio::test]
async fn relay_full_duplex_webrtc_webrtc() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::PCMU,
        TransportMode::WebRtc,
        CodecType::PCMU,
    )
    .await;
    let a_playback = playback_ssrc(&h, LegSide::A);
    let b_playback = playback_ssrc(&h, LegSide::B);
    let a_relay = relay_ssrc(&h, LegSide::A);
    let b_relay = relay_ssrc(&h, LegSide::B);
    h.bridge_and_accept().await;
    h.assert_relay(true);

    let a_to_b = h
        .send_and_receive(CodecType::PCMU, 10000)
        .await
        .expect("A→B");
    assert!(!a_to_b.data.is_empty());
    let raw = a_to_b.raw_packet.as_ref().expect("raw packet");
    assert_ne!(raw.header.ssrc, b_playback);
    assert_eq!(raw.header.ssrc, b_relay);
    assert_has_mid(
        raw,
        "WebRTC destination: relay must stamp MID for browser attribution",
    );

    let b_to_a = h
        .send_b_to_a_receive(CodecType::PCMU, 10000)
        .await
        .expect("B→A");
    assert!(!b_to_a.data.is_empty());
    let raw = b_to_a.raw_packet.as_ref().expect("raw packet");
    assert_ne!(raw.header.ssrc, a_playback);
    assert_eq!(raw.header.ssrc, a_relay);
    assert_has_mid(
        raw,
        "WebRTC destination: relay must stamp MID for browser attribution",
    );

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// WebRTC(A) ↔ RTP(B): caller uses WebRTC, agent is plain RTP.
/// A→B: RTP destination → distinct SSRC, no MID.
/// B→A: WebRTC destination → separate relay SSRC and MID present.
#[tokio::test]
async fn relay_full_duplex_webrtc_rtp() {
    let mut h = TestMediaHarness::create(
        TransportMode::WebRtc,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
    )
    .await;
    let a_playback = playback_ssrc(&h, LegSide::A);
    let b_playback = playback_ssrc(&h, LegSide::B);
    let a_relay = relay_ssrc(&h, LegSide::A);
    h.bridge_and_accept().await;
    h.assert_relay(true);

    // A→B: WebRTC caller → plain RTP agent
    let a_to_b = h
        .send_and_receive(CodecType::PCMU, 10000)
        .await
        .expect("A→B");
    assert!(!a_to_b.data.is_empty());
    let raw = a_to_b.raw_packet.as_ref().expect("raw packet");
    assert_ne!(
        raw.header.ssrc, b_playback,
        "RTP destination: relay SSRC must be distinct"
    );
    assert_no_mid(raw);

    // B→A: plain RTP agent → WebRTC caller (original bug direction)
    let b_to_a = h
        .send_b_to_a_receive(CodecType::PCMU, 10000)
        .await
        .expect("B→A");
    assert!(!b_to_a.data.is_empty());
    let raw = b_to_a.raw_packet.as_ref().expect("raw packet");
    assert_ne!(raw.header.ssrc, a_playback);
    assert_eq!(raw.header.ssrc, a_relay);
    assert_has_mid(
        raw,
        "WebRTC destination: relay must stamp MID (the original 'bitrate but no audio' bug)",
    );

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// RTP(A) ↔ WebRTC(B): caller is plain RTP, agent is WebRTC.
/// A→B: WebRTC destination → separate relay SSRC and MID present.
/// B→A: RTP destination → distinct SSRC, no MID.
#[tokio::test]
async fn relay_full_duplex_rtp_webrtc() {
    let mut h = TestMediaHarness::create(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::WebRtc,
        CodecType::PCMU,
    )
    .await;
    let a_playback = playback_ssrc(&h, LegSide::A);
    let b_playback = playback_ssrc(&h, LegSide::B);
    let b_relay = relay_ssrc(&h, LegSide::B);
    h.bridge_and_accept().await;
    h.assert_relay(true);

    // A→B: plain RTP caller → WebRTC agent
    let a_to_b = h
        .send_and_receive(CodecType::PCMU, 10000)
        .await
        .expect("A→B");
    assert!(!a_to_b.data.is_empty());
    let raw = a_to_b.raw_packet.as_ref().expect("raw packet");
    assert_ne!(raw.header.ssrc, b_playback);
    assert_eq!(raw.header.ssrc, b_relay);
    assert_has_mid(raw, "WebRTC destination: relay must stamp MID");

    // B→A: WebRTC agent → plain RTP caller
    let b_to_a = h
        .send_b_to_a_receive(CodecType::PCMU, 10000)
        .await
        .expect("B→A");
    assert!(!b_to_a.data.is_empty());
    let raw = b_to_a.raw_packet.as_ref().expect("raw packet");
    assert_ne!(
        raw.header.ssrc, a_playback,
        "RTP destination: relay SSRC must be distinct"
    );
    assert_no_mid(raw);

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// The rewrite bridge stamps MID on relayed packets when the destination is
/// WebRTC (`!strip_extensions`); RTP destinations never receive extensions.
fn assert_has_mid(raw: &rustrtc::rtp::RtpPacket, msg: &str) {
    assert!(
        raw.header.extension.is_some(),
        "{}: relay to WebRTC must carry a header extension (MID)",
        msg,
    );
}

/// RTP destinations must never carry MID (or any) extensions.
fn assert_no_mid(raw: &rustrtc::rtp::RtpPacket) {
    assert!(
        raw.header.extension.is_none(),
        "RTP destination: relayed packet must have no header extensions"
    );
}

fn playback_ssrc(h: &TestMediaHarness, side: LegSide) -> u32 {
    rustpbx_media::leg::sender_ssrc_for_kind(
        h.mb.leg(side).unwrap().pc(),
        rustrtc::MediaKind::Audio,
    )
}

fn relay_ssrc(h: &TestMediaHarness, side: LegSide) -> u32 {
    h.mb.leg(side).unwrap().relay_audio_ssrc()
}

/// Outbound RFC 2833 telephone-event (DTMF) from a leg must reach the facing
/// peer on the negotiated telephone-event payload type, with start (E=0) and
/// end (E=1) packets per digit.
#[tokio::test]
async fn leg_send_dtmf_emits_telephone_events_to_peer() {
    // Build a PCMU + telephone-event(101) codec list so both sides negotiate a
    // DTMF payload type.
    let mut pcmu = MediaNegotiator::codec_info_for_type(CodecType::PCMU);
    pcmu.payload_type = 0;
    let te = CodecInfo {
        payload_type: 101,
        codec: CodecType::TelephoneEvent,
        clock_rate: 8000,
        channels: 1,
        fmtp: None,
    };
    let codecs = vec![pcmu.clone(), te.clone()];

    let leg_caps = {
        let mut c = rustrtc::config::MediaCapabilities::default();
        c.audio = vec![
            rustrtc::config::AudioCapability {
                payload_type: 0,
                codec_name: "PCMU".into(),
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
                rtcp_fbs: vec![],
            },
            rustrtc::config::AudioCapability {
                payload_type: 101,
                codec_name: "telephone-event".into(),
                clock_rate: 8000,
                channels: 1,
                fmtp: None,
                rtcp_fbs: vec![],
            },
        ];
        c
    };
    let mut leg_cfg = rtc_config(TransportMode::Rtp, &pcmu);
    leg_cfg.media_capabilities = Some(leg_caps);
    let leg = LegInner::from_rtc_config("a", leg_cfg, codecs.clone(), true, -35.0, None).unwrap();

    // Peer: offer PCMU + telephone-event so the leg's DTMF PT gets negotiated.
    let mut caps = rustrtc::config::MediaCapabilities::default();
    caps.audio = vec![
        rustrtc::config::AudioCapability {
            payload_type: 0,
            codec_name: "PCMU".into(),
            clock_rate: 8000,
            channels: 1,
            fmtp: None,
            rtcp_fbs: vec![],
        },
        rustrtc::config::AudioCapability {
            payload_type: 101,
            codec_name: "telephone-event".into(),
            clock_rate: 8000,
            channels: 1,
            fmtp: None,
            rtcp_fbs: vec![],
        },
    ];
    let test_a = TestPeer::with_caps(TransportMode::Rtp, pcmu.clone(), caps);
    negotiate(&test_a, &leg).await;

    // Wait for the leg's RTP transport + SRTP session to be ready so the raw
    // DTMF send does not error on "transport not ready".
    leg.pc()
        .wait_for_rtp_transport_ready(Duration::from_secs(10))
        .await
        .expect("leg transport ready");
    assert!(
        leg.negotiated().is_some_and(|p| p.dtmf.is_some()),
        "leg must negotiate a telephone-event codec"
    );

    // Capture what arrives at the peer.
    let cap = std::sync::Arc::new(DtmfCapture::default());
    test_a.pc.add_observer(cap.clone());

    leg.send_dtmf("12").await.expect("send_dtmf must succeed");

    // Give the packets time to traverse the loopback transport.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let events = cap.events.lock().unwrap();
        if events.len() >= 4 {
            break;
        }
        drop(events);
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let events = cap.events.lock().unwrap();
    // 2 digits × (start + end) = 4 telephone-event packets, all on PT 101.
    assert!(
        events.len() >= 4,
        "expected >=4 DTMF packets, got {:?}",
        *events
    );
    let playback_ssrc =
        rustpbx_media::leg::sender_ssrc_for_kind(leg.pc(), rustrtc::MediaKind::Audio);
    for &(pt, code, _end, ssrc) in events.iter() {
        assert_eq!(pt, 101, "telephone-event must use negotiated PT 101");
        assert!(code == 1 || code == 2, "digit code must be 1 or 2");
        assert_eq!(ssrc, playback_ssrc, "local DTMF follows playback SSRC");
        assert_ne!(ssrc, leg.relay_audio_ssrc());
    }
    let starts: Vec<_> = events.iter().filter(|(_, _, e, _)| !e).collect();
    let ends: Vec<_> = events.iter().filter(|(_, _, e, _)| *e).collect();
    assert_eq!(starts.len(), 2, "2 start packets expected");
    assert_eq!(ends.len(), 2, "2 end packets expected");

    leg.stop();
    test_a.pc.close();
}

// ── Recording: A-leg only (regression for the recording-stutter bug) ─────

use rustpbx_media::ingress_tap::PacketDirection;
use rustpbx_media::media_recorder::{MediaRecorder, SipflowRecorder};
use rustpbx_sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats};

struct CaptureBackend {
    tx: tokio::sync::mpsc::UnboundedSender<SipFlowItem>,
}

#[async_trait::async_trait]
impl SipFlowBackend for CaptureBackend {
    fn record(&self, _call_id: std::borrow::Cow<'_, str>, item: SipFlowItem) -> anyhow::Result<()> {
        let _ = self.tx.send(item);
        Ok(())
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn query_flow(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<SipFlowItem>> {
        Ok(Vec::new())
    }

    async fn query_media_stats(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<SipFlowMediaStats>> {
        Ok(Vec::new())
    }

    async fn query_media(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

fn recorder_capture(
    call_id: &str,
) -> (
    Box<dyn MediaRecorder>,
    tokio::sync::mpsc::UnboundedReceiver<SipFlowItem>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let backend = std::sync::Arc::new(CaptureBackend { tx });
    (
        Box::new(SipflowRecorder::new(backend, call_id.to_string())),
        rx,
    )
}

/// Drain a recorder sender's channel into a list of (direction, PT) tuples
/// for inspection. Returns the captured items collected over `window_ms`.
async fn drain_sipflow_items(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SipFlowItem>,
    window_ms: u64,
) -> Vec<(PacketDirection, u8)> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(window_ms);
    let mut out = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(item)) => {
                let direction = match item.leg {
                    Some(0) => PacketDirection::Ingress,
                    Some(1) => PacketDirection::Egress,
                    leg => panic!("unexpected recording leg: {leg:?}"),
                };
                let payload_type = item
                    .payload
                    .get(1)
                    .map(|value| value & 0x7f)
                    .expect("captured item must contain an RTP header");
                out.push((direction, payload_type));
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    out
}

/// On a bridged P2P call with the recorder mounted on leg A only, verify
/// that **exactly** the A leg's two directions are captured and the B leg
/// contributes nothing.
///
/// The assertions are direction-presence/absence based (deterministic, not
/// packet-count based) so they cleanly distinguish the fix from the bug:
///
/// | Phase               | Fix (A-only)         | Bug (both legs)               |
/// |---------------------|----------------------|-------------------------------|
/// | send test_a only    | Ingress ✓, Egress ✗ | Ingress ✓, Egress ✓ (B.on_eg) |
/// | send test_b only    | Egress ✓, Ingress ✗ | Egress ✓, Ingress ✓ (B.on_in) |
///
/// In the relay fast-path `fire_ingress` fires on the receiving transport and
/// `target.fire_egress` fires on the *destination* transport, so only A's tap
/// (which holds the recorder) ever produces items.
#[tokio::test]
async fn recording_captures_a_leg_only_not_b_leg() {
    let (recorder, mut rx) = recorder_capture("a-leg-only");
    let mut h = TestMediaHarness::create_with_recorder(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
        recorder,
    )
    .await;
    h.bridge_and_accept().await;

    let frames = encode_codec_frames(CodecType::PCMU, 20);
    let rate = CodecType::PCMU.samplerate();

    // ── Phase 1: send from test_a (caller → leg A ingress) only ──────────
    // Relay forwards to leg B; B's tap has no recorder so NO egress item is
    // produced. With the bug (both legs recorded) B.on_egress WOULD fire.
    let mut ts = 0u32;
    for _ in 0..10 {
        h.test_a.send_audio(frames[0].clone(), ts);
        ts = ts.wrapping_add(rate / 50);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let phase1 = drain_sipflow_items(&mut rx, 400).await;
    let p1_ingress = phase1.iter().any(|(d, _)| *d == PacketDirection::Ingress);
    let p1_egress = phase1.iter().any(|(d, _)| *d == PacketDirection::Egress);
    assert!(
        p1_ingress,
        "Phase 1 (test_a): A-ingress must be recorded, got {:?}",
        phase1
    );
    assert!(
        !p1_egress,
        "Phase 1 (test_a): NO egress expected (B has no recorder), got {:?}",
        phase1
    );

    // ── Phase 2: send from test_b (callee → leg B → relay → leg A egress) ─
    // B's tap fires ingress (no recorder → dropped). The relay targets A's
    // transport → A.on_egress fires → recorded. With the bug B.on_ingress
    // WOULD also fire and produce an Ingress item.
    let mut ts = 0u32;
    for _ in 0..10 {
        h.test_b.send_audio(frames[0].clone(), ts);
        ts = ts.wrapping_add(rate / 50);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let phase2 = drain_sipflow_items(&mut rx, 400).await;
    let p2_egress = phase2.iter().any(|(d, _)| *d == PacketDirection::Egress);
    let p2_ingress = phase2.iter().any(|(d, _)| *d == PacketDirection::Ingress);
    assert!(
        p2_egress,
        "Phase 2 (test_b): A-egress (relayed callee) must be recorded, got {:?}",
        phase2
    );
    assert!(
        !p2_ingress,
        "Phase 2 (test_b): NO ingress expected (B has no recorder), got {:?}",
        phase2
    );

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

/// Locally-generated IVR audio (played via `play_file` on the A leg) must be
/// recorded as A-leg egress. This directly validates the concern that "IVR is
/// sent-out audio and won't be captured by sipflow": the IngressTap observes
/// egress at the transport plaintext boundary (`send_rtp → fire_egress`), so
/// the egress pipeline's frames DO reach the recorder.
#[tokio::test]
async fn ivr_playback_is_recorded_as_a_leg_egress() {
    use rustpbx_media::audio_source::FileAudioSource;

    let (recorder, mut rx) = recorder_capture("ivr-egress");
    let mut h = TestMediaHarness::create_with_recorder(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
        recorder,
    )
    .await;
    // Only the A leg + test_a are needed for IVR playback; do not bridge.
    h.mb.accept(LegSide::A).await;

    // Play a real WAV (the same fixture used by the codec tests) on the A leg.
    // The egress pipeline encodes it to PCMU and pushes frames through the
    // RtpSender → transport.send_rtp → fire_egress → A tap.on_egress.
    let wav_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("sample.wav");
    let audio = FileAudioSource::new(wav_path.to_string_lossy().to_string(), false)
        .await
        .expect("load sample.wav");
    let _handle = h.mb.play(LegSide::A, Box::new(audio), false).await;

    let captured = drain_sipflow_items(&mut rx, 600).await;
    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let has_egress = captured.iter().any(|(d, _)| *d == PacketDirection::Egress);
    assert!(
        has_egress,
        "IVR/locally-generated audio must be recorded as A-leg egress, got {:?}",
        captured.iter().map(|(d, _)| *d).collect::<Vec<_>>()
    );
}

/// Mid-call re-INVITE codec change must resync the leg's egress encoder AND
/// sender PT.
///
/// Regression: `Leg::apply_profile_from_sdp` (the session's re-INVITE answer
/// path) used to only update the negotiated profile, leaving the egress
/// encoder on the old codec while the answer promised the new one. rustrtc's
/// sender stamps the track's params PT (not the frame's), so the wire then
/// carried e.g. a PCMU payload under the PCMA PT — the remote decoded the
/// playback as silence/garbage. After the fix the same function updates the
/// egress codec + sender PT, so locally-generated audio goes out in the new
/// codec.
#[tokio::test]
async fn reinvite_profile_resyncs_egress_codec_and_sender_pt() {
    let (recorder, mut rx) = recorder_capture("reinvite-pt");
    let mut h = TestMediaHarness::create_with_recorder(
        TransportMode::Rtp,
        CodecType::PCMU,
        TransportMode::Rtp,
        CodecType::PCMU,
        recorder,
    )
    .await;
    h.mb.accept(LegSide::A).await;

    // The session's re-INVITE answer changes the A leg's negotiated codec to
    // PCMA. In the real flow this answer is built outside `Leg::apply_sdp` and
    // surfaced onto the media-bridge leg via `apply_profile_from_sdp`.
    let pcma_answer_sdp = concat!(
        "v=0\r\n",
        "o=- 1 1 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=audio 10000 RTP/AVP 8\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "a=sendrecv\r\n",
    )
    .to_string();
    let leg_a = h.mb.leg(LegSide::A).expect("leg A");
    leg_a
        .apply_profile_from_sdp(&pcma_answer_sdp)
        .await
        .expect("apply_profile_from_sdp");

    // The negotiated profile is renegotiated to PCMA...
    let profile = leg_a.negotiated().expect("negotiated profile");
    let audio = profile.audio.as_ref().expect("audio codec");
    assert_eq!(audio.codec, CodecType::PCMA);
    assert_eq!(audio.payload_type, 8);

    // ...and the sender's wire PT is synced to the new codec.
    let sender_pt = leg_a
        .pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == rustrtc::MediaKind::Audio)
        .and_then(|t| t.sender())
        .map(|s| s.params().payload_type);
    assert_eq!(
        sender_pt,
        Some(8),
        "sender PT must follow the renegotiated codec"
    );

    // Locally-generated playback must be encoded with the NEW codec (PCMA) —
    // observed as PT 8 on the wire, not stale PCMU (PT 0).
    let _handle =
        h.mb.play(LegSide::A, Box::new(TestBeep::new(8000)), true)
            .await
            .expect("play");
    let captured = drain_sipflow_items(&mut rx, 600).await;
    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert!(
        captured
            .iter()
            .any(|(d, pt)| *d == PacketDirection::Egress && *pt == 8),
        "re-INVITE'd leg must send playback as PCMA (PT 8), got {:?}",
        captured
    );
    assert!(
        !captured
            .iter()
            .any(|(d, pt)| *d == PacketDirection::Egress && *pt == 0),
        "must not keep sending PCMU (PT 0) after re-INVITE codec change, got {:?}",
        captured
    );
}

/// Transcoded call recording (the exact scenario of the reported stutter bug
/// `0duu44tqb5hs0f8rii8n`: WebRTC-Opus caller ↔ RTP-PCMU callee).
///
/// With the recorder on leg A only:
/// - A-ingress = caller Opus (PT 111) — caller voice.
/// - A-egress = callee PCMU transcoded → Opus (PT 111) sent to caller.
/// Both directions are in the **caller's codec** (Opus), single stream each —
/// no mixing, no timeline corruption.
///
/// B-leg (PCMU) contributes nothing: B's tap has no recorder, and the
/// transcode egress pipeline on B fires `send_rtp → fire_egress` on B's
/// transport only.
#[tokio::test]
async fn recording_transcoded_call_captures_both_speakers_on_a_leg() {
    let (recorder, mut rx) = recorder_capture("transcoded-call");
    let mut h = TestMediaHarness::create_with_recorder(
        TransportMode::Rtp,
        CodecType::Opus,
        TransportMode::Rtp,
        CodecType::PCMU,
        recorder,
    )
    .await;
    h.bridge_and_accept().await;

    // Caller audio (Opus, PT 111): test_a → leg A ingress.
    let opus_frames = encode_codec_frames(CodecType::Opus, 20);
    assert!(!opus_frames.is_empty(), "must have opus frames");
    let opus_rate = CodecType::Opus.samplerate();
    let mut ts = 0u32;
    for _ in 0..10 {
        h.test_a.send_audio(opus_frames[0].clone(), ts);
        ts = ts.wrapping_add(opus_rate / 50);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let phase1 = drain_sipflow_items(&mut rx, 500).await;
    // A-ingress must be Opus (PT 111 = caller's codec).
    let p1_ingress_opus = phase1
        .iter()
        .any(|(d, pt)| *d == PacketDirection::Ingress && *pt == 111);
    assert!(
        p1_ingress_opus,
        "Phase 1 (caller): A-ingress must be Opus PT 111, got {:?}",
        phase1
    );

    // Callee audio (PCMU, PT 0): test_b → leg B → transcode → leg A egress.
    let pcmu_frames = encode_codec_frames(CodecType::PCMU, 20);
    assert!(!pcmu_frames.is_empty(), "must have pcmu frames");
    let pcmu_rate = CodecType::PCMU.samplerate();
    let mut ts = 0u32;
    for _ in 0..10 {
        h.test_b.send_audio(pcmu_frames[0].clone(), ts);
        ts = ts.wrapping_add(pcmu_rate / 50);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let phase2 = drain_sipflow_items(&mut rx, 500).await;
    // A-egress must be Opus (PT 111): callee's PCMU is transcoded to the
    // caller's codec before being sent to (and observed on) leg A.
    let p2_egress_opus = phase2
        .iter()
        .any(|(d, pt)| *d == PacketDirection::Egress && *pt == 111);
    assert!(
        p2_egress_opus,
        "Phase 2 (callee): A-egress must be transcoded Opus PT 111, got {:?}",
        phase2
    );
    // B-leg ingress (raw PCMU PT 0) must NOT leak into the recorder.
    let p2_ingress_pcmu = phase2
        .iter()
        .any(|(d, pt)| *d == PacketDirection::Ingress && *pt == 0);
    assert!(
        !p2_ingress_pcmu,
        "Phase 2 (callee): B-ingress PCMU must NOT be recorded (B has no recorder), got {:?}",
        phase2
    );

    h.close();
    tokio::time::sleep(Duration::from_millis(80)).await;
}

// ── helpers ──────────────────────────────────────────────────────────────

/// A constant-amplitude looping PCM source used to produce real playable
/// frames (control case: local playback to a WebRTC leg carries MID).
struct TestBeep {
    rate: u32,
    pos: usize,
}

impl TestBeep {
    fn new(rate: u32) -> Self {
        Self { rate, pos: 0 }
    }
}

impl rustpbx_media::audio_source::AudioSource for TestBeep {
    fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
        for (i, s) in buffer.iter_mut().enumerate() {
            *s = ((self.pos + i) as i32 * 1000) as i16;
        }
        self.pos += buffer.len();
        buffer.len()
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> u16 {
        1
    }
    fn has_data(&self) -> bool {
        true
    }
    fn reset(&mut self) -> anyhow::Result<()> {
        self.pos = 0;
        Ok(())
    }
}

// ── helpers ──────────────────────────────────────────────────────────────
