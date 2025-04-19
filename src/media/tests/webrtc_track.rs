use crate::media::track::{rtp::*, Track, TrackConfig};
use anyhow::Result;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_rtp_track_creation() -> Result<()> {
    let track_id = "test-rtp-track".to_string();
    let cancel_token = CancellationToken::new();
    let builder = RtpTrackBuilder::new(track_id.clone(), TrackConfig::default())
        .with_cancel_token(cancel_token.clone());
    let track = builder.build().await?;

    assert_eq!(track.id(), &track_id);

    // Test with modified configuration
    let sample_rate = 16000;
    let config = TrackConfig::default().with_sample_rate(sample_rate);
    let builder = RtpTrackBuilder::new(track_id, config).with_cancel_token(cancel_token);
    let _track = builder.build().await?;

    Ok(())
}
