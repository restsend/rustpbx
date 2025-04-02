use crate::event::test_utils::dummy_event_sender;
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

#[tokio::test]
#[ignore = "Need to implement webrtc track"]
async fn test_rtp_track_send_receive() -> Result<()> {
    let cancel_token = CancellationToken::new();
    let track_id = "test-rtp-send-receive".to_string();

    let mut sender_track = RtpTrack::new(track_id.clone()).with_cancel_token(cancel_token.clone());

    let receiver_track_id = "test-rtp-receiver".to_string();
    let mut receiver_track =
        RtpTrack::new(receiver_track_id).with_cancel_token(cancel_token.clone());

    let sender_config = RtpTrackConfig {
        local_addr: "127.0.0.1:6000".parse().unwrap(),
        remote_addr: "127.0.0.1:6001".parse().unwrap(),
        payload_type: 0,
        ssrc: 12345,
    };

    let receiver_config = RtpTrackConfig {
        local_addr: "127.0.0.1:6001".parse().unwrap(),
        remote_addr: "127.0.0.1:6000".parse().unwrap(),
        payload_type: 0,
        ssrc: 54321,
    };

    sender_track = sender_track.with_rtp_config(sender_config);
    receiver_track = receiver_track.with_rtp_config(receiver_config);

    sender_track.setup_rtp_socket().await?;
    receiver_track.setup_rtp_socket().await?;

    let (sender, _receiver) = mpsc::unbounded_channel();

    receiver_track
        .start(cancel_token.clone(), dummy_event_sender(), sender)
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let audio_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(vec![0; 160]), // 20ms of silence at 8kHz
        timestamp: 0,
        sample_rate: 8000,
    };

    for _ in 0..5 {
        sender_track.send_packet(&audio_frame).await?;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    let received_packets = receiver_track.get_received_packets().await;

    println!("Received {} packets", received_packets.len());

    sender_track.stop().await?;
    receiver_track.stop().await?;
    cancel_token.cancel();

    Ok(())
}

#[tokio::test]
#[ignore = "Need to implement webrtc track"]
async fn test_multiple_rtp_packets() -> Result<()> {
    let cancel_token = CancellationToken::new();
    let track_id = "test-multiple-packets".to_string();

    let mut sender_track = RtpTrack::new(track_id.clone()).with_cancel_token(cancel_token.clone());

    let receiver_track_id = "test-multiple-receiver".to_string();
    let mut receiver_track =
        RtpTrack::new(receiver_track_id).with_cancel_token(cancel_token.clone());

    let sender_config = RtpTrackConfig {
        local_addr: "127.0.0.1:6002".parse().unwrap(),
        remote_addr: "127.0.0.1:6003".parse().unwrap(),
        payload_type: 0,
        ssrc: 12346,
    };

    let receiver_config = RtpTrackConfig {
        local_addr: "127.0.0.1:6003".parse().unwrap(),
        remote_addr: "127.0.0.1:6002".parse().unwrap(),
        payload_type: 0,
        ssrc: 54322,
    };

    sender_track = sender_track.with_rtp_config(sender_config);
    receiver_track = receiver_track.with_rtp_config(receiver_config);

    sender_track.setup_rtp_socket().await?;
    receiver_track.setup_rtp_socket().await?;

    let (sender, _receiver) = mpsc::unbounded_channel();

    receiver_track
        .start(cancel_token.clone(), dummy_event_sender(), sender)
        .await?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let num_packets = 5;
    for i in 0..num_packets {
        let audio_frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM(vec![i as i16; 160]),
            timestamp: i as u64 * 20,
            sample_rate: 8000,
        };

        for _ in 0..3 {
            sender_track.send_packet(&audio_frame).await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    let received_packets = receiver_track.get_received_packets().await;

    println!("Received {} packets", received_packets.len());

    sender_track.stop().await?;
    receiver_track.stop().await?;
    cancel_token.cancel();

    Ok(())
}
