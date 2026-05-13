//! Regression test: 183 early-media + identical-SDP 200 OK → silent B→A leg
//!
//! **Bug reference**: Production call `2ists8lo22030sqar0c6`
//! `rtp_to_webrtc_pps=0` for the entire 33-second call.
//!
//! **Root cause** (in `sip_session.rs::prepare_caller_answer_from_callee_sdp`):
//!
//! 1. B-leg (CCP/PSTN) sends **183 Session Progress + SDP**  
//!    → `rtp_pc.set_remote_description(sdp_183)` succeeds  
//!    → `rtp_pc` registers its RTP listen port as **P1** (e.g. 18368)
//!
//! 2. B-leg then sends **200 OK with the IDENTICAL SDP** (same `o=` session-id
//!    and session-version, same media port)  
//!    → code detects `rtp_pc.remote_description().is_some()` → enters re-negotiate path  
//!    → calls `rtp_pc.create_offer()` → rustrtc allocates a **new** listen port **P2**  
//!    → `rtp_pc.set_local_description(new_offer)` → port P1 is no longer active
//!
//! 3. The proxy already sent its INVITE to B-leg with P1 in the `m=audio` line.
//!    B-leg keeps sending RTP to P1.  P2 receives nothing.
//!    SSRC latching on P2 never fires → `PeerConnectionEvent::Track` never emitted  
//!    → `spawn_bidirectional_forwarder` never starts the B→A forwarder  
//!    → **`rtp_to_webrtc_pps = 0` throughout the entire call**
//!
//! **Production log details (B-leg)**:
//! ```
//! 183 SDP:    o=- 1778630403 1778630404 IN IP4 58.246.19.74
//!             m=audio 10832 RTP/AVP 8 126
//! 200 OK SDP: o=- 1778630403 1778630404 IN IP4 58.246.19.74  ← SAME version
//!             m=audio 10832 RTP/AVP 8 126                    ← SAME port
//! Bridge offer to B-leg: m=audio 18368 RTP/AVP ...          ← proxy's P1
//! After re-negotiate: new port allocated, B-leg still sends to 18368
//! ```
//!
//! **This test FAILS before the fix and PASSES after the fix.**

use super::e2e_test_server::E2eTestServer;
use super::rtp_utils::{RtpPacket, RtpReceiver, RtpSender, RtpStats, extract_media_endpoint};
use super::test_ua::TestUaEvent;
use crate::config::MediaProxyMode;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

// ─── SDP helpers ─────────────────────────────────────────────────────────────

/// PCMA SDP with a **fixed** `o=` session-id / session-version.
///
/// Using the exact values from the production CCP-server SDPs so that
/// the 183 and 200 OK carry bit-for-bit identical session lines — which
/// is precisely what triggers the re-negotiate bug.
fn pcma_sdp_fixed(ip: &str, port: u16) -> String {
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

/// Plain PCMU SDP for the A-leg (caller).
fn pcmu_sdp(ip: &str, port: u16) -> String {
    let sid = chrono::Utc::now().timestamp();
    format!(
        "v=0\r\n\
         o=- {sid} {sid} IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 8 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

// ─── Test ─────────────────────────────────────────────────────────────────────

/// **Regression test** — B→A leg is silent when callee sends 183 + SDP followed
/// by 200 OK with the *identical* SDP (same `o=` session-version, same port).
///
/// The test asserts that after call establishment the callee→caller RTP direction
/// actually delivers packets.  With the bug this assertion fails (0 packets).
#[tokio::test]
async fn test_early_media_183_then_same_sdp_200ok_rtp_flow() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // ── 1. Server (full media proxy, plain RTP mode) ─────────────────────────
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    // ── 2. RTP sockets ───────────────────────────────────────────────────────
    // caller (alice) — only needs a receiver for the B→A direction
    let caller_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let caller_port = caller_receiver.port()?;

    // callee (bob) — only needs a sender for the B→A direction
    let callee_sender = RtpSender::bind().await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let callee_port = callee_receiver.port()?;

    // ── 3. User-agents ───────────────────────────────────────────────────────
    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    // Fixed PCMA SDP that bob will use for BOTH 183 and 200 OK.
    // Identical o= session-version is the key that triggers the bug.
    let bob_early_sdp = pcma_sdp_fixed("127.0.0.1", callee_port);
    let bob_200ok_sdp = bob_early_sdp.clone(); // ← intentionally identical

    // Alice's INVITE offer to the proxy
    let alice_sdp = pcmu_sdp("127.0.0.1", caller_port);

    // ── 4. Concurrent call setup ─────────────────────────────────────────────
    // Alice calls bob — `make_call` blocks until it receives a final response.
    let caller_ua_clone = caller_ua.clone();
    let caller_handle =
        tokio::spawn(async move { caller_ua_clone.make_call("bob", Some(alice_sdp)).await });

    // Bob's side: capture IncomingCall, then send 183 + SDP, wait, then 200 OK.
    let mut bob_dialog_id = None;
    let mut bob_received_offer: Option<String> = None;

    // Wait up to 5 s for the INVITE to arrive at bob
    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                bob_dialog_id = Some(id.clone());
                bob_received_offer = offer; // proxy's INVITE SDP (contains P1 port)

                // Step A — 183 Session Progress with SDP (early media)
                info!(dialog_id = %id, "Bob: sending 183 + early-media SDP");
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

    // Simulate the CCP-server inter-packet gap between 183 and 200 OK
    sleep(Duration::from_millis(400)).await;

    // Step B — 200 OK with the SAME SDP (same session-version → bug trigger)
    info!(dialog_id = %bob_id, "Bob: sending 200 OK with SAME SDP (bug trigger)");
    callee_ua.answer_call(&bob_id, Some(bob_200ok_sdp)).await?;

    // Wait for Alice's INVITE to complete
    let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("Alice's INVITE timed out waiting for 200 OK"))?
        .map_err(|e| anyhow::anyhow!("Join error: {}", e))?
        .map_err(|e| anyhow::anyhow!("Call setup failed: {}", e))?;

    info!(dialog_id = %caller_id, "Call established");

    // ── 5. Resolve proxy media endpoints ─────────────────────────────────────
    // caller_answer = the SDP the proxy returned to Alice (proxy's A-leg port)
    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("Alice has no negotiated answer SDP"))?;

    // callee_offer = the SDP the proxy sent to Bob in its outgoing INVITE
    // This is the port (P1) the proxy originally advertised to Bob.
    // After the bug Bob should still be sending RTP here.
    let callee_offer = bob_received_offer
        .ok_or_else(|| anyhow::anyhow!("Bob never received an offer SDP in the INVITE"))?;

    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse proxy A-leg endpoint from caller_answer"))?;
    let callee_target = extract_media_endpoint(&callee_offer).ok_or_else(|| {
        anyhow::anyhow!("Cannot parse proxy B-leg endpoint from callee_offer (P1)")
    })?;

    info!(
        caller_target = %caller_target,
        callee_target = %callee_target,
        "Proxy media endpoints"
    );

    // ── 6. RTP exchange: focus on B→A direction (the broken one) ─────────────
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    // Bob sends RTP to callee_target (proxy B-leg port P1, the original INVITE port).
    // With the bug, the proxy is now listening on P2, so P1 gets no forwarder.
    let callee_packets = RtpPacket::create_sequence(
        100,            // 100 packets × 20 ms = 2 s of audio
        1000,           // seq start
        60000,          // timestamp start
        0xB2B2_B2B2u32, // SSRC
        8,              // PCMA payload type
        160,            // payload size
        160,            // timestamp increment
    );
    callee_sender.start_sending(callee_target, callee_packets, 20);

    // Alice also sends to the proxy A-leg so the forwarder in the other
    // direction stays alive (not strictly needed for the assertion but
    // mirrors a real call more closely).
    let caller_packets = RtpPacket::create_sequence(
        100,
        2000,
        50000,
        0xA1A1_A1A1u32,
        0, // PCMU payload type
        160,
        160,
    );
    caller_sender.start_sending(caller_target, caller_packets, 20);

    // Wait for the full 2-second window plus some margin
    sleep(Duration::from_millis(2500)).await;

    callee_sender.stop();
    caller_sender.stop();
    sleep(Duration::from_millis(300)).await;

    let caller_stats = caller_receiver.get_stats().await;
    let callee_stats = callee_receiver.get_stats().await;

    info!(
        caller_received = caller_stats.packets_received,
        callee_received = callee_stats.packets_received,
        "RTP stats after 183→200OK-same-SDP call"
    );

    // ── 7. Hang up ────────────────────────────────────────────────────────────
    caller_ua.hangup(&caller_id).await.ok();

    // ── 8. Stop infra ─────────────────────────────────────────────────────────
    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();

    // ── 9. Assertions ─────────────────────────────────────────────────────────
    // B → A direction: Bob's RTP must reach Alice through the proxy.
    // This is the direction that is BROKEN by the bug
    // (proxy re-creates rtp_pc with a new port, SSRC-latch never fires).
    assert!(
        caller_stats.packets_received > 0,
        "BUG REPRODUCED: Alice received 0 RTP packets from Bob (rtp_to_webrtc=0). \
         Root cause: after 183+SDP followed by 200-OK with identical SDP, \
         rtp_pc.create_offer() allocates a new port, but Bob still sends RTP to \
         the original INVITE port (P1). SSRC latching never triggers on the new port, \
         so the B→A forwarder never starts."
    );

    // A → B direction: should be working even with the bug
    assert!(
        callee_stats.packets_received > 0,
        "Alice→Bob direction also broken (unexpected). Got 0 packets."
    );

    info!("test_early_media_183_then_same_sdp_200ok_rtp_flow PASSED");
    Ok(())
}
