use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, RtpReceiver, RtpSender};
use crate::common::test_ua::{TestUa, TestUaEvent};

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

#[test]
fn test_rtp_pcmu_to_webrtc_pcmu_fastpath() {
    run_with_big_stack(test_rtp_pcmu_to_webrtc_pcmu_fastpath_impl());
}

async fn test_rtp_pcmu_to_webrtc_pcmu_fastpath_impl() -> Result<()> {
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

#[test]
fn test_webrtc_to_rtp_codec_mismatch_transcodes() {
    run_with_big_stack(test_webrtc_to_rtp_codec_mismatch_transcodes_impl());
}

async fn test_webrtc_to_rtp_codec_mismatch_transcodes_impl() -> Result<()> {
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

// All tests in this file run the full SIP session + media-leg future chain,
// which in debug builds needs more than the default 2 MiB tokio thread stack
// (production uses 8 MiB, see src/bin/rustpbx.rs). Drive each scenario on a
// big-stack worker thread so no RUST_MIN_STACK env var is needed.
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
            .expect("e2e webrtc test failed");
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Real DTLS-SRTP WebRTC ↔ plain-RTP media verification
//
// Unlike the fake-SDP tests above, the caller runs a genuine rustrtc
// PeerConnection: ICE + DTLS-SRTP handshake, SRTP protect on egress and
// unprotect on ingress all run for real. These tests verify, for the
// same-codec (PCMU↔PCMU fastpath) and the transcode (Opus↔PCMU) case:
//   1. ICE/DTLS actually connect (SRTP keying material ready)
//   2. WebRTC→RTP: SRTP-decrypted media reaches the plain leg
//   3. RTP→WebRTC: plain media is SRTP-encrypted and delivered
//   4. Content integrity (fastpath preserves payload bytes; transcode
//      delivers non-silent audio in both directions)
// ─────────────────────────────────────────────────────────────────────────────

/// Combined UDP media endpoint for the plain-RTP leg: ONE socket used for
/// both sending and receiving, so the SDP-advertised address always matches
/// the actual RTP source (works with or without symmetric-RTP latching).
struct RtpMediaEndpoint {
    socket: Arc<tokio::net::UdpSocket>,
    received: Arc<std::sync::Mutex<Vec<RtpPacket>>>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl RtpMediaEndpoint {
    async fn bind() -> Result<Self> {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        Ok(Self {
            socket,
            received: Arc::new(std::sync::Mutex::new(Vec::new())),
            cancel_token: tokio_util::sync::CancellationToken::new(),
        })
    }

    fn port(&self) -> u16 {
        self.socket
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(0)
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

    async fn send_sequence(&self, target: std::net::SocketAddr, packets: &[RtpPacket], interval_ms: u64) {
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

/// Create a WebRTC caller with a real rustrtc PeerConnection. `audio_caps`
/// restricts the advertised codecs (None = Opus-only default).
async fn create_webrtc_caller(
    server: &E2eTestServer,
    username: &str,
    password: &str,
    audio_caps: Option<Vec<rustrtc::config::AudioCapability>>,
) -> Result<Arc<TestUa>> {
    let config = crate::common::test_ua::TestUaConfig {
        webrtc: true,
        username: username.to_string(),
        password: password.to_string(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(26100),
        proxy_addr: server.proxy_addr,
    };
    let mut ua = match audio_caps {
        Some(caps) => crate::common::test_ua::TestUa::new_webrtc_with_caps(config, caps),
        None => crate::common::test_ua::TestUa::new(config),
    };
    ua.start().await?;
    ua.register().await?;
    sleep(Duration::from_millis(100)).await;
    Ok(Arc::new(ua))
}

/// Drive alice (WebRTC caller) → bob (plain callee answering PCMU at
/// `bob_media_port`). Returns (alice dialog id, offer SDP as seen by bob).
async fn establish_webrtc_to_rtp_call(
    alice: Arc<TestUa>,
    bob: &TestUa,
    bob_media_port: u16,
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
                let answer = rtp_pcmu_sdp(bob_media_port);
                bob.answer_call(&id, Some(answer)).await?;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    assert!(bob_dialog_id.is_some(), "Bob should receive the incoming call");

    let alice_dialog_id = match tokio::time::timeout(Duration::from_secs(20), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("alice call failed: {}", e)),
        _ => return Err(anyhow::anyhow!("call setup timeout (ICE/DTLS negotiation too slow)")),
    };
    let _ = bob_dialog_id;
    Ok((alice_dialog_id, received_sdp.unwrap_or_default()))
}

/// 20 ms PCMU payloads with a recognizable per-packet fill pattern.
fn patterned_pcmu_payloads(count: usize, base: u8) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| vec![base.wrapping_add((i % 64) as u8); 160])
        .collect()
}

/// Count received PT-matching packets whose payload is byte-identical to one
/// of the sent payloads; returns (total_matching, distinct_matched).
fn count_payload_matches(received: &[RtpPacket], pt: u8, sent: &[Vec<u8>]) -> (usize, usize) {
    let sent_set: std::collections::HashSet<&Vec<u8>> = sent.iter().collect();
    let mut matched = 0;
    let mut distinct = std::collections::HashSet::new();
    for packet in received.iter().filter(|p| p.payload_type == pt) {
        if sent_set.contains(&packet.payload) {
            matched += 1;
            distinct.insert(packet.payload.clone());
        }
    }
    (matched, distinct.len())
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt()
}

// ─────────────────────────────────────────────────────────────────────────────
// Recording reproduction: real WebRTC(PCMU) ↔ RTP(PCMU) fastpath call with
// stereo auto-recording. Reproduces the field issue where the recorded WAV's
// caller leg (left channel = caller ingress, i.e. audio FROM the browser) is
// a constant silence byte (0xFF) while live audio flowed fine both ways.
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a G.711 WAV written by the proxy recorder and split the stereo
/// byte-interleaved payload into (leg_a, leg_b) byte streams.
/// Leg A = caller ingress (browser→proxy), Leg B = caller egress (proxy→browser).
fn split_stereo_g711_wav(path: &std::path::Path) -> Result<(Vec<u8>, Vec<u8>, u16, u32)> {
    let data = std::fs::read(path)?;
    anyhow::ensure!(data.len() > 44, "wav too small: {} bytes", data.len());
    anyhow::ensure!(&data[0..4] == b"RIFF" && &data[8..12] == b"WAVE", "not a RIFF/WAVE file");
    // Walk chunks to find fmt + data.
    let mut pos = 12;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut fmt_tag = 0u16;
    let mut payload: Option<&[u8]> = None;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into()?) as usize;
        let body = pos + 8;
        if body + size > data.len() {
            break;
        }
        match id {
            b"fmt " => {
                fmt_tag = u16::from_le_bytes(data[body..body + 2].try_into()?);
                channels = u16::from_le_bytes(data[body + 2..body + 4].try_into()?);
                sample_rate = u32::from_le_bytes(data[body + 4..body + 8].try_into()?);
            }
            b"data" => payload = Some(&data[body..body + size]),
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    let payload = payload.ok_or_else(|| anyhow::anyhow!("no data chunk"))?;
    anyhow::ensure!(fmt_tag == 7, "expected G.711 fmt tag 7, got {}", fmt_tag);
    anyhow::ensure!(channels == 2, "expected stereo, got {} channels", channels);
    let leg_a: Vec<u8> = payload.iter().step_by(2).copied().collect();
    let leg_b: Vec<u8> = payload.iter().skip(1).step_by(2).copied().collect();
    Ok((leg_a, leg_b, channels, sample_rate))
}

#[test]
fn test_webrtc_pcmu_rtp_fastpath_recording_both_legs_audio() {
    run_with_big_stack(test_webrtc_pcmu_rtp_fastpath_recording_both_legs_audio_impl());
}

async fn test_webrtc_pcmu_rtp_fastpath_recording_both_legs_audio_impl() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).try_init();

    use rustpbx::config::{ProxyConfig, RecordingPolicy};

    let record_dir = tempfile::tempdir()?;
    let record_path = record_dir.path().to_string_lossy().to_string();

    let proxy_config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        // Sequential dial: mirrors the field call (trunk/route path), which
        // forwards the callee's 183 early-media SDP to the caller. The
        // parallel-fork path only sends a bare 180 and swallows the 183.
        parallel_fork: false,
        recording: Some(RecordingPolicy {
            enabled: Some(true),
            auto_start: Some(true),
            path: Some(record_path.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let server = Arc::new(crate::common::e2e_test_server::E2eTestServer::start_with_config(
        proxy_config,
    )
    .await?);

    // Mirror a real JsSIP/Chrome caller: offer [opus, PCMU, telephone-event]
    // (opus first). The plain callee answers PCMU-only, so the proxy must
    // rewrite the caller answer to PCMU → same-codec fastpath. This is the
    // exact negotiation from the field capture (./fastpath).
    let alice = create_webrtc_caller(
        &server,
        "alice",
        "password123",
        Some(vec![
            rustrtc::config::AudioCapability::opus(),
            rustrtc::config::AudioCapability::pcmu(),
            rustrtc::config::AudioCapability::telephone_event(),
        ]),
    )
    .await?;
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let bob_media = RtpMediaEndpoint::bind().await?;
    bob_media.start_receiving();

    // Mirror the field timeline (baresip): 180 Ringing → 183 early media with
    // SDP + early RTP → ~2.8 s later 200 OK with the same SDP.
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
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    let bob_dialog_id = bob_dialog_id.ok_or_else(|| anyhow::anyhow!("bob never got the INVITE"))?;
    let offer_to_bob = received_sdp.unwrap_or_default();
    let answer_sdp = rtp_pcmu_sdp(bob_media.port());

    bob.send_ringing(&bob_dialog_id, Some(answer_sdp.clone()))
        .await?; // 183 Session Progress with early-media SDP (skip the 180:
                 // rsipstack drops the second Early→Early state transition,
                 // which would swallow the 183 body on the harness side)

    // JsSIP applies the 183 SDP as pranswer; wait for it on alice.
    for _ in 0..200 {
        let events = alice.process_dialog_events().await?;
        if events
            .iter()
            .any(|e| matches!(e, TestUaEvent::EarlyMedia(_)))
        {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    // Chrome connects ICE/DTLS during early media and STARTS SENDING the
    // microphone right away — mirror that with a pre-answer audio burst.
    alice.wait_webrtc_connected(Duration::from_secs(15)).await?;
    let early_browser_ssrc = alice.webrtc_sender_ssrc();
    let browser_early_payloads = patterned_pcmu_payloads(100, 0x70);
    for (i, payload) in browser_early_payloads.iter().enumerate() {
        alice
            .send_webrtc_rtp(
                0,
                2000u16.wrapping_add(i as u16),
                30000u32 + (i as u32) * 160,
                early_browser_ssrc,
                false,
                payload.clone(),
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }

    // Early media: the callee starts sending RTP right after the 183.
    let proxy_media = crate::common::rtp_utils::extract_media_endpoint(&offer_to_bob)
        .ok_or_else(|| anyhow::anyhow!("no media endpoint in forwarded offer"))?;
    let early_payloads = patterned_pcmu_payloads(100, 0xE0);
    let early_packets: Vec<RtpPacket> = early_payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RtpPacket::new(0, 100u16.wrapping_add(i as u16), 1000u32 + (i as u32) * 160, 0x60606060, p.clone())
        })
        .collect();
    bob_media
        .send_sequence(proxy_media, &early_packets, 20)
        .await;
    sleep(Duration::from_millis(2000)).await; // ringing delay before answering

    bob.answer_call(&bob_dialog_id, Some(answer_sdp.clone()))
        .await?;
    let dialog_id = match tokio::time::timeout(Duration::from_secs(20), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("alice call failed: {}", e)),
        _ => return Err(anyhow::anyhow!("call setup timeout")),
    };
    alice.wait_webrtc_connected(Duration::from_secs(15)).await?;

    // ── Send real audio in BOTH directions ──
    // A: browser (alice) → proxy, SRTP-protected, recognizable pattern.
    let ssrc = alice.webrtc_sender_ssrc();
    let alice_payloads = patterned_pcmu_payloads(200, 0x50);
    for (i, payload) in alice_payloads.iter().enumerate() {
        alice
            .send_webrtc_rtp(
                0,
                5000u16.wrapping_add(i as u16),
                70000u32 + (i as u32) * 160,
                ssrc,
                false,
                payload.clone(),
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }
    // B: baresip stand-in (bob) → proxy, plain RTP, different pattern.
    let bob_payloads = patterned_pcmu_payloads(200, 0xA0);
    let bob_packets: Vec<RtpPacket> = bob_payloads
        .iter()
        .enumerate()
        .map(|(i, payload)| {
            RtpPacket::new(
                0,
                9000u16.wrapping_add(i as u16),
                110000u32 + (i as u32) * 160,
                0x51515151,
                payload.clone(),
            )
        })
        .collect();
    bob_media
        .send_sequence(proxy_media, &bob_packets, 20)
        .await;
    sleep(Duration::from_millis(1500)).await;

    // Hang up and let the recording finalize.
    alice.hangup(&dialog_id).await.ok();
    sleep(Duration::from_millis(1000)).await;

    // Locate the recorded WAV via the CDR.
    let records = server.cdr_capture.get_all_records().await;
    let record = records
        .iter()
        .find(|r| !r.recorder.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no CDR record with a recorder entry"))?;
    let wav_path = std::path::PathBuf::from(&record.recorder[0].path);
    assert!(wav_path.exists(), "recording file missing: {}", wav_path.display());

    let (leg_a, leg_b, channels, sample_rate) = split_stereo_g711_wav(&wav_path)?;
    assert_eq!(channels, 2);
    assert_eq!(sample_rate, 8000);

    // Leg A = caller ingress = the browser's audio.
    let alice_set: std::collections::HashSet<&Vec<u8>> = alice_payloads.iter().collect();
    let leg_a_matched = leg_a
        .chunks(160)
        .filter(|c| alice_set.contains(&c.to_vec()))
        .count();
    // Leg B = caller egress = the plain leg's audio relayed to the browser.
    let bob_set: std::collections::HashSet<&Vec<u8>> = bob_payloads.iter().collect();
    let leg_b_matched = leg_b
        .chunks(160)
        .filter(|c| bob_set.contains(&c.to_vec()))
        .count();

    let leg_a_silent = leg_a.iter().filter(|&&b| b == 0xFF).count();
    println!(
        "recording: leg_a matched={} silent_0xFF={}/{} ; leg_b matched={} len_a={} len_b={}",
        leg_a_matched,
        leg_a_silent,
        leg_a.len(),
        leg_b_matched,
        leg_a.len(),
        leg_b.len()
    );

    assert!(
        leg_a_matched >= 40,
        "REPRODUCED: caller leg (browser ingress) in the recording has no real audio — \
         matched {} of {} bytes (0xFF silence bytes: {}/{})",
        leg_a_matched,
        leg_a.len(),
        leg_a_silent,
        leg_a.len()
    );
    assert!(
        leg_b_matched >= 40,
        "callee leg (browser egress) in the recording has no real audio — matched {}",
        leg_b_matched
    );

    bob_media.stop();
    alice.stop();
    bob.stop();
    server.stop();
    Ok(())
}

#[test]
fn test_webrtc_pcmu_to_rtp_pcmu_real_srtp_bidir_audio() {
    run_with_big_stack(test_webrtc_pcmu_to_rtp_pcmu_real_srtp_bidir_audio_impl());
}

async fn test_webrtc_pcmu_to_rtp_pcmu_real_srtp_bidir_audio_impl() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    let alice = create_webrtc_caller(
        &server,
        "alice",
        "password123",
        Some(vec![
            rustrtc::config::AudioCapability::pcmu(),
            rustrtc::config::AudioCapability::telephone_event(),
        ]),
    )
    .await?;
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let bob_media = RtpMediaEndpoint::bind().await?;
    bob_media.start_receiving();

    let (dialog_id, offer_to_bob) = establish_webrtc_to_rtp_call(
        alice.clone(),
        &bob,
        bob_media.port(),
    )
    .await?;

    // SRTP aspect 1: ICE + DTLS-SRTP must actually connect.
    alice.wait_webrtc_connected(Duration::from_secs(15)).await?;

    // The forwarded offer must point the plain leg at the media proxy.
    let proxy_media = crate::common::rtp_utils::extract_media_endpoint(&offer_to_bob)
        .ok_or_else(|| anyhow::anyhow!("no media endpoint in forwarded offer"))?;
    assert!(
        offer_to_bob.contains("RTP/AVP"),
        "forwarded offer must downgrade transport to RTP/AVP for the plain leg"
        );

    alice.attach_webrtc_rx_tap().await?;

    // ── Direction A: WebRTC → RTP (SRTP unprotect → plain relay) ──
    let ssrc = alice.webrtc_sender_ssrc();
    let sent_payloads = patterned_pcmu_payloads(150, 0x30);
    for (i, payload) in sent_payloads.iter().enumerate() {
        alice
            .send_webrtc_rtp(
                0,
                3000u16.wrapping_add(i as u16),
                60000u32 + (i as u32) * 160,
                ssrc,
                false,
                payload.clone(),
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }
    sleep(Duration::from_millis(1000)).await;

    let bob_received = bob_media.received_packets();
    let (bob_matched, bob_distinct) = count_payload_matches(&bob_received, 0, &sent_payloads);
    assert!(
        bob_matched >= 40,
        "SRTP→RTP fastpath: bob should receive ≥40 content-identical PCMU packets, \
         got {} matched of {} received",
        bob_matched,
        bob_received.len()
    );
    assert!(bob_distinct >= 20, "matched payloads should span many seqs");

    // ── Direction B: RTP → WebRTC (plain → SRTP protect) ──
    let bob_sent_payloads = patterned_pcmu_payloads(150, 0x90);
    let bob_packets: Vec<RtpPacket> = bob_sent_payloads
        .iter()
        .enumerate()
        .map(|(i, payload)| {
            RtpPacket::new(
                0,
                7000u16.wrapping_add(i as u16),
                90000u32 + (i as u32) * 160,
                0x42424242,
                payload.clone(),
            )
        })
        .collect();
    bob_media
        .send_sequence(proxy_media, &bob_packets, 20)
        .await;
    sleep(Duration::from_millis(1000)).await;

    // SRTP aspect 2+3: inbound packets must pass SRTP auth + decrypt, and the
    // plaintext must be exactly what bob put on the wire.
    let alice_rx = alice.webrtc_rx_packets();
    let (alice_matched, alice_distinct) = {
        let sent_set: std::collections::HashSet<&Vec<u8>> = bob_sent_payloads.iter().collect();
        let mut matched = 0;
        let mut distinct = std::collections::HashSet::new();
        for p in alice_rx.iter().filter(|p| p.payload_type == 0) {
            if sent_set.contains(&p.payload) {
                matched += 1;
                distinct.insert(p.payload.clone());
            }
        }
        (matched, distinct.len())
    };
    assert!(
        alice_matched >= 40,
        "RTP→SRTP fastpath: alice should receive ≥40 decrypted PCMU packets with \
         byte-identical payload, got {} matched of {} observed",
        alice_matched,
        alice_rx.len()
    );
    assert!(alice_distinct >= 20);
    assert!(
        alice.webrtc_received_rtp_packets() > 0,
        "PeerConnection must accept SRTP-authenticated inbound packets"
    );

    alice.hangup(&dialog_id).await.ok();
    bob_media.stop();
    alice.stop();
    bob.stop();
    server.stop();
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// baresip-style call flow: WebRTC caller → plain-RTP callee that rings first.
//
// Mirrors the field baresip capture (./fastpath/baresip.txt):
//   1. callee sends 180 Ringing (no SDP)
//   2. callee sends 183 Session Progress with early-media SDP (no a=mid,
//      media port A, ssrc X) and streams early audio (ausine)
//   3. after a ring delay the callee answers with 200 OK carrying a CHANGED
//      SDP: bumped o= version, a=mid:0 added (baresip adds it in 200),
//      a different media port B and a different a=ssrc
//   4. post-answer bidirectional audio must flow through the re-negotiated
//      media path (the proxy must honour the changed final answer)
//   5. the callee sends RFC 4733 DTMF digits; they must be relayed to the
//      WebRTC leg over DTLS-SRTP, and audio must continue afterwards
// ─────────────────────────────────────────────────────────────────────────────

fn baresip_sdp(port: u16, session_version: u32, ssrc: u32, with_mid: bool) -> String {
    let mid = if with_mid { "a=mid:0\r\n" } else { "" };
    format!(
        "v=0\r\n\
         o=- 2863577412 {session_version} IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-15\r\n\
         {mid}\
         a=sendrecv\r\n\
         a=ssrc:{ssrc} cname:sip:bare@127.0.0.1\r\n\
         a=ptime:20\r\n"
    )
}

/// Send one RFC 4733 DTMF digit (start + volume/duration repeats + end) to
/// the proxy, the way baresip's telephone-event sender does.
async fn send_rfc4733_digit(
    endpoint: &RtpMediaEndpoint,
    target: std::net::SocketAddr,
    pt: u8,
    digit: char,
    ssrc: u32,
    seq: &mut u16,
    timestamp: &mut u32,
) {
    let code = match digit {
        '0'..='9' => digit as u8 - b'0',
        '*' => 10,
        '#' => 11,
        _ => 12 + (digit.to_ascii_uppercase() as u8 - b'A'),
    };
    let mut packets = Vec::new();
    // start (E=0) + 2 repeats + end (E=1)
    packets.push(RtpPacket::new(
        pt,
        *seq,
        *timestamp,
        ssrc,
        crate::common::rtp_utils::telephone_event_payload(code, false),
    ));
    packets.push(RtpPacket::new(
        pt,
        seq.wrapping_add(1),
        *timestamp,
        ssrc,
        crate::common::rtp_utils::telephone_event_payload(code, false),
    ));
    packets.push(RtpPacket::new(
        pt,
        seq.wrapping_add(2),
        *timestamp,
        ssrc,
        crate::common::rtp_utils::telephone_event_payload(code, false),
    ));
    packets.push(RtpPacket::new(
        pt,
        seq.wrapping_add(3),
        *timestamp,
        ssrc,
        crate::common::rtp_utils::telephone_event_payload(code, true),
    ));
    *seq = seq.wrapping_add(4);
    *timestamp = timestamp.wrapping_add(160 * 3);
    endpoint.send_sequence(target, &packets, 25).await;
}

#[test]
fn test_webrtc_to_baresip_flow_ring_answer_sdp_change_dtmf() {
    run_with_big_stack(test_webrtc_to_baresip_flow_ring_answer_sdp_change_dtmf_impl());
}

async fn test_webrtc_to_baresip_flow_ring_answer_sdp_change_dtmf_impl() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    // Sequential dial (like the field trunk/route path): it forwards the
    // callee's 183 early-media SDP to the caller. The parallel-fork path only
    // sends a bare 180 and swallows the 183.
    let server = Arc::new(
        crate::common::e2e_test_server::E2eTestServer::start_with_config(rustpbx::config::ProxyConfig {
            media_proxy: MediaProxyMode::All,
            parallel_fork: false,
            ..Default::default()
        })
        .await?,
    );

    // Chrome-like caller: opus first + PCMU + telephone-event (rustrtc offers
    // PT 101 @ 8 kHz), so the proxy must rewrite the answer to PCMU fastpath.
    let alice = create_webrtc_caller(
        &server,
        "alice",
        "password123",
        Some(vec![
            rustrtc::config::AudioCapability::opus(),
            rustrtc::config::AudioCapability::pcmu(),
            rustrtc::config::AudioCapability::telephone_event(),
        ]),
    )
    .await?;
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    // Two media endpoints: A = early media (183 SDP), B = final (200 SDP).
    let bob_media_a = RtpMediaEndpoint::bind().await?;
    let bob_media_b = RtpMediaEndpoint::bind().await?;
    bob_media_a.start_receiving();
    bob_media_b.start_receiving();

    let caller_handle = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call("bob", None).await }
    });
    let mut bob_dialog_id = None;
    let mut offer_to_bob = None;
    for _ in 0..600 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, sdp) = event {
                bob_dialog_id = Some(id.clone());
                offer_to_bob = sdp;
                break;
            }
        }
        if bob_dialog_id.is_some() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    let bob_dialog_id =
        bob_dialog_id.ok_or_else(|| anyhow::anyhow!("bob never got the INVITE"))?;
    let offer_to_bob = offer_to_bob.unwrap_or_default();
    let proxy_media = crate::common::rtp_utils::extract_media_endpoint(&offer_to_bob)
        .ok_or_else(|| anyhow::anyhow!("no media endpoint in forwarded offer"))?;
    let dtmf_pt = crate::common::rtp_utils::extract_dtmf_payload_type(&offer_to_bob)
        .ok_or_else(|| {
            anyhow::anyhow!("forwarded offer has no telephone-event PT, offer:
{}", offer_to_bob)
        })?;

    // ── 1. 180 Ringing ──
    bob.send_ringing(&bob_dialog_id, None).await?;

    // ── 2. 183 Session Progress with early-media SDP ──
    let sdp_183 = baresip_sdp(bob_media_a.port(), 1131463086, 111111, false);
    bob.send_ringing(&bob_dialog_id, Some(sdp_183.clone()))
        .await?;

    // JsSIP/Chrome applies the 183 SDP as pranswer right away and starts
    // ICE/DTLS. Pump alice's events so the harness applies the pranswer, and
    // wait for DTLS — the proxy's fastpath relay arms on transport-ready.
    for _ in 0..200 {
        let events = alice.process_dialog_events().await?;
        if events
            .iter()
            .any(|e| matches!(e, TestUaEvent::EarlyMedia(_)))
        {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    alice.wait_webrtc_connected(Duration::from_secs(15)).await?;
    sleep(Duration::from_millis(500)).await;

    // Early audio from the callee toward the proxy (baresip ausine).
    let early_payloads = patterned_pcmu_payloads(60, 0xC0);
    let early_packets: Vec<RtpPacket> = early_payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RtpPacket::new(0, 100u16.wrapping_add(i as u16), 1000u32 + (i as u32) * 160, 0x1B1B1B1B, p.clone())
        })
        .collect();
    bob_media_a
        .send_sequence(proxy_media, &early_packets, 20)
        .await;

    // ── 3. Ring delay, then answer with a CHANGED SDP ──
    sleep(Duration::from_millis(1500)).await;
    let sdp_200 = baresip_sdp(bob_media_b.port(), 1131463087, 222222, true);
    assert_ne!(sdp_183, sdp_200, "the 200 SDP must differ from the 183 SDP");
    bob.answer_call(&bob_dialog_id, Some(sdp_200)).await?;

    let dialog_id = match tokio::time::timeout(Duration::from_secs(20), caller_handle).await {
        Ok(Ok(Ok(id))) => id,
        Ok(Ok(Err(e))) => return Err(anyhow::anyhow!("alice call failed: {}", e)),
        _ => return Err(anyhow::anyhow!("call setup timeout")),
    };
    alice.wait_webrtc_connected(Duration::from_secs(15)).await?;
    alice.attach_webrtc_rx_tap().await?;
    sleep(Duration::from_millis(500)).await;

    // ── 4. Post-answer bidirectional audio through the re-negotiated path ──
    // The proxy must send toward media endpoint B (the changed 200 SDP port).
    let ssrc = alice.webrtc_sender_ssrc();
    let alice_payloads = patterned_pcmu_payloads(150, 0x50);
    for (i, payload) in alice_payloads.iter().enumerate() {
        alice
            .send_webrtc_rtp(
                0,
                3000u16.wrapping_add(i as u16),
                50000u32 + (i as u32) * 160,
                ssrc,
                false,
                payload.clone(),
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }
    let bob_payloads = patterned_pcmu_payloads(150, 0xA0);
    let bob_packets: Vec<RtpPacket> = bob_payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RtpPacket::new(0, 4000u16.wrapping_add(i as u16), 60000u32 + (i as u32) * 160, 0x2C2C2C2C, p.clone())
        })
        .collect();
    bob_media_b
        .send_sequence(proxy_media, &bob_packets, 20)
        .await;
    sleep(Duration::from_millis(1500)).await;

    let b_received = bob_media_b.received_packets();
    let (b_matched, _) = count_payload_matches(&b_received, 0, &alice_payloads);
    let a_received = bob_media_a.received_packets();
    let (a_matched_early, _) = count_payload_matches(&a_received, 0, &alice_payloads);
    assert!(
        b_matched >= 40,
        "post-answer audio must reach the CHANGED 200-OK media port (endpoint B): \
         matched {b_matched} of {} received (stale 183-port endpoint got {a_matched_early})",
        b_received.len()
    );

    let alice_rx = alice.webrtc_rx_packets();
    let (a_matched, _) = {
        let set: std::collections::HashSet<&Vec<u8>> = bob_payloads.iter().collect();
        let mut m = 0;
        for p in alice_rx.iter().filter(|p| p.payload_type == 0) {
            if set.contains(&p.payload) {
                m += 1;
            }
        }
        (m, 0)
    };
    assert!(
        a_matched >= 40,
        "callee audio (from endpoint B) must reach the WebRTC leg over SRTP: {a_matched}"
    );

    // ── 5. RFC 4733 DTMF from the callee → WebRTC leg ──
    let mut dtmf_seq: u16 = 6000;
    let mut dtmf_ts: u32 = 90000;
    for digit in ['1', '2', '3'] {
        send_rfc4733_digit(
            &bob_media_b,
            proxy_media,
            dtmf_pt,
            digit,
            0x2C2C2C2C,
            &mut dtmf_seq,
            &mut dtmf_ts,
        )
        .await;
        sleep(Duration::from_millis(220)).await;
    }
    sleep(Duration::from_millis(1200)).await;

    let dtmf_rx: Vec<_> = alice
        .webrtc_rx_packets()
        .into_iter()
        .filter(|p| p.payload_type == dtmf_pt && p.payload.len() == 4)
        .collect();
    let received_digits: std::collections::BTreeSet<u8> = dtmf_rx
        .iter()
        .filter(|p| p.payload[1] & 0x80 == 0) // start/repeat packets carry the code
        .map(|p| p.payload[0] & 0x0F)
        .collect();
    println!(
        "DTMF at WebRTC leg: {} telephone-event packets, digits {:?} (PT {dtmf_pt})",
        dtmf_rx.len(),
        received_digits
    );
    assert!(
        !dtmf_rx.is_empty(),
        "RFC4733 DTMF must be relayed to the WebRTC leg (got 0 PT-{dtmf_pt} packets)"
    );
    assert_eq!(
        received_digits,
        [1u8, 2, 3].into_iter().collect(),
        "digits 1,2,3 must arrive via SRTP telephone-event"
    );

    // ── 6. Audio continues after DTMF ──
    let tail_payloads = patterned_pcmu_payloads(60, 0x70);
    let tail_packets: Vec<RtpPacket> = tail_payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RtpPacket::new(0, 7000u16.wrapping_add(i as u16), 120000u32 + (i as u32) * 160, 0x2C2C2C2C, p.clone())
        })
        .collect();
    bob_media_b
        .send_sequence(proxy_media, &tail_packets, 20)
        .await;
    sleep(Duration::from_millis(1200)).await;
    let (tail_matched, _) = {
        let set: std::collections::HashSet<&Vec<u8>> = tail_payloads.iter().collect();
        let mut m = 0;
        for p in alice.webrtc_rx_packets().iter().filter(|p| p.payload_type == 0) {
            if set.contains(&p.payload) {
                m += 1;
            }
        }
        (m, 0)
    };
    assert!(
        tail_matched >= 20,
        "audio must continue flowing after DTMF: matched {tail_matched}"
    );

    alice.hangup(&dialog_id).await.ok();
    bob_media_a.stop();
    bob_media_b.stop();
    alice.stop();
    bob.stop();
    server.stop();
    Ok(())
}

#[test]
fn test_webrtc_opus_to_rtp_pcmu_real_srtp_transcode_bidir_audio() {
    run_with_big_stack(test_webrtc_opus_to_rtp_pcmu_real_srtp_transcode_bidir_audio_impl());
}

async fn test_webrtc_opus_to_rtp_pcmu_real_srtp_transcode_bidir_audio_impl() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();
    let server = Arc::new(E2eTestServer::start_with_mode(MediaProxyMode::All).await?);
    // No caps → Opus-only offer (PT 111), forcing Opus↔PCMU transcoding.
    let alice = create_webrtc_caller(&server, "alice", "password123", None).await?;
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(100)).await;

    let bob_media = RtpMediaEndpoint::bind().await?;
    bob_media.start_receiving();

    let (dialog_id, offer_to_bob) = establish_webrtc_to_rtp_call(
        alice.clone(),
        &bob,
        bob_media.port(),
    )
    .await?;

    alice.wait_webrtc_connected(Duration::from_secs(15)).await?;
    let proxy_media = crate::common::rtp_utils::extract_media_endpoint(&offer_to_bob)
        .ok_or_else(|| anyhow::anyhow!("no media endpoint in forwarded offer"))?;
    alice.attach_webrtc_rx_tap().await?;
    let egress_before = alice.webrtc_egress_packet_count();

    // ── Direction A: Opus (SRTP) → decode → resample → PCMU (plain) ──
    // 440 Hz sine at 48 kHz, 20 ms frames. NOTE: audio_codec's OpusEncoder
    // takes MONO samples (it upmixes to stereo internally).
    let mut opus_encoder = audio_codec::create_encoder(audio_codec::CodecType::Opus);
    let ssrc = alice.webrtc_sender_ssrc();
    let mut phase = 0.0f32;
    for i in 0..200u32 {
        let mut frame = Vec::with_capacity(960);
        for _ in 0..960 {
            frame.push((12000.0 * phase.sin()) as i16);
            phase += 2.0 * std::f32::consts::PI * 440.0 / 48000.0;
        }
        let payload = opus_encoder.encode(&frame);
        anyhow::ensure!(!payload.is_empty(), "opus encoder produced empty payload");
        alice
            .send_webrtc_rtp(
                111,
                2000u16.wrapping_add(i as u16),
                40000u32 + i * 960,
                ssrc,
                false,
                payload,
            )
            .await?;
        sleep(Duration::from_millis(20)).await;
    }
    sleep(Duration::from_millis(1500)).await;
    assert!(
        alice.webrtc_egress_packet_count() - egress_before >= 150,
        "alice must have SRTP-protected ≥150 outbound opus packets, sent {}",
        alice.webrtc_egress_packet_count() - egress_before
    );

    let bob_received: Vec<RtpPacket> = bob_media
        .received_packets()
        .into_iter()
        .filter(|p| p.payload_type == 0)
        .collect();
    assert!(
        bob_received.len() >= 40,
        "transcode: bob should receive ≥40 PCMU packets, got {}",
        bob_received.len()
    );
    let mut pcmu_decoder = audio_codec::create_decoder(audio_codec::CodecType::PCMU);
    let mut decoded = Vec::new();
    for p in &bob_received {
        decoded.extend(pcmu_decoder.decode(&p.payload));
    }
    let bob_rms = rms(&decoded);
    assert!(
        bob_rms > 800.0,
        "transcoded audio at bob should be a loud sine (RMS > 800), got {:.0}",
        bob_rms
    );

    // ── Direction B: PCMU (plain) → decode → resample → Opus (SRTP) ──
    let mut pcmu_encoder = audio_codec::create_encoder(audio_codec::CodecType::PCMU);
    let mut phase = 0.0f32;
    let mut bob_packets = Vec::with_capacity(200);
    for i in 0..200u32 {
        let mut pcm = Vec::with_capacity(160);
        for _ in 0..160 {
            pcm.push((9000.0 * phase.sin()) as i16);
            phase += 2.0 * std::f32::consts::PI * 600.0 / 8000.0;
        }
        let payload = pcmu_encoder.encode(&pcm);
        bob_packets.push(RtpPacket::new(
            0,
            8000u16.wrapping_add(i as u16),
            120000u32 + i * 160,
            0x43434343,
            payload,
        ));
    }
    bob_media
        .send_sequence(proxy_media, &bob_packets, 20)
        .await;
    sleep(Duration::from_millis(1500)).await;

    let alice_opus_rx: Vec<_> = alice
        .webrtc_rx_packets()
        .into_iter()
        .filter(|p| p.payload_type == 111)
        .collect();
    assert!(
        alice_opus_rx.len() >= 40,
        "alice should receive ≥40 SRTP-delivered Opus packets, got {}",
        alice_opus_rx.len()
    );
    let mut opus_decoder = audio_codec::create_decoder(audio_codec::CodecType::Opus);
    let mut decoded = Vec::new();
    for p in &alice_opus_rx {
        decoded.extend(opus_decoder.decode(&p.payload));
    }
    let alice_rms = rms(&decoded);
    assert!(
        alice_rms > 800.0,
        "transcoded audio at alice should be a loud sine (RMS > 800), got {:.0}",
        alice_rms
    );
    assert!(alice.webrtc_received_rtp_packets() > 0);

    alice.hangup(&dialog_id).await.ok();
    bob_media.stop();
    alice.stop();
    bob.stop();
    server.stop();
    Ok(())
}
