//! Regression test: WebRTC→RTP bridge caller_gate never opens when
//! callee sends 183 early media with SDP followed by 200 OK with the
//! *identical* SDP.
//!
//! **Bug reference**: Production call `k0pg5a11mcqslqoghebt`
//! `caller_to_callee_pps=0` for the entire 35-second call.
//!
//! **Root cause** (in `sip_session.rs::prepare_caller_answer_from_callee_sdp`):
//!
//! 1. Callee sends **183 Session Progress + SDP**
//!    → `prepare_caller_answer_from_callee_sdp(..., is_early_media=true)` runs
//!    → Generates caller answer, sets `self.media.answer`
//!    → `open_caller_gate()` is NOT called (gate stays closed for early media)
//!
//! 2. Callee sends **200 OK with IDENTICAL SDP**
//!    → `prepare_caller_answer_from_callee_sdp(..., is_early_media=false)` runs
//!    → Line 3943: `self.media.answer.is_some() && !sdp_changed && !force_regenerate`
//!    → **Returns early** — never reaches the `open_caller_gate()` call at line 4170
//!    → Gate stays closed for the entire call
//!
//! 3. Bridge `run_forward_loop` silently drops every caller→callee packet at
//!    `bridge.rs:1740-1744` (gate check with no logging).
//!
//! **This test FAILS before the fix and PASSES after the fix.**

use super::e2e_test_server::E2eTestServer;
use super::test_ua::TestUaEvent;
use crate::config::MediaProxyMode;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::info;

fn pcma_rtp_sdp(ip: &str, port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1778630403 1778630404 IN IP4 {ip}\r\n\
        s=-\r\n\
        c=IN IP4 {ip}\r\n\
        t=0 0\r\n\
        m=audio {port} RTP/AVP 8 126\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:126 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

fn webrtc_sdp(ip: &str, port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 654321 654321 IN IP4 {ip}\r\n\
        s=-\r\n\
        c=IN IP4 {ip}\r\n\
        t=0 0\r\n\
        m=audio {port} UDP/TLS/RTP/SAVPF 111 8 101\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n\
        a=setup:actpass\r\n\
        a=ice-ufrag:testufrag\r\n\
        a=ice-pwd:testicepwd1234567890\r\n\
        a=mid:0\r\n\
        a=sendrecv\r\n\
        a=rtcp-mux\r\n"
    )
}

#[tokio::test]
async fn test_webrtc_rtp_caller_gate_opens_on_same_sdp_200ok() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Standard test users: alice=WebRTC, bob=RTP
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(200)).await;

    let bob_sdp = pcma_rtp_sdp("127.0.0.1", 12345);
    let alice_sdp = webrtc_sdp("127.0.0.1", 54321);

    // alice calls bob
    let alice_clone = alice.clone();
    let caller_handle = crate::utils::spawn(async move {
        timeout(
            Duration::from_secs(15),
            alice_clone.make_call("bob", Some(alice_sdp)),
        )
        .await
    });

    // bob: receive INVITE → send 183 + SDP → wait → send 200 OK with SAME SDP
    let mut bob_dialog_id = None;
    for _ in 0..100 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _offer) = event {
                bob_dialog_id = Some(id.clone());

                // Step 1: 183 Session Progress with SDP (early media)
                info!("Bob: sending 183 + early-media SDP");
                bob.send_ringing(&id, Some(bob_sdp.clone())).await?;

                // Wait to simulate real-world gap between 183 and 200 OK
                sleep(Duration::from_millis(400)).await;

                // Step 2: 200 OK with the IDENTICAL SDP (triggers the bug)
                info!("Bob: sending 200 OK with SAME SDP (bug trigger)");
                bob.answer_call(&id, Some(bob_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let bob_id = bob_dialog_id.ok_or_else(|| anyhow::anyhow!("Bob never received INVITE"))?;

    // Wait for call to establish
    let alice_dialog_id = caller_handle
        .await
        .map_err(|_| anyhow::anyhow!("Caller task panicked"))?
        .map_err(|_| anyhow::anyhow!("Call timed out"))?
        .map_err(|e| anyhow::anyhow!("Call setup failed: {}", e))?;

    info!(
        "Call established: alice={}, bob={}",
        alice_dialog_id, bob_id
    );

    // Wait for snapshot to update
    sleep(Duration::from_millis(500)).await;

    // Keep processing dialog events so snapshots update
    for _ in 0..10 {
        let _ = alice.process_dialog_events().await;
        let _ = bob.process_dialog_events().await;
        sleep(Duration::from_millis(100)).await;
    }

    // Find the session in registry and check caller_gate_open
    let registry = &server.registry;
    let sessions = registry.list_recent(10);

    let session_id = sessions
        .first()
        .map(|s| s.session_id.clone())
        .ok_or_else(|| anyhow::anyhow!("No active session found in registry"))?;

    info!("Found session: {}", session_id);

    let handle = registry
        .get_handle(&session_id)
        .ok_or_else(|| anyhow::anyhow!("No handle for session"))?;

    let snapshot = handle
        .snapshot()
        .ok_or_else(|| anyhow::anyhow!("No snapshot available"))?;

    info!(
        session_id = %snapshot.id,
        state = ?snapshot.state,
        bridge_active = snapshot.bridge_active,
        caller_gate_open = snapshot.caller_gate_open,
        "Session snapshot after 200 OK with identical SDP"
    );

    // Clean up
    alice.hangup(&alice_dialog_id).await.ok();
    sleep(Duration::from_millis(200)).await;
    server.stop();

    // THE ASSERTION: caller_gate must be open after 200 OK
    // Before the fix: caller_gate_open = false (bug)
    // After the fix: caller_gate_open = true
    assert!(
        snapshot.caller_gate_open,
        "BUG REPRODUCED: caller_gate is still closed after 200 OK with same SDP. \
         Root cause: prepare_caller_answer_from_callee_sdp returns early at line 3943 \
         without calling open_caller_gate(). The bridge's run_forward_loop silently \
         drops all WebRTC→RTP audio at bridge.rs:1740-1744."
    );

    info!("test_webrtc_rtp_caller_gate_opens_on_same_sdp_200ok PASSED");
    Ok(())
}
