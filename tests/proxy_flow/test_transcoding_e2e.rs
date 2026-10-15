use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender, extract_media_endpoint};
use crate::common::test_helpers;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use rustrtc::config::AudioCapability;

fn pcmu_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {port} RTP/AVP 0 101\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

fn g729_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {port} RTP/AVP 18 101\r\n\
        a=rtpmap:18 G729/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

fn pcma_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {port} RTP/AVP 8 101\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    )
}

#[tokio::test]
async fn test_pcmu_to_g729_transcode() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let caller_sdp = pcmu_sdp(caller_port);
    let callee_sdp = g729_sdp(callee_port);
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
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
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "Call should be established");

    // Send PCMU RTP to the proxy's caller-leg media endpoint (from the
    // negotiated answer SDP) — the proxy transcodes and forwards to the
    // callee's advertised endpoint.
    let answer_sdp = alice
        .get_negotiated_answer_sdp(alice_dialog_id.as_ref().unwrap())
        .await
        .expect("negotiated answer SDP for caller");
    let proxy_media =
        extract_media_endpoint(&answer_sdp).expect("media endpoint in caller answer SDP");
    let caller_sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x11111111, 0, 160, 160);
    caller_sender.start_sending(proxy_media, packets, 20);
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let callee_stats = callee_receiver.get_stats().await;
    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive RTP after transcoding"
    );
    assert!(
        callee_stats.payload_types.contains(&18),
        "Callee should receive G729 RTP (PT=18) after PCMU→G729 transcode, got {:?}",
        callee_stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_pcmu_to_pcma_transcode() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let caller_sdp = pcmu_sdp(caller_port);
    let callee_sdp = pcma_sdp(callee_port);
    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "Call should be established");

    let answer_sdp = alice
        .get_negotiated_answer_sdp(alice_dialog_id.as_ref().unwrap())
        .await
        .expect("negotiated answer SDP for caller");
    let proxy_media =
        extract_media_endpoint(&answer_sdp).expect("media endpoint in caller answer SDP");
    let caller_sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x22222222, 0, 160, 160);
    caller_sender.start_sending(proxy_media, packets, 20);
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let callee_stats = callee_receiver.get_stats().await;
    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive PCMA RTP"
    );
    assert!(
        callee_stats.payload_types.contains(&8),
        "Callee should receive PCMA RTP (PT=8) after PCMU→PCMA transcode, got {:?}",
        callee_stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_g722_to_g729_transcode() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let callee_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    let callee_port = callee_receiver.port()?;
    caller_receiver.start_receiving();
    callee_receiver.start_receiving();

    let g722_sdp = format!(
        "v=0\r\n\
        o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
        m=audio {caller_port} RTP/AVP 9 101\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\n\
        a=sendrecv\r\n"
    );
    let g729_sdp = g729_sdp(callee_port);

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = g722_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob_dialog_id = Some(id.clone());
                bob.answer_call(&id, Some(g729_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "G722→G729 call should establish");

    let answer_sdp = alice
        .get_negotiated_answer_sdp(alice_dialog_id.as_ref().unwrap())
        .await
        .expect("negotiated answer SDP for caller");
    let proxy_media =
        extract_media_endpoint(&answer_sdp).expect("media endpoint in caller answer SDP");
    let caller_sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x33333333, 9, 160, 160);
    caller_sender.start_sending(proxy_media, packets, 20);
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let callee_stats = callee_receiver.get_stats().await;
    assert!(
        callee_stats.packets_received > 0,
        "Callee should receive G729 RTP after transcoding from G722"
    );
    assert!(
        callee_stats.payload_types.contains(&18),
        "Callee should receive G729 RTP (PT=18) after G722→G729 transcode, got {:?}",
        callee_stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}

/// End-to-end regression for the WebRTC-G729 pass-through, mirroring the
/// production "G729 → IVR → sipbot(g729,opus)" scenario:
///
/// - codec policy (`[proxy] audio_codecs`) allows G729 (listed first),
/// - caller offers G729 only (plain RTP),
/// - agent is a WebRTC peer advertising G729 + Opus.
///
/// The agent MUST receive G729 in the DTLS-SRTP offer, negotiate it, and
/// actually receive G729 RTP on its WebRTC leg — negotiation and media handling
/// must agree.
#[tokio::test]
async fn test_g729_caller_to_webrtc_g729_opus_agent() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let mut cfg = test_helpers::test_proxy_config(0);
    cfg.media_proxy = MediaProxyMode::All;
    cfg.audio_codecs = Some(vec![
        "g729".into(),
        "pcma".into(),
        "pcmu".into(),
        "g722".into(),
        "opus".into(),
    ]);
    let server = Arc::new(E2eTestServer::start_with_config(cfg).await?);

    // Caller: plain RTP UA offering only G729.
    let alice = Arc::new(server.create_ua("alice").await?);

    // Agent: WebRTC UA advertising G729 + Opus (the "sipbot(g729,opus)" peer).
    // NOTE: `charlie` is the WebRTC-capable test user (bob is not).
    let agent_config = TestUaConfig {
        webrtc: true,
        username: "charlie".into(),
        password: "password789".into(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27410),
        proxy_addr: server.proxy_addr,
    };
    let mut bob = TestUa::new_webrtc_with_caps(
        agent_config,
        vec![
            AudioCapability::g729(),
            AudioCapability::opus(),
            AudioCapability::telephone_event(),
        ],
    );
    bob.start().await?;
    bob.register().await?;
    sleep(Duration::from_millis(150)).await;

    let caller_receiver = RtpReceiver::bind(0).await?;
    let caller_port = caller_receiver.port()?;
    caller_receiver.start_receiving();
    let caller_sdp = g729_sdp(caller_port);

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("charlie", Some(sdp)).await }
    });

    // Capture the offer the WebRTC agent receives, and answer it.
    let mut bob_dialog_id = None;
    let mut offer_to_bob = None;
    for _ in 0..200 {
        for event in bob.process_dialog_events().await? {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                offer_to_bob = sdp;
                if let Err(e) = bob.answer_call(&id, None).await {
                    eprintln!(
                        "answer_call failed: {e}\noffer_to_agent=\n{}",
                        offer_to_bob.clone().unwrap_or_default()
                    );
                    return Err(e);
                }
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(
        bob_dialog_id.is_some(),
        "the WebRTC agent must receive the call"
    );

    let offer_to_bob = offer_to_bob.unwrap_or_default();
    assert!(
        offer_to_bob.contains("UDP/TLS/RTP/SAVPF") && offer_to_bob.contains("a=fingerprint"),
        "the agent must receive a DTLS-SRTP offer:\n{offer_to_bob}"
    );
    assert!(
        offer_to_bob.contains("a=rtpmap:18 G729/8000"),
        "G729 must be offered to the WebRTC agent when audio_codecs allows it:\n{offer_to_bob}"
    );
    assert!(
        offer_to_bob.contains("a=rtpmap:111 opus/48000/2"),
        "Opus must still be offered alongside G729:\n{offer_to_bob}"
    );

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(8), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "the call must establish");

    // The agent's negotiated answer must have selected G729.
    if let Some(id) = bob_dialog_id.as_ref() {
        if let Some(answer) = bob.get_negotiated_answer_sdp(id).await {
            assert!(
                answer.to_lowercase().contains("g729"),
                "the agent answer must select G729:\n{answer}"
            );
        }
    }

    // Media plane: the agent's WebRTC leg must receive real G729 RTP (PT 18).
    bob.attach_webrtc_rx_tap().await?;
    let answer_sdp = alice
        .get_negotiated_answer_sdp(alice_dialog_id.as_ref().unwrap())
        .await
        .expect("negotiated answer SDP for caller");
    let proxy_media =
        extract_media_endpoint(&answer_sdp).expect("media endpoint in caller answer SDP");
    let caller_sender = RtpSender::bind().await?;
    // G729: 20-byte payload = one 20 ms frame; timestamp step 160 @ 8 kHz.
    let packets = RtpPacket::create_sequence(60, 1000, 50000, 0x4a4a4a4a, 18, 20, 160);
    caller_sender.start_sending(proxy_media, packets, 20);
    sleep(Duration::from_millis(1500)).await;
    caller_sender.stop();

    let rx = bob.webrtc_rx_packets();
    assert!(
        rx.iter().any(|p| p.payload_type == 18),
        "the WebRTC agent must receive G729 RTP (PT 18); got {:?}",
        rx.iter().map(|p| p.payload_type).collect::<Vec<_>>()
    );

    if let Some(id) = alice_dialog_id.as_ref() {
        alice.hangup(id).await.ok();
    }
    server.stop();
    Ok(())
}

