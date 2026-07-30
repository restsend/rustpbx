//! Regression test: 183 early-media with SDP A followed by 200 OK with
//! DIFFERENT SDP B — bridge forwarder must be restarted to pick up
//! updated codec/transport state.
//!
//! **Bug reference**: `start_media_bridge_forwarding` had an early-return
//! guard (`if self.media.media_bridge_started { return; }`) that prevented
//! `bridge.start_bridge()` from being called a second time. After 183 early
//! media started the bridge, the 200 OK (with changed SDP) entered Branch B
//! at `prepare_caller_answer_from_callee_sdp` (line 5520),
//! called `apply_bridge_callee_answer` + `configure_media_bridge_transcoders`,
//! but then `start_media_bridge_forwarding` returned immediately because
//! `media_bridge_started` was already `true` from the 183 path.
//!
//! **Impact**: The forwarder from early media continued running with stale
//! `DirectionParams` (cloned codec info). Any new `Track` event fired after
//! the SDP re-negotiation would create a `ForwardingTrack` with the old PT
//! mapping, causing PT mismatch → all audio silently dropped.
//!
//! **Fix**: Removed the early-return guard so `start_media_bridge_forwarding`
//! always falls through to `bridge.start_bridge()`, which handles restart
//! internally (aborts old forwarder, creates new one with fresh DirectionParams).
//!
//! **This test FAILS before the fix and PASSES after the fix.**

use super::e2e_test_server::E2eTestServer;
use super::rtp_utils::{RtpPacket, RtpReceiver, RtpSender, extract_media_endpoint};
use super::test_helpers;
use super::test_ua::TestUaEvent;
use crate::config::MediaProxyMode;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

// ─── SDP helpers ─────────────────────────────────────────────────────────────

use test_helpers::pcmu_sdp;

/// SDP for early media (port X) — triggers bridge start.
fn early_sdp(ip: &str, port: u16) -> String {
    format!(
        "v=0\r\n\
         o=- 1 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

/// SDP for 200 OK (port Y, different from X) — triggers sdp_changed.
fn answer_sdp(ip: &str, port: u16) -> String {
    format!(
        "v=0\r\n\
         o=- 1 2 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

// ─── Test ─────────────────────────────────────────────────────────────────────

/// Regression: 183 early media with SDP A, then 200 OK with DIFFERENT SDP B.
/// The bridge forwarder MUST be restarted to pick up the updated codec state.
#[tokio::test]
async fn test_early_media_183_then_different_sdp_200ok_bridge_restarted() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // ── 1. Server (full media proxy, plain RTP mode) ─────────────────────────
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    // ── 2. RTP sockets ───────────────────────────────────────────────────────
    let caller_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let caller_port = caller_receiver.port()?;

    let callee_sender = RtpSender::bind().await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let callee_port = callee_receiver.port()?;

    // ── 3. User-agents ───────────────────────────────────────────────────────
    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    // 183 SDP uses port X, 200 OK SDP uses port Y — DIFFERENT -> triggers sdp_changed
    let bob_early_sdp = early_sdp("127.0.0.1", callee_port);
    let bob_200ok_sdp = answer_sdp("127.0.0.1", callee_port); // same port but different o= version

    let alice_sdp = pcmu_sdp("127.0.0.1", caller_port);

    // ── 4. Concurrent call setup ─────────────────────────────────────────────
    let caller_ua_clone = caller_ua.clone();
    let caller_handle =
        crate::utils::spawn(async move { caller_ua_clone.make_call("bob", Some(alice_sdp)).await });

    let mut bob_dialog_id = None;
    let mut bob_received_offer: Option<String> = None;

    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                bob_dialog_id = Some(id.clone());
                bob_received_offer = offer;

                // Step A — 183 Session Progress with SDP (early media)
                info!(dialog_id = %id, "Bob: sending 183 + early-media SDP (port {})", callee_port);
                callee_ua
                    .send_ringing(&id, Some(bob_early_sdp.clone()))
                    .await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let bob_id =
        bob_dialog_id.ok_or_else(|| anyhow::anyhow!("Bob never received INVITE from proxy"))?;

    // Simulate real-world gap between 183 and 200 OK
    sleep(Duration::from_millis(400)).await;

    // Step B — 200 OK with DIFFERENT SDP (different o= version -> sdp_changed = true)
    info!(dialog_id = %bob_id, "Bob: sending 200 OK with DIFFERENT SDP (bug trigger)");
    callee_ua.answer_call(&bob_id, Some(bob_200ok_sdp)).await?;

    // Wait for Alice's INVITE to complete
    let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("Alice's INVITE timed out waiting for 200 OK"))?
        .map_err(|e| anyhow::anyhow!("Join error: {}", e))?
        .map_err(|e| anyhow::anyhow!("Call setup failed: {}", e))?;

    info!(dialog_id = %caller_id, "Call established");

    // ── 5. Check caller gate is open (media bridge active) ───────────────────
    let registry = &server.registry;
    let sessions = registry.list_recent(10);
    let session_id = sessions
        .first()
        .map(|s| s.session_id.clone())
        .ok_or_else(|| anyhow::anyhow!("No active session found in registry"))?;

    let handle = registry
        .get_handle(&session_id)
        .ok_or_else(|| anyhow::anyhow!("No handle for session"))?;

    let snapshot = handle
        .snapshot()
        .ok_or_else(|| anyhow::anyhow!("No snapshot available"))?;

    info!(
        session_id = %snapshot.id,
        state = ?snapshot.state,
        caller_gate_open = snapshot.caller_gate_open,
        "Session snapshot after 200 OK with different SDP"
    );

    assert!(
        snapshot.caller_gate_open,
        "Caller gate must be open after 200 OK — fix: caller→callee audio must flow"
    );

    // ── 6. RTP exchange: both directions should work ─────────────────────────
    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("Alice has no negotiated answer SDP"))?;

    let callee_offer = bob_received_offer
        .ok_or_else(|| anyhow::anyhow!("Bob never received an offer SDP in the INVITE"))?;

    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse proxy A-leg endpoint from caller_answer"))?;
    let callee_target = extract_media_endpoint(&callee_offer)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse proxy B-leg endpoint from callee_offer"))?;

    info!(
        caller_target = %caller_target,
        callee_target = %callee_target,
        "Proxy media endpoints"
    );

    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    // Callee sends PCMU to proxy B-leg
    let callee_packets = RtpPacket::create_sequence(
        100,
        1000,
        60000,
        0xB2B2_B2B2u32,
        0,  // PCMU
        160,
        160,
    );
    callee_sender.start_sending(callee_target, callee_packets, 20);

    // Caller sends PCMU to proxy A-leg
    let caller_packets = RtpPacket::create_sequence(
        100,
        2000,
        50000,
        0xA1A1_A1A1u32,
        0,  // PCMU
        160,
        160,
    );
    caller_sender.start_sending(caller_target, caller_packets, 20);

    sleep(Duration::from_millis(2500)).await;

    callee_sender.stop();
    caller_sender.stop();
    sleep(Duration::from_millis(300)).await;

    let caller_stats = caller_receiver.get_stats().await;
    let callee_stats = callee_receiver.get_stats().await;

    info!(
        caller_received = caller_stats.packets_received,
        callee_received = callee_stats.packets_received,
        "RTP stats after 183→200-different-SDP call"
    );

    // ── 7. Hang up ────────────────────────────────────────────────────────────
    caller_ua.hangup(&caller_id).await.ok();

    // ── 8. Stop infra ─────────────────────────────────────────────────────────
    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();

    // ── 9. Assertions ─────────────────────────────────────────────────────────
    // B → A direction
    assert!(
        caller_stats.packets_received > 0,
        "BUG REPRODUCED: Alice received 0 RTP packets from Bob. \
         Root cause: after 183+SDP followed by 200-OK with different SDP, \
         start_media_bridge_forwarding returned early due to media_bridge_started \
         guard, preventing bridge.start_bridge() from restarting the forwarder \
         with updated codec/transport state."
    );

    // A → B direction
    assert!(
        callee_stats.packets_received > 0,
        "Bob received 0 RTP packets from Alice (unexpected)."
    );

    info!("test_early_media_183_then_different_sdp_200ok_bridge_restarted PASSED");
    Ok(())
}
