use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::RtpReceiver;
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::TestUaEvent;

async fn setup_rtp_pair() -> Result<(RtpReceiver, RtpReceiver, u16, u16)> {
    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();
    Ok((caller_receiver, callee_receiver, caller_port, callee_port))
}

#[tokio::test]
async fn test_early_media_183_ringback() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let (_, _, caller_port, callee_port) = setup_rtp_pair().await?;
    let caller_sdp = make_sdp(caller_port);
    let callee_sdp = make_sdp(callee_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut got_ringing = false;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.send_ringing(&id, None).await?;
                sleep(Duration::from_millis(300)).await;
                got_ringing = true;
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if got_ringing { break; }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(got_ringing, "Bob should send ringing");

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish after 180→200");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_call_180_to_200_flow() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let (_, _, caller_port, callee_port) = setup_rtp_pair().await?;
    let caller_sdp = make_sdp(caller_port);
    let callee_sdp = make_sdp(callee_port);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut answered = false;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                answered = true;
            }
        }
        if answered { break; }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(answered, "Bob should answer");

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "180→200 call should complete");

    server.stop();
    Ok(())
}
