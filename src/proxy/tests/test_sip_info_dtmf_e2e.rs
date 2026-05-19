//! E2E tests verifying SIP INFO DTMF forwarding in B2B proxy mode.
//!
//! Covers:
//! - Caller → Callee: SIP INFO with application/dtmf-relay is forwarded to the connected callee
//! - Callee → Caller: SIP INFO with application/dtmf-relay is forwarded back to the caller

use super::e2e_test_server::E2eTestServer;
use super::test_ua::TestUaEvent;
use crate::config::MediaProxyMode;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

/// SDP with telephone-event for both sides
fn make_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
         o=- 123456 123456 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-15\r\n\
         a=sendrecv\r\n",
        port
    )
}

/// Helper: establish a B2B call between alice and bob.
/// Returns (alice_dialog_id, bob_dialog_id).
async fn establish_call(
    server: &E2eTestServer,
    alice: &Arc<super::test_ua::TestUa>,
    bob: &super::test_ua::TestUa,
) -> Result<(rsipstack::dialog::DialogId, rsipstack::dialog::DialogId)> {
    let alice_sdp = make_sdp(20000);
    let bob_sdp = make_sdp(20010);

    // Alice dials Bob (blocks until 200 OK)
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob answers
    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(bob_sdp.clone())).await?;
                bob_dialog_id = Some(id);
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    let bob_dialog_id = bob_dialog_id.expect("Bob should receive the call");

    let alice_dialog_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .expect("make_call timed out")
        .expect("make_call task panicked")
        .expect("make_call failed");

    // Let registrations and session state settle
    let _ = server.wait_for_active_call(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(200)).await;

    info!(
        "Call established alice={} bob={}",
        alice_dialog_id, bob_dialog_id
    );
    Ok((alice_dialog_id, bob_dialog_id))
}

/// Wait for a DtmfInfo event with the expected digit, polling up to `timeout`.
async fn wait_for_dtmf(
    ua: &super::test_ua::TestUa,
    expected_digit: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let events = ua.process_dialog_events().await.unwrap_or_default();
        for event in events {
            if let TestUaEvent::DtmfInfo(_, digit) = event {
                if digit == expected_digit {
                    return true;
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Verify that a SIP INFO DTMF sent by the caller is forwarded to the callee.
#[tokio::test]
async fn test_sip_info_dtmf_forwarded_caller_to_callee() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let (alice_dialog_id, _bob_dialog_id) = establish_call(&server, &alice, &bob).await?;

    // Alice sends digit '5' via SIP INFO
    alice.send_dtmf_info(&alice_dialog_id, "5").await?;
    info!("Alice sent SIP INFO DTMF '5'");

    // Bob should receive it
    let received = wait_for_dtmf(&bob, "5", Duration::from_secs(3)).await;
    assert!(
        received,
        "Bob should receive DTMF '5' forwarded from Alice via SIP INFO"
    );

    // Cleanup
    alice.hangup(&alice_dialog_id).await.ok();
    sleep(Duration::from_millis(200)).await;
    server.stop();

    Ok(())
}

/// Verify that a SIP INFO DTMF sent by the callee is forwarded back to the caller.
#[tokio::test]
async fn test_sip_info_dtmf_forwarded_callee_to_caller() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let (alice_dialog_id, bob_dialog_id) = establish_call(&server, &alice, &bob).await?;

    // Bob sends digit '9' via SIP INFO
    bob.send_dtmf_info(&bob_dialog_id, "9").await?;
    info!("Bob sent SIP INFO DTMF '9'");

    // Alice should receive it
    let received = wait_for_dtmf(&*alice, "9", Duration::from_secs(3)).await;
    assert!(
        received,
        "Alice should receive DTMF '9' forwarded from Bob via SIP INFO"
    );

    // Cleanup
    alice.hangup(&alice_dialog_id).await.ok();
    sleep(Duration::from_millis(200)).await;
    server.stop();

    Ok(())
}

/// Verify bidirectional SIP INFO DTMF forwarding in a single call.
#[tokio::test]
async fn test_sip_info_dtmf_bidirectional() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let (alice_dialog_id, bob_dialog_id) = establish_call(&server, &alice, &bob).await?;

    // Alice → Bob: digit '1'
    alice.send_dtmf_info(&alice_dialog_id, "1").await?;
    let bob_got_1 = wait_for_dtmf(&bob, "1", Duration::from_secs(3)).await;
    assert!(bob_got_1, "Bob should receive DTMF '1' from Alice");

    // Bob → Alice: digit '#'
    bob.send_dtmf_info(&bob_dialog_id, "#").await?;
    let alice_got_hash = wait_for_dtmf(&*alice, "#", Duration::from_secs(3)).await;
    assert!(alice_got_hash, "Alice should receive DTMF '#' from Bob");

    // Cleanup
    alice.hangup(&alice_dialog_id).await.ok();
    sleep(Duration::from_millis(200)).await;
    server.stop();

    Ok(())
}
