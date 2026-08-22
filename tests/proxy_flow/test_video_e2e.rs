use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender};
use crate::common::test_ua::TestUaEvent;

/// Parse per-m-line media endpoints (kind, addr) from an SDP. The connection
/// host is tracked from the active `c=` line (global or per-msection).
fn media_endpoints(sdp: &str) -> Vec<(&'static str, SocketAddr)> {
    let mut host = "127.0.0.1".to_string();
    let mut out = Vec::new();
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
            host = rest.split_whitespace().next().unwrap_or("127.0.0.1").to_string();
        } else if let Some(rest) = line.strip_prefix("m=") {
            let mut parts = rest.split_whitespace();
            let kind = match parts.next() {
                Some("audio") => "audio",
                Some("video") => "video",
                _ => continue,
            };
            // m=<media> <port> <proto> <fmt...>
            let port: u16 = parts
                .next()
                .and_then(|p| p.split(':').next().unwrap_or(p).parse().ok())
                .unwrap_or(0);
            if port > 0 {
                if let Ok(addr) = format!("{host}:{port}").parse() {
                    out.push((kind, addr));
                }
            }
        }
    }
    out
}

fn endpoint_for(sdp: &str, kind: &str) -> Option<SocketAddr> {
    media_endpoints(sdp)
        .into_iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, a)| a)
}

async fn establish(server: &Arc<E2eTestServer>, caller_sdp: String, callee_sdp: String)
    -> anyhow::Result<(String, Option<String>)>
{
    let alice = server.create_ua("alice").await?;
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    let mut bob_offer_sdp = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                bob_dialog_id = Some(id.clone());
                bob_offer_sdp = offer;
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
    anyhow::ensure!(alice_dialog_id.is_some(), "call should be established");

    let answer_sdp = alice
        .get_negotiated_answer_sdp(alice_dialog_id.as_ref().unwrap())
        .await
        .ok_or_else(|| anyhow::anyhow!("no negotiated answer SDP for caller"))?;
    Ok((answer_sdp, bob_offer_sdp))
}

#[tokio::test]
async fn test_video_h264_passthrough_call_establishes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let caller_sdp = format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 10000 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n\
         m=video 10001 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\na=sendrecv\r\n"
    );
    let callee_sdp = format!(
        "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 20000 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n\
         m=video 20001 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\na=sendrecv\r\n"
    );

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
    assert!(
        alice_dialog_id.is_some(),
        "Video call should be established"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_video_rtp_relay_bidirectional() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    // Four receivers: caller/callee × audio/video.
    let ca_rx = RtpReceiver::bind(0).await?;
    let cv_rx = RtpReceiver::bind(0).await?;
    let ba_rx = RtpReceiver::bind(0).await?;
    let bv_rx = RtpReceiver::bind(0).await?;
    let (ca_port, cv_port, ba_port, bv_port) = (ca_rx.port()?, cv_rx.port()?, ba_rx.port()?, bv_rx.port()?);
    for rx in [&ca_rx, &cv_rx, &ba_rx, &bv_rx] {
        rx.start_receiving();
    }

    let caller_sdp = format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {ca_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n\
         m=video {cv_port} RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\na=sendrecv\r\n"
    );
    let callee_sdp = format!(
        "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {ba_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n\
         m=video {bv_port} RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\na=sendrecv\r\n"
    );

    let (answer_sdp, offer_sdp) = establish(&server, caller_sdp, callee_sdp).await?;
    let offer_sdp = offer_sdp.ok_or_else(|| anyhow::anyhow!("callee offer SDP missing"))?;

    // Proxy-side endpoints: caller leg from the negotiated answer, callee leg
    // from the offer the callee received.
    let proxy_a_audio = endpoint_for(&answer_sdp, "audio")
        .ok_or_else(|| anyhow::anyhow!("no audio endpoint in caller answer SDP:\n{answer_sdp}"))?;
    let proxy_a_video = endpoint_for(&answer_sdp, "video")
        .ok_or_else(|| anyhow::anyhow!("no video endpoint in caller answer SDP:\n{answer_sdp}"))?;
    let proxy_b_audio = endpoint_for(&offer_sdp, "audio")
        .ok_or_else(|| anyhow::anyhow!("no audio endpoint in callee offer SDP"))?;
    let proxy_b_video = endpoint_for(&offer_sdp, "video")
        .ok_or_else(|| anyhow::anyhow!("no video endpoint in callee offer SDP"))?;

    // Forward direction: caller sends audio (PCMU) + video (H264).
    let sender_a = RtpSender::bind().await?;
    sender_a.start_sending(
        proxy_a_audio,
        RtpPacket::create_sequence(60, 1000, 50000, 0xAAAA1111, 0, 160, 160),
        20,
    );
    sender_a.start_sending(
        proxy_a_video,
        RtpPacket::create_sequence(60, 2000, 900000, 0xBBBB2222, 96, 900, 3000),
        20,
    );
    sleep(Duration::from_millis(1500)).await;
    sender_a.stop();

    let ba_stats = ba_rx.get_stats().await;
    assert!(
        ba_stats.payload_types.contains(&0) && ba_stats.packets_received >= 20,
        "callee audio leg should receive relayed PCMU, got {:?} ({} pkts)",
        ba_stats.payload_types,
        ba_stats.packets_received
    );
    let bv_stats = bv_rx.get_stats().await;
    assert!(
        bv_stats.payload_types.contains(&96) && bv_stats.packets_received >= 20,
        "callee video leg should receive relayed H264 (PT=96), got {:?} ({} pkts)",
        bv_stats.payload_types,
        bv_stats.packets_received
    );
    // The relay rewrites SSRCs (its own playback SSRC) — require exactly one
    // stable video SSRC rather than the raw sender SSRC.
    assert_eq!(
        bv_stats.ssrcs.len(),
        1,
        "relayed video should carry a single (rewritten) SSRC, got {:?}",
        bv_stats.ssrcs
    );

    // Reverse direction: callee sends audio + video back.
    let sender_b = RtpSender::bind().await?;
    sender_b.start_sending(
        proxy_b_audio,
        RtpPacket::create_sequence(60, 3000, 70000, 0xCCCC3333, 0, 160, 160),
        20,
    );
    sender_b.start_sending(
        proxy_b_video,
        RtpPacket::create_sequence(60, 4000, 1800000, 0xDDDD4444, 96, 900, 3000),
        20,
    );
    sleep(Duration::from_millis(1500)).await;
    sender_b.stop();

    let ca_stats = ca_rx.get_stats().await;
    assert!(
        ca_stats.payload_types.contains(&0) && ca_stats.packets_received >= 20,
        "caller audio leg should receive reverse-direction PCMU, got {:?} ({} pkts)",
        ca_stats.payload_types,
        ca_stats.packets_received
    );
    let cv_stats = cv_rx.get_stats().await;
    assert!(
        cv_stats.payload_types.contains(&96) && cv_stats.packets_received >= 20,
        "caller video leg should receive reverse-direction H264, got {:?} ({} pkts)",
        cv_stats.payload_types,
        cv_stats.packets_received
    );
    assert_eq!(
        cv_stats.ssrcs.len(),
        1,
        "reverse video should carry a single (rewritten) SSRC, got {:?}",
        cv_stats.ssrcs
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_video_offer_audio_only_answer_downgrade() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);

    // Caller offers audio+video; the callee answers audio-ONLY (no m=video).
    // The call must establish and audio must relay; the video track simply
    // has no far-end counterpart (graceful downgrade instead of 488).
    let ca_rx = RtpReceiver::bind(0).await?;
    let ba_rx = RtpReceiver::bind(0).await?;
    let ca_port = ca_rx.port()?;
    let ba_port = ba_rx.port()?;
    ca_rx.start_receiving();
    ba_rx.start_receiving();

    let caller_sdp = format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {ca_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n\
         m=video 15001 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\na=sendrecv\r\n"
    );
    let callee_sdp = format!(
        "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {ba_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
    );

    let (answer_sdp, _) = establish(&server, caller_sdp, callee_sdp).await?;

    let proxy_a_audio = endpoint_for(&answer_sdp, "audio")
        .ok_or_else(|| anyhow::anyhow!("no audio endpoint in caller answer SDP"))?;
    let sender_a = RtpSender::bind().await?;
    sender_a.start_sending(
        proxy_a_audio,
        RtpPacket::create_sequence(50, 1000, 50000, 0x11111111, 0, 160, 160),
        20,
    );
    sleep(Duration::from_millis(1200)).await;
    sender_a.stop();

    let ba_stats = ba_rx.get_stats().await;
    assert!(
        ba_stats.payload_types.contains(&0) && ba_stats.packets_received >= 15,
        "audio must relay despite the video offer being downgraded, got {:?}",
        ba_stats.payload_types
    );

    server.stop();
    Ok(())
}
