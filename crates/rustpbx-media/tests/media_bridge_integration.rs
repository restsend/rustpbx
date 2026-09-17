//! Integration tests for the new media layer: exercises [`MediaBridge`],
//! [`Leg`] SDP exchange, bridging, playback, and recorder wiring together
//! (loopback, no SIP). Validates that the new modules compose correctly.
//!
//! The full RTP end-to-end matrix (fast-path relay, transcoding, recording
//! content, DTMF) lives in `rtp_transport_tests.rs` (TestMediaHarness).

use rustpbx_media::audio_source::FileAudioSource;
use rustpbx_media::ingress_tap::PacketDirection;
use rustpbx_media::leg::{LegConfig, LegInner};
use rustpbx_media::media_bridge::{LegSide, MediaBridge};
use rustpbx_media::media_recorder::{MediaRecorder, RecordingSession, SipflowRecorder};
use rustpbx_media::negotiate;
use rustpbx_sipflow::{SipFlowBackend, SipFlowItem, SipFlowMediaStats};

struct CaptureBackend {
    tx: tokio::sync::mpsc::UnboundedSender<SipFlowItem>,
}

#[async_trait::async_trait]
impl SipFlowBackend for CaptureBackend {
    fn record(&self, _call_id: std::borrow::Cow<'_, str>, item: SipFlowItem) -> anyhow::Result<()> {
        let _ = self.tx.send(item);
        Ok(())
    }

    async fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn query_flow(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<SipFlowItem>> {
        Ok(Vec::new())
    }

    async fn query_media_stats(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<SipFlowMediaStats>> {
        Ok(Vec::new())
    }

    async fn query_media(
        &self,
        _call_id: &str,
        _start_time: chrono::DateTime<chrono::Local>,
        _end_time: chrono::DateTime<chrono::Local>,
    ) -> anyhow::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

fn recorder_capture(
    call_id: &str,
) -> (
    Box<dyn MediaRecorder>,
    tokio::sync::mpsc::UnboundedReceiver<SipFlowItem>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let backend = std::sync::Arc::new(CaptureBackend { tx });
    (
        Box::new(SipflowRecorder::new(backend, call_id.to_string())),
        rx,
    )
}

fn captured_direction(item: &SipFlowItem) -> PacketDirection {
    match item.leg {
        Some(0) => PacketDirection::Ingress,
        Some(1) => PacketDirection::Egress,
        leg => panic!("unexpected recording leg: {leg:?}"),
    }
}

fn captured_payload_type(item: &SipFlowItem) -> u8 {
    item.payload
        .get(1)
        .map(|value| value & 0x7f)
        .expect("captured item must contain an RTP header")
}

/// Two RTP/PCMU legs: SDP offer/answer completes and both legs carry a
/// negotiated audio profile.
#[tokio::test]
async fn two_rtp_legs_negotiate_via_sdp() {
    let mut mb = MediaBridge::new("it-sdp");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();

    // a offers, b answers.
    let offer = a.create_offer().await.expect("offer");
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
    let mut mb = MediaBridge::new("it-bridge");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;

    // SDP exchange so both legs have negotiated audio profiles.
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.unwrap();
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
    let mut mb = MediaBridge::new("it-play");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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

/// A recorder sender installed on a tap receives packets from the observer.
#[tokio::test]
async fn recorder_sender_receives_ingress_via_tap() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let (recorder, mut rx) = recorder_capture("it-rec");

    let mut mb = MediaBridge::new("it-rec");
    let mut recording = RecordingSession::default();
    let recorder_sender = recording.setup_recorder_task().unwrap();
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), Some(recorder_sender)).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    recording.set_recorder(recorder, None).await.unwrap();

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
    assert_eq!(captured_direction(&item), PacketDirection::Ingress);
    assert_eq!(captured_payload_type(&item), 0);

    mb.close();
}

/// Telephone-event RTP reaches the recording backend in both directions.
#[tokio::test]
async fn recorder_sender_captures_dtmf_rtp_packets() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let (recorder, mut rx) = recorder_capture("it-dtmf-rec");

    let mut mb = MediaBridge::new("it-dtmf-rec");
    let mut recording = RecordingSession::default();
    let recorder_sender = recording.setup_recorder_task().unwrap();
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), Some(recorder_sender)).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    recording.set_recorder(recorder, None).await.unwrap();
    let leg_a = mb.leg(LegSide::A).unwrap();
    let tap = leg_a.ingress_tap();
    tap.set_dtmf_payload_types(vec![101]);

    let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

    // Feed a DTMF "5" end-event telephone-event packet (PT 101).
    // RFC 4733 payload: digit 5, end bit set, volume 10, duration 160.
    let dtmf = RtpPacket::new(RtpHeader::new(101, 1, 0, 1), vec![5u8, 0x8A, 0, 0xA0]);
    tap.on_ingress(&dtmf, addr);
    tap.on_egress(&dtmf, addr);

    for direction in [PacketDirection::Ingress, PacketDirection::Egress] {
        let item = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("telephone-event RTP must reach the recorder")
            .expect("recorder channel closed");
        assert_eq!(captured_direction(&item), direction);
        assert_eq!(captured_payload_type(&item), 101);
    }

    mb.close();
}

/// DTMF bus fans out a detected digit from a leg's tap, tagged with LegSide.
#[tokio::test]
async fn dtmf_bus_fans_out_digit() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let mut mb = MediaBridge::new("it-dtmf");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    assert_eq!(side.as_str(), "a");
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
    let mut mb = MediaBridge::new("it-onend");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
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

/// `bridge_play_pcm` — the raw-PCM streaming inject used by `voip_bridge` and
/// the realtime app — must pace written frames out the leg egress and resolve
/// the `on_end` callback when the writer drops the channel (natural end).
/// Guard test: any regression in `ChannelAudioSource` pacing/EOF semantics
/// breaks both consumers, so it is pinned here at the integration level.
#[tokio::test]
async fn bridge_play_pcm_streams_and_fires_on_end() {
    let mut mb = MediaBridge::new("it-pcm-stream");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply answer");

    let (end_tx, end_rx) = tokio::sync::oneshot::channel::<bool>();
    let end_slot = std::sync::Mutex::new(Some(end_tx));
    let on_end: rustpbx_media::egress::EgressEndCallback =
        std::sync::Arc::new(move |interrupted| {
            if let Ok(mut slot) = end_slot.lock() {
                if let Some(tx) = slot.take() {
                    let _ = tx.send(interrupted);
                }
            }
        });

    // 20ms @ 8kHz = 160 samples per frame; stream 10 frames (200ms) then drop.
    let mut tx = mb
        .bridge_play_pcm(LegSide::A, 8000, Some(on_end))
        .await
        .expect("bridge_play_pcm");
    let frame = vec![1000i16; 160];
    for _ in 0..10 {
        tx.send(frame.clone())
            .await
            .expect("pcm channel must accept frames while streaming");
    }
    drop(tx);

    let interrupted = tokio::time::timeout(std::time::Duration::from_secs(3), end_rx)
        .await
        .expect("bridge_play_pcm must fire on_end after writer drop")
        .expect("on_end channel must resolve");
    assert!(
        !interrupted,
        "writer drop is a natural end — must not be flagged interrupted"
    );

    mb.close();
}

/// MediaBridge::hold breaks the route, then MediaBridge::resume re-arms it.
#[tokio::test]
async fn mediabridge_hold_resume_preserves_route() {
    let mut mb = MediaBridge::new("it-hold-route");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;

    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.unwrap();
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
    mb.resume().await.unwrap();
    assert!(mb.is_bridged(), "resume must re-arm the route");

    mb.close();
}

/// hold with looping music source does not terminate (has_data stays true).
#[tokio::test]
async fn hold_with_music_loops() {
    let mut mb = MediaBridge::new("it-hold-music");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    // Generate a tiny WAV and play it as looping hold music.
    let wav = tempfile_wav_silence(8000, 1, 160);
    let audio = FileAudioSource::new(wav, true).await.expect("source");
    mb.hold(LegSide::A, Some(Box::new(audio)))
        .await
        .expect("hold with music");

    // Let a few ticks pass — no panic means the looping source worked.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    mb.resume().await.expect("resume");
    mb.close();
}

/// hold_file convenience method works end-to-end.
#[tokio::test]
async fn mediabridge_hold_file_plays_loop() {
    let mut mb = MediaBridge::new("it-hold-file");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;

    let wav = tempfile_wav_silence(8000, 1, 160);
    mb.hold_file(LegSide::A, wav).await.expect("hold_file");

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    mb.resume().await.expect("resume");
    mb.close();
}

/// Attach the unified recorder sender to a leg's tap, feed real non-silence
/// PCMU packets, then verify its file output contains non-silence audio.
#[tokio::test]
async fn file_recorder_writes_wav() {
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    let tmp = std::env::temp_dir().join(format!(
        "it_rec_{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = tmp.to_string_lossy().to_string();

    let mut mb = MediaBridge::new("it-rec-file");
    let mut recording = RecordingSession::default();
    let recorder_sender = recording.setup_recorder_task().unwrap();

    // Create the caller and its recording task together, then install the
    // file backend through the independent recording session.
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), Some(recorder_sender)).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply answer");
    recording.start_recording(la.negotiated().unwrap(), path.clone(), 2, false, None)
        .await
        .expect("file output start");

    // Feed non-silence PCMU packets through the caller tap.
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

    // Closing the media connection must not destroy recording control.
    mb.close();
    let result = recording
        .stop_recording()
        .await
        .expect("file recording finalize")
        .expect("file recording result");
    assert_eq!(result.path, path);

    let bytes = std::fs::read(&path).unwrap_or_else(|_| panic!("recorder must create: {path}"));
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
    let mut mb = MediaBridge::new("it-gate");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    let mut mb = MediaBridge::new("it-rtp-timeout");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    let mut mb = MediaBridge::new("it-rtp-timeout-hold");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    let mut mb = MediaBridge::new("it-rtp-timeout-disarm");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    let mut mb = MediaBridge::new("it-rtp-timeout-app-paused");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    assert!(
        slept2.is_err(),
        "timeout must NOT fire after re-arm while app-paused"
    );

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
    use rustrtc::rtp::{RtpHeader, RtpPacket};

    let mut mb = MediaBridge::new("it-rtp-active");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let remote = LegInner::new("remote", &LegConfig::rtp_pcmu(), None).unwrap();
    let offer = a.create_offer().await.unwrap();
    let answer = remote.answer(&offer).await.unwrap();
    a.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
    remote.pc().wait_for_rtp_transport_ready(std::time::Duration::from_secs(2)).await.unwrap();
    mb.replace_leg(LegSide::A, a).await;

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
        remote.pc().send_raw_rtp(pkt).await.unwrap();
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

    remote.stop();
    mb.close();
}

/// `MediaBridge::play` pauses the RTP inactivity timeout (the peer may stay
/// silent while a prompt plays); natural EOF resumes it. Verify the timeout
/// does NOT fire during playback, then fires once playback ends and no RTP
/// arrives.
#[tokio::test]
async fn rtp_timeout_paused_during_playback_resumes_on_end() {
    let mut mb = MediaBridge::new("it-rtp-play");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    let mut mb = MediaBridge::new("it-rtp-rearm");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
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
    let mut mb = MediaBridge::new("it-rtp-b");
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
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
        ice_servers: Vec::new(),
        relay_only: false,
        enable_ice_lite: false,
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
        enable_latching: true,
        probation_max_packets: None,
    };
    let mut mb = MediaBridge::new("it-rtp-webrtc");
    let a = LegInner::new("a", &cfg, None).unwrap();
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
    let mut mb = MediaBridge::new("it-e2e-relay");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();

    // 1. SDP exchange so both legs have negotiated audio profiles.
    let offer = la.create_offer().await.expect("offer");
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
    mb.resume().await.unwrap();
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
    let mut mb = MediaBridge::new("it-e2e-play-bridge");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();

    // SDP + bridge + accept both.
    let offer = la.create_offer().await.expect("offer");
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
    mb.resume().await.unwrap();
    assert!(mb.is_bridged());
    mb.unbridge().await.unwrap();
    mb.close();
}

/// Workflow with multiple bridge/unbridge and play cycles.
#[tokio::test]
async fn multi_cycle_bridge_play_hold_resume_unbridge() {
    let mut mb = MediaBridge::new("it-e2e-multi");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();

    let offer = la.create_offer().await.expect("offer");
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
    mb.resume().await.unwrap();
    mb.unbridge().await.unwrap();

    // Cycle 2: re-bridge → hold → resume → unbridge.
    mb.bridge().await.unwrap();
    mb.hold(LegSide::A, None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    mb.resume().await.unwrap();
    mb.unbridge().await.unwrap();

    // Cycle 3: bridge → hold_file → resume → unbridge → play → mute.
    mb.bridge().await.unwrap();
    let wav2 = tempfile_wav_silence(8000, 1, 160);
    mb.hold_file(LegSide::A, wav2).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    mb.resume().await.unwrap();
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
    let mut mb = MediaBridge::new("it-p1-natural");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
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
    let mut mb = MediaBridge::new("it-p1-interrupt");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
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
    let mut mb = MediaBridge::new("it-p1-loop");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
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

/// Contract: the second `play_file`'s internal `unbridge()` switches leg A
/// away from its just-started Media source, so the FIRST handle resolves
/// immediately with `interrupted: true` (mirror kill). The second handle
/// still completes naturally. This is why "both"-leg announcements must use
/// [`MediaBridge::play_file_both`] — the test pins the footgun so a future
/// refactor either preserves or consciously removes this behavior.
#[tokio::test]
async fn double_play_file_interrupts_first_handle_contract() {
    let mut mb = MediaBridge::new("it-double-play");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply");
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());

    // ~500ms file: long enough that only the mirror kill can end leg A early.
    let wav = tempfile_wav_silence(8000, 1, 4000);
    let ha = mb.play_file(LegSide::A, &wav, false).await.expect("play A");
    let hb = mb.play_file(LegSide::B, &wav, false).await.expect("play B");
    assert!(!mb.is_bridged(), "play must break the route");

    let ra = tokio::time::timeout(std::time::Duration::from_millis(300), ha.done)
        .await
        .expect("first handle must resolve (mirror kill is immediate)")
        .expect("done channel must resolve");
    assert!(
        ra.interrupted,
        "second play_file's unbridge() must interrupt leg A's playback"
    );

    let rb = tokio::time::timeout(std::time::Duration::from_secs(3), hb.done)
        .await
        .expect("second handle must finish naturally")
        .expect("done channel must resolve");
    assert!(!rb.interrupted, "leg B playback must run to EOF");

    mb.resume().await.expect("resume");
    assert!(mb.is_bridged(), "resume must re-arm the route");
    mb.unbridge().await.unwrap();
    mb.close();
}

/// Contract for [`MediaBridge::play_file_both`] (the fixed console-API
/// insert-play primitive): breaks the route exactly once, BOTH handles run to
/// natural EOF (neither is mirror-killed), and `resume()` re-arms the route.
#[tokio::test]
async fn play_file_both_handles_complete_and_resume_rebridges() {
    let mut mb = MediaBridge::new("it-play-both");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply");
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());

    // ~300ms file.
    let wav = tempfile_wav_silence(8000, 1, 2400);
    let mut handles = mb
        .play_file_both(&wav, false)
        .await
        .expect("play_file_both");
    assert_eq!(handles.len(), 2, "both legs exist → both must play");
    assert!(!mb.is_bridged(), "play_file_both must break the route");

    let hb = handles.pop().expect("leg B handle");
    let ha = handles.pop().expect("leg A handle");
    let ra = tokio::time::timeout(std::time::Duration::from_secs(3), ha.done)
        .await
        .expect("leg A handle must finish")
        .expect("done channel must resolve");
    let rb = tokio::time::timeout(std::time::Duration::from_secs(3), hb.done)
        .await
        .expect("leg B handle must finish")
        .expect("done channel must resolve");
    assert!(
        !ra.interrupted && !rb.interrupted,
        "neither leg may be mirror-killed: a={{{ra:?}}} b={{{rb:?}}}"
    );

    mb.resume().await.expect("resume");
    assert!(mb.is_bridged(), "resume must re-arm the route");
    mb.unbridge().await.unwrap();
    mb.close();
}

/// Contract: `play_file_both` on a bridge whose B leg is missing (app-mode
/// call) degrades to an A-only announcement instead of failing.
#[tokio::test]
async fn play_file_both_degrades_to_single_leg_without_b() {
    let mut mb = MediaBridge::new("it-play-both-nob");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;

    // No B leg at all (app-mode call shape).
    let wav = tempfile_wav_silence(8000, 1, 240); // ~30ms
    let mut handles = mb
        .play_file_both(&wav, false)
        .await
        .expect("play_file_both must tolerate a missing B leg");
    assert_eq!(handles.len(), 1, "only leg A must play");
    assert!(!mb.is_bridged());
    let ha = handles.pop().expect("leg A handle");
    let r = tokio::time::timeout(std::time::Duration::from_secs(3), ha.done)
        .await
        .expect("leg A handle must finish")
        .expect("done channel must resolve");
    assert!(!r.interrupted);
    mb.close();
}

/// Stopping held peers sets silence and leaves the bridge resumable.
#[tokio::test]
async fn stop_play_on_held_peers_allows_resume() {
    let mut mb = MediaBridge::new("it-stop-noop");
    mb.replace_leg(
        LegSide::A,
        LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    mb.replace_leg(
        LegSide::B,
        LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap(),
    )
    .await;
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.expect("offer");
    let answer = lb.answer(&offer).await.expect("answer");
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .expect("apply");
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());

    // Hold music on A.
    let wav = tempfile_wav_silence(8000, 1, 800);
    mb.hold_file(LegSide::A, wav).await.expect("hold_file");

    // Stop both sources without reconnecting the pair.
    mb.stop_play(LegSide::A).await.expect("stop_play");
    assert!(!mb.is_bridged(), "stop must not re-bridge");
    mb.stop_play(LegSide::B).await.expect("stop_play");

    // The bridge must still be resumable after stopping both sources.
    mb.resume().await.expect("resume");
    assert!(mb.is_bridged());
    mb.unbridge().await.unwrap();
    mb.close();
}

/// Regression: replacing a leg (transfer / REFER) must release the replaced
/// leg and exit the RTCP-relay forwarder tasks that previously pinned it.
///
/// Before the fix, `wire_rtcp_sender_forward` tasks held `Leg` (`Arc<LegInner>`)
/// clones and never exited (the broadcast RTCP receiver only closes when the
/// PeerConnection closes, and the PC only closes when the last Arc drops) — a
/// reference cycle that leaked the replaced PeerConnection forever.
#[tokio::test]
async fn replace_leg_releases_replaced_leg_and_rtcp_tasks() {
    use std::sync::Arc;
    use std::time::Duration;

    let mut mb = MediaBridge::new("it-replace");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b.clone()).await;

    // SDP exchange + accept both → fast-path relay, RTCP forwarders spawned.
    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.unwrap();
    let answer = lb.answer(&offer).await.unwrap();
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());
    assert!(
        mb.active_rtcp_forwarders() > 0,
        "fast-path bridge must wire RTCP forwarders"
    );

    // Weak handle to the (soon-to-be-replaced) B leg.
    let b_weak = Arc::downgrade(&b);

    // Transfer: replace B with a fresh, not-yet-negotiated leg.
    let b2 = LegInner::new("b2", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::B, b2).await;
    drop(lb);
    drop(b);

    // Give the cancelled RTCP tasks + per-leg monitor tasks time to exit.
    for _ in 0..100 {
        if b_weak.upgrade().is_none() && mb.active_rtcp_forwarders() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        b_weak.upgrade().is_none(),
        "replaced leg must be fully released (RTCP relay tasks pinned it before the fix); live forwarders={}",
        mb.active_rtcp_forwarders()
    );
    assert_eq!(
        mb.active_rtcp_forwarders(),
        0,
        "RTCP relay tasks must exit after the leg is replaced"
    );

    mb.close();
}

/// Regression: repeated unbridge/bridge cycles (hold → play → resume) must
/// not accumulate RTCP-relay forwarder tasks within a session.
#[tokio::test]
async fn unbridge_bridge_cycles_do_not_accumulate_rtcp_tasks() {
    use std::time::Duration;

    let mut mb = MediaBridge::new("it-cycles");
    let a = LegInner::new("a", &LegConfig::rtp_pcmu(), None).unwrap();
    let b = LegInner::new("b", &LegConfig::rtp_pcmu(), None).unwrap();
    mb.replace_leg(LegSide::A, a).await;
    mb.replace_leg(LegSide::B, b).await;

    let la = mb.leg(LegSide::A).unwrap();
    let lb = mb.leg(LegSide::B).unwrap();
    let offer = la.create_offer().await.unwrap();
    let answer = lb.answer(&offer).await.unwrap();
    la.apply_sdp(&answer, rustrtc::SdpType::Answer)
        .await
        .unwrap();
    mb.accept(LegSide::A).await;
    mb.accept(LegSide::B).await;
    assert!(mb.is_bridged());

    for _ in 0..5 {
        mb.unbridge().await.unwrap();
        mb.bridge().await.unwrap();
    }
    // Let cancelled generations observe cancellation and exit.
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        mb.active_rtcp_forwarders(),
        2,
        "only the current generation (audio A→B + B→A) may remain; old ones must not accumulate"
    );
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

/// An IVR peer can play, observe input, and record without a bridge.
#[tokio::test]
async fn standalone_peer_playback_input_and_recording() {
    use std::time::Duration;
    use rustpbx_media::audio_source::ToneAudioSource;
    use rustpbx_media::media_recorder::RecordingSession;
    use rustrtc::rtp::{RtpHeader, RtpPacket};

    let mut recording = RecordingSession::default();
    let capture = recording.setup_recorder_task().unwrap();
    let mut cfg = LegConfig::rtp_pcmu();
    cfg.codecs.push(negotiate::CodecInfo {
        payload_type: 101, codec: audio_codec::CodecType::TelephoneEvent,
        clock_rate: 8000, channels: 1, fmtp: Some("0-16".into()),
    });
    let peer = LegInner::new("ivr", &cfg, Some(capture)).unwrap();
    let remote = LegInner::new("phone", &cfg, None).unwrap();
    let offer = peer.create_offer().await.unwrap();
    let answer = remote.answer(&offer).await.unwrap();
    peer.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
    peer.accept();
    remote.accept();
    let (recorder, mut captured) = recorder_capture("single-peer");
    recording.set_recorder(recorder, None).await.unwrap();
    let mut digits = peer.subscribe_dtmf();
    let mut pcm = peer.pcm_stream(tokio_util::sync::CancellationToken::new()).unwrap();
    let first = peer.play_media(Box::new(ToneAudioSource::new(440, Duration::from_secs(1), 8000).unwrap()), true).await.unwrap();
    let second = peer.play_media(Box::new(ToneAudioSource::new(600, Duration::from_secs(1), 8000).unwrap()), true).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), first.done).await.unwrap().unwrap().interrupted);
    remote.play_media(Box::new(ToneAudioSource::new(800, Duration::from_secs(1), 8000).unwrap()), true).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while pcm.recv().await.unwrap().silence {}
        while remote.pc().received_rtp_packets() == 0 { tokio::task::yield_now().await; }
    }).await.expect("independent incoming and outgoing audio");
    remote.pc().send_raw_rtp(RtpPacket::new(RtpHeader::new(101, 500, 8000, 1234), vec![5, 0x80, 0, 160])).await.unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(2), digits.recv()).await.unwrap().unwrap().digit, '5');
    tokio::time::timeout(Duration::from_secs(2), captured.recv()).await.unwrap().unwrap();
    peer.stop_playback().await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), second.done).await.unwrap().unwrap().interrupted,
        "completion of old playback must not prevent stopping its replacement");
    recording.stop_recording().await.unwrap();
    remote.stop();
    peer.stop();
}

#[tokio::test]
async fn bridge_rtp_timeout_tracks_fast_path_ingress() {
    use std::time::Duration;
    use rustpbx_media::audio_source::ToneAudioSource;
    let mut peers = Vec::new();
    let mut phones = Vec::new();
    for id in ["a", "b"] {
        let peer = LegInner::new(id, &LegConfig::rtp_pcmu(), None).unwrap();
        let phone = LegInner::new(format!("phone-{id}"), &LegConfig::rtp_pcmu(), None).unwrap();
        let offer = peer.create_offer().await.unwrap();
        let answer = phone.answer(&offer).await.unwrap();
        peer.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
        peer.accept();
        phone.accept();
        peers.push(peer);
        phones.push(phone);
    }
    let mut bridge = MediaBridge::new("fast-path-timeout");
    bridge.select_pair(peers[0].clone(), peers[1].clone()).await.unwrap();
    bridge.bridge().await.unwrap();
    assert!(peers[0].egress_is_relay());
    assert!(peers[1].egress_is_relay());
    let mut timeout = bridge.arm_rtp_timeout(LegSide::A, Duration::from_millis(500)).unwrap();
    phones[0].play_media(Box::new(ToneAudioSource::new(440, Duration::from_secs(1), 8000).unwrap()), true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(peers[0].pc().received_rtp_packets() > 0);
    assert!(phones[1].pc().received_rtp_packets() > 0, "fast path must forward the input");
    assert!(matches!(timeout.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
        "incoming fast-path RTP must keep the watchdog alive");
    phones[0].stop();
    tokio::time::timeout(Duration::from_secs(2), timeout).await.unwrap().unwrap();
    phones[1].stop();
    bridge.close();
}

#[tokio::test]
async fn caller_dtmf_subscription_closes_when_peer_drops() {
    let caller = LegInner::new("caller", &LegConfig::rtp_pcmu(), None).unwrap();
    let remote = LegInner::new("remote", &LegConfig::rtp_pcmu(), None).unwrap();
    let offer = caller.create_offer().await.unwrap();
    let answer = remote.answer(&offer).await.unwrap();
    caller.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
    caller.pc().wait_for_rtp_transport_ready(std::time::Duration::from_secs(2)).await.unwrap();
    let mut rx = caller.subscribe_dtmf();
    drop(caller);
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap(),
        Err(tokio::sync::broadcast::error::RecvError::Closed)
    ));
    remote.stop();
}

#[tokio::test]
async fn relay_timeline_survives_hold_playback_and_source_switches() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use rustpbx_media::audio_source::ToneAudioSource;
    use rustrtc::peer_connection::RtpObserver;
    use rustrtc::rtp::{RtpHeader, RtpPacket};

    struct Capture(std::sync::Mutex<Vec<(Instant, RtpHeader)>>);
    impl RtpObserver for Capture {
        fn on_ingress(&self, packet: &RtpPacket, _: std::net::SocketAddr) {
            self.0.lock().unwrap().push((Instant::now(), packet.header.clone()));
        }
    }

    // Test the reported G722 path as well as the original PCMU path.
    for (codec, pt) in [(audio_codec::CodecType::PCMU, 0), (audio_codec::CodecType::G722, 9)] {
        let mut cfg = LegConfig::rtp_pcmu();
        cfg.codecs[0].codec = codec;
        cfg.codecs[0].payload_type = pt;
        let mut peers = Vec::new();
        let mut phones = Vec::new();
        let capture = Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
        for id in ["a", "b", "c"] {
            let peer = LegInner::new(id, &cfg, None).unwrap();
            let phone = LegInner::new(format!("phone-{id}"), &cfg, None).unwrap();
            if id == "b" { phone.pc().add_observer(capture.clone()); }
            let offer = peer.create_offer().await.unwrap();
            let answer = phone.answer(&offer).await.unwrap();
            peer.apply_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
            peer.accept();
            phone.accept();
            peers.push(peer);
            phones.push(phone);
        }
        let mut bridge = MediaBridge::new("relay-timeline");
        bridge.select_pair(peers[0].clone(), peers[1].clone()).await.unwrap();
        bridge.bridge().await.unwrap();
        for phone in [&phones[0], &phones[2]] {
            phone.play_media(Box::new(ToneAudioSource::new(440, Duration::from_secs(10), codec.samplerate()).unwrap()), true).await.unwrap();
        }
        let relay_ssrc = peers[1].outbound_audio_ssrc();
        tokio::time::sleep(Duration::from_millis(200)).await;
        for (source, play_hold_music) in [(0, false), (0, true), (2, true), (0, false)] {
            bridge.unbridge().await.unwrap();
            if play_hold_music {
                peers[1].play_media(Box::new(ToneAudioSource::new(660, Duration::from_secs(10), codec.samplerate()).unwrap()), true).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            let before = capture.0.lock().unwrap().iter().rev()
                .find(|(_, h)| h.ssrc == relay_ssrc).cloned().expect("initial relay packet");
            let resumed_at = Instant::now();
            bridge.select_pair(peers[source].clone(), peers[1].clone()).await.unwrap();
            bridge.bridge().await.unwrap();
            let after = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let packet = capture.0.lock().unwrap().iter()
                        .find(|(time, h)| *time >= resumed_at && h.ssrc == relay_ssrc).cloned();
                    if let Some(packet) = packet { break packet; }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }).await.unwrap_or_else(|_| panic!("{codec:?} source={source} hold_music={play_hold_music}: relay must resume"));
            assert_eq!(after.1.sequence_number, before.1.sequence_number.wrapping_add(1), "{codec:?}: sequence reset on resume");
            assert_eq!(after.1.timestamp.wrapping_sub(before.1.timestamp), 160,
                "{codec:?}: resume must continue one packet after the sender's last timestamp");
            tokio::time::sleep(Duration::from_millis(100)).await;
            let packets = capture.0.lock().unwrap();
            let resumed: Vec<_> = packets.iter().filter(|(time, h)| *time >= after.0 && h.ssrc == relay_ssrc).collect();
            assert!(resumed.len() >= 3, "continued audio after resume");
            for pair in resumed.windows(2) {
                assert_eq!(pair[1].1.sequence_number, pair[0].1.sequence_number.wrapping_add(1));
                if !pair[1].1.marker {
                    assert_eq!(pair[1].1.timestamp.wrapping_sub(pair[0].1.timestamp), 160,
                        "{codec:?} source={source} hold_music={play_hold_music}: relay must retain source timestamp deltas: {:?} -> {:?}", pair[0].1, pair[1].1);
                }
            }
        }
        // Conference output shares the leg's wire timeline with the relay.
        // Feed isolated mixed frames so Inject alternates speech and silence.
        bridge.unbridge().await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let injected_at = Instant::now();
        peers[1].set_egress_source(rustpbx_media::egress::EgressSource::Inject {
            rx: parking_lot::Mutex::new(rx),
        }).await.unwrap();
        let mut encoder = audio_codec::create_encoder(codec);
        for index in 0..3 {
            tx.send(rustrtc::media::MediaSample::Audio(rustrtc::media::AudioFrame {
                rtp_timestamp: 1_000_000 + index * 160,
                clock_rate: 8000,
                data: encoder.encode(&vec![2_000i16; codec.samplerate() as usize / 50]).into(),
                sequence_number: Some(5000 + index as u16),
                payload_type: Some(pt),
                marker: true,
                header_extension: None,
                raw_packet: None,
                source_addr: None,
            })).await.unwrap();
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        assert!(capture.0.lock().unwrap().iter()
            .filter(|(time, h)| *time >= injected_at && h.ssrc == relay_ssrc && h.marker)
            .count() >= 3, "{codec:?}: injected audio must reach the receiver");
        bridge.unbridge().await.unwrap();
        bridge.bridge().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        bridge.force_transcode().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!peers[1].egress_is_relay());
        {
            let packets = capture.0.lock().unwrap();
            let output: Vec<_> = packets.iter().filter(|(_, h)| h.ssrc == relay_ssrc).collect();
            for pair in output.windows(2) {
                assert_eq!(pair[1].1.sequence_number, pair[0].1.sequence_number.wrapping_add(1),
                    "{codec:?}: all output paths must share the sender sequence: {:?} -> {:?}", pair[0], pair[1]);
                let advance = pair[1].1.timestamp.wrapping_sub(pair[0].1.timestamp);
                assert!(advance > 0 && advance < 8000,
                    "{codec:?}: output timestamp must advance through playback/mixer/relay/transcode: {advance}");
            }
        }
        bridge.close();
        for peer in peers.into_iter().chain(phones) { peer.stop(); }
    }
}
