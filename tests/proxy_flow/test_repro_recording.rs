//! Reproduction tests for the 0.5.0-rc.1 field report:
//!
//! Default config `[recording] enabled = true` + default `video_policy`
//! (pass-through):
//!   1. Audio-only call: connects but no audio flows either way.
//!   2. Audio+video call: the call is rejected with 488 Not Acceptable Here.
//!
//! Both use plain-RTP SIP endpoints (softphone-style), matching the field
//! report.

use anyhow::{Result, anyhow};
use rustpbx::config::{MediaProxyMode, ProxyConfig, RecordingPolicy};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, extract_media_endpoint};
use crate::common::test_ua::TestUaEvent;

/// Combined UDP media endpoint for a plain-RTP leg: one socket for both
/// sending and receiving, so the SDP-advertised address matches the actual
/// RTP source (symmetric-RTP friendly).
struct MediaEndpoint {
    socket: Arc<tokio::net::UdpSocket>,
    received: Arc<std::sync::Mutex<Vec<RtpPacket>>>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl MediaEndpoint {
    async fn bind() -> Result<Self> {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        Ok(Self {
            socket,
            received: Arc::new(std::sync::Mutex::new(Vec::new())),
            cancel_token: tokio_util::sync::CancellationToken::new(),
        })
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    fn start_receiving(&self) {
        let socket = Arc::clone(&self.socket);
        let received = self.received.clone();
        let cancel_token = self.cancel_token.clone();
        rustpbx::utils::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    result = socket.recv_from(&mut buf) => match result {
                        Ok((len, _)) => {
                            if let Ok(packet) = RtpPacket::decode(&buf[..len]) {
                                received.lock().unwrap().push(packet);
                            }
                        }
                        Err(_) => break,
                    },
                }
            }
        });
    }

    async fn send_sequence(
        &self,
        target: std::net::SocketAddr,
        packets: &[RtpPacket],
        interval_ms: u64,
    ) {
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
        for packet in packets {
            interval.tick().await;
            let _ = self.socket.send_to(&packet.encode(), target).await;
        }
    }

    fn received_packets(&self) -> Vec<RtpPacket> {
        self.received.lock().unwrap().clone()
    }

    fn stop(&self) {
        self.cancel_token.cancel();
    }
}

fn pcmu_sdp_with_port(port: u16) -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        port
    )
}

fn audio_video_sdp(port: u16) -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {} RTP/AVP 0 8 9 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:9 G722/8000\r\na=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-16\r\na=sendrecv\r\n\
         m=video {} RTP/AVP 99 100\r\n\
         a=rtpmap:99 H264/90000\r\na=fmtp:99 packetization-mode=1;profile-level-id=42e01f\r\n\
         a=rtpmap:100 H264/90000\r\na=fmtp:100 packetization-mode=0;profile-level-id=42e01f\r\n\
         a=sendrecv\r\n",
        port,
        port + 2
    )
}

/// 20 ms PCMU payloads with a recognizable per-packet fill pattern.
fn patterned_pcmu_payloads(count: usize, base: u8) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| vec![base.wrapping_add(i as u8); 160])
        .collect()
}

fn count_payload_matches(received: &[RtpPacket], pt: u8, sent: &[Vec<u8>]) -> usize {
    let set: std::collections::HashSet<&Vec<u8>> = sent.iter().collect();
    received
        .iter()
        .filter(|p| p.payload_type == pt && set.contains(&p.payload))
        .count()
}

async fn server_with_recording(record_path: String) -> Result<Arc<E2eTestServer>> {
    let proxy_config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        recording: Some(RecordingPolicy {
            enabled: Some(true),
            auto_start: Some(true),
            path: Some(record_path),
            ..Default::default()
        }),
        ..Default::default()
    };
    Ok(Arc::new(
        E2eTestServer::start_with_config(proxy_config).await?,
    ))
}

/// Bug 1 repro: audio-only call with recording enabled must carry RTP in BOTH
/// directions (caller→proxy→callee and callee→proxy→caller).
#[tokio::test]
async fn repro_audio_only_call_rtp_flows_both_ways_with_recording() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let record_dir = tempfile::tempdir()?;
    let server = server_with_recording(record_dir.path().to_string_lossy().to_string()).await?;
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_media = MediaEndpoint::bind().await?;
    let callee_media = MediaEndpoint::bind().await?;
    caller_media.start_receiving();
    callee_media.start_receiving();

    let caller_sdp = pcmu_sdp_with_port(caller_media.port());
    let callee_sdp = pcmu_sdp_with_port(callee_media.port());

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    let mut offer_to_bob = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                offer_to_bob = sdp;
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let bob_dialog_id = bob_dialog_id.ok_or_else(|| anyhow!("bob never received the call"))?;
    let offer_to_bob = offer_to_bob.unwrap_or_default();
    assert!(
        !offer_to_bob.contains("a=rtcp-mux") && !offer_to_bob.contains("a=group:BUNDLE"),
        "audio-only offer to a plain-RTP callee must be standard RTP/AVP (no rtcp-mux/BUNDLE):\n{offer_to_bob}"
    );
    let proxy_b_leg = extract_media_endpoint(&offer_to_bob)
        .ok_or_else(|| anyhow!("no media endpoint in offer to callee"))?;

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(anyhow!("alice call failed: {e}")),
        _ => return Err(anyhow!("call setup timeout")),
    };
    let answer_to_alice = alice
        .get_negotiated_answer_sdp(&alice_id)
        .await
        .ok_or_else(|| anyhow!("no answer SDP for alice"))?;
    let proxy_a_leg = extract_media_endpoint(&answer_to_alice)
        .ok_or_else(|| anyhow!("no media endpoint in answer to caller"))?;

    // Caller → proxy → callee (pattern 0x50).
    let caller_payloads = patterned_pcmu_payloads(150, 0x50);
    let caller_packets: Vec<RtpPacket> = caller_payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RtpPacket::new(
                0,
                1000u16.wrapping_add(i as u16),
                30000u32 + (i as u32) * 160,
                0x10101010,
                p.clone(),
            )
        })
        .collect();
    caller_media
        .send_sequence(proxy_a_leg, &caller_packets, 20)
        .await;

    // Callee → proxy → caller (pattern 0xA0).
    let callee_payloads = patterned_pcmu_payloads(150, 0xA0);
    let callee_packets: Vec<RtpPacket> = callee_payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RtpPacket::new(
                0,
                9000u16.wrapping_add(i as u16),
                50000u32 + (i as u32) * 160,
                0x20202020,
                p.clone(),
            )
        })
        .collect();
    callee_media
        .send_sequence(proxy_b_leg, &callee_packets, 20)
        .await;

    sleep(Duration::from_millis(1500)).await;

    let callee_received = callee_media.received_packets();
    let caller_received = caller_media.received_packets();
    let callee_matched = count_payload_matches(&callee_received, 0, &caller_payloads);
    let caller_matched = count_payload_matches(&caller_received, 0, &callee_payloads);

    println!(
        "audio repro: callee_matched={}/{} caller_matched={}/{}",
        callee_matched,
        caller_payloads.len(),
        caller_matched,
        callee_payloads.len()
    );

    assert!(
        callee_matched >= 30,
        "REPRODUCED bug 1: caller→callee audio did not reach the callee (matched {callee_matched})"
    );
    assert!(
        caller_matched >= 30,
        "REPRODUCED bug 1: callee→caller audio did not reach the caller (matched {caller_matched})"
    );

    alice.hangup(&alice_id).await.ok();
    let _ = bob.hangup(&bob_dialog_id).await;
    sleep(Duration::from_millis(500)).await;
    caller_media.stop();
    callee_media.stop();
    server.stop();
    Ok(())
}

/// Bug 2 repro: audio+video (H264) call with recording enabled must establish —
/// the caller must NOT receive 488 Not Acceptable Here.
#[tokio::test]
async fn repro_video_call_with_recording_establishes_no_488() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();

    let record_dir = tempfile::tempdir()?;
    let server = server_with_recording(record_dir.path().to_string_lossy().to_string()).await?;
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_media = MediaEndpoint::bind().await?;
    let callee_media = MediaEndpoint::bind().await?;
    caller_media.start_receiving();
    callee_media.start_receiving();

    let caller_sdp = audio_video_sdp(caller_media.port());
    let callee_sdp = audio_video_sdp(callee_media.port());

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    let mut offer_to_bob = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                offer_to_bob = sdp;
                bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let bob_dialog_id = bob_dialog_id.ok_or_else(|| anyhow!("bob never received the call"))?;
    let offer_to_bob = offer_to_bob.unwrap_or_default();

    assert!(
        offer_to_bob.contains("m=video") && offer_to_bob.contains("H264"),
        "offer to callee should carry a video m-line with H264:\n{offer_to_bob}"
    );

    // The offer to a plain-RTP SIP peer must be a standard RTP/AVP offer: one
    // distinct UDP port per m-line, no BUNDLE group and no rtcp-mux. WebRTC
    // style BUNDLE (audio+video sharing a single port + rtcp-mux) is rejected
    // by strict SIP softphones with 488 Not Acceptable Here.
    assert!(
        !offer_to_bob.contains("a=group:BUNDLE"),
        "offer to plain-RTP callee must not use BUNDLE:\n{offer_to_bob}"
    );
    assert!(
        !offer_to_bob.contains("a=rtcp-mux"),
        "offer to plain-RTP callee must not use rtcp-mux:\n{offer_to_bob}"
    );
    assert!(
        !offer_to_bob.contains("m=application"),
        "offer to plain-RTP callee must not include m=application:\n{offer_to_bob}"
    );
    println!("video offer_to_bob:\n{offer_to_bob}");
    let audio_port = offer_to_bob
        .lines()
        .find(|l| l.starts_with("m=audio "))
        .and_then(|l| l.split_whitespace().nth(1))
        .map(|p| p.to_string());
    let video_port = offer_to_bob
        .lines()
        .find(|l| l.starts_with("m=video "))
        .and_then(|l| l.split_whitespace().nth(1))
        .map(|p| p.to_string());
    assert!(
        audio_port.is_some() && video_port.is_some() && audio_port != video_port,
        "audio and video m-lines must use distinct ports in a plain-RTP offer:\n{offer_to_bob}"
    );

    let alice_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => {
            return Err(anyhow!(
                "REPRODUCED bug 2: video call failed (488 Not Acceptable Here?): {e}"
            ));
        }
        _ => return Err(anyhow!("call setup timeout")),
    };

    let answer_to_alice = alice
        .get_negotiated_answer_sdp(&alice_id)
        .await
        .ok_or_else(|| anyhow!("no answer SDP for alice"))?;
    assert!(
        answer_to_alice.contains("m=video"),
        "answer to caller should carry a video m-line:\n{answer_to_alice}"
    );

    println!(
        "video repro: call established (no 488). offer_to_bob has video={} answer_to_alice has video={}",
        offer_to_bob.contains("m=video"),
        answer_to_alice.contains("m=video")
    );

    alice.hangup(&alice_id).await.ok();
    let _ = bob.hangup(&bob_dialog_id).await;
    sleep(Duration::from_millis(500)).await;
    caller_media.stop();
    callee_media.stop();
    server.stop();
    Ok(())
}
