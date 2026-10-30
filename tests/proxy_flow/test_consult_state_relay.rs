use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_helpers::make_sdp;
use crate::common::test_ua::TestUaEvent;
use crate::common::wait::wait_until;

/// restend private convention: the local 3-way merge edges (a pure
/// client-side conference the server does not know about) are announced by
/// the transferor's UA via a body-less in-dialog UPDATE carrying a
/// `User-to-User: consult-state=…` marker header — the B2BUA relays it to
/// the opposite side of the dialog (xlsx r5-⑤ consulted-party state sync /
/// r9-② recording segmentation signal).
#[tokio::test]
async fn test_consult_state_marker_update_relays_to_peer_leg() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    // create_ua returns once the UA is registered.
    let bob = server.create_ua("bob").await?;

    let sdp = make_sdp(40000);

    let caller = tokio::spawn({
        let a = alice.clone();
        let sdp = sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..100 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(bob_dialog_id.is_some(), "Call should ring bob");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    let alice_dialog_id = alice_dialog_id.expect("Call should establish");

    // Alice sends the marker-carrying body-less UPDATE (merge install edge).
    alice
        .send_update_with_headers(
            &alice_dialog_id,
            vec![rsipstack::sip::Header::UserToUser(
                "consult-state=merged".into(),
            )],
        )
        .await?;

    // Bob must receive the relayed marker UPDATE.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = None;
    while std::time::Instant::now() < deadline {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::ConsultStateUpdated(_, phase) = event {
                seen = Some(phase);
            }
        }
        if seen.is_some() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        seen.as_deref(),
        Some("merged"),
        "consult_state marker must relay to the peer leg"
    );

    // Exit edge: the talk marker relays as well.
    alice
        .send_update_with_headers(
            &alice_dialog_id,
            vec![rsipstack::sip::Header::UserToUser(
                "consult-state=talk".into(),
            )],
        )
        .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut talk_seen = false;
    while std::time::Instant::now() < deadline {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::ConsultStateUpdated(_, phase) = event {
                if phase == "talk" {
                    talk_seen = true;
                }
            }
        }
        if talk_seen {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    assert!(talk_seen, "consult-state=talk must relay as well");

    // Alice hangs up to end the call.
    alice.hangup(&alice_dialog_id).await?;
    let bob = &bob;
    wait_until(Duration::from_secs(5), || async move {
        bob.process_dialog_events()
            .await
            .map(|events| {
                events
                    .iter()
                    .any(|e| matches!(e, TestUaEvent::CallTerminated(_)))
            })
            .unwrap_or(false)
    })
    .await
    .expect("hangup must reach bob");
    server.stop();
    Ok(())
}
