use super::*;
use rustrtc::TransportMode;

#[tokio::test]
async fn test_media_track_webrtc_mode_basic() {
    // Test WebRTC mode - default mode
    let track = RtpTrackBuilder::new("test-track-webrtc".to_string())
        .with_mode(TransportMode::WebRtc)
        .build();

    assert_eq!(track.id(), "test-track-webrtc");

    // Test local_description - should generate offer
    let offer = track.local_description().await;
    assert!(
        offer.is_ok(),
        "Failed to generate local description: {:?}",
        offer.err()
    );

    let offer_sdp = offer.unwrap();
    assert!(offer_sdp.contains("v=0"), "SDP should contain version");
    assert!(
        offer_sdp.contains("m=audio"),
        "SDP should contain audio media line"
    );
}

#[tokio::test]
async fn test_media_track_rtp_mode_basic() {
    // Test RTP mode
    let track = RtpTrackBuilder::new("test-track-rtp".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(20000, 20100)
        .build();

    assert_eq!(track.id(), "test-track-rtp");

    // Test local_description
    let offer = track.local_description().await;
    assert!(
        offer.is_ok(),
        "Failed to generate local description: {:?}",
        offer.err()
    );

    let offer_sdp = offer.unwrap();
    assert!(offer_sdp.contains("v=0"), "SDP should contain version");
    assert!(
        offer_sdp.contains("m=audio"),
        "SDP should contain audio media line"
    );
}

#[tokio::test]
async fn test_media_track_rtp_with_external_ip() {
    // Test RTP mode with external IP
    let track = RtpTrackBuilder::new("test-track-rtp-ext-ip".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(30000, 30100)
        .with_external_ip("203.0.113.1".to_string())
        .build();

    let offer = track.local_description().await;
    assert!(
        offer.is_ok(),
        "Failed to generate local description: {:?}",
        offer.err()
    );

    let offer_sdp = offer.unwrap();
    // External IP might appear in connection line (c=) or as a candidate
    // Just verify the SDP is valid and contains basic elements
    assert!(offer_sdp.contains("v=0"), "SDP should contain version");
    assert!(
        offer_sdp.contains("m=audio"),
        "SDP should contain audio media"
    );
}

#[tokio::test]
async fn test_media_track_codec_preference() {
    // Test codec preference
    let track = RtpTrackBuilder::new("test-track-codec".to_string())
        .with_codec_preference(vec![CodecType::PCMU, CodecType::PCMA])
        .build();

    let offer = track.local_description().await;
    assert!(offer.is_ok());

    let offer_sdp = offer.unwrap();
    // Check that PCMU (payload type 0) appears in the SDP
    assert!(
        offer_sdp.contains("PCMU") || offer_sdp.contains("0 PCMU"),
        "SDP should contain PCMU codec"
    );
}

/// A PCMU-only codec preference (used for carrier-trunk originates, where the
/// RWI conference bridge sends a fixed PCMU payload type) must produce an offer
/// that ADVERTISES PCMU and EXCLUDES the wideband/compressed codecs. Regression
/// guard for the trunk-audio garble bug: offering opus/G729/G722 ahead of PCMU
/// let a carrier answer G.729, which the PCMU-only bridge then mis-decoded.
#[tokio::test]
async fn test_pcmu_only_preference_excludes_wideband_codecs() {
    let track = RtpTrackBuilder::new("test-track-trunk-pcmu".to_string())
        .with_codec_preference(vec![CodecType::PCMU])
        .build();

    let offer_sdp = track.local_description().await.unwrap();

    assert!(
        offer_sdp.contains("PCMU"),
        "trunk offer must advertise PCMU, got:\n{offer_sdp}"
    );
    for banned in ["opus", "G729", "G722", "PCMA"] {
        assert!(
            !offer_sdp.contains(banned),
            "trunk offer must NOT advertise {banned} (bridge is PCMU-only), got:\n{offer_sdp}"
        );
    }
}

#[tokio::test]
async fn test_media_track_preserves_custom_dtmf_rtpmap() {
    let track = RtpTrackBuilder::new("test-track-dtmf-rtpmap".to_string())
        .with_mode(TransportMode::Rtp)
        .with_codec_info(vec![
            negotiate::CodecInfo {
                payload_type: 96,
                codec: CodecType::Opus,
                clock_rate: 48000,
                channels: 2,
                fmtp: Some("minptime=10;useinbandfec=1".to_string()),
            },
            negotiate::CodecInfo {
                payload_type: 101,
                codec: CodecType::TelephoneEvent,
                clock_rate: 48000,
                channels: 1,
                fmtp: Some("0-16".to_string()),
            },
            negotiate::CodecInfo {
                payload_type: 97,
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
                channels: 1,
                fmtp: Some("0-16".to_string()),
            },
        ])
        .build();

    let offer_sdp = track.local_description().await.unwrap();
    assert!(
        offer_sdp.contains("a=fmtp:96 minptime=10;useinbandfec=1"),
        "SDP should preserve the configured Opus fmtp"
    );
    assert!(
        !offer_sdp.contains("stereo=1"),
        "SDP must not append an unconfigured Opus fmtp parameter"
    );
    assert!(
        offer_sdp.contains("a=rtpmap:101 telephone-event/48000"),
        "SDP should preserve telephone-event/48000"
    );
    assert!(
        offer_sdp.contains("a=rtpmap:97 telephone-event/8000"),
        "SDP should preserve telephone-event/8000"
    );
}

#[tokio::test]
async fn test_media_track_handshake() {
    // Test offer-answer handshake
    let track1 = RtpTrackBuilder::new("track1".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(40000, 40100)
        .build();

    let track2 = RtpTrackBuilder::new("track2".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(40100, 40200)
        .build();

    // Track1 creates offer
    let offer = track1.local_description().await.unwrap();

    // Track2 responds with answer
    let answer = track2.handshake(offer, rustrtc::SdpType::Answer).await;
    assert!(answer.is_ok(), "Handshake failed: {:?}", answer.err());

    let answer_sdp = answer.unwrap();
    assert!(answer_sdp.contains("v=0"));
    assert!(answer_sdp.contains("m=audio"));
}

#[tokio::test]
async fn test_media_track_pranswer_then_answer_reuses_rtp_transport() {
    let offerer = RtpTrackBuilder::new("offerer".to_string())
        .with_mode(TransportMode::Rtp)
        .build();
    let answerer = RtpTrackBuilder::new("answerer".to_string())
        .with_mode(TransportMode::Rtp)
        .build();

    let offer = offerer.local_description().await.unwrap();
    let offer_desc = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &offer).unwrap();
    let offerer_port = offer_desc.first_audio_section().unwrap().port;

    let pranswer = answerer
        .handshake(offer.clone(), rustrtc::SdpType::Pranswer)
        .await
        .unwrap();
    let pranswer_desc =
        rustrtc::SessionDescription::parse(rustrtc::SdpType::Pranswer, &pranswer).unwrap();
    let answerer_port = pranswer_desc.first_audio_section().unwrap().port;
    offerer
        .set_remote_description(&pranswer, rustrtc::SdpType::Pranswer)
        .await
        .unwrap();

    let offerer_pc = offerer.get_peer_connection().await.unwrap();
    let answerer_pc = answerer.get_peer_connection().await.unwrap();
    assert_eq!(
        offerer_pc.signaling_state(),
        rustrtc::SignalingState::HaveLocalOffer
    );
    assert_eq!(
        answerer_pc.signaling_state(),
        rustrtc::SignalingState::HaveRemoteOffer
    );

    let answer = answerer
        .handshake(offer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    let answer_desc =
        rustrtc::SessionDescription::parse(rustrtc::SdpType::Answer, &answer).unwrap();
    assert_eq!(
        answer_desc.first_audio_section().unwrap().port,
        answerer_port,
        "the final answer must keep the RTP port allocated for the 183"
    );

    offerer
        .set_remote_description(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    assert_eq!(
        offerer_pc.signaling_state(),
        rustrtc::SignalingState::Stable
    );
    assert_eq!(
        answerer_pc.signaling_state(),
        rustrtc::SignalingState::Stable
    );

    let final_offer = offerer_pc.local_description().unwrap();
    assert_eq!(
        final_offer.first_audio_section().unwrap().port,
        offerer_port,
        "processing 183 then 200 must not replace the offerer's RTP socket"
    );
}

#[tokio::test]
async fn test_media_track_stop() {
    let track = RtpTrackBuilder::new("test-track-stop".to_string()).build();

    // Generate an offer to ensure PC is active
    let _ = track.local_description().await;

    // Stop should not panic
    track.stop().await;
}

#[tokio::test]
async fn test_media_track_get_peer_connection() {
    let track = RtpTrackBuilder::new("test-track-pc".to_string()).build();

    // PC should be available immediately after construction
    let pc = track.get_peer_connection().await;
    assert!(pc.is_some(), "PeerConnection should be available");
}

#[tokio::test]
async fn test_media_track_multiple_operations() {
    // Test that multiple operations work correctly with the new design
    let track = RtpTrackBuilder::new("multi-op-track".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(50000, 50100)
        .build();

    // First operation
    let offer1 = track.local_description().await;
    assert!(offer1.is_ok());

    // Second operation should also work
    let offer2 = track.local_description().await;
    assert!(offer2.is_ok());

    // Both should be identical since PC state hasn't changed
    assert_eq!(offer1.unwrap(), offer2.unwrap());
}
