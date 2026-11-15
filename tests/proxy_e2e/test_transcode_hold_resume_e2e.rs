//! E2E regression: hold → unhold on a G729↔PCMA **transcoded** bridge must
//! keep the resumed downlink audio intact.
//!
//! Production incident 2026-11-13 (cluster call `c8935420…`): a G729↔PCMA
//! transcode bridge carried clean audio for 25 s; the agent then held and
//! resumed the call, and the far end (the customer) heard broadband noise
//! afterwards. Server-side evidence: hold detaches the bridge route
//! (`unbridge`, hold music takes the egress) and resume rebuilds it
//! (`transcoding activated` a second time — fresh decoders attaching to both
//! RTP streams mid-stream).
//!
//! This test reproduces the same sequence at the proxy layer with a G729
//! caller and a PCMA callee (matching the production codec split), triggers
//! hold/unhold exactly like the production flow (the agent side sends a
//! sendonly then a sendrecv in-dialog re-INVITE), and asserts the **downlink
//! audio content** each party receives after the rebuild: the far party's
//! pure tone, not noise.

use crate::common::audio_analysis::{dominant_frequency, rms};
use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, extract_media_endpoint};
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use audio_codec::{CodecType, create_decoder, create_encoder};
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

// Keep the unused-marker import honest without a bare `use X as _` clash.
#[allow(unused_imports)]

const CALLER_TONE_HZ: f64 = 400.0; // caller feeds G729
const CALLEE_TONE_HZ: f64 = 800.0; // callee feeds PCMA
const SAMPLE_RATE: u32 = 8000;
const RMS_FLOOR: f64 = 800.0;
const FREQ_TOLERANCE_HZ: f64 = 60.0;

/// Talk 1.5 s → HOLD 2.8 s (matches the production 2.7 s hold) → talk 2.5 s.
const TONE_PACKETS: usize = 600; // 600 × 20 ms = 12 s, longer than the call

fn tone_payload(codec: CodecType, freq_hz: f64) -> Vec<u8> {
    let samples_per_frame = (SAMPLE_RATE / 50) as usize;
    let pcm: Vec<i16> = (0..samples_per_frame)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            (8000.0 * (2.0 * std::f64::consts::PI * freq_hz * t).sin()) as i16
        })
        .collect();
    create_encoder(codec).encode(&pcm).to_vec()
}

/// RTP receiver that keeps every audio packet (payload included) so the test
/// can assert the *decoded content* of the downlink in a given phase.
struct RtpLink {
    socket: Arc<tokio::net::UdpSocket>,
    audio: Arc<Mutex<Vec<RtpPacket>>>,
}

impl RtpLink {
    async fn bind() -> Result<Self> {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        let audio = Arc::new(Mutex::new(Vec::new()));
        let (rx_socket, rx_audio) = (Arc::clone(&socket), Arc::clone(&audio));
        rustpbx::utils::spawn(async move {
            let mut buf = vec![0u8; 1500];
            loop {
                match rx_socket.recv_from(&mut buf).await {
                    Ok((len, _)) => {
                        if let Ok(packet) = RtpPacket::decode(&buf[..len]) {
                            if packet.payload.len() > 2 {
                                rx_audio.lock().unwrap().push(packet);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self { socket, audio })
    }

    fn local_port(&self) -> Result<u16> {
        Ok(self.socket.local_addr()?.port())
    }

    fn start_tone(&self, target: SocketAddr, payload: Vec<u8>, pt: u8, ssrc: u32, interval_ms: u64) {
        let socket = Arc::clone(&self.socket);
        rustpbx::utils::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let samples_per_frame = (SAMPLE_RATE / 50) as u32;
            for i in 0..TONE_PACKETS {
                interval.tick().await;
                let packet = RtpPacket::new(
                    pt,
                    (i + 1) as u16,
                    (i as u32).wrapping_mul(samples_per_frame),
                    ssrc,
                    payload.clone(),
                );
                if socket.send_to(&packet.encode(), target).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Decode the audio packets whose receive order falls in `[from, to)` and
    /// return the concatenated PCM.
    fn decoded_window(&self, codec: CodecType, from: f64, to: f64) -> Vec<i16> {
        let all = self.audio.lock().unwrap();
        let start = ((all.len() as f64) * from) as usize;
        let end = ((all.len() as f64) * to) as usize;
        let mut decoder = create_decoder(codec);
        let mut pcm = Vec::new();
        for packet in &all[start..end.max(start)] {
            pcm.extend_from_slice(&decoder.decode(&packet.payload));
        }
        pcm
    }

    fn total_received(&self) -> usize {
        self.audio.lock().unwrap().len()
    }
}

/// The recorded call timeline (fractions of each downlink's packet history):
///   0.0 talk → 1.5 HOLD(callee re-INVITE sendonly) → 4.3 UNHOLD(sendrecv) → 6.8 hangup
const PRE_WINDOW: (f64, f64) = (0.35, 0.45);
const POST_WINDOW: (f64, f64) = (0.70, 0.90);

fn assert_tone(samples: &[i16], freq: f64, label: &str) {
    assert!(
        rms(samples) > RMS_FLOOR,
        "{label}: rms {:.0} below floor {RMS_FLOOR} — silence/garbage",
        rms(samples)
    );
    let (peak, ratio) = dominant_frequency(samples, SAMPLE_RATE, &[freq]);
    assert!(
        (peak - freq).abs() <= FREQ_TOLERANCE_HZ && ratio > 0.5,
        "{label}: dominant {peak:.0} Hz (share {ratio:.2}) — expected {freq:.0} Hz; \
         the resumed downlink must carry the far party's tone, not noise"
    );
}

#[tokio::test]
async fn test_transcode_hold_resume_keeps_downlink_audio_intact() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let proxy_config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        ..Default::default()
    };
    let server = Arc::new(E2eTestServer::start_with_config(proxy_config).await?);

    let caller_media = RtpLink::bind().await?; // G729 side
    let callee_media = RtpLink::bind().await?; // PCMA side

    // Caller = a real WebRTC (DTLS-SRTP) UA advertising G729 only, exactly
    // like the production restsend rtc leg that negotiated G729.
    let alice_config = TestUaConfig {
        webrtc: true,
        username: "alice".into(),
        password: "password123".into(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27420),
        proxy_addr: server.proxy_addr,
    };
    let mut alice_ua = TestUa::new_webrtc_with_caps(
        alice_config,
        vec![
            rustrtc::config::AudioCapability::g729(),
            rustrtc::config::AudioCapability::telephone_event(),
        ],
    );
    alice_ua.start().await?;
    alice_ua.register().await?;
    let alice = Arc::new(alice_ua);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(150)).await;

    let caller_handle = rustpbx::utils::spawn({
        let a = alice.clone();
        async move { a.make_call("bob", None).await }
    });

    let mut bob_dialog = None;
    let mut pbx_callee_endpoint: Option<SocketAddr> = None;
    for _ in 0..60 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                // The PBX-side callee offer must be PCMA (bridged from the
                // G729 caller), answered with PCMA-only media.
                pbx_callee_endpoint = offer.as_deref().and_then(extract_media_endpoint);
                let answer = format!(
                    "v=0\r\n\
                     o=- 2 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                     m=audio {} RTP/AVP 8 101\r\n\
                     a=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
                    callee_media.local_port()?
                );
                bob.set_answer_sdp(&id, &answer).await;
                bob.answer_call(&id, Some(answer)).await?;
                bob_dialog = Some(id);
                break;
            }
        }
        if bob_dialog.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let bob_dialog = bob_dialog.expect("bob must receive the call");
    let pbx_callee_endpoint =
        pbx_callee_endpoint.expect("INVITE offer must carry the PBX callee media endpoint");

    let alice_dialog = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .expect("call setup timed out")
        .expect("alice make_call failed")
        .expect("call must be established");

    let answer_sdp = alice
        .get_negotiated_answer_sdp(&alice_dialog)
        .await
        .expect("caller answer SDP must be negotiated");
    // The WebRTC caller's downlink must be negotiated as G729.
    assert!(
        answer_sdp.contains("G729"),
        "caller answer must negotiate G729, got: {answer_sdp}"
    );
    alice
        .attach_webrtc_rx_tap()
        .await
        .expect("webrtc rx tap");
    let _ = extract_media_endpoint(&answer_sdp);

    // Caller streams G729-encoded 400 Hz through the DTLS-SRTP transport;
    // callee streams PCMA-encoded 800 Hz.
    let g729_payload = tone_payload(CodecType::G729, CALLER_TONE_HZ);
    rustpbx::utils::spawn({
        let alice = alice.clone();
        async move {
            let samples_per_frame = (SAMPLE_RATE / 50) as u32;
            for i in 0..TONE_PACKETS {
                alice
                    .send_webrtc_rtp(
                        18,
                        (i + 1) as u16,
                        (i as u32).wrapping_mul(samples_per_frame),
                        0x1111_0001,
                        false,
                        g729_payload.clone(),
                    )
                    .await
                    .ok();
                sleep(Duration::from_millis(20)).await;
            }
        }
    });
    callee_media.start_tone(
        pbx_callee_endpoint,
        tone_payload(CodecType::PCMA, CALLEE_TONE_HZ),
        8,
        0x2222_0002,
        20,
    );
    sleep(Duration::from_millis(1500)).await;

    // Baseline: the transcode bridge carries both tones across.
    assert!(
        callee_media.total_received() > 30,
        "callee must receive transcoded downlink (got {})",
        callee_media.total_received()
    );
    let pre_callee = callee_media.decoded_window(CodecType::PCMA, PRE_WINDOW.0, PRE_WINDOW.1);
    assert_tone(&pre_callee, CALLER_TONE_HZ, "pre-hold callee downlink (400Hz)");

    // ── HOLD: the agent side sends a sendonly re-INVITE (production flow) ──
    let hold_sdp = format!(
        "v=0\r\n\
         o=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {} RTP/AVP 8 101\r\n\
         a=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendonly\r\n",
        callee_media.local_port()?
    );
    bob.send_reinvite(&bob_dialog, Some(hold_sdp.clone())).await?;
    sleep(Duration::from_millis(2800)).await;

    // ── UNHOLD: sendrecv re-INVITE → bridge route rebuilt ─────────────────
    let unhold_sdp = hold_sdp.replace("a=sendonly", "a=sendrecv").replace("o=- 2 2", "o=- 2 3");
    bob.send_reinvite(&bob_dialog, Some(unhold_sdp)).await?;
    sleep(Duration::from_millis(2500)).await;

    alice.hangup(&alice_dialog).await?;
    sleep(Duration::from_millis(300)).await;

    // ── Downlink content assertions ───────────────────────────────────────
    // The callee's downlink after the rebuild must be the caller's 400 Hz
    // tone (G729→PCMA transcoded again), NOT noise. The production defect
    // surfaced exactly in this window.
    let post_callee = callee_media.decoded_window(CodecType::PCMA, POST_WINDOW.0, POST_WINDOW.1);
    assert!(
        callee_media.total_received() > 60,
        "callee must keep receiving downlink (got {})",
        callee_media.total_received()
    );
    assert_tone(
        &post_callee,
        CALLER_TONE_HZ,
        "post-resume callee downlink (400Hz via rebuilt transcode)",
    );

    // Symmetric direction: the caller's WebRTC downlink after the rebuild
    // must be the callee's 800 Hz tone (PCMA→G729).
    let rx = alice.webrtc_rx_packets();
    assert!(
        rx.len() > 60,
        "caller must keep receiving downlink (got {} packets)",
        rx.len()
    );
    let mut decoder = create_decoder(CodecType::G729);
    // Post-resume = last ~2.5 s of received packets (50 pps).
    let start = rx.len().saturating_sub(125);
    let mut pcm = Vec::new();
    for packet in &rx[start..] {
        pcm.extend_from_slice(&decoder.decode(&packet.payload));
    }
    assert_tone(
        &pcm,
        CALLEE_TONE_HZ,
        "post-resume caller downlink (800Hz via rebuilt transcode, webrtc)",
    );

    server.stop();
    Ok(())
}
