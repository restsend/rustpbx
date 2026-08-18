use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::RtpReceiver;
use crate::common::test_ua::TestUaEvent;

#[tokio::test]
async fn test_hold_unhold_via_reinvite() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let callee_receiver = RtpReceiver::bind(0).await?;
    let callee_port = callee_receiver.port()?;
    callee_receiver.start_receiving();

    let callee_sdp = format!(
        "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {callee_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
    );

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = callee_sdp.clone();
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

    // Send hold re-INVITE: a=sendonly
    let hold_sdp = format!(
        "v=0\r\no=- 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 10000 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendonly\r\n"
    );
    alice
        .send_reinvite(&alice_dialog_id.clone().unwrap(), Some(hold_sdp))
        .await?;
    sleep(Duration::from_millis(500)).await;

    // Send unhold re-INVITE: a=sendrecv
    let unhold_sdp = format!(
        "v=0\r\no=- 4 4 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 10002 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
    );
    alice
        .send_reinvite(&alice_dialog_id.clone().unwrap(), Some(unhold_sdp))
        .await?;
    sleep(Duration::from_millis(500)).await;

    server.stop();
    Ok(())
}
