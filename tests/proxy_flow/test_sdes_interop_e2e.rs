use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_ua::{ObservedRtp, TestUa, TestUaEvent};

/// Serialize the scenarios: each spins a full SIP server + two media UAs and
/// debug builds are slow enough that concurrent runs trip the INVITE timeout.
static SCENARIO_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn run_with_big_stack<F>(fut: F)
where
    F: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_stack_size(32 * 1024 * 1024)
        .build()
        .expect("failed to build test runtime");
    rt.block_on(async move {
        tokio::spawn(fut)
            .await
            .expect("test task panicked")
            .expect("e2e sdes test failed");
    });
}

/// Create an SDES-SRTP (RFC 4568) UA with a real rustrtc PeerConnection:
/// offers/answers `RTP/SAVP` + `a=crypto`, no ICE/DTLS attributes.
async fn create_sdes_ua(
    server: &E2eTestServer,
    username: &str,
    password: &str,
) -> Result<Arc<TestUa>> {
    let config = crate::common::test_ua::TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: password.to_string(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27100),
        proxy_addr: server.proxy_addr,
    };
    let mut ua = TestUa::new_srtp_with_caps(
        config,
        vec![
            rustrtc::config::AudioCapability::pcmu(),
            rustrtc::config::AudioCapability::telephone_event(),
        ],
    );
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(100)).await;
    Ok(Arc::new(ua))
}

/// Drive alice (SDES caller) → bob (SDES callee, answers via its
/// PeerConnection). Returns (alice dialog id, offer SDP as seen by bob).
async fn establish_sdes_call(
    alice: Arc<TestUa>,
    bob: Arc<TestUa>,
) -> Result<(rsipstack::dialog::DialogId, String)> {
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call("bob", None).await }
    });

    let mut bob_dialog_id = None;
    let mut received_sdp = None;
    for _ in 0..600 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                received_sdp = sdp;
                bob.answer_call(&id, None).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(
        bob_dialog_id.is_some(),
        "Bob should receive the incoming call"
    );

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(20), caller_handle).await
    {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("alice call failed: {}", e)),
        _ => return Err(anyhow::anyhow!("call setup timeout (SDES negotiation)")),
    };
    Ok((alice_dialog_id, received_sdp.unwrap_or_default()))
}

fn count_payload_matches(
    received: &[ObservedRtp],
    pt: u8,
    sent: &[Vec<u8>],
) -> (usize, usize) {
    let sent_set: std::collections::HashSet<&Vec<u8>> = sent.iter().collect();
    let mut matched = 0;
    let mut distinct = std::collections::HashSet::new();
    for packet in received.iter().filter(|p| p.payload_type == pt) {
        if sent_set.contains(&packet.payload) {
            matched += 1;
            distinct.insert(packet.sequence_number);
        }
    }
    (matched, distinct.len())
}

fn patterned_pcmu_payloads(count: usize, base: u8) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| vec![base.wrapping_add((i % 64) as u8); 160])
        .collect()
}

/// Poll `packets` until `min` PT-matching content-identical packets show up
/// (relay arming + DTLS handshakes take variable time in debug builds), or
/// the deadline expires.
async fn wait_for_matches<F>(
    mut packets: F,
    pt: u8,
    sent: &[Vec<u8>],
    min: usize,
    deadline: Duration,
) -> (usize, usize)
where
    F: FnMut() -> Vec<ObservedRtp>,
{
    let start = std::time::Instant::now();
    loop {
        let (matched, distinct) = count_payload_matches(&packets(), pt, sent);
        if matched >= min || start.elapsed() >= deadline {
            return (matched, distinct);
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Regression test for rustpbx issue #281: two SDES-SRTP endpoints calling
/// through the proxy must get end-to-end SRTP media relayed by the PBX.
///
/// This reproduces the Groundwire ↔ Groundwire scenario exactly:
///   1. The caller offers `RTP/SAVP` + `a=crypto` with MKI `|2^31|1:1`.
///   2. The PBX caller leg must answer with `RTP/SAVP` + `a=crypto` (echoing
///      the MKI params) — not downgrade to plain `RTP/AVP` (issue #281).
///   3. The PBX callee leg must forward an `RTP/SAVP` + `a=crypto` offer.
///   4. Media must flow both ways: the PBX unprotects one leg's SRTP and
///      re-protects with the other leg's key — including the MKI framing
///      that previously failed authentication on every packet.
#[test]
fn test_sdes_to_sdes_call_bridges_srtp() {
    run_with_big_stack(test_sdes_to_sdes_call_bridges_srtp_impl());
}

async fn test_sdes_to_sdes_call_bridges_srtp_impl() -> Result<()> {
    let _guard = SCENARIO_LOCK.lock().await;
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = create_sdes_ua(&server, "alice", "password123").await?;
    let bob = create_sdes_ua(&server, "bob", "password456").await?;
    sleep(Duration::from_millis(100)).await;

    let (alice_dialog_id, offer_to_bob) = establish_sdes_call(alice.clone(), bob.clone()).await?;

    // The offer the PBX forwards to the callee must keep the SDES profile
    // ("secure in → secure out") and carry a crypto attribute.
    assert!(
        offer_to_bob.contains("m=audio") && offer_to_bob.contains("RTP/SAVP"),
        "offer forwarded to callee must use RTP/SAVP:\n{offer_to_bob}"
    );
    assert!(
        offer_to_bob.contains("a=crypto:"),
        "offer forwarded to callee must carry a=crypto:\n{offer_to_bob}"
    );

    // The answer alice received must keep the SDES profile and echo the MKI
    // params alice offered (RFC 4568 §7.1.2) — a plain `RTP/AVP` downgrade
    // makes strict SRTP clients refuse to flow media (issue #281).
    let answer_to_alice = alice
        .get_negotiated_answer_sdp(&alice_dialog_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("no negotiated answer on alice"))?;
    assert!(
        answer_to_alice.contains("RTP/SAVP"),
        "answer to caller must use RTP/SAVP:\n{answer_to_alice}"
    );
    assert!(
        answer_to_alice.contains("a=crypto:"),
        "answer to caller must carry a=crypto:\n{answer_to_alice}"
    );
    // MKI is intentionally never advertised back (or offered) — deployed
    // peers advertise `|1:1` without implementing it (issue #281).
    assert!(
        answer_to_alice.contains("|2^31") && !answer_to_alice.contains("1:1"),
        "answer must keep the lifetime but no MKI:\n{answer_to_alice}"
    );

    // Both legs must have SDES keying ready (transport armed).
    alice
        .wait_webrtc_connected(Duration::from_secs(10))
        .await?;
    bob.wait_webrtc_connected(Duration::from_secs(10)).await?;
    alice.attach_webrtc_rx_tap().await?;
    bob.attach_webrtc_rx_tap().await?;

    // ── Direction A → B: caller SRTP → PBX relay → callee SRTP ──
    let a_ssrc = alice.webrtc_sender_ssrc();
    let a_sent = patterned_pcmu_payloads(150, 0x30);
    for (i, payload) in a_sent.iter().enumerate() {
        alice
            .send_webrtc_rtp(
                0,
                3000u16.wrapping_add(i as u16),
                60000u32 + (i as u32) * 160,
                a_ssrc,
                false,
                payload.clone(),
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }

    let (bob_matched, bob_distinct) = wait_for_matches(
        || bob.webrtc_rx_packets(),
        0,
        &a_sent,
        40,
        Duration::from_secs(10),
    )
    .await;
    let bob_received = bob.webrtc_rx_packets();
    assert!(
        bob_matched >= 40,
        "SDES→SDES relay: bob should receive ≥40 content-identical PCMU packets, \
         got {} matched of {} received",
        bob_matched,
        bob_received.len()
    );
    assert!(bob_distinct >= 20, "matched payloads should span many seqs");

    // ── Direction B → A: callee SRTP → PBX relay → caller SRTP ──
    let b_ssrc = bob.webrtc_sender_ssrc();
    let b_sent = patterned_pcmu_payloads(150, 0x90);
    for (i, payload) in b_sent.iter().enumerate() {
        bob.send_webrtc_rtp(
            0,
            7000u16.wrapping_add(i as u16),
            90000u32 + (i as u32) * 160,
            b_ssrc,
            false,
            payload.clone(),
        )
        .await?;
        sleep(Duration::from_millis(20)).await;
    }

    let (alice_matched, alice_distinct) = wait_for_matches(
        || alice.webrtc_rx_packets(),
        0,
        &b_sent,
        40,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        alice_matched >= 40,
        "SDES→SDES relay: alice should receive ≥40 content-identical PCMU packets, \
         got {} matched of {} received",
        alice_matched,
        alice.webrtc_rx_packets().len()
    );
    assert!(
        alice_distinct >= 20,
        "matched payloads should span many seqs"
    );

    alice.hangup(&alice_dialog_id).await.ok();
    server.stop();
    Ok(())
}

/// SDES caller → WebRTC (DTLS-SRTP) callee: anchored media must interwork
/// SDES on the caller leg with DTLS-SRTP on the callee leg. The caller's
/// answer must stay `RTP/SAVP` + `a=crypto` (issue #281) while the callee
/// gets a WebRTC offer.
#[test]
fn test_sdes_caller_to_webrtc_callee_mixed_bridge() {
    run_with_big_stack(test_sdes_caller_to_webrtc_callee_mixed_bridge_impl());
}

async fn create_webrtc_ua(
    server: &E2eTestServer,
    username: &str,
    password: &str,
) -> Result<Arc<TestUa>> {
    let config = crate::common::test_ua::TestUaConfig {
        webrtc: true,
        username: username.to_string(),
        password: password.to_string(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27300),
        proxy_addr: server.proxy_addr,
    };
    let mut ua = TestUa::new_webrtc_with_caps(
        config,
        vec![
            rustrtc::config::AudioCapability::pcmu(),
            rustrtc::config::AudioCapability::telephone_event(),
        ],
    );
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(100)).await;
    Ok(Arc::new(ua))
}

async fn test_sdes_caller_to_webrtc_callee_mixed_bridge_impl() -> Result<()> {
    let _guard = SCENARIO_LOCK.lock().await;
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = create_sdes_ua(&server, "alice", "password123").await?;
    let bob = create_webrtc_ua(&server, "charlie", "password789").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call("charlie", None).await }
    });
    let mut bob_dialog_id = None;
    let mut offer_to_bob = None;
    for _ in 0..600 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                offer_to_bob = sdp;
                bob.answer_call(&id, None).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(bob_dialog_id.is_some(), "bob should receive the call");
    let offer_to_bob = offer_to_bob.unwrap_or_default();
    // The WebRTC callee must receive a WebRTC offer (DTLS), not SDES.
    assert!(
        offer_to_bob.contains("UDP/TLS/RTP/SAVPF") && offer_to_bob.contains("a=fingerprint"),
        "offer to WebRTC callee must be DTLS-SRTP:\n{offer_to_bob}"
    );

    let alice_dialog_id =
        match tokio::time::timeout(Duration::from_secs(20), caller_handle).await {
            Ok(Ok(Ok(id))) => id,
            Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("alice call failed: {}", e)),
            _ => return Err(anyhow::anyhow!("call setup timeout")),
        };

    // Alice's answer must still be SAVP + crypto (caller leg stays SDES).
    let answer_to_alice = alice
        .get_negotiated_answer_sdp(&alice_dialog_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("no negotiated answer on alice"))?;
    assert!(
        answer_to_alice.contains("RTP/SAVP") && answer_to_alice.contains("a=crypto:"),
        "answer to SDES caller must keep RTP/SAVP + a=crypto:\n{answer_to_alice}"
    );

    alice
        .wait_webrtc_connected(Duration::from_secs(15))
        .await?;
    bob.wait_webrtc_connected(Duration::from_secs(15)).await?;
    bob.attach_webrtc_rx_tap().await?;

    // SDES caller → PBX → DTLS-SRTP callee: payload must arrive intact.
    // The mixed-transport relay occasionally needs a fresh packet burst to
    // latch the SDES leg's source address, so retry with new bursts.
    let a_ssrc = alice.webrtc_sender_ssrc();
    let a_sent = patterned_pcmu_payloads(150, 0x55);
    let mut bob_matched = 0;
    for burst in 0..3u16 {
        let offset = burst * 150;
        for (i, payload) in a_sent.iter().enumerate() {
            alice
                .send_webrtc_rtp(
                    0,
                    4000u16.wrapping_add(offset + i as u16),
                    70000u32 + ((offset + i as u16) as u32) * 160,
                    a_ssrc,
                    false,
                    payload.clone(),
                )
                .await?;
            sleep(Duration::from_millis(20)).await;
        }
        let (matched, _) = wait_for_matches(
            || bob.webrtc_rx_packets(),
            0,
            &a_sent,
            40,
            Duration::from_secs(5),
        )
        .await;
        bob_matched = matched;
        if bob_matched >= 40 {
            break;
        }
    }
    assert!(
        bob_matched >= 40,
        "SDES→DTLS relay: bob should receive ≥40 content-identical PCMU packets, \
         got {} matched of {} received",
        bob_matched,
        bob.webrtc_rx_packets().len()
    );

    alice.hangup(&alice_dialog_id).await.ok();
    server.stop();
    Ok(())
}

/// NAT simulation (rustpbx issue #281 topology): the SDES caller's SDP
/// advertises an unreachable `c=` address (what a CGNAT'd softphone sends),
/// so the PBX can only deliver audio back by symmetric-RTP latching —
/// following the observed SRTP packet source. Asserts both directions carry
/// content-identical media: alice→bob directly, and bob→alice ONLY if the
/// caller leg latched onto alice's real source address.
#[test]
fn test_sdes_nat_latching_call() {
    run_with_big_stack(test_sdes_nat_latching_call_impl());
}

async fn test_sdes_nat_latching_call_impl() -> Result<()> {
    let _guard = SCENARIO_LOCK.lock().await;
    let _ = tracing_subscriber::fmt::try_init();
    eprintln!("[nat] starting server");
    let server = Arc::new(
        E2eTestServer::start_with_mode_and_latching(MediaProxyMode::All, true).await?,
    );
    eprintln!("[nat] server up; creating alices");
    let alice = create_sdes_ua(&server, "alice", "password123").await?;
    let bob = create_sdes_ua(&server, "bob", "password456").await?;
    sleep(Duration::from_millis(100)).await;
    eprintln!("[nat] uas registered; dialing with spoofed c-line");

    // Advertise 192.0.2.1 (TEST-NET-1, unroutable) as alice's media address.
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call_spoofed_cline("bob", "192.0.2.1").await }
    });

    // Drive bob's answer (real SDES answer from his PeerConnection).
    let mut bob_dialog_id = None;
    for _ in 0..600 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _sdp) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, None).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(bob_dialog_id.is_some(), "bob should receive the call");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(20), caller_handle).await
    {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("alice call failed: {}", e)),
        _ => return Err(anyhow::anyhow!("call setup timeout (SDES NAT sim)")),
    };
    eprintln!("[nat] call established");

    alice
        .wait_webrtc_connected(Duration::from_secs(10))
        .await?;
    bob.wait_webrtc_connected(Duration::from_secs(10)).await?;
    eprintln!("[nat] both legs connected");
    alice.attach_webrtc_rx_tap().await?;
    bob.attach_webrtc_rx_tap().await?;
    eprintln!("[nat] taps attached");

    // alice → PBX → bob: alice's SRTP arrives at the PBX from her REAL
    // address, arming the caller leg's latch.
    let a_ssrc = alice.webrtc_sender_ssrc();
    let a_sent = patterned_pcmu_payloads(150, 0x30);
    for (i, payload) in a_sent.iter().enumerate() {
        alice
            .send_webrtc_rtp(
                0,
                3000u16.wrapping_add(i as u16),
                60000u32 + (i as u32) * 160,
                a_ssrc,
                false,
                payload.clone(),
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }
    let (bob_matched, _) = wait_for_matches(
        || bob.webrtc_rx_packets(),
        0,
        &a_sent,
        40,
        Duration::from_secs(10),
    )
    .await;
    eprintln!("[nat] a→b matched={bob_matched}");
    assert!(
        bob_matched >= 40,
        "NAT sim: bob should receive ≥40 content-identical PCMU packets, \
         got {} matched of {} received",
        bob_matched,
        bob.webrtc_rx_packets().len()
    );

    // bob → PBX → alice: this direction only works if the PBX caller leg
    // latched onto alice's real source address instead of the spoofed
    // 192.0.2.1 c= line.
    let b_ssrc = bob.webrtc_sender_ssrc();
    let b_sent = patterned_pcmu_payloads(150, 0x90);
    for (i, payload) in b_sent.iter().enumerate() {
        bob.send_webrtc_rtp(
            0,
            7000u16.wrapping_add(i as u16),
            90000u32 + (i as u32) * 160,
            b_ssrc,
            false,
            payload.clone(),
        )
        .await?;
        sleep(Duration::from_millis(20)).await;
    }
    let (alice_matched, _) = wait_for_matches(
        || alice.webrtc_rx_packets(),
        0,
        &b_sent,
        40,
        Duration::from_secs(10),
    )
    .await;
    eprintln!("[nat] b→a matched={alice_matched}");
    assert!(
        alice_matched >= 40,
        "NAT sim: alice should receive ≥40 content-identical PCMU packets via \
         symmetric-RTP latching, got {} matched of {} received",
        alice_matched,
        alice.webrtc_rx_packets().len()
    );

    alice.hangup(&alice_dialog_id).await.ok();
    server.stop();
    Ok(())
}
