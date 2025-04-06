use crate::event::create_event_sender;
use crate::media::track::{rtp::*, webrtc::*, Track, TrackConfig};
use crate::{AudioFrame, Samples};
use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_rtp_track_creation() -> Result<()> {
    let track_id = "test-rtp-track".to_string();
    let track = RtpTrack::new(track_id.clone());

    assert_eq!(track.id(), &track_id);

    // Test with modified configuration
    let sample_rate = 16000;
    let track = RtpTrack::new(track_id)
        .with_sample_rate(sample_rate)
        .with_config(TrackConfig::default().with_sample_rate(sample_rate));

    // Configure RTP
    let rtp_config = RtpTrackConfig {
        local_addr: "127.0.0.1:0".parse().unwrap(),
        remote_addr: "127.0.0.1:12345".parse().unwrap(),
        payload_type: 0,
        ssrc: 12345,
    };

    let _track = track.with_rtp_config(rtp_config);

    Ok(())
}

#[tokio::test]
async fn test_webrtc_track_creation() -> Result<()> {
    let track_id = "test-webrtc-track".to_string();
    let track = WebrtcTrack::new(track_id.clone());

    assert_eq!(track.id(), &track_id);

    // Test with modified configuration
    let sample_rate = 16000;
    let track = WebrtcTrack::new(track_id)
        .with_sample_rate(sample_rate)
        .with_config(TrackConfig::default().with_sample_rate(sample_rate));

    let _track = track.with_stream_id("test-stream".to_string());

    Ok(())
}
