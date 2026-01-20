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
    let answer = track2.handshake(offer).await;
    assert!(answer.is_ok(), "Handshake failed: {:?}", answer.err());

    let answer_sdp = answer.unwrap();
    assert!(answer_sdp.contains("v=0"));
    assert!(answer_sdp.contains("m=audio"));
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
async fn test_file_track_basic() {
    let track = FileTrack::new("file-track-test".to_string());

    assert_eq!(track.id(), "file-track-test");

    let offer = track.local_description().await;
    assert!(
        offer.is_ok(),
        "Failed to generate local description: {:?}",
        offer.err()
    );
}

#[tokio::test]
async fn test_file_track_rtp_mode() {
    let track = FileTrack::new("file-track-rtp".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(50000, 50100);

    let offer = track.local_description().await;
    assert!(offer.is_ok());

    let offer_sdp = offer.unwrap();
    assert!(offer_sdp.contains("m=audio"));
}

#[tokio::test]
async fn test_file_track_with_external_ip() {
    let track = FileTrack::new("file-track-ext-ip".to_string())
        .with_external_ip("198.51.100.1".to_string());

    let offer = track.local_description().await;
    assert!(offer.is_ok());

    let offer_sdp = offer.unwrap();
    // Verify SDP is valid
    assert!(offer_sdp.contains("v=0"));
    assert!(offer_sdp.contains("m=audio"));
}

#[tokio::test]
async fn test_file_track_handshake() {
    let track = FileTrack::new("file-track-handshake".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(60000, 60100);

    // Create a simple offer
    let offerer = RtpTrackBuilder::new("offerer".to_string())
        .with_mode(TransportMode::Rtp)
        .with_rtp_range(60100, 60200)
        .build();

    let offer = offerer.local_description().await.unwrap();

    // FileTrack should be able to respond
    let answer = track.handshake(offer).await;
    assert!(
        answer.is_ok(),
        "FileTrack handshake failed: {:?}",
        answer.err()
    );
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
