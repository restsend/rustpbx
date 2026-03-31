//! E2E tests for WebRTC ↔ RTP SDP bridging via BridgePeer.
//!
//! These tests verify that the SDP exchange between WebRTC callers and RTP callees
//! through a BridgePeer produces correct SDP with real DTLS fingerprints,
//! real ICE credentials, and proper DTLS setup roles.
//!
//! Also tests that codec selection is driven by allow_codecs (not hardcoded),
//! that no-transcode paths are preferred, and that transport-incompatible codecs
//! (e.g., G729 on WebRTC side) are correctly filtered.

use rustpbx::media::{RtpTrackBuilder, Track};
use rustrtc::TransportMode;
use rustrtc::sdp::{SessionDescription, SdpType};
use audio_codec::CodecType;

/// Helper: create a WebRTC track simulating a browser caller (e.g., JsSIP).
fn create_webrtc_caller(port_start: u16) -> Box<dyn Track> {
    Box::new(
        RtpTrackBuilder::new(format!("webrtc-caller-{}", port_start))
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(port_start, port_start + 100)
            .with_codec_preference(vec![CodecType::PCMU, CodecType::Opus])
            .build(),
    )
}

/// Helper: create an RTP track simulating a SIP callee.
fn create_rtp_callee(port_start: u16) -> Box<dyn Track> {
    Box::new(
        RtpTrackBuilder::new(format!("rtp-callee-{}", port_start))
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(port_start, port_start + 100)
            .with_codec_preference(vec![CodecType::PCMU])
            .build(),
    )
}

/// E2E test: WebRTC caller → BridgePeer → RTP callee
///
/// Mirrors the actual flow in sip_session.rs after the fix:
/// 1. Caller sends WebRTC INVITE (offer)
/// 2. Bridge creates RTP offer for callee
/// 3. Callee answers RTP offer
/// 4. Bridge answers caller's WebRTC offer using real PeerConnection
/// 5. Caller processes the answer
#[tokio::test]
async fn test_e2e_webrtc_caller_to_rtp_callee_via_bridge() {
    use rustpbx::media::bridge::BridgePeerBuilder;

    let port_base: u16 = 40000;

    // 1. Create bridge — only RTP side (callee-facing) creates offer
    let bridge = BridgePeerBuilder::new("e2e-webrtc-rtp".to_string())
        .with_rtp_port_range(port_base, port_base + 100)
        .build();
    bridge.setup_bridge().await.unwrap();

    // Only create offer on callee-facing RTP side
    let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
    bridge.rtp_pc().set_local_description(rtp_offer).unwrap();
    bridge.start_bridge().await;

    // 2. Simulate WebRTC caller (JsSIP) generating INVITE offer
    let caller = create_webrtc_caller(port_base + 100);
    let caller_offer = caller.local_description().await.unwrap();
    assert!(caller_offer.contains("UDP/TLS/RTP/SAVPF"), "Caller offer must be WebRTC");
    assert!(caller_offer.contains("setup:actpass"), "Caller should offer actpass");

    // 3. Bridge sends its RTP offer to the callee
    let bridge_rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
    assert!(bridge_rtp_sdp.contains("RTP/AVP"), "Bridge to callee must be plain RTP");
    assert!(!bridge_rtp_sdp.contains("fingerprint"), "Bridge RTP offer must not have fingerprint");

    // 4. Callee processes the RTP offer and creates answer
    let callee = create_rtp_callee(port_base + 200);
    let callee_answer = callee.handshake(bridge_rtp_sdp).await.unwrap();
    assert!(callee_answer.contains("RTP/AVP"), "Callee answer must be plain RTP");

    // 5. Bridge sets callee's answer on its RTP side
    let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
    bridge.rtp_pc().set_remote_description(callee_desc).await.unwrap();

    // 6. Bridge sets caller's WebRTC offer and creates real answer
    let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
    bridge.webrtc_pc().set_remote_description(caller_desc).await.unwrap();

    let bridge_answer = bridge.webrtc_pc().create_answer().await.unwrap();
    bridge.webrtc_pc().set_local_description(bridge_answer).unwrap();

    let answer_sdp = bridge.webrtc_pc().local_description().unwrap().to_sdp_string();

    // 7. Verify the critical SDP properties that were previously broken
    assert!(answer_sdp.contains("UDP/TLS/RTP/SAVPF"), "Answer must use SAVPF (WebRTC)");
    assert!(answer_sdp.contains("fingerprint:sha-256"), "Answer must have real DTLS fingerprint");
    assert!(answer_sdp.contains("ice-ufrag:"), "Answer must have real ICE ufrag");
    assert!(answer_sdp.contains("ice-pwd:"), "Answer must have real ICE password");
    assert!(!answer_sdp.contains("setup:actpass"), "Answer MUST NOT have setup:actpass (was causing 488)");
    assert!(
        answer_sdp.contains("setup:passive") || answer_sdp.contains("setup:active"),
        "Answer must have a chosen DTLS role (passive or active)"
    );

    // 8. Caller processes the answer (simulates JsSIP setRemoteDescription)
    caller.set_remote_description(&answer_sdp).await.unwrap();

    // Verify full connectivity
    assert!(bridge.rtp_pc().remote_description().is_some());
    assert!(bridge.webrtc_pc().local_description().is_some());
    assert!(bridge.webrtc_pc().remote_description().is_some());

    // Cleanup
    bridge.stop().await;
}

/// E2E test: RTP caller → BridgePeer → WebRTC callee
///
/// The reverse direction: SIP phone calls a WebRTC endpoint through the bridge.
#[tokio::test]
async fn test_e2e_rtp_caller_to_webrtc_callee_via_bridge() {
    use rustpbx::media::bridge::BridgePeerBuilder;

    let port_base: u16 = 42000;

    // 1. Create bridge — only WebRTC side (callee-facing) creates offer
    let bridge = BridgePeerBuilder::new("e2e-rtp-webrtc".to_string())
        .with_rtp_port_range(port_base, port_base + 100)
        .build();
    bridge.setup_bridge().await.unwrap();

    // Only create offer on callee-facing WebRTC side
    let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
    bridge.webrtc_pc().set_local_description(webrtc_offer).unwrap();
    bridge.start_bridge().await;

    // 2. Simulate RTP caller (SIP phone) generating INVITE offer
    let caller = create_rtp_callee(port_base + 100);
    let caller_offer = caller.local_description().await.unwrap();
    assert!(caller_offer.contains("RTP/AVP"), "RTP caller offer must be plain RTP");

    // 3. Bridge sends its WebRTC offer to the callee
    let bridge_webrtc_sdp = bridge.webrtc_pc().local_description().unwrap().to_sdp_string();
    assert!(bridge_webrtc_sdp.contains("SAVPF"), "Bridge to callee must be WebRTC");
    assert!(bridge_webrtc_sdp.contains("fingerprint"), "Bridge WebRTC offer must have fingerprint");

    // 4. Callee processes the WebRTC offer and creates answer
    let callee = create_webrtc_caller(port_base + 200);
    let callee_answer = callee.handshake(bridge_webrtc_sdp).await.unwrap();
    assert!(callee_answer.contains("SAVPF"), "Callee answer must be WebRTC");

    // 5. Bridge sets callee's answer on its WebRTC side
    let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
    bridge.webrtc_pc().set_remote_description(callee_desc).await.unwrap();

    // 6. Bridge sets caller's RTP offer and creates real answer
    let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
    bridge.rtp_pc().set_remote_description(caller_desc).await.unwrap();

    let bridge_answer = bridge.rtp_pc().create_answer().await.unwrap();
    bridge.rtp_pc().set_local_description(bridge_answer).unwrap();

    let answer_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();

    // 7. Verify the answer is plain RTP (no WebRTC artifacts)
    assert!(answer_sdp.contains("RTP/AVP"), "Answer must be plain RTP");
    assert!(!answer_sdp.contains("fingerprint"), "RTP answer must not have DTLS fingerprint");
    assert!(!answer_sdp.contains("ice-ufrag"), "RTP answer must not have ICE");

    // 8. Caller processes the answer
    caller.set_remote_description(&answer_sdp).await.unwrap();

    // Verify full connectivity
    assert!(bridge.rtp_pc().remote_description().is_some());
    assert!(bridge.rtp_pc().local_description().is_some());
    assert!(bridge.webrtc_pc().remote_description().is_some());

    // Cleanup
    bridge.stop().await;
}

/// Verify that SdpBridge::rtp_to_webrtc no longer uses setup:actpass (defensive check).
#[test]
fn test_sdp_bridge_setup_role_is_passive() {
    use rustpbx::media::sdp_bridge::SdpBridge;

    let rtp_sdp = "v=0\r\n\
o=- 123456 123456 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 54321 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=sendrecv\r\n";

    let webrtc_sdp = SdpBridge::rtp_to_webrtc(
        rtp_sdp,
        "AA:BB:CC:DD:EE:FF",
        "ufrag",
        "pwd",
    ).unwrap();

    assert!(!webrtc_sdp.contains("setup:actpass"),
        "SdpBridge must not produce setup:actpass — it should be passive for answers");
    assert!(webrtc_sdp.contains("setup:passive"),
        "SdpBridge should produce setup:passive for SDP answers");
}

// ── Codec-aware E2E tests ──────────────────────────────────────

/// E2E: WebRTC caller (Opus+PCMU) → Bridge (allow_codecs=[PCMU]) → RTP callee (PCMU)
///
/// Verifies that when allow_codecs restricts to PCMU only:
/// - Bridge RTP side SDP contains PCMU but NOT Opus
/// - Bridge WebRTC side SDP contains PCMU
/// - Full SDP negotiation completes successfully
#[tokio::test]
async fn test_e2e_webrtc_caller_rtp_callee_pcmu_only_allow_codecs() {
    use rustpbx::media::bridge::BridgePeerBuilder;
    use rustpbx::media::negotiate::MediaNegotiator;
    use rustrtc::RtpCodecParameters;

    let port_base: u16 = 44000;

    // 1. WebRTC caller offers Opus + PCMU
    let caller = create_webrtc_caller(port_base + 100);
    let caller_offer = caller.local_description().await.unwrap();
    assert!(caller_offer.contains("opus"), "Caller must offer Opus for this test");

    // 2. Build codec lists: allow_codecs=[PCMU,TelephoneEvent] → Opus filtered out
    let codec_lists = MediaNegotiator::build_bridge_codec_lists(
        &caller_offer,
        true,  // caller is WebRTC
        false, // callee is RTP
        &[CodecType::PCMU, CodecType::TelephoneEvent],
    );

    // Verify Opus is NOT in either side (not in allow_codecs)
    assert!(!codec_lists.caller_side.iter().any(|c| c.codec == CodecType::Opus),
        "Opus should be filtered out (not in allow_codecs)");
    assert!(codec_lists.caller_side.iter().any(|c| c.codec == CodecType::PCMU),
        "PCMU must be present on caller side");

    // 3. Build bridge with computed capabilities
    let webrtc_caps: Vec<_> = codec_lists.caller_side.iter()
        .filter_map(|c| c.to_audio_capability()).collect();
    let rtp_caps: Vec<_> = codec_lists.callee_side.iter()
        .filter_map(|c| c.to_audio_capability()).collect();

    let webrtc_sender = codec_lists.caller_side.iter()
        .find(|c| !c.is_dtmf()).map(|c| c.to_params())
        .unwrap_or(RtpCodecParameters { payload_type: 0, clock_rate: 8000, channels: 1 });
    let rtp_sender = codec_lists.callee_side.iter()
        .find(|c| !c.is_dtmf()).map(|c| c.to_params())
        .unwrap_or(RtpCodecParameters { payload_type: 0, clock_rate: 8000, channels: 1 });

    let bridge = BridgePeerBuilder::new("e2e-pcmu-only".to_string())
        .with_rtp_port_range(port_base, port_base + 100)
        .with_webrtc_audio_capabilities(webrtc_caps)
        .with_rtp_audio_capabilities(rtp_caps)
        .with_sender_codecs(webrtc_sender, rtp_sender)
        .build();

    bridge.setup_bridge().await.unwrap();

    // 4. RTP side creates offer for callee
    let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
    bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

    let bridge_rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
    assert!(bridge_rtp_sdp.contains("PCMU/8000"), "RTP side must offer PCMU");
    assert!(!bridge_rtp_sdp.contains("opus"), "RTP side must NOT offer Opus (filtered by allow_codecs)");
    assert!(bridge_rtp_sdp.contains("RTP/AVP"), "Must be plain RTP");

    // 5. RTP callee answers
    let callee = create_rtp_callee(port_base + 200);
    let callee_answer = callee.handshake(bridge_rtp_sdp).await.unwrap();

    let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
    bridge.rtp_pc().set_remote_description(callee_desc).await.unwrap();

    // 6. Bridge answers WebRTC caller
    let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
    bridge.webrtc_pc().set_remote_description(caller_desc).await.unwrap();

    let answer = bridge.webrtc_pc().create_answer().await.unwrap();
    bridge.webrtc_pc().set_local_description(answer).unwrap();

    let answer_sdp = bridge.webrtc_pc().local_description().unwrap().to_sdp_string();
    assert!(answer_sdp.contains("SAVPF"), "Answer must be WebRTC");
    assert!(answer_sdp.contains("fingerprint:sha-256"), "Must have real fingerprint");
    assert!(!answer_sdp.contains("setup:actpass"), "Must NOT have actpass");

    // 7. Caller processes answer
    caller.set_remote_description(&answer_sdp).await.unwrap();

    // Verify connectivity
    assert!(bridge.rtp_pc().remote_description().is_some());
    assert!(bridge.webrtc_pc().remote_description().is_some());

    bridge.stop().await;
}

/// E2E: RTP caller (G729+PCMU) → Bridge → WebRTC callee
///
/// Verifies that G729 is accepted on the RTP side but dropped on the WebRTC side
/// (G729 is not in the WebRTC supported codec set).
#[tokio::test]
async fn test_e2e_rtp_caller_g729_dropped_on_webrtc_side() {
    use rustpbx::media::bridge::BridgePeerBuilder;
    use rustpbx::media::negotiate::MediaNegotiator;

    let port_base: u16 = 46000;

    // 1. RTP caller offers G729 + PCMU
    let caller = Box::new(
        RtpTrackBuilder::new(format!("rtp-caller-g729-{}", port_base))
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(port_base + 100, port_base + 200)
            .with_codec_preference(vec![CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent])
            .build(),
    );
    let caller_offer = caller.local_description().await.unwrap();
    assert!(caller_offer.contains("G729"), "Caller must offer G729 for this test");

    // 2. Build codec lists: allow G729+PCMU but WebRTC callee side should drop G729
    let codec_lists = MediaNegotiator::build_bridge_codec_lists(
        &caller_offer,
        false, // caller is RTP
        true,  // callee is WebRTC
        &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent],
    );

    // G729 should be on caller (RTP) side but NOT on callee (WebRTC) side
    assert!(codec_lists.caller_side.iter().any(|c| c.codec == CodecType::G729),
        "G729 must be on RTP caller side");
    assert!(!codec_lists.callee_side.iter().any(|c| c.codec == CodecType::G729),
        "G729 must be dropped on WebRTC callee side (not in WebRTC supported set)");

    // 3. Build bridge
    let webrtc_caps: Vec<_> = codec_lists.callee_side.iter()
        .filter_map(|c| c.to_audio_capability()).collect();
    let rtp_caps: Vec<_> = codec_lists.caller_side.iter()
        .filter_map(|c| c.to_audio_capability()).collect();

    let webrtc_sender = codec_lists.callee_side.iter()
        .find(|c| !c.is_dtmf()).map(|c| c.to_params()).unwrap();
    let rtp_sender = codec_lists.caller_side.iter()
        .find(|c| !c.is_dtmf()).map(|c| c.to_params()).unwrap();

    let bridge = BridgePeerBuilder::new("e2e-g729-drop".to_string())
        .with_rtp_port_range(port_base, port_base + 100)
        .with_webrtc_audio_capabilities(webrtc_caps)
        .with_rtp_audio_capabilities(rtp_caps)
        .with_sender_codecs(webrtc_sender, rtp_sender)
        .build();

    bridge.setup_bridge().await.unwrap();

    // 4. WebRTC callee side creates offer
    let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
    bridge.webrtc_pc().set_local_description(webrtc_offer).unwrap();

    let bridge_webrtc_sdp = bridge.webrtc_pc().local_description().unwrap().to_sdp_string();
    assert!(bridge_webrtc_sdp.contains("SAVPF"), "WebRTC side must use SAVPF");
    assert!(!bridge_webrtc_sdp.contains("G729"), "WebRTC side must NOT offer G729");
    assert!(bridge_webrtc_sdp.contains("PCMU"), "WebRTC side must offer PCMU as fallback");

    // 5. WebRTC callee answers
    let callee = create_webrtc_caller(port_base + 200);
    let callee_answer = callee.handshake(bridge_webrtc_sdp).await.unwrap();

    let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
    bridge.webrtc_pc().set_remote_description(callee_desc).await.unwrap();

    // 6. Bridge answers RTP caller
    let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
    bridge.rtp_pc().set_remote_description(caller_desc).await.unwrap();

    let rtp_answer = bridge.rtp_pc().create_answer().await.unwrap();
    bridge.rtp_pc().set_local_description(rtp_answer).unwrap();

    let rtp_answer_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
    assert!(rtp_answer_sdp.contains("RTP/AVP"), "RTP answer must be plain RTP");
    assert!(!rtp_answer_sdp.contains("fingerprint"), "RTP answer must not have fingerprint");

    // 7. Caller processes answer
    caller.set_remote_description(&rtp_answer_sdp).await.unwrap();

    // Verify connectivity
    assert!(bridge.rtp_pc().remote_description().is_some());
    assert!(bridge.webrtc_pc().remote_description().is_some());

    bridge.stop().await;
}

/// E2E: Verify no-transcode path — both bridge sides negotiate same codec.
///
/// WebRTC caller offers Opus first + PCMU, allow_codecs=[Opus,PCMU] →
/// both sides should have Opus as first codec, meaning no transcoding needed.
#[tokio::test]
async fn test_e2e_no_transcode_path_same_codec_on_both_sides() {
    use rustpbx::media::negotiate::MediaNegotiator;

    let port_base: u16 = 48000;

    // 1. WebRTC caller offers Opus FIRST, then PCMU
    let caller = Box::new(
        RtpTrackBuilder::new(format!("webrtc-caller-opus-first-{}", port_base))
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(port_base + 100, port_base + 200)
            .with_codec_preference(vec![CodecType::Opus, CodecType::PCMU])
            .build(),
    );
    let caller_offer = caller.local_description().await.unwrap();
    assert!(caller_offer.contains("opus"), "Caller must offer Opus for this test");

    // 2. Build codec lists with allow_codecs=[Opus,PCMU]
    let codec_lists = MediaNegotiator::build_bridge_codec_lists(
        &caller_offer,
        true,  // caller is WebRTC
        false, // callee is RTP
        &[CodecType::Opus, CodecType::PCMU, CodecType::TelephoneEvent],
    );

    // Both sides should have Opus as first audio codec (no transcode)
    let caller_first = codec_lists.caller_side.iter()
        .find(|c| !c.is_dtmf()).unwrap();
    let callee_first = codec_lists.callee_side.iter()
        .find(|c| !c.is_dtmf()).unwrap();

    assert_eq!(caller_first.codec, CodecType::Opus, "Caller side first codec must be Opus");
    assert_eq!(callee_first.codec, CodecType::Opus, "Callee side first codec must be Opus");
    assert_eq!(caller_first.codec, callee_first.codec,
        "No transcode: both sides should use same first codec");
}

/// E2E: Verify caller's payload types are preserved on caller-facing side.
///
/// If caller offers PCMU at PT 0 and PCMA at PT 8, the bridge's caller-facing
/// side must preserve these PTs (per RFC 3264, answer uses same PT as offer).
#[tokio::test]
async fn test_e2e_caller_payload_types_preserved() {
    use rustpbx::media::negotiate::MediaNegotiator;

    let port_base: u16 = 50000;

    // RTP caller offers PCMU at PT 0, PCMA at PT 8
    let caller = Box::new(
        RtpTrackBuilder::new(format!("rtp-caller-pt-{}", port_base))
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(port_base + 100, port_base + 200)
            .with_codec_preference(vec![CodecType::PCMU, CodecType::PCMA, CodecType::TelephoneEvent])
            .build(),
    );
    let caller_offer = caller.local_description().await.unwrap();

    let codec_lists = MediaNegotiator::build_bridge_codec_lists(
        &caller_offer,
        false, // caller is RTP
        false, // callee is RTP
        &[CodecType::PCMU, CodecType::PCMA, CodecType::TelephoneEvent],
    );

    // Caller side should preserve PTs from caller SDP
    let caller_pcmu = codec_lists.caller_side.iter()
        .find(|c| c.codec == CodecType::PCMU).unwrap();
    let caller_pcma = codec_lists.caller_side.iter()
        .find(|c| c.codec == CodecType::PCMA).unwrap();

    assert_eq!(caller_pcmu.payload_type, 0, "PCMU PT must be 0 (from caller SDP)");
    assert_eq!(caller_pcma.payload_type, 8, "PCMA PT must be 8 (from caller SDP)");

    // Callee side should use standard PTs
    let callee_pcmu = codec_lists.callee_side.iter()
        .find(|c| c.codec == CodecType::PCMU).unwrap();
    let callee_pcma = codec_lists.callee_side.iter()
        .find(|c| c.codec == CodecType::PCMA).unwrap();

    assert_eq!(callee_pcmu.payload_type, 0, "Callee PCMU uses standard PT 0");
    assert_eq!(callee_pcma.payload_type, 8, "Callee PCMA uses standard PT 8");
}

/// E2E: Verify codec ordering follows allow_codecs preference on callee side.
///
/// If allow_codecs=[G722,PCMU,PCMA], the callee-facing side should list
/// codecs in that exact order (respecting transport compatibility).
#[tokio::test]
async fn test_e2e_callee_side_follows_allow_codecs_order() {
    use rustpbx::media::negotiate::MediaNegotiator;

    let _port_base: u16 = 52000;

    let caller_sdp = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 10000 RTP/AVP 0 8 101\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:101 telephone-event/8000\r\n";

    let codec_lists = MediaNegotiator::build_bridge_codec_lists(
        caller_sdp,
        false, // caller is RTP
        false, // callee is RTP
        &[CodecType::G722, CodecType::PCMU, CodecType::PCMA, CodecType::TelephoneEvent],
    );

    // Callee side should follow allow_codecs order: G722, PCMU, PCMA, telephone-event
    let callee_audio: Vec<_> = codec_lists.callee_side.iter()
        .filter(|c| !c.is_dtmf()).collect();
    assert_eq!(callee_audio.len(), 3, "Should have 3 audio codecs");
    assert_eq!(callee_audio[0].codec, CodecType::G722, "G722 first per allow_codecs");
    assert_eq!(callee_audio[1].codec, CodecType::PCMU, "PCMU second per allow_codecs");
    assert_eq!(callee_audio[2].codec, CodecType::PCMA, "PCMA third per allow_codecs");
}
