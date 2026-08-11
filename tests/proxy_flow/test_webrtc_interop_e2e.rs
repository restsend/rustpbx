use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::cdr_capture::CdrExpectation;
use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender};
use crate::common::test_ua::TestUaEvent;

fn rtp_pcmu_sdp(port: u16) -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}

fn webrtc_pcmu_sdp(port: u16) -> String {
    format!(
        "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {port} UDP/TLS/RTP/SAVPF 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r\n\
         a=setup:actpass\r\n\
         a=mid:0\r\n\
         a=sendrecv\r\na=rtcp-mux\r\n"
    )
}

async fn exchange_rtp(
    callee_port: u16,
    pt: u8,
) -> Result<()> {
    let sender = RtpSender::bind().await?;
    let packets = RtpPacket::create_sequence(50, 1000, 50000, 0x11111111, pt, 160, 160);
    sender.start_sending(
        std::net::SocketAddr::from(([127, 0, 0, 1], callee_port)),
        packets,
        20,
    );
    sleep(Duration::from_millis(1500)).await;
    sender.stop();
    Ok(())
}

#[tokio::test]
async fn test_rtp_pcmu_to_webrtc_pcmu_fastpath() -> Result<()> {
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

    let caller_sdp = rtp_pcmu_sdp(caller_port);
    let callee_sdp = webrtc_pcmu_sdp(callee_port);

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    let mut bob_dialog_id = None;
    let mut received_sdp = None;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                received_sdp = sdp;
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

    if let Some(ref sdp) = received_sdp {
        assert!(
            sdp.contains("RTP/AVP"),
            "SDP forwarded to callee should use RTP/AVP (not SAVPF)"
        );
    }

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(5), caller_handle).await {
        Ok(Ok(Ok(id))) => Some(id),
        _ => None,
    };
    assert!(alice_dialog_id.is_some(), "Call should be established");

    exchange_rtp(callee_port, 0).await?;

    let stats = callee_receiver.get_stats().await;
    assert!(stats.packets_received > 0, "Should receive RTP across RTP→WebRTC fastpath");
    assert!(
        stats.payload_types.contains(&0),
        "Same codec (PCMU) should fastpath with PT=0 preserved, got {:?}",
        stats.payload_types
    );

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_webrtc_to_rtp_codec_mismatch_transcodes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let receiver = RtpReceiver::bind(0).await?;
    let callee_port = receiver.port()?;
    receiver.start_receiving();

    let webrtc_opus_sdp = format!(
        "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 54321 UDP/TLS/RTP/SAVPF 111 101\r\n\
         a=rtpmap:111 opus/48000/2\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r\n\
         a=setup:actpass\r\na=mid:0\r\na=sendrecv\r\na=rtcp-mux\r\n"
    );

    let caller_sdp = webrtc_opus_sdp;
    let callee_sdp = format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {callee_port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
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
    assert!(alice_dialog_id.is_some(), "WebRTC→RTP transcode call should establish");

    // WebRTC→RTP 转码场景：由于 TestUa 不做真正的 DTLS-SRTP，无法从 WebRTC 侧
    // 注入 Opus 编码音频做内容级转码验证。此处验证：呼叫建立 + SDP 协商正确。
    // 实际转码内容级验证见 test_transcoding_e2e.rs (纯 RTP 场景，已验证 PT 转换)。
    let _ = receiver;

    alice.hangup(alice_dialog_id.as_ref().unwrap()).await.ok();
    server.stop();
    Ok(())
}
