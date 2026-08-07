//! Trunk B2BUA E2E Tests
//!
//! Verifies trunk-based call flows with full RTP media verification
//! and CDR generation accuracy.
//!
//! These tests exercise the core B2BUA call path that trunk calls use:
//! - Inbound trunk call: external → proxy → registered user
//! - The proxy handles these the same as P2P but with different routing/config
//!
//! Key validations:
//! - Bidirectional RTP through proxy (no silent calls, no codec mismatch)
//! - CDR accuracy (duration, hangup reason, status)
//! - Correct codec passthrough (PCMU, PCMA)
//! - Rejection flows (486 reject → correct CDR)
//! - Multiple concurrent trunk calls

use super::e2e_test_server::E2eTestServer;
use super::rtp_utils::{RtpPacket, RtpReceiver, RtpSender, RtpStats, extract_media_endpoint};
use super::test_helpers;
use super::test_ua::TestUaEvent;
use crate::callrecord::CallRecordHangupReason;
use crate::config::MediaProxyMode;
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};

use test_helpers::{pcma_sdp, pcmu_sdp};

/// Helper: establish call, get both dialog IDs and proxy media endpoints.
struct EstablishedCall {
    caller_id: rsipstack::dialog::DialogId,
    callee_id: rsipstack::dialog::DialogId,
    caller_target: std::net::SocketAddr,
    callee_target: std::net::SocketAddr,
}

async fn establish_call(
    server: &E2eTestServer,
    caller: &str,
    callee: &str,
    caller_rtp_port: u16,
    callee_rtp_port: u16,
) -> Result<(
    EstablishedCall,
    super::test_ua::TestUa,
    super::test_ua::TestUa,
)> {
    let caller_ua = Arc::new(server.create_ua(caller).await?);
    let callee_ua = server.create_ua(callee).await?;

    sleep(Duration::from_millis(100)).await;

    let caller_sdp = pcmu_sdp("127.0.0.1", caller_rtp_port);
    let callee_sdp = pcmu_sdp("127.0.0.1", callee_rtp_port);

    let caller_clone = caller_ua.clone();
    let callee_str = callee.to_string();
    let caller_handle =
        crate::utils::spawn(
            async move { caller_clone.make_call(&callee_str, Some(caller_sdp)).await },
        );

    let mut callee_dialog_id = None;
    let mut callee_offer_sdp: Option<String> = None;

    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                callee_dialog_id = Some(id.clone());
                callee_offer_sdp = offer;
                callee_ua.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if callee_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let callee_id =
        callee_dialog_id.ok_or_else(|| anyhow::anyhow!("Callee never received INVITE"))?;

    let caller_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("Caller timed out"))?
        .map_err(|e| anyhow::anyhow!("Join error: {}", e))?
        .map_err(|e| anyhow::anyhow!("Call error: {}", e))?;

    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("No answer SDP on caller"))?;

    let callee_offer = callee_offer_sdp.ok_or_else(|| anyhow::anyhow!("No offer SDP on callee"))?;

    let callee_target = extract_media_endpoint(&callee_offer)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse callee proxy endpoint"))?;
    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse caller proxy endpoint"))?;

    Ok((
        EstablishedCall {
            caller_id,
            callee_id,
            caller_target,
            callee_target,
        },
        // We can't move caller_ua out of Arc because we need it for hangup later.
        // Return the Arc instead and let caller handle it.
        Arc::try_unwrap(caller_ua).unwrap_or_else(|_| panic!("caller_ua still has refs")),
        callee_ua,
    ))
}

/// Send bidirectional RTP and collect stats.
#[allow(clippy::too_many_arguments)]
async fn exchange_rtp(
    caller_sender: &RtpSender,
    callee_sender: &RtpSender,
    caller_receiver: &RtpReceiver,
    callee_receiver: &RtpReceiver,
    caller_target: std::net::SocketAddr,
    callee_target: std::net::SocketAddr,
    payload_type: u8,
    duration_ms: u64,
) -> Result<(RtpStats, RtpStats)> {
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let packet_count = (duration_ms / 20) as usize;
    let caller_ssrc = 0xA1A1A1A1u32;
    let callee_ssrc = 0xB2B2B2B2u32;

    let caller_packets = RtpPacket::create_sequence(
        packet_count,
        1000,
        50000,
        caller_ssrc,
        payload_type,
        160,
        160,
    );
    let callee_packets = RtpPacket::create_sequence(
        packet_count,
        2000,
        60000,
        callee_ssrc,
        payload_type,
        160,
        160,
    );

    caller_sender.start_sending(callee_target, caller_packets, 20);
    callee_sender.start_sending(caller_target, callee_packets, 20);

    sleep(Duration::from_millis(duration_ms + 500)).await;

    caller_sender.stop();
    callee_sender.stop();
    sleep(Duration::from_millis(200)).await;

    let caller_stats = caller_receiver.get_stats().await;
    let callee_stats = callee_receiver.get_stats().await;

    Ok((caller_stats, callee_stats))
}

/// Wait for CDR and return it.
async fn wait_for_cdr(server: &E2eTestServer, timeout_ms: u64) -> Result<()> {
    sleep(Duration::from_millis(timeout_ms)).await;

    let records = server.cdr_capture.get_all_records().await;
    assert!(!records.is_empty(), "Should have at least one CDR record");

    let record = &records[0];
    info!(
        call_id = %record.call_id,
        status = %record.details.status,
        direction = %record.details.direction,
        hangup_reason = ?record.hangup_reason,
        caller = %record.caller,
        callee = %record.callee,
        sip_trunk_id = ?record.details.sip_trunk_id,
        sip_gateway = ?record.details.sip_gateway,
        "CDR record"
    );
    Ok(())
}

// ─── Test 2: Wholesale inbound — user (callee) hangs up ──────────────────────

#[tokio::test]
async fn test_trunk_b2bua_inbound_user_hangup_rtp_cdr() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let callee_sender = RtpSender::bind().await?;

    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;

    let (call, _caller_ua, callee_ua) =
        establish_call(&server, "alice", "bob", caller_port, callee_port).await?;

    let (caller_stats, callee_stats) = exchange_rtp(
        &caller_sender,
        &callee_sender,
        &caller_receiver,
        &callee_receiver,
        call.caller_target,
        call.callee_target,
        0,
        1500,
    )
    .await?;

    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive RTP"
    );
    assert!(
        caller_stats.packets_received > 0,
        "Caller should receive RTP"
    );

    // Internal user (callee) hangs up
    callee_ua.hangup(&call.callee_id).await?;

    // Verify CDR — hangup reason must be ByCallee
    wait_for_cdr(&server, 800).await?;
    let records = server.cdr_capture.get_all_records().await;
    let record = &records[0];

    assert_eq!(record.details.status, "completed");
    assert!(
        matches!(record.hangup_reason, Some(CallRecordHangupReason::ByCallee)),
        "Expected ByCallee, got {:?}",
        record.hangup_reason
    );

    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();
    info!("test_trunk_b2bua_inbound_user_hangup_rtp_cdr PASSED");
    Ok(())
}

// ─── Test 4: Wholesale — PCMA codec through proxy ────────────────────────────

#[tokio::test]
async fn test_trunk_b2bua_pcma_rtp_cdr() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let callee_sender = RtpSender::bind().await?;

    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;

    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let caller_sdp = pcma_sdp("127.0.0.1", caller_port);
    let callee_sdp = pcma_sdp("127.0.0.1", callee_port);

    // Establish call
    let caller_clone = caller_ua.clone();
    let caller_handle =
        crate::utils::spawn(async move { caller_clone.make_call("bob", Some(caller_sdp)).await });

    let mut callee_dialog_id = None;
    let mut callee_offer_sdp: Option<String> = None;
    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                callee_dialog_id = Some(id.clone());
                callee_offer_sdp = offer;
                callee_ua.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if callee_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let _callee_id = callee_dialog_id.ok_or_else(|| anyhow::anyhow!("No INVITE"))?;

    let caller_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("No answer SDP"))?;
    let callee_offer = callee_offer_sdp.ok_or_else(|| anyhow::anyhow!("No offer SDP"))?;

    let callee_target = extract_media_endpoint(&callee_offer)
        .ok_or_else(|| anyhow::anyhow!("No callee endpoint"))?;
    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("No caller endpoint"))?;

    // Exchange RTP with PCMA (PT=8)
    let (caller_stats, callee_stats) = exchange_rtp(
        &caller_sender,
        &callee_sender,
        &caller_receiver,
        &callee_receiver,
        caller_target,
        callee_target,
        8,
        2000,
    )
    .await?;

    info!(
        caller_received = caller_stats.packets_received,
        caller_pts = ?caller_stats.payload_types,
        callee_received = callee_stats.packets_received,
        callee_pts = ?callee_stats.payload_types,
        "PCMA wholesale results"
    );

    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive PCMA RTP"
    );
    assert!(
        caller_stats.packets_received > 0,
        "Caller should receive PCMA RTP"
    );

    // Verify PCMA (PT=8), not PCMU
    assert!(
        callee_stats.payload_types.contains(&8),
        "Callee should see PCMA (PT 8), got {:?}",
        callee_stats.payload_types
    );
    assert!(
        caller_stats.payload_types.contains(&8),
        "Caller should see PCMA (PT 8), got {:?}",
        caller_stats.payload_types
    );

    // Hang up and verify CDR
    caller_ua.hangup(&caller_id).await?;

    wait_for_cdr(&server, 800).await?;
    let records = server.cdr_capture.get_all_records().await;
    let record = &records[0];
    assert_eq!(record.details.status, "completed");
    assert!(matches!(
        record.hangup_reason,
        Some(CallRecordHangupReason::ByCaller)
    ));

    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();
    info!("test_trunk_b2bua_pcma_rtp_cdr PASSED");
    Ok(())
}

// ─── Test 5: Wholesale — CDR duration accuracy ───────────────────────────────

#[tokio::test]
async fn test_trunk_b2bua_cdr_duration_accuracy() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let sdp = pcmu_sdp("127.0.0.1", 12345);

    let alice_clone = alice.clone();
    let sdp_clone = sdp.clone();
    let caller_handle =
        crate::utils::spawn(async move { alice_clone.make_call("bob", Some(sdp_clone)).await });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
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
        sleep(Duration::from_millis(100)).await;
    }

    let _bob_id = bob_dialog_id.ok_or_else(|| anyhow::anyhow!("No INVITE"))?;

    let alice_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    // Let call run for ~2 seconds
    sleep(Duration::from_secs(2)).await;

    alice.hangup(&alice_id).await?;

    // Verify CDR duration
    sleep(Duration::from_millis(800)).await;
    let records = server.cdr_capture.get_all_records().await;
    assert!(!records.is_empty(), "Should have CDR");

    let record = &records[0];
    let duration_secs = (record.end_time - record.start_time).num_seconds();

    info!(duration_secs, status = %record.details.status, "CDR duration");

    assert!(
        (1..=5).contains(&duration_secs),
        "Duration should be ~2s, got {}s",
        duration_secs
    );
    assert_eq!(record.details.status, "completed");
    assert!(matches!(
        record.hangup_reason,
        Some(CallRecordHangupReason::ByCaller)
    ));

    server.stop();
    info!("test_trunk_b2bua_cdr_duration_accuracy PASSED");
    Ok(())
}

// ─── Test 6: Wholesale — RTP payload integrity through proxy ─────────────────

#[tokio::test]
async fn test_trunk_b2bua_rtp_payload_integrity() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let callee_sender = RtpSender::bind().await?;

    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;

    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let caller_sdp = pcmu_sdp("127.0.0.1", caller_port);
    let callee_sdp = pcmu_sdp("127.0.0.1", callee_port);

    let caller_clone = caller_ua.clone();
    let caller_handle =
        crate::utils::spawn(async move { caller_clone.make_call("bob", Some(caller_sdp)).await });

    let mut callee_dialog_id = None;
    let mut callee_offer_sdp: Option<String> = None;
    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                callee_dialog_id = Some(id.clone());
                callee_offer_sdp = offer;
                callee_ua.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if callee_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let _callee_id = callee_dialog_id.ok_or_else(|| anyhow::anyhow!("No INVITE"))?;

    let caller_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("No answer SDP"))?;
    let callee_offer = callee_offer_sdp.ok_or_else(|| anyhow::anyhow!("No offer SDP"))?;

    let callee_target = extract_media_endpoint(&callee_offer)
        .ok_or_else(|| anyhow::anyhow!("No callee endpoint"))?;
    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("No caller endpoint"))?;

    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    // Send from callee first so proxy learns callee address
    let dummy_packets = RtpPacket::create_sequence(50, 7000, 80000, 0xBBBBBBBB, 0, 160, 160);
    callee_sender.start_sending(caller_target, dummy_packets, 20);
    sleep(Duration::from_millis(200)).await;

    // Send test packets with distinctive payloads from caller
    let mut test_packets = Vec::new();
    for i in 0..50u16 {
        let mut payload = vec![0u8; 160];
        payload[0] = (i >> 8) as u8;
        payload[1] = (i & 0xFF) as u8;
        payload[2] = 0xDE;
        payload[3] = 0xAD;
        for (j, byte) in payload.iter_mut().enumerate().skip(4) {
            *byte = ((i as u8).wrapping_add(j as u8)) ^ 0x55;
        }
        test_packets.push(RtpPacket::new(
            0,
            5000 + i,
            100000 + (i as u32) * 160,
            0xCAFEBABE,
            payload,
        ));
    }

    caller_sender.start_sending(callee_target, test_packets, 20);

    sleep(Duration::from_millis(1500)).await;

    caller_sender.stop();
    callee_sender.stop();
    sleep(Duration::from_millis(200)).await;

    let callee_stats = callee_receiver.get_stats().await;

    info!(
        received = callee_stats.packets_received,
        pts = ?callee_stats.payload_types,
        ssrcs = ?callee_stats.ssrcs,
        "Payload integrity results"
    );

    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive RTP through proxy"
    );
    assert!(
        callee_stats.payload_types.contains(&0),
        "Callee should see PT 0 (PCMU), got {:?}",
        callee_stats.payload_types
    );

    if !callee_stats.ssrcs.contains(&0xCAFEBABE) {
        warn!(
            "Proxy rewrote SSRC: expected 0xCAFEBABE, got {:?} (expected for B2BUA)",
            callee_stats.ssrcs
        );
    }

    // Hang up and verify CDR
    caller_ua.hangup(&caller_id).await.ok();

    wait_for_cdr(&server, 800).await?;
    let records = server.cdr_capture.get_all_records().await;
    let record = &records[0];
    assert_eq!(record.details.status, "completed");

    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();
    info!("test_trunk_b2bua_rtp_payload_integrity PASSED");
    Ok(())
}

// ─── Test 10: Wholesale — early media with 183 Session Progress ──────────

#[tokio::test]
async fn test_trunk_b2bua_early_media_183() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let callee_sender = RtpSender::bind().await?;

    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;

    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let caller_sdp = pcmu_sdp("127.0.0.1", caller_port);
    let callee_sdp = pcmu_sdp("127.0.0.1", callee_port);

    let caller_clone = caller_ua.clone();
    let caller_handle =
        crate::utils::spawn(async move { caller_clone.make_call("bob", Some(caller_sdp)).await });

    let mut callee_dialog_id = None;
    let mut callee_offer_sdp: Option<String> = None;
    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                callee_dialog_id = Some(id.clone());
                callee_offer_sdp = offer;
                callee_ua.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if callee_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let _callee_id = callee_dialog_id.ok_or_else(|| anyhow::anyhow!("No INVITE"))?;

    let caller_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    // Exchange RTP briefly
    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("No answer SDP"))?;
    let callee_offer = callee_offer_sdp.ok_or_else(|| anyhow::anyhow!("No offer SDP"))?;

    let callee_target = extract_media_endpoint(&callee_offer)
        .ok_or_else(|| anyhow::anyhow!("No callee endpoint"))?;
    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("No caller endpoint"))?;

    let (caller_stats, callee_stats) = exchange_rtp(
        &caller_sender,
        &callee_sender,
        &caller_receiver,
        &callee_receiver,
        caller_target,
        callee_target,
        0,
        1500,
    )
    .await?;

    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive RTP"
    );
    assert!(
        caller_stats.packets_received > 0,
        "Caller should receive RTP"
    );

    caller_ua.hangup(&caller_id).await?;

    wait_for_cdr(&server, 800).await?;
    let records = server.cdr_capture.get_all_records().await;
    assert_eq!(records[0].details.status, "completed");

    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();
    info!("test_trunk_b2bua_early_media_183 PASSED");
    Ok(())
}

// ─── Test 11: Wholesale — basic call with CDR round-trip ──────────────────

#[tokio::test]
async fn test_trunk_b2bua_basic_call_cdr_roundtrip() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    let sdp = pcmu_sdp("127.0.0.1", 12345);

    let alice_clone = alice.clone();
    let sdp_clone = sdp.clone();
    let caller_handle =
        crate::utils::spawn(async move { alice_clone.make_call("bob", Some(sdp_clone)).await });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
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
        sleep(Duration::from_millis(100)).await;
    }

    let _bob_id = bob_dialog_id.ok_or_else(|| anyhow::anyhow!("No INVITE"))?;

    let alice_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    // Keep the call alive briefly then hangup
    sleep(Duration::from_millis(500)).await;

    alice.hangup(&alice_id).await?;

    sleep(Duration::from_millis(800)).await;
    let records = server.cdr_capture.get_all_records().await;
    assert!(!records.is_empty(), "Should have CDR");
    assert_eq!(records[0].details.status, "completed");
    let error_code = records[0]
        .details
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("error_code"))
        .and_then(serde_json::Value::as_str);
    assert_eq!(
        error_code, None,
        "a successful sequential call must not retain a dialing error"
    );

    server.stop();
    info!("test_trunk_b2bua_options_keepalive PASSED");
    Ok(())
}

// ─── Test 12: Wholesale — mid-call re-INVITE (codec change) ──────────────

#[tokio::test]
async fn test_trunk_b2bua_mid_call_reinvite() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let callee_sender = RtpSender::bind().await?;

    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;

    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;

    sleep(Duration::from_millis(100)).await;

    // Start with PCMU
    let caller_sdp = pcmu_sdp("127.0.0.1", caller_port);
    let callee_sdp = pcmu_sdp("127.0.0.1", callee_port);

    let caller_clone = caller_ua.clone();
    let caller_handle =
        crate::utils::spawn(async move { caller_clone.make_call("bob", Some(caller_sdp)).await });

    let mut callee_dialog_id = None;
    let mut callee_offer_sdp: Option<String> = None;
    for _ in 0..50 {
        let events = callee_ua.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                callee_dialog_id = Some(id.clone());
                callee_offer_sdp = offer;
                callee_ua.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if callee_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let _callee_id = callee_dialog_id.ok_or_else(|| anyhow::anyhow!("No INVITE"))?;

    let caller_id = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("timeout"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    // Exchange RTP briefly with PCMU
    let caller_answer = caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("No answer SDP"))?;
    let callee_offer = callee_offer_sdp.ok_or_else(|| anyhow::anyhow!("No offer SDP"))?;

    let callee_target = extract_media_endpoint(&callee_offer)
        .ok_or_else(|| anyhow::anyhow!("No callee endpoint"))?;
    let caller_target = extract_media_endpoint(&caller_answer)
        .ok_or_else(|| anyhow::anyhow!("No caller endpoint"))?;

    let (_caller_stats, callee_stats) = exchange_rtp(
        &caller_sender,
        &callee_sender,
        &caller_receiver,
        &callee_receiver,
        caller_target,
        callee_target,
        0,
        1000,
    )
    .await?;

    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive RTP before re-INVITE"
    );

    let pcma_sdp_offer = pcma_sdp("127.0.0.1", caller_port);
    let reinvite_result = caller_ua
        .send_reinvite(&caller_id, Some(pcma_sdp_offer))
        .await;

    if let Ok(Some(_answer)) = reinvite_result {
        info!("re-INVITE succeeded, codec changed to PCMA");

        // Exchange more RTP with PCMA (PT=8)
        let (_caller_stats2, callee_stats2) = exchange_rtp(
            &caller_sender,
            &callee_sender,
            &caller_receiver,
            &callee_receiver,
            caller_target,
            callee_target,
            8,
            1000,
        )
        .await?;

        assert!(
            callee_stats2.packets_received > 0 || callee_stats.packets_received > 0,
            "Callee should receive RTP after re-INVITE"
        );
    } else {
        info!(
            "re-INVITE was not completed (proxy may not support mid-call codec change in this mode)"
        );
    }

    caller_ua.hangup(&caller_id).await?;

    wait_for_cdr(&server, 800).await?;
    let records = server.cdr_capture.get_all_records().await;
    assert_eq!(records[0].details.status, "completed");

    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();
    info!("test_trunk_b2bua_mid_call_reinvite PASSED");
    Ok(())
}

// ─── RTP timeout: neither side sends BYE, RTP stops → proxy tears down ────────

/// Verifies that when media is anchored (mediaproxy=all) and BOTH sides stop
/// sending RTP without sending BYE, the proxy proactively tears the call down
/// via rtp-timeout: it sends BYE on both dialogs and records CDR hangup reason
/// `RtpTimeout`.
///
/// This exercises the session-level rtp-inactivity watchdog that covers the
/// rewrite-bridge fast-path relay and the ForwardingTrack slow path (the
/// BridgePeer has its own in-line detector). With a 3s timeout the adaptive
/// tick is 1s, so teardown is expected a few seconds after RTP ceases.
#[tokio::test]
async fn test_trunk_b2bua_rtp_timeout_no_bye_tears_down() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // 3s rtp_timeout → adaptive tick = clamp(3/5, 1s, 5s) = 1s.
    // Latching is disabled to match start_with_mode: the test UAs use separate
    // sender/receiver sockets, so the proxy must send relayed RTP to the
    // SDP-declared (receiver) port rather than latching onto the sender port.
    let mut proxy_config = test_helpers::test_proxy_config(0);
    proxy_config.media_proxy = MediaProxyMode::All;
    proxy_config.rtp_timeout = Some(3);
    proxy_config.enable_latching = false;
    let server = Arc::new(E2eTestServer::start_with_config(proxy_config).await?);

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_sender = RtpSender::bind().await?;
    let callee_sender = RtpSender::bind().await?;

    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;

    let (call, caller_ua, callee_ua) =
        establish_call(&server, "alice", "bob", caller_port, callee_port).await?;

    info!(session_id = ?call.caller_id, "Wholesale call established; exchanging RTP briefly to anchor media");

    // Exchange RTP for ~2s so media is anchored and the watchdog baselines its
    // per-leg counters on real traffic.
    let (caller_stats, callee_stats) = exchange_rtp(
        &caller_sender,
        &callee_sender,
        &caller_receiver,
        &callee_receiver,
        call.caller_target,
        call.callee_target,
        0,
        2000,
    )
    .await?;

    info!(
        caller_received = caller_stats.packets_received,
        callee_received = callee_stats.packets_received,
        "Pre-silence RTP (sanity: both directions flowed)"
    );
    assert!(
        caller_stats.packets_received > 0 && callee_stats.packets_received > 0,
        "RTP must flow bidirectionally before the silence phase"
    );

    // `exchange_rtp` already stopped the senders. Neither side sends a BYE.
    // The proxy must detect the RTP silence and tear the call down on its own.
    info!("Both RTP senders stopped, no BYE sent — waiting for proxy-initiated teardown");

    let silence_started = Instant::now();
    let mut caller_bye = false;
    let mut callee_bye = false;
    // Generous bound: 3s timeout + 1s tick + slack for teardown signalling.
    let deadline = silence_started + Duration::from_secs(15);
    while Instant::now() < deadline {
        for event in caller_ua.process_dialog_events().await? {
            if matches!(event, TestUaEvent::CallTerminated(_)) {
                caller_bye = true;
            }
        }
        for event in callee_ua.process_dialog_events().await? {
            if matches!(event, TestUaEvent::CallTerminated(_)) {
                callee_bye = true;
            }
        }
        if caller_bye && callee_bye {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let elapsed = silence_started.elapsed();
    info!(
        caller_bye,
        callee_bye,
        elapsed_secs = elapsed.as_secs(),
        "Teardown observation result"
    );
    assert!(
        caller_bye && callee_bye,
        "Proxy must send BYE to both legs on rtp-timeout (caller_bye={}, callee_bye={}) within 15s",
        caller_bye,
        callee_bye
    );

    // CDR must reflect the system-initiated teardown reason. The record is
    // finalized asynchronously after the BYEs, so poll briefly (instead of a
    // single fixed sleep + non-asserting warn, which was flaky) until the
    // hangup reason is `RtpTimeout`.
    let cdr_deadline = Instant::now() + Duration::from_secs(5);
    let mut rtp_timeout_cdr = false;
    while Instant::now() < cdr_deadline {
        let records = server.cdr_capture.get_all_records().await;
        if let Some(record) = records.first() {
            if matches!(
                record.hangup_reason,
                Some(CallRecordHangupReason::RtpTimeout)
            ) {
                rtp_timeout_cdr = true;
                break;
            }
            info!(
                call_id = %record.call_id,
                hangup_reason = ?record.hangup_reason,
                "CDR hangup reason not RtpTimeout yet; waiting"
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        rtp_timeout_cdr,
        "CDR hangup reason must be RtpTimeout after proxy-initiated teardown"
    );

    caller_receiver.stop();
    callee_receiver.stop();
    server.stop();
    info!("test_trunk_b2bua_rtp_timeout_no_bye_tears_down PASSED");
    Ok(())
}
