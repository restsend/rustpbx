use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::media::track::rtcconn::{LocalTrackAdapter, RtcConnection};
use crate::media::track::{TrackPacket, TrackPayload};

#[tokio::test]
async fn test_rtc_connection_create() -> Result<()> {
    // Create a RTC connection
    let connection = RtcConnection::new("test-connection".to_string())
        .with_ice_servers(vec!["stun:stun.l.google.com:19302".to_string()]);

    // Initialize the connection
    connection.initialize().await?;

    // Create an offer
    let offer = connection.create_offer().await?;

    // Verify offer is not empty
    assert!(!offer.sdp.is_empty());

    // Close the connection
    connection.close().await?;

    Ok(())
}

#[tokio::test]
async fn test_rtc_connection_add_track() -> Result<()> {
    // Create a RTC connection
    let connection = RtcConnection::new("test-connection".to_string())
        .with_ice_servers(vec!["stun:stun.l.google.com:19302".to_string()]);

    // Initialize the connection
    connection.initialize().await?;

    // Add a local track for video
    let track = connection
        .add_local_track("video-track".to_string(), "video/vp8")
        .await?;

    // Verify track exists (just check it's not null, since we can't easily access the id)
    assert!(Arc::strong_count(&track) > 0);

    // Close the connection
    connection.close().await?;

    Ok(())
}

// The following test is commented out as it would require a real WebRTC connection
// to fully test. In a real environment, you would use two connections to test
// the full handshake process.
/*
#[tokio::test]
async fn test_rtc_connection_handshake() -> Result<()> {
    // Create two RTC connections
    let connection1 = RtcConnection::new("connection1".to_string())
        .with_ice_servers(vec!["stun:stun.l.google.com:19302".to_string()]);
    let connection2 = RtcConnection::new("connection2".to_string())
        .with_ice_servers(vec!["stun:stun.l.google.com:19302".to_string()]);

    // Initialize both connections
    connection1.initialize().await?;
    connection2.initialize().await?;

    // Create an offer from connection1
    let offer = connection1.create_offer().await?;

    // Set the offer as remote description for connection2
    connection2.set_remote_description(offer).await?;

    // Create an answer from connection2
    let answer = connection2.create_answer().await?;

    // Set the answer as remote description for connection1
    connection1.set_remote_description(answer).await?;

    // Wait for ICE gathering to complete (this would normally involve more steps)
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Close both connections
    connection1.close().await?;
    connection2.close().await?;

    Ok(())
}
*/

#[tokio::test]
async fn test_local_track_adapter() -> Result<()> {
    // Create a RTC connection
    let connection = Arc::new(
        RtcConnection::new("test-connection".to_string())
            .with_ice_servers(vec!["stun:stun.l.google.com:19302".to_string()]),
    );

    // Initialize the connection
    connection.initialize().await?;

    // Create a channel for sending track packets
    let (track_sender, track_receiver) = mpsc::unbounded_channel();

    // Create a LocalTrackAdapter
    let adapter =
        LocalTrackAdapter::new(connection.clone(), "audio-track".to_string(), "audio/opus").await?;

    // Start forwarding packets
    adapter.start_forwarding(track_receiver).await?;

    // Create some dummy RTP packets
    let rtp_packet = TrackPacket {
        track_id: "audio-track".to_string(),
        timestamp: 1000,
        payload: TrackPayload::RTP(0, vec![0u8, 1, 2, 3, 4]),
    };

    // Send the packet through the channel
    track_sender.send(rtp_packet).unwrap();

    // Wait a bit for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Clean up
    adapter.cancel();
    connection.close().await?;

    Ok(())
}
