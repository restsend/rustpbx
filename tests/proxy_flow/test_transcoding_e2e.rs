use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender};
use crate::common::test_ua::TestUaEvent;

fn pcmu_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {port} RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

fn g729_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {port} RTP/AVP 18 101\r\n\
        a=rtpmap:18 G729/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

fn pcma_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {port} RTP/AVP 8 101\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

#[tokio::test]
async fn test_pcmu_to_g729_transcode() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let caller_sdp = pcmu_sdp(caller_port);
    let callee_sdp = g729_sdp(callee_port);
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the call");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "Call should be established");

    // Send PCMU RTP from caller
    let caller_sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x11111111, 0, 160, 160);
    caller_sender.start_sending(
        std::net::SocketAddr::from(([127, 0, 0, 1], callee_port)),
        packets,
        20,
    );
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let callee_stats = callee_receiver.get_stats().await;
    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive RTP after transcoding"
    );
    assert!(
        callee_stats.payload_types.contains(&18),
        "Callee should receive G729 RTP (PT=18) after PCMU→G729 transcode, got {:?}",
        callee_stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_pcmu_to_pcma_transcode() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let caller_sdp = pcmu_sdp(caller_port);
    let callee_sdp = pcma_sdp(callee_port);
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "Call should be established");

    let caller_sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x22222222, 0, 160, 160);
    caller_sender.start_sending(
        std::net::SocketAddr::from(([127, 0, 0, 1], callee_port)),
        packets,
        20,
    );
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let callee_stats = callee_receiver.get_stats().await;
    assert!(callee_stats.packets_received > 0, "Callee should receive PCMA RTP");
    assert!(
        callee_stats.payload_types.contains(&8),
        "Callee should receive PCMA RTP (PT=8) after PCMU→PCMA transcode, got {:?}",
        callee_stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_g722_to_g729_transcode() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let g722_sdp = format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {caller_port} RTP/AVP 9 101\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    );
    let g729_sdp = g729_sdp(callee_port);

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = g722_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(g729_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "G722→G729 call should establish");

    let caller_sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x33333333, 9, 160, 160);
    caller_sender.start_sending(
        std::net::SocketAddr::from(([127, 0, 0, 1], callee_port)),
        packets,
        20,
    );
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let callee_stats = callee_receiver.get_stats().await;
    assert!(callee_stats.packets_received > 0, "Callee should receive G729 RTP after transcoding from G722");
    assert!(
        callee_stats.payload_types.contains(&18),
        "Callee should receive G729 RTP (PT=18) after G722→G729 transcode, got {:?}",
        callee_stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}
