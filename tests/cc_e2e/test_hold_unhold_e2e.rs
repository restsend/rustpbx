//! E2E test for cc-phone re-INVITE hold/unhold propagation.
//!
//! Validates that when the caller sends a hold (sendonly) or unhold (sendrecv)
//! re-INVITE through a B2BUA PBX, the server-side `build_local_dialog_answer`
//! properly processes the direction change and responds with the correct answer
//! SDP direction.

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_ua::{TestUaEvent, create_test_sdp};
use rustpbx::config::MediaProxyMode;
use rsipstack::dialog::DialogId;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

/// Modify an SDP string's audio direction attribute.
fn modify_sdp_direction(sdp: &str, direction: &str) -> String {
    let mut seen = false;
    let mut result = String::new();
    for line in sdp.lines() {
        if !seen
            && (line.contains("a=sendrecv")
                || line.contains("a=sendonly")
                || line.contains("a=recvonly")
                || line.contains("a=inactive"))
        {
            result.push_str(direction);
            result.push('\n');
            seen = true;
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

/// Extract the audio direction line from an SDP string.
fn extract_sdp_direction(sdp: &str) -> Option<&str> {
    for line in sdp.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("a=sendrecv")
            || trimmed.starts_with("a=sendonly")
            || trimmed.starts_with("a=recvonly")
            || trimmed.starts_with("a=inactive")
        {
            return Some(trimmed);
        }
    }
    None
}

#[tokio::test]
async fn test_hold_unhold_via_reinvite() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    info!("=== Starting hold/unhold e2e test ===");

    let server = Arc::new(
        E2eTestServer::start_with_mode(MediaProxyMode::All)
            .await
            .expect("E2E server start failed"),
    );

    let alice = Arc::new(
        server
            .create_ua("alice")
            .await
            .expect("create alice failed"),
    );
    let bob = server.create_ua("bob").await.expect("create bob failed");

    alice.register().await.expect("alice register failed");
    bob.register().await.expect("bob register failed");
    sleep(Duration::from_millis(200)).await;

    // Phase 1: Alice calls Bob
    info!("=== Phase 1: Alice calls Bob ===");
    let alice_sdp = create_test_sdp("127.0.0.1", 10000, false);
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = alice_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob receives incoming call and answers
    let mut bob_dialog_id: Option<DialogId> = None;
    for _ in 0..50 {
        let events = bob
            .process_dialog_events()
            .await
            .expect("bob process events");
        for event in &events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                let answer_sdp = create_test_sdp("127.0.0.1", 20000, false);
                bob.answer_call(id, Some(answer_sdp))
                    .await
                    .expect("bob answer failed");
                info!("Bob answered call: {}", id);
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let bob_dialog_id = bob_dialog_id.expect("Bob did not receive incoming call");

    // Bob should also get the CallEstablished event
    for _ in 0..30 {
        let events = bob
            .process_dialog_events()
            .await
            .expect("bob process events");
        let established = events
            .iter()
            .any(|e| matches!(e, TestUaEvent::CallEstablished(_)));
        if established {
            info!("Bob call established");
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    // Wait for Alice call to complete
    let alice_dialog_id: DialogId =
        match tokio::time::timeout(Duration::from_secs(10), caller_handle).await {
            Ok(Ok(Ok(id))) => {
                info!("Alice call established: {:?}", id);
                id
            }
            Ok(Ok(Err(e))) => panic!("Alice call failed: {:?}", e),
            Ok(Err(join_err)) => panic!("Alice call join error: {:?}", join_err),
            Err(_) => panic!("Alice call timed out"),
        };

    // Let both sides settle
    sleep(Duration::from_millis(500)).await;

    // Phase 2: Alice sends hold re-INVITE (sendonly)
    info!("=== Phase 2: Hold ===");
    let hold_sdp = modify_sdp_direction(&alice_sdp, "a=sendonly");
    info!("Hold offer direction: sendonly");

    let hold_answer = alice
        .send_reinvite(&alice_dialog_id, Some(hold_sdp))
        .await
        .expect("hold re-INVITE failed");

    if let Some(ref sdp) = hold_answer {
        let dir = extract_sdp_direction(sdp);
        info!("Hold answer direction: {:?}", dir);
        assert_eq!(
            dir,
            Some("a=recvonly"),
            "Hold answer should be recvonly (RFC 3264 mirror of sendonly), got: {}",
            sdp
        );
    } else {
        info!("Hold answer has no SDP body (signaling-only mode)");
    }

    // Let server process hold propagation
    sleep(Duration::from_millis(500)).await;

    // Phase 3: Alice sends unhold re-INVITE (sendrecv)
    info!("=== Phase 3: Unhold ===");
    let unhold_sdp = modify_sdp_direction(&alice_sdp, "a=sendrecv");
    info!("Unhold offer direction: sendrecv");

    let unhold_answer = alice
        .send_reinvite(&alice_dialog_id, Some(unhold_sdp))
        .await
        .expect("unhold re-INVITE failed");

    if let Some(ref sdp) = unhold_answer {
        let dir = extract_sdp_direction(sdp);
        info!("Unhold answer direction: {:?}", dir);
        assert_eq!(
            dir,
            Some("a=sendrecv"),
            "Unhold answer should be sendrecv (mirror of sendrecv), got: {}",
            sdp
        );
    } else {
        info!("Unhold answer has no SDP body (signaling-only mode)");
    }

    // Phase 4: Cleanup
    info!("=== Phase 4: Cleanup ===");
    alice.hangup(&alice_dialog_id).await.ok();
    bob.hangup(&bob_dialog_id).await.ok();
    sleep(Duration::from_millis(200)).await;
    info!("=== Hold/unhold e2e test complete ===");
}
