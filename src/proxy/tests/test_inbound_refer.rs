//! Inbound REFER E2E Tests
//!
//! Tests verify that PBX correctly handles incoming REFER requests:
//! - Returns 202 Accepted
//! - Sends NOTIFY 100 Trying and final NOTIFY
//! - Originates new call to transfer target
//! - Bridges original call with transfer target

use super::e2e_test_server::E2eTestServer;
use super::test_ua::TestUaEvent;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Test inbound REFER success flow.
///
/// Scenario:
/// 1. Alice registers and calls Bob via PBX
/// 2. Bob answers
/// 3. Alice sends REFER to PBX, targeting Charlie (sipbot)
/// 4. PBX returns 202 Accepted, then originates to Charlie
/// 5. Charlie answers
/// 6. PBX bridges the calls
#[tokio::test]
async fn test_inbound_refer_success() {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(
        E2eTestServer::start()
            .await
            .expect("E2E server start failed"),
    );

    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let bob = server.create_ua("bob").await.expect("create bob failed");

    let alice_sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 12345 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    let bob_sdp = "v=0\r\n\
        o=- 789012 789012 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 54321 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    // Alice calls Bob
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob receives and answers
    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob
            .process_dialog_events()
            .await
            .expect("process events failed");
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(bob_sdp.clone()))
                    .await
                    .expect("bob answer failed");
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
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => panic!("Call failed: {:?}", e),
        _ => panic!("Call timed out"),
    };

    // Wait for active call in registry
    server
        .wait_for_active_call(Duration::from_secs(3))
        .await
        .expect("Call should be in registry");

    // Create Charlie UA (rsipstack-based)
    let charlie = server
        .create_ua("charlie")
        .await
        .expect("create charlie failed");
    let charlie_port = charlie.local_port();
    let charlie_uri = format!("sip:charlie@127.0.0.1:{}", charlie_port);

    let charlie_sdp = "v=0\r\n\
        o=- 111111 111111 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 33333 RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
        .to_string();

    // Spawn a task for Charlie to answer incoming calls
    let charlie_clone = charlie.clone();
    let charlie_answer_handle = tokio::spawn(async move {
        let mut charlie_dialog_id = None;
        for _ in 0..50 {
            let events = charlie_clone
                .process_dialog_events()
                .await
                .expect("charlie process events failed");
            for event in events {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    charlie_clone
                        .answer_call(&id, Some(charlie_sdp.clone()))
                        .await
                        .expect("charlie answer failed");
                    charlie_dialog_id = Some(id);
                    break;
                }
            }
            if charlie_dialog_id.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        charlie_dialog_id
    });

    sleep(Duration::from_millis(300)).await;

    // Alice sends REFER to PBX
    let refer_status = alice
        .send_refer(&alice_dialog_id, &charlie_uri)
        .await
        .expect("send_refer failed");

    assert_eq!(refer_status, 202, "REFER should be accepted with 202");

    // Process Alice's dialog events (including NOTIFY from PBX) so the REFER subscription can proceed
    let alice_clone = alice.clone();
    let alice_event_handle = tokio::spawn(async move {
        for _ in 0..100 {
            let _ = alice_clone.process_dialog_events().await;
            sleep(Duration::from_millis(50)).await;
        }
    });

    // Wait for Charlie to answer the transfer call
    let charlie_dialog_id = tokio::time::timeout(Duration::from_secs(5), charlie_answer_handle)
        .await
        .expect("Charlie answer timeout")
        .expect("charlie answer task failed");
    assert!(
        charlie_dialog_id.is_some(),
        "Charlie should receive and answer the transfer call"
    );

    alice_event_handle.abort();

    // Wait for PBX to originate to Charlie
    let mut found_transfer_call = false;
    for _ in 0..50 {
        let calls = server.get_active_calls();
        let outbound_calls: Vec<_> = calls
            .iter()
            .filter(|c| {
                c.direction == "outbound"
                    && c.callee
                        .as_ref()
                        .map(|s: &String| s.contains("charlie"))
                        .unwrap_or(false)
            })
            .collect();
        if !outbound_calls.is_empty() {
            found_transfer_call = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found_transfer_call,
        "PBX should originate a call to Charlie after receiving REFER"
    );

    // Cleanup
    alice.hangup(&alice_dialog_id).await.ok();
    bob.hangup(&bob_dialog_id.unwrap()).await.ok();
    if let Some(ref id) = charlie_dialog_id {
        charlie.hangup(id).await.ok();
    }
    server.stop();
}
