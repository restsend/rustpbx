use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::RtpReceiver;
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::TestUaEvent;

#[tokio::test]
async fn test_call_hangup_writes_cdr() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sdp = make_sdp(caller_receiver.port()?);
    let callee_sdp = make_sdp(callee_receiver.port()?);

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

    let alice_id = match tokio::time::timeout(Duration::from_secs(8), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    // Let the call run so it's fully established, then hang up properly.
    // A clean hangup MUST produce a CDR (verified product behavior).
    sleep(Duration::from_millis(1000)).await;
    alice.hangup(&alice_id.unwrap()).await.ok();
    sleep(Duration::from_millis(800)).await;

    let all_records = server.cdr_capture.get_all_records().await;
    assert!(
        !all_records.is_empty(),
        "Clean hangup should write CDR for the call"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_server_restart_after_stop() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // First server
    let server1 = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    server1.stop();

    // Second server on a fresh port - should start cleanly
    let server2 = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server2.create_ua("alice").await?);
    let bob = server2.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sdp = make_sdp(caller_receiver.port()?);
    let callee_sdp = make_sdp(callee_receiver.port()?);

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
    assert!(alice_id.is_some(), "Call should establish on restarted server");
    server2.stop();
    Ok(())
}
