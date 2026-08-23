//! BC → ABC consult-transfer test (partially in-process).
//!
//! Phases 1–2 are real SIP: A calls B, B consults C, both bridged through a
//! real in-process `SipServer`. Phase 3 drives `ConsultTransferManager`
//! directly on a DETACHED `CcAddonState` (not wired to the live sessions),
//! so the SIP REFER / re-INVITE transfer signaling and the post-merge media
//! path are NOT exercised here — this covers the transfer-manager state
//! machine only. SIP-level transfer coverage lives in
//! `tests/proxy_e2e/test_inbound_refer.rs` and the Python e2e suite.
//!
//! Flow:
//! 1. A calls B (established)
//! 2. B holds A, calls C (BC consulting)
//! 3. Merge to ABC conference (state machine)
//! 4. B exits
//! 5. A/C legs tear down cleanly (no wedged sessions)

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_ua::TestUaEvent;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

#[tokio::test]
async fn test_consult_transfer_bc_to_abc_to_ac() {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(
        E2eTestServer::start()
            .await
            .expect("E2E server start failed"),
    );

    // Create UAs
    let alice = server
        .create_ua("alice")
        .await
        .expect("create alice failed");
    let bob = server.create_ua("bob").await.expect("create bob failed");
    let charlie = server
        .create_ua("charlie")
        .await
        .expect("create charlie failed");

    let alice_sdp = default_sdp("127.0.0.1", 12345);
    let bob_sdp = default_sdp("127.0.0.1", 54321);
    let charlie_sdp = default_sdp("127.0.0.1", 33333);

    // ========== Phase 1: A calls B ==========
    info!("=== Phase 1: A calls B ===");
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

    // Wait for active call
    server
        .wait_for_active_call(Duration::from_secs(3))
        .await
        .expect("Call should be active");

    info!("=== Phase 1 complete: A-B established ===");
    sleep(Duration::from_millis(500)).await;

    // ========== Phase 2: B holds A, calls C (BC consulting) ==========
    info!("=== Phase 2: B holds A, calls C ===");

    // Bob sends re-INVITE to put A on hold
    // For simplicity in this test, we'll skip the actual hold SDP renegotiation
    // and directly initiate the consult call

    let charlie_port = charlie.local_port();
    let charlie_uri = format!("sip:charlie@127.0.0.1:{}", charlie_port);

    // Bob calls Charlie
    let bob_to_charlie_handle = tokio::spawn({
        let b = bob.clone();
        let sdp = bob_sdp.clone();
        async move { b.make_call("charlie", Some(sdp)).await }
    });

    // Charlie receives and answers
    let mut charlie_dialog_id = None;
    for _ in 0..50 {
        let events = charlie
            .process_dialog_events()
            .await
            .expect("process events failed");
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                charlie_dialog_id = Some(id.clone());
                charlie
                    .answer_call(&id, Some(charlie_sdp.clone()))
                    .await
                    .expect("charlie answer failed");
                break;
            }
        }
        if charlie_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        charlie_dialog_id.is_some(),
        "Charlie should receive the call"
    );

    let _bob_to_charlie_dialog =
        match tokio::time::timeout(Duration::from_secs(5), bob_to_charlie_handle).await {
            Ok(Ok(Ok(id))) => id,
            Ok(Ok(Err(e))) => panic!("Bob-Charlie call failed: {:?}", e),
            _ => panic!("Bob-Charlie call timed out"),
        };

    // Wait for second call in registry
    let mut found_second_call = false;
    for _ in 0..50 {
        let calls = server.get_active_calls();
        if calls.len() >= 2 {
            found_second_call = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found_second_call,
        "Should have 2 active calls (A-B and B-C)"
    );

    info!("=== Phase 2 complete: BC consulting established ===");
    sleep(Duration::from_millis(500)).await;

    // ========== Phase 3: Merge to ABC conference ==========
    info!("=== Phase 3: Merge to ABC conference ===");

    // Use CC Addon to manage the transfer
    #[cfg(feature = "addon-cc")]
    {
        let cc_addon_state = Arc::new(tokio::sync::RwLock::new(
            rustpbx::addons::cc::CcAddonState::new(),
        ));
        let cc = cc_addon_state.read().await;
        let mut tm: tokio::sync::RwLockWriteGuard<
            '_,
            rustpbx::addons::cc::transfer::ConsultTransferManager,
        > = cc.transfer_manager.write().await;

        // Initiate transfer (A-B session is the original, B-C is consultation)
        let transfer_id = "e2e-tx-001".to_string();
        tm.initiate(
            transfer_id.clone(),
            alice_dialog_id.to_string(),
            "bob".to_string(),
            charlie_uri.clone(),
        );

        // Mark consultation as connected
        tm.consultation_connected(&transfer_id, _bob_to_charlie_dialog.to_string())
            .unwrap();

        // Merge to conference
        let conf_id: String = tm
            .merge_to_conference(&transfer_id)
            .await
            .expect("merge to conference failed");
        assert!(!conf_id.is_empty(), "conference id must be assigned");
        info!("Conference created: {}", conf_id);

        // The manager must have recorded the transfer as Completed with the
        // conference it created (A + B-C legs joined).
        let state = tm
            .get_state(&transfer_id)
            .expect("transfer must be tracked after merge");
        assert!(
            matches!(
                state,
                rustpbx::addons::cc::transfer::TransferState::Completed { conf_id: cid, .. }
                    if *cid == conf_id
            ),
            "transfer must complete after merge_to_conference, got {state:?}"
        );

        info!("=== Phase 3 complete: ABC conference established ===");

        // ========== Phase 3b: owner-anchored same-session merge ==========
        // When consult is LegAdd on the A-B session, session_b == session_a
        // and merge must JoinMixerLeg(caller+callee+consult).
        let live_registry = server.registry.clone();
        let mut tm_owner = rustpbx::addons::cc::transfer::ConsultTransferManager::new(Arc::new(
            rustpbx::call::runtime::ConferenceManager::new(),
        ))
        .with_call_registry(live_registry);
        let owner_tx = "e2e-tx-owner-001".to_string();
        // Prefer a live proxy session id when available.
        let live_sid = server
            .get_active_calls()
            .into_iter()
            .next()
            .map(|c| c.session_id)
            .unwrap_or_else(|| alice_dialog_id.to_string());
        tm_owner.initiate(
            owner_tx.clone(),
            live_sid.clone(),
            "bob".to_string(),
            charlie_uri.clone(),
        );
        tm_owner
            .consultation_connected(&owner_tx, live_sid.clone())
            .unwrap();
        let owner_conf = tm_owner
            .merge_to_conference(&owner_tx)
            .await
            .expect("owner-anchored merge");
        assert!(owner_conf.starts_with("conf-"));
        info!("=== Phase 3b complete: owner-anchored merge {owner_conf} ===");
    }
    #[cfg(not(feature = "addon-cc"))]
    {
        info!("CC addon disabled, skipping consult transfer");
    }

    sleep(Duration::from_millis(500)).await;

    // ========== Phase 4: B exits ==========
    info!("=== Phase 4: B exits ===");

    // Bob hangs up both calls
    if let Some(ref id) = bob_dialog_id {
        bob.hangup(id).await.ok();
    }
    // Note: Bob's call to Charlie is a separate dialog, but in this test scenario,
    // Bob would typically send a BYE to end the consultation

    sleep(Duration::from_millis(500)).await;

    // ========== Phase 5: legs tear down cleanly ==========
    info!("=== Phase 5: legs tear down cleanly ===");

    // Cleanup
    if let Some(ref id) = charlie_dialog_id {
        charlie.hangup(id).await.ok();
    }
    alice.hangup(&alice_dialog_id).await.ok();

    // Regression guard: after every party hangs up, the server's active-call
    // registry must drain — a wedged session here is exactly the
    // hangup/ownership leak class this week's fixes target.
    let drained = {
        let mut drained = false;
        for _ in 0..50 {
            if server.get_active_calls().is_empty() {
                drained = true;
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        drained
    };
    assert!(
        drained,
        "active-call registry must drain after all parties hang up, still active: {:?}",
        server
            .get_active_calls()
            .iter()
            .map(|c| c.session_id.clone())
            .collect::<Vec<_>>()
    );

    server.stop();
    info!("=== Test complete ===");
}

fn default_sdp(ip: &str, port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- {} {} IN IP4 {}\r\n\
        s=-\r\n\
        c=IN IP4 {}\r\n\
        t=0 0\r\n\
        m=audio {} RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n",
        port, port, ip, ip, port
    )
}
