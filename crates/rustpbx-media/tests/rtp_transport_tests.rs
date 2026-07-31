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
use rustpbx_media::media_bridge::{BridgeOpts, LegSide, MediaBridge};
use rustpbx_media::negotiate::{CodecInfo, MediaNegotiator};
use rustrtc::config::{BufferDropStrategy, SdpCompatibilityMode};
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::sample_track;
use rustrtc::media::MediaStreamTrack;
use rustrtc::{PeerConnection, RtcConfiguration, SessionDescription, SdpType, TransportMode};

/// A standalone peer that faces one leg of the MediaBridge. Sends audio via a
/// track source; receives via its receiver track.
struct TestPeer {
    pc: PeerConnection,
    tx: rustrtc::media::track::SampleStreamSource,
    codec: CodecInfo,
}

impl TestPeer {
    fn new(transport: TransportMode, codec: CodecInfo) -> Self {
        let cfg = rtc_config(transport, &codec);
        let pc = PeerConnection::new(cfg);
        let (tx, track, _) = sample_track(rustrtc::media::MediaKind::Audio, 500);
        pc.add_track(track, codec.to_params()).unwrap();
        Self { pc, tx, codec }
    }    /// Push an audio frame (already encoded in `codec`) to the remote peer.
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
            match tokio::time::timeout(Duration::from_secs(10), test_peer.pc.wait_for_connected()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => panic!("test peer failed to connect: {e}"),
                Err(_) => panic!("test peer connection timed out (codec {:?})", test_peer.codec),
            }
        }
        _ => {
            tokio::time::timeout(
                Duration::from_secs(10),
                test_peer.pc.wait_for_rtp_transport_ready(Duration::from_secs(10)),
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
        let codec_a = MediaNegotiator::codec_info_for_type(codec_a);
        let codec_b = MediaNegotiator::codec_info_for_type(codec_b);

        let leg_a = LegInner::from_rtc_config(
            "a",
            rtc_config(transport_a.clone(), &codec_a),
            vec![codec_a.clone()],
        )
        .unwrap();
        let leg_b = LegInner::from_rtc_config(
            "b",
            rtc_config(transport_b.clone(), &codec_b),
            vec![codec_b.clone()],
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
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    leg.pc().wait_for_connected(),
                )
                .await;
            }
        }

        let mut mb = MediaBridge::new("rtp-test", BridgeOpts::default());
        mb.replace_leg(LegSide::A, leg_a.clone()).await;
        mb.replace_leg(LegSide::B, leg_b.clone()).await;

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
        assert!(self.mb.is_bridged(), "route must be active after both accept");
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
        assert!(!frames.is_empty(), "must have encoded frames for {send_codec:?}");
        let rate = send_codec.samplerate();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(recv_timeout_ms);
        let mut idx = 0usize;
        let mut ts = 0u32;
        let mut last_rebridge = tokio::time::Instant::now();

        loop {
            // Feed a few frames each ~30ms for the whole window.
            for _ in 0..2 {
                self.test_a.send_audio(frames[idx % frames.len()].clone(), ts);
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
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::PCMU, TransportMode::Rtp, CodecType::PCMU)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 5000).await.expect("B must receive audio");
    assert_eq!(frame.payload_type, Some(0), "same codec → PT 0");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_rtp_g722_rtp_g722() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::G722, TransportMode::Rtp, CodecType::G722)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::G722, 5000).await.expect("B must receive audio");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_srtp_pcmu_srtp_pcmu() {
    let mut h = TestMediaHarness::create(TransportMode::Srtp, CodecType::PCMU, TransportMode::Srtp, CodecType::PCMU)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 5000).await.expect("B must receive audio over SDES-SRTP");
    assert_eq!(frame.payload_type, Some(0));
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_webrtc_pcmu_webrtc_pcmu() {
    let mut h = TestMediaHarness::create(TransportMode::WebRtc, CodecType::PCMU, TransportMode::WebRtc, CodecType::PCMU)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 8000).await.expect("B must receive audio over DTLS-SRTP");
    assert_eq!(frame.payload_type, Some(0));
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_cross_transport_rtp_to_webrtc() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::PCMU, TransportMode::WebRtc, CodecType::PCMU)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 8000).await.expect("B must receive audio across RTP→WebRTC");
    assert_eq!(frame.payload_type, Some(0));
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn fast_path_webrtc_opus_webrtc_opus() {
    let mut h = TestMediaHarness::create(TransportMode::WebRtc, CodecType::Opus, TransportMode::WebRtc, CodecType::Opus)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::Opus, 8000).await.expect("B must receive Opus audio");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

// ── C2: Transcoding (different codec, TranscodePeer) ─────────────────────

#[tokio::test]
async fn transcode_rtp_pcmu_to_rtp_pcma() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::PCMU, TransportMode::Rtp, CodecType::PCMA)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 8000).await.expect("B must receive transcoded audio");
    assert_eq!(frame.payload_type, Some(8), "PCMU→PCMA must emit PT 8");
    assert!(!frame.data.is_empty(), "transcoded payload must be non-empty");
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn transcode_rtp_pcmu_to_webrtc_opus() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::PCMU, TransportMode::WebRtc, CodecType::Opus)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 8000).await.expect("B must receive PCMU→Opus transcoded audio");
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
    let mut h = TestMediaHarness::create(TransportMode::WebRtc, CodecType::Opus, TransportMode::Rtp, CodecType::PCMU)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::Opus, 8000).await.expect("B must receive Opus→PCMU transcoded audio");
    assert_eq!(frame.payload_type, Some(0), "Opus→PCMU must emit PT 0");
    assert!(!frame.data.is_empty());
    h.close();
    // Let background tasks (ICE/DTLS/sender loops) drain before the test
    // runtime drops — sync close() cannot await task completion.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
}

#[tokio::test]
async fn transcode_rtp_g722_to_webrtc_opus() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::G722, TransportMode::WebRtc, CodecType::Opus)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::G722, 8000).await.expect("B must receive G722→Opus transcoded audio");
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
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::G729, TransportMode::Rtp, CodecType::G729)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::G729, 8000).await.expect("B must receive G729 audio");
    assert_eq!(frame.payload_type, Some(18), "G729 must keep PT 18");
    assert!(!frame.data.is_empty());
    h.close();
}

/// G729 → PCMU transcoding (different codec → TranscodePeer).
#[tokio::test]
async fn transcode_rtp_g729_to_rtp_pcmu() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::G729, TransportMode::Rtp, CodecType::PCMU)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::G729, 8000).await.expect("B must receive G729→PCMU transcoded audio");
    assert_eq!(frame.payload_type, Some(0), "G729→PCMU must emit PT 0");
    assert!(!frame.data.is_empty());
    h.close();
}

/// G729 → Opus transcoding across transport (RTP G729 → WebRTC Opus).
#[tokio::test]
async fn transcode_rtp_g729_to_webrtc_opus() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::G729, TransportMode::WebRtc, CodecType::Opus)
        .await;
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::G729, 8000).await.expect("B must receive G729→Opus transcoded audio");
    let pt = frame.payload_type.expect("frame must carry a payload type");
    assert_ne!(pt, 18, "G729→Opus must not keep G729 PT 18; got PT {pt}");
    assert!(!frame.data.is_empty());
    h.close();
}

/// Same-codec fast-path with SSRC verification: the packet forwarded to test_b
/// must carry test_b's expected sender SSRC (the SSRC leg_b advertised in SDP).
#[tokio::test]
async fn fast_path_rtp_pcmu_rtp_pcmu_ssrc_rewrite() {
    let mut h = TestMediaHarness::create(TransportMode::Rtp, CodecType::PCMU, TransportMode::Rtp, CodecType::PCMU)
        .await;
    let expected_ssrc = h
        .mb
        .leg(LegSide::B)
        .unwrap()
        .pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == rustrtc::MediaKind::Audio)
        .and_then(|t| t.sender())
        .map(|s| s.ssrc())
        .expect("leg B audio sender");
    h.bridge_and_accept().await;
    let frame = h.send_and_receive(CodecType::PCMU, 8000).await.expect("B must receive audio");
    let raw = frame
        .raw_packet
        .as_ref()
        .expect("received frame must carry the raw RTP packet");
    assert_eq!(
        raw.header.ssrc, expected_ssrc,
        "forwarded packet SSRC must be leg B's sender SSRC (rewritten by fast-path)"
    );
    assert_eq!(raw.header.payload_type, 0);
    h.close();
}

// ── helpers ──────────────────────────────────────────────────────────────

/// Parse an SDP string for sending to a TestPeer.
#[allow(dead_code)]
fn parse_sdp(sdp: &str) -> SessionDescription {
    SessionDescription::parse(SdpType::Answer, sdp).unwrap()
}



