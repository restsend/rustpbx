//! Integration tests for the new media layer: exercises [`MediaBridge`],
//! [`Leg`] SDP exchange, bridging, playback, and recorder wiring together
//! (loopback, no SIP). Validates that the new modules compose correctly.
//!
//! The full RTP end-to-end matrix (fast-path relay, transcoding, recording
//! content, DTMF) lives in `rtp_transport_tests.rs` (TestMediaHarness).

use rustpbx_media::audio_source::FileAudioSource;
use rustpbx_media::ingress_tap::PacketDirection;
use rustpbx_media::leg::{LegConfig, LegInner};
use rustpbx_media::media_bridge::{BridgeOpts, LegSide, MediaBridge};
use rustpbx_media::media_recorder::SipflowRecorder;
use rustpbx_media::negotiate;
use rustpbx_media::recorder::Leg;

/// Two RTP/PCMU legs: SDP offer/answer completes and both legs carry a
/// negotiated audio profile.
#[tokio::test]
async fn two_rtp_legs_negotiate_via_sdp() {
    let mut mb = MediaBridge::new("it-sdp", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();

    // a offers, b answers.
    let offer = a.create_offer(vec![]).await.expect("offer");
    assert!(offer.contains("RTP/AVP"), "RTP leg offer must be RTP/AVP");
    let answer = b.answer(&offer).await.expect("answer");
    // a applies the answer.
    a.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply answer");

    // Both legs now have a negotiated audio codec.
    let pa = a.negotiated().expect("a negotiated");
    let pb = b.negotiated().expect("b negotiated");
    assert!(pa.audio.is_some(), "leg a must negotiate an audio codec");
    assert!(pb.audio.is_some(), "leg b must negotiate an audio codec");

    mb.close();
}

/// Bridge two same-codec legs through the A/B model.
#[tokio::test]
async fn bridge_records_symmetric_routes() {
    let mut mb = MediaBridge::new("it-bridge", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;

    // SDP exchange so both legs have negotiated audio profiles.
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer(vec![]).await.unwrap();
    let answer = lb.answer(&offer).await.unwrap();
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();

    // Without accept, bridge() is a no-op (both still gated).
    mb.bridge().await.unwrap();
    assert!(!mb.is_bridged(), "route must not be active while gated");

    // Accept both → relay activates (same codec).
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged(), "route must be active after both accept");

    mb.unbridge().await.unwrap();
    assert!(!mb.is_bridged());
    mb.close();
}

/// Switching a leg's egress source (play/mute) must not panic.
#[tokio::test]
async fn play_then_mute_switches_egress_source() {
    let mut mb = MediaBridge::new("it-play", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    // Generate a 100ms 8kHz mono WAV of silence to play.
    let wav = tempfile_wav_silence(8000, 1, 800); // 100ms
    mb.play(
        LegSide::A,
        Box::new(FileAudioSource::new(wav, false).await.unwrap()),
        false,
    )
    .await
    .unwrap();
    // Let the pacing task emit a few frames.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    mb.mute(LegSide::A).await.unwrap();

    mb.close();
}

/// A SipflowRecorder backend attached via set_recorder receives packets fed
/// through a leg's IngressTap (the RtpObserver path).
#[tokio::test]
async fn sipflow_recorder_receives_ingress_via_tap() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let rec = SipflowRecorder::new(tx);

    let mut mb = MediaBridge::new("it-rec", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.leg(LegSide::A)
        .unwrap()
        .ingress_tap()
        .set_recorder(Some(rec.clone()));

    // Synthesize an ingress packet by calling the tap directly (the real RTP
    // path is covered by the transport tests).
    let pkt = RtpPacket::new(RtpHeader::new(0, 1, 160, 1234), vec![0xFFu8; 80]);
    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    mb.leg(LegSide::A)
        .unwrap()
        .ingress_tap()
        .on_ingress(&pkt, addr);

    let item = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("timed out")
        .expect("no item");
    assert_eq!(item.direction, PacketDirection::Ingress);
    assert_eq!(item.payload_type, 0);

    mb.close();
}

/// DTMF telephone-event RTP packets must reach the SipflowRecorder's channel
/// (not be dropped by the IngressTap's DTMF short-circuit). Without this, the
/// sipflow WAV export has no DTMF data and cannot synthesize keypress tones.
#[tokio::test]
async fn sipflow_recorder_receives_dtmf_rtp_packets() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let rec = SipflowRecorder::new(tx);

    let mut mb = MediaBridge::new("it-dtmf-rec", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    let leg_a = mb.leg(LegSide::A).unwrap();
    let tap = leg_a.ingress_tap();
    tap.set_dtmf_payload_types(vec![101]);
    tap.set_recorder(Some(rec.clone()));

    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

    // Feed a DTMF "5" end-event telephone-event packet (PT 101).
    // RFC 4733 payload: [digit_code=5, end_bit=0x80, volume=10, duration=160].
    let dtmf = RtpPacket::new(RtpHeader::new(101, 1, 0, 1), vec![5u8, 0x80, 10, 0xA0]);
    tap.on_ingress(&dtmf, addr);

    let item = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("timed out waiting for DTMF RTP item")
        .expect("no item");

    // The SipflowRecorder must have received the raw telephone-event packet
    // via write_sample — this is what wav_utils parses to synthesize the tone.
    assert_eq!(item.direction, PacketDirection::Ingress);
    assert_eq!(
        item.payload_type, 101,
        "DTMF telephone-event PT must be preserved"
    );
    // Verify the raw RTP payload contains the RFC 4733 event data.
    assert!(
        !item.raw.is_empty(),
        "raw RTP must be stored for WAV synthesis"
    );

    mb.close();
}

/// DTMF bus fans out a detected digit from a leg's tap, tagged with LegSide.
#[tokio::test]
async fn dtmf_bus_fans_out_digit() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let mut mb = MediaBridge::new("it-dtmf", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.leg(LegSide::A)
        .unwrap()
        .ingress_tap()
        .set_dtmf_payload_types(vec![101]);
    let mut rx = mb.dtmf_bus();

    let pkt = RtpPacket::new(RtpHeader::new(101, 1, 0, 1), vec![1u8, 0x80, 10, 0xA0]);
    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    mb.leg(LegSide::A)
        .unwrap()
        .ingress_tap()
        .on_ingress(&pkt, addr);

    let (side, ev) = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("timed out")
        .expect("no event");
    assert_eq!(side, LegSide::A);
    assert_eq!(ev.digit, '1');

    mb.close();
}

/// detect_transport correctly classifies SDP bodies.
#[test]
fn detect_transport_classification() {
    use rustrtc::TransportMode;
    assert_eq!(
        negotiate::detect_transport("m=audio 1234 RTP/AVP 0\r\n"),
        TransportMode::Rtp
    );
    assert_eq!(
        negotiate::detect_transport(
            "a=fingerprint:sha-256 XX\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n"
        ),
        TransportMode::WebRtc
    );
}

/// Play a file and verify the on_end callback fires when the file completes.
#[tokio::test]
async fn play_file_fires_on_end() {
    let mut mb = MediaBridge::new("it-onend", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply answer");

    // Create a tiny WAV (10ms of silence @8kHz = 80 samples).
    let wav = tempfile_wav_silence(8000, 1, 80);

    // play_file returns a handle whose done resolves on natural EOF.
    let handle = mb
        .play_file(LegSide::A, wav, false)
        .await
        .expect("play_file");

    // Wait for playback to finish (10ms file + margin).
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle.done)
        .await
        .expect("playback must finish")
        .expect("done channel must resolve");
    assert!(!result.interrupted, "natural EOF must not be interrupted");

    mb.close();
}

/// Leg::hold(None) sets egress to silence, resume restores it to silence.
#[tokio::test]
async fn leg_hold_then_resume_switches_egress() {
    let mut mb = MediaBridge::new("it-hold", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    let la = mb.leg(LegSide::A).unwrap();

    // hold without music → silence.
    la.hold(None).await.expect("hold");
    // resume → silence (also sets egress to silence).
    la.resume().await.expect("resume");

    mb.close();
}

/// MediaBridge::hold breaks the route, then MediaBridge::resume re-arms it.
#[tokio::test]
async fn mediabridge_hold_resume_preserves_route() {
    let mut mb = MediaBridge::new("it-hold-route", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;

    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer(vec![]).await.unwrap();
    let answer = lb.answer(&offer).await.unwrap();
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();

    // Bridge + accept both, then hold 'a' → route broken.
    mb.bridge().await.unwrap();
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());
    mb.hold(LegSide::A, None).await.unwrap();
    assert!(!mb.is_bridged(), "hold must break the route");

    // Resume 'a' → route re-armed.
    mb.resume(LegSide::A).await.unwrap();
    assert!(mb.is_bridged(), "resume must re-arm the route");

    mb.close();
}

/// hold with looping music source does not terminate (has_data stays true).
#[tokio::test]
async fn hold_with_music_loops() {
    let mut mb = MediaBridge::new("it-hold-music", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    // Generate a tiny WAV and play it as looping hold music.
    let wav = tempfile_wav_silence(8000, 1, 160);
    let audio = FileAudioSource::new(wav, true).await.expect("source");
    mb.hold(LegSide::A, Some(Box::new(audio)))
        .await
        .expect("hold with music");

    // Let a few ticks pass — no panic means the looping source worked.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    mb.resume(LegSide::A).await.expect("resume");
    mb.close();
}

/// hold_file convenience method works end-to-end.
#[tokio::test]
async fn mediabridge_hold_file_plays_loop() {
    let mut mb = MediaBridge::new("it-hold-file", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let wav = tempfile_wav_silence(8000, 1, 160);
    mb.hold_file(LegSide::A, wav).await.expect("hold_file");

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    mb.resume(LegSide::A).await.expect("resume");
    mb.close();
}

/// Attach a FileRecorder to a leg's tap, feed real non-silence PCMU packets
/// through the RtpObserver, then verify the WAV contains non-silence audio.
#[tokio::test]
async fn file_recorder_writes_wav() {
    use rustpbx_media::media_recorder::FileRecorder;
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let profiles = [
        (
            Leg::A,
            rustpbx_media::negotiate::NegotiatedLegProfile::default(),
        ),
        (
            Leg::B,
            rustpbx_media::negotiate::NegotiatedLegProfile::default(),
        ),
    ];
    let tmp = std::env::temp_dir().join(format!(
        "it_rec_{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = tmp.to_string_lossy().to_string();

    let rec = FileRecorder::start(path.clone(), profiles)
        .await
        .expect("FileRecorder start");

    // Create a leg, attach recorder, feed non-silence PCMU packets.
    let mut mb = MediaBridge::new("it-rec-file", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    let la = mb.leg(LegSide::A).unwrap();
    la.ingress_tap().set_recorder(Some(rec));

    // Alternating µ-law codewords = audible non-silence PCMU (silence is 0xFF).
    let payload: Vec<u8> = (0..80)
        .map(|i| if i % 2 == 0 { 0x55 } else { 0xAA })
        .collect();
    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    for i in 0..10 {
        let pkt = RtpPacket::new(
            RtpHeader::new(0, i + 1, 160 * (i + 1) as u32, 1234),
            payload.clone(),
        );
        la.ingress_tap().on_ingress(&pkt, addr);
    }

    // Stop recording and check file exists with real content.
    la.ingress_tap().finalize_recorder();
    // FileRecorder finalize is fire-and-forget; let its background thread flush
    // the WAV before we read it back.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    mb.close();

    let bytes = std::fs::read(&path).unwrap_or_else(|_| panic!("FileRecorder must create: {path}"));
    assert!(
        bytes.len() >= 44,
        "WAV file must have at least header (44 bytes), got {}",
        bytes.len()
    );
    // G.711 PCMU WAV: the data chunk holds raw µ-law bytes; 0xFF is silence.
    // Assert we captured the non-silence codewords.
    let data = &bytes[44..];
    let non_silence = data.iter().filter(|&&b| b != 0xFF).count();
    assert!(
        non_silence > 0,
        "recorded PCMU data must contain non-silence codewords (got {} bytes, {} non-silence)",
        data.len(),
        non_silence
    );
    let _ = std::fs::remove_file(&path);
}

/// Leg starts as gated; accept() opens the gate.
#[tokio::test]
async fn leg_gate_starts_closed_then_accept_opens() {
    let mut mb = MediaBridge::new("it-gate", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    let la = mb.leg(LegSide::A).unwrap();

    assert!(la.is_gated(), "leg must start gated (not answered)");
    la.accept();
    assert!(!la.is_gated(), "leg must become un-gated after accept");

    mb.close();
}

/// RTP timeout fires when no packets arrive after the leg is accepted.
#[tokio::test]
async fn rtp_timeout_fires_on_inactive_leg() {
    let mut mb = MediaBridge::new("it-rtp-timeout", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("leg A timeout armed");

    // Accept the leg to start the timer.
    mb.accept(LegSide::A).await;

    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut rx)
        .await
        .expect("timeout must fire");
    assert!(res.is_ok(), "RTP timeout must fire on inactive leg");

    mb.close();
}

/// RTP timeout is paused during hold and does NOT fire; resume re-arms it.
#[tokio::test]
async fn rtp_timeout_paused_on_hold_resumes_after() {
    let mut mb = MediaBridge::new("it-rtp-timeout-hold", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("timeout armed");

    // Pause the timeout — it must NOT fire even after the duration passes.
    mb.pause_rtp_timeout(LegSide::A);
    let slept = tokio::time::timeout(std::time::Duration::from_millis(400), &mut rx).await;
    assert!(slept.is_err(), "timeout must NOT fire while paused");

    // Resume with a fresh receiver — it must fire now.
    let mut rx2 = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("re-armed");
    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut rx2)
        .await
        .expect("timeout must fire after resume");
    assert!(res.is_ok(), "RTP timeout must fire after resume");

    mb.close();
}

/// Disarming the timeout drops the sender → a pending receiver gets Canceled.
#[tokio::test]
async fn rtp_timeout_disarm_cancels_receiver() {
    let mut mb = MediaBridge::new("it-rtp-timeout-disarm", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(1000))
        .expect("timeout armed");
    mb.disarm_rtp_timeout(LegSide::A);

    let res = tokio::time::timeout(std::time::Duration::from_millis(500), &mut rx)
        .await
        .expect("receiver must resolve");
    assert!(res.is_err(), "disarm must cancel the receiver");

    mb.close();
}

/// App-level suppression (`set_app_paused`) keeps the timeout from firing even
/// when the leg is re-armed. This is used during a blind-transfer window (new
/// B-leg ringing) where media may legitimately stall. Unpausing lets it fire
/// again.
#[tokio::test]
async fn rtp_timeout_app_paused_suppresses_even_when_rearmed() {
    let mut mb = MediaBridge::new("it-rtp-timeout-app-paused", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("timeout armed");

    // App suppresses the watchdog — it must NOT fire after the duration passes.
    mb.set_app_paused(LegSide::A, true);
    let slept = tokio::time::timeout(std::time::Duration::from_millis(400), &mut rx).await;
    assert!(slept.is_err(), "timeout must NOT fire while app-paused");

    // Re-arm the timer while still suppressed (e.g. a transfer window that
    // re-arms on a fresh leg): the receiver must still NOT fire despite
    // active=true.
    let mut rx2 = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("re-armed while app-paused");
    let slept2 = tokio::time::timeout(std::time::Duration::from_millis(400), &mut rx2).await;
    assert!(slept2.is_err(), "timeout must NOT fire after re-arm while app-paused");

    // App releases the session → the timeout must fire now.
    mb.set_app_paused(LegSide::A, false);
    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut rx2)
        .await
        .expect("timeout must fire after app releases");
    assert!(res.is_ok(), "RTP timeout must fire after app unpauses");

    mb.close();
}

/// RTP timeout must NOT fire while ingress packets keep arriving. The monitor
/// resets its countdown on every new packet; a regression that stops resetting
/// (or double-fires) would tear down active calls. We feed a packet every 50ms
/// for ~500ms against a 300ms timeout and assert the receiver stays pending.
#[tokio::test]
async fn rtp_timeout_does_not_fire_on_active_rtp() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let mut mb = MediaBridge::new("it-rtp-active", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    let tap = mb.leg(LegSide::A).unwrap().ingress_tap().clone();
    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(300))
        .expect("timeout armed");

    // Feed a packet every 50ms for 500ms (well past the 300ms window).
    let start = std::time::Instant::now();
    let mut seq = 1u16;
    while start.elapsed() < std::time::Duration::from_millis(500) {
        let pkt = RtpPacket::new(
            RtpHeader::new(0, seq, 160 * seq as u32, 1234),
            vec![0x00u8; 160],
        );
        tap.on_ingress(&pkt, addr);
        seq = seq.wrapping_add(1);
        // Give the monitor tick a chance to observe the new counter.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "RTP timeout must NOT fire while packets keep arriving (seq={seq})"
        );
    }

    // Stop feeding → the timeout must now fire shortly after the last packet.
    let res = tokio::time::timeout(std::time::Duration::from_secs(2), &mut rx)
        .await
        .expect("timeout must fire after traffic stops");
    assert!(res.is_ok(), "RTP timeout must fire once traffic stops");

    mb.close();
}

/// `MediaBridge::play` pauses the RTP inactivity timeout (the peer may stay
/// silent while a prompt plays); natural EOF resumes it. Verify the timeout
/// does NOT fire during playback, then fires once playback ends and no RTP
/// arrives.
#[tokio::test]
async fn rtp_timeout_paused_during_playback_resumes_on_end() {
    let mut mb = MediaBridge::new("it-rtp-play", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("timeout armed");

    // 400ms file — long enough to outlast the 150ms timeout window.
    let wav = tempfile_wav_silence(8000, 1, 3200);
    let handle = mb
        .play_file(LegSide::A, wav, false)
        .await
        .expect("play_file");

    // Wait past the timeout duration while playback is active — must NOT fire.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        rx.try_recv().is_err(),
        "RTP timeout must NOT fire during playback (timeout is paused)"
    );

    // Playback ends (~400ms) → on_end resumes the timeout.
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle.done)
        .await
        .expect("playback must finish")
        .expect("done must resolve");
    assert!(!result.interrupted);

    // With no RTP after resume, the timeout fires.
    let res = tokio::time::timeout(std::time::Duration::from_secs(2), &mut rx)
        .await
        .expect("timeout must fire after playback resumes it");
    assert!(res.is_ok(), "RTP timeout must fire after playback ends");

    mb.close();
}

/// Arming a second timeout replaces the first: the first receiver is dropped
/// (gets `Err(Canceled)`) and only the second fires. Guards against the
/// session re-arming (e.g. hold resume) and the stale receiver firing late.
#[tokio::test]
async fn rtp_timeout_rearm_replaces_previous() {
    let mut mb = MediaBridge::new("it-rtp-rearm", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx_first = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(1000))
        .expect("first arm");
    let mut rx_second = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("second arm (replaces first)");

    // The first receiver is superseded → Canceled immediately.
    let first_res = tokio::time::timeout(std::time::Duration::from_millis(500), &mut rx_first)
        .await
        .expect("first receiver must resolve");
    assert!(
        first_res.is_err(),
        "first arm must be canceled by the re-arm"
    );

    // The second receiver fires on its own shorter window.
    let second_res = tokio::time::timeout(std::time::Duration::from_secs(2), &mut rx_second)
        .await
        .expect("second receiver must resolve");
    assert!(second_res.is_ok(), "second arm must fire");
    mb.close();
}

/// RTP timeout also applies to the B (callee) leg — mirror of the A-leg test.
#[tokio::test]
async fn rtp_timeout_fires_on_inactive_b_leg() {
    let mut mb = MediaBridge::new("it-rtp-b", BridgeOpts::default());
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::B, b).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::B, std::time::Duration::from_millis(150))
        .expect("leg B timeout armed");
    mb.accept(LegSide::B).await;

    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut rx)
        .await
        .expect("B-leg timeout must fire");
    assert!(res.is_ok());
    mb.close();
}

/// The RTP inactivity watchdog is transport-agnostic: a WebRTC (DTLS-SRTP)
/// leg whose remote never completes DTLS gets no ingress packets and must time
/// out the same way as an RTP leg.
#[tokio::test]
async fn rtp_timeout_fires_on_inactive_webrtc_leg() {
    use rustrtc::TransportMode;

    let cfg = LegConfig {
        transport: TransportMode::WebRtc,
        codecs: vec![rustpbx_media::negotiate::CodecInfo {
            payload_type: 111,
            codec: audio_codec::CodecType::Opus,
            clock_rate: 48000,
            channels: 2,
            fmtp: None,
            }],
            video_codecs: Vec::new(),
            rtp_port_range: None,
        external_ip: None,
        bind_ip: None,
        cname: Some("rtp-timeout-webrtc".to_string()),
        comfort_noise: true,
        comfort_noise_level_db: -35.0,
    };
    let mut mb = MediaBridge::new("it-rtp-webrtc", BridgeOpts::default());
    let a = LegInner::new("a", &cfg).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let mut rx = mb
        .arm_rtp_timeout(LegSide::A, std::time::Duration::from_millis(150))
        .expect("webrtc leg timeout armed");

    let res = tokio::time::timeout(std::time::Duration::from_millis(1500), &mut rx)
        .await
        .expect("WebRTC leg timeout must fire");
    assert!(res.is_ok());
    mb.close();
}

/// Full relay lifecycle: bridge defers until both accept; hold breaks the
/// route; resume re-arms it; unbridge tears it down.
#[tokio::test]
async fn relay_full_lifecycle_with_accept_gate() {
    let mut mb = MediaBridge::new("it-e2e-relay", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();

    // 1. SDP exchange so both legs have negotiated audio profiles.
    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply a");

    // 2. Both legs start gated.
    assert!(la.is_gated() && lb.is_gated(), "both must start gated");

    // 3. Bridge → relay not active (both gated).
    mb.bridge().await.unwrap();
    assert!(!mb.is_bridged());

    // 4. Accept 'a' only → still inactive.
    mb.accept(LegSide::A).await;
    assert!(!la.is_gated() && lb.is_gated(), "a accepted, b still gated");
    assert!(!mb.is_bridged());

    // 5. Accept 'b' → relay activates (both accepted, same codec).
    mb.accept(LegSide::B).await;
    assert!(!la.is_gated() && !lb.is_gated(), "both accepted");
    assert!(mb.is_bridged(), "route active after both accept");

    // 6. Hold 'a' → route broken, egress silence.
    mb.hold(LegSide::A, None).await.unwrap();
    assert!(!mb.is_bridged());

    // 7. Resume 'a' → relay re-armed.
    mb.resume(LegSide::A).await.unwrap();
    assert!(mb.is_bridged());

    // 8. Unbridge → relay torn down.
    mb.unbridge().await.unwrap();
    assert!(!mb.is_bridged());
    mb.close();
}

/// Play a file on one leg while it is bridged — the play takes over egress,
/// then resume restores the relay.
#[tokio::test]
async fn play_file_during_bridge_then_resume() {
    let mut mb = MediaBridge::new("it-e2e-play-bridge", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();

    // SDP + bridge + accept both.
    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply a");
    mb.bridge().await.unwrap();
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());

    // Play a file on leg 'a' (100ms silence).
    let wav = tempfile_wav_silence(8000, 1, 800);
    mb.hold_file(LegSide::A, wav).await.unwrap(); // hold with music (looping)
    assert!(!mb.is_bridged(), "play must break the route");
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    // Resume — relay re-arms.
    mb.resume(LegSide::A).await.unwrap();
    assert!(mb.is_bridged());
    mb.unbridge().await.unwrap();
    mb.close();
}

/// Workflow with multiple bridge/unbridge and play cycles.
#[tokio::test]
async fn multi_cycle_bridge_play_hold_resume_unbridge() {
    let mut mb = MediaBridge::new("it-e2e-multi", BridgeOpts::default());
    let a = LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();

    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply a");

    // Cycle 1: bridge → play → resume → unbridge.
    mb.bridge().await.unwrap();
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    let wav1 = tempfile_wav_silence(8000, 1, 160);
    mb.play_file(LegSide::A, wav1, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    mb.resume(LegSide::A).await.unwrap();
    mb.unbridge().await.unwrap();

    // Cycle 2: re-bridge → hold → resume → unbridge.
    mb.bridge().await.unwrap();
    mb.hold(LegSide::A, None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    mb.resume(LegSide::A).await.unwrap();
    mb.unbridge().await.unwrap();

    // Cycle 3: bridge → hold_file → resume → unbridge → play → mute.
    mb.bridge().await.unwrap();
    let wav2 = tempfile_wav_silence(8000, 1, 160);
    mb.hold_file(LegSide::A, wav2).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    mb.resume(LegSide::A).await.unwrap();
    mb.unbridge().await.unwrap();
    let wav3 = tempfile_wav_silence(8000, 1, 80);
    mb.play_file(LegSide::A, wav3, false).await.unwrap();
    mb.mute(LegSide::A).await.unwrap();

    mb.close();
}

/// P1: play_file returns a handle whose done resolves with `interrupted: false`
/// on natural EOF (non-loop file that ends).
#[tokio::test]
async fn play_file_handle_completes_on_natural_eof() {
    let mut mb = MediaBridge::new("it-p1-natural", BridgeOpts::default());
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply");

    // 30ms silence @8kHz = 240 samples.
    let wav = tempfile_wav_silence(8000, 1, 240);
    let handle = mb
        .play_file(LegSide::A, wav, false)
        .await
        .expect("play_file");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle.done)
        .await
        .expect("must finish")
        .expect("done channel must resolve");
    assert!(
        !result.interrupted,
        "natural EOF must report interrupted=false"
    );

    mb.close();
}

/// P1: stop_play interrupts playback → done resolves with `interrupted: true`.
#[tokio::test]
async fn stop_play_interrupts_handle() {
    let mut mb = MediaBridge::new("it-p1-interrupt", BridgeOpts::default());
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply");

    // Long looping file so it keeps playing until we interrupt it.
    let wav = tempfile_wav_silence(8000, 1, 8000);
    let handle = mb
        .play_file(LegSide::A, wav, true)
        .await
        .expect("play_file");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    mb.stop_play(LegSide::A).await.expect("stop_play");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle.done)
        .await
        .expect("must finish")
        .expect("done channel must resolve");
    assert!(result.interrupted, "stop_play must report interrupted=true");

    mb.close();
}

/// P1: a looping file does NOT resolve done on its own (keeps playing).
#[tokio::test]
async fn loop_playback_does_not_resolve_until_stopped() {
    let mut mb = MediaBridge::new("it-p1-loop", BridgeOpts::default());
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu()).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu()).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer(vec![]).await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply");

    let wav = tempfile_wav_silence(8000, 1, 80); // tiny file, loops forever
    let mut handle = mb
        .play_file(LegSide::A, wav, true)
        .await
        .expect("play_file");

    // Give the pacing task plenty of time to exhaust the tiny file and loop.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        handle.done.try_recv().is_err(),
        "looping playback must NOT resolve done before stop_play"
    );

    mb.stop_play(LegSide::A).await.expect("stop_play");
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle.done)
        .await
        .expect("must finish")
        .expect("done must resolve");
    assert!(result.interrupted);

    mb.close();
}

// ── helpers ──────────────────────────────────────────────────────────────

/// Write a minimal silent PCM WAV to a temp path and return the path.
fn tempfile_wav_silence(sample_rate: u32, channels: u16, frames: u32) -> String {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!(
        "it_media_{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let data_len = frames * (channels as u32) * 2; // 16-bit
    let mut f = std::fs::File::create(&path).unwrap();
    // RIFF/WAVE header + PCM fmt + data
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&sample_rate.to_le_bytes()).unwrap();
    f.write_all(&(sample_rate * channels as u32 * 2).to_le_bytes())
        .unwrap(); // byte rate
    f.write_all(&(channels * 2).to_le_bytes()).unwrap(); // block align
    f.write_all(&16u16.to_le_bytes()).unwrap(); // bits
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    f.write_all(&vec![0u8; data_len as usize]).unwrap();
    path.to_string_lossy().to_string()
}
