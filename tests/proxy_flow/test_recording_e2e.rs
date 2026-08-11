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
async fn test_recording_cdr_generated() -> Result<()> {
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
    assert!(answered);

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_id.is_some(), "Call should establish");

    sleep(Duration::from_millis(500)).await;
    alice.hangup(&alice_id.clone().unwrap()).await.ok();
    sleep(Duration::from_millis(500)).await;
    let all_records = server.cdr_capture.get_all_records().await;
    assert!(!all_records.is_empty(), "CDR should be generated");

    server.stop();
    Ok(())
}
