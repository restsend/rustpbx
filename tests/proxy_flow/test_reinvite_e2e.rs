use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender};
use crate::common::test_helpers::{make_sdp, pcma_sdp};
use crate::common::test_ua::TestUaEvent;

#[tokio::test]
async fn test_reinvite_codec_change_pcmu_to_pcma() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let caller_sdp = make_sdp(caller_port);
    let callee_sdp = make_sdp(callee_port);

    let caller = tokio::spawn({
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
        if bob_dialog_id.is_some() { break; }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    // Verify PCMU (PT=0) flows before re-INVITE
    let sender = RtpSender::bind().await?;
    let pcmu_packets = RtpPacket::create_sequence(10, 1000, 50000, 0x11111111, 0, 160, 160);
    sender.start_sending(
        std::net::SocketAddr::from(([127, 0, 0, 1], callee_port)),
        pcmu_packets,
        20,
    );
    sleep(Duration::from_millis(500)).await;
    sender.stop();
    let stats = callee_receiver.get_stats().await;
    assert!(
        stats.payload_types.contains(&0),
        "PCMU (PT=0) should flow before re-INVITE, got {:?}",
        stats.payload_types
    );

    // Mid-call re-INVITE: switch to PCMA (PT=8)
    let new_caller_port = RtpReceiver::bind(0).await?.port()?;
    let new_callee_port = RtpReceiver::bind(0).await?.port()?;
    let pcma_offer = pcma_sdp("127.0.0.1", new_caller_port);
    let reinvite_result = alice.send_reinvite(&alice_id.clone().unwrap(), Some(pcma_offer)).await;
    assert!(reinvite_result.is_ok(), "re-INVITE should be accepted");

    sleep(Duration::from_millis(500)).await;

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_reinvite_hold_unhold_via_sdp_direction() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    let caller_sdp = make_sdp(caller_port);
    let callee_sdp = make_sdp(callee_port);

    let caller = tokio::spawn({
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
        if bob_dialog_id.is_some() { break; }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    // Hold: re-INVITE with a=sendonly
    let hold_sdp = format!(
        "v=0\r\no=- 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {caller_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendonly\r\n"
    );
    let hold_result = alice.send_reinvite(&alice_id.clone().unwrap(), Some(hold_sdp)).await;
    assert!(hold_result.is_ok(), "Hold re-INVITE should be accepted");
    sleep(Duration::from_millis(300)).await;

    // Unhold: re-INVITE with a=sendrecv
    let unhold_sdp = format!(
        "v=0\r\no=- 4 4 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {caller_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
    );
    let unhold_result = alice.send_reinvite(&alice_id.clone().unwrap(), Some(unhold_sdp)).await;
    assert!(unhold_result.is_ok(), "Unhold re-INVITE should be accepted");
    sleep(Duration::from_millis(300)).await;

    server.stop();
    Ok(())
}
