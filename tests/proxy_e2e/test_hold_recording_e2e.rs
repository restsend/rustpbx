//! E2E regression: hold must not lose recording audio nor break the RTP
//! timeline on a bridged B2BUA call.
//!
//! Two production defects are pinned here (both fire when a leg is held on a
//! call whose media bridge was active — e.g. the CTI `call.hold` flow):
//!
//! 1. **Recording loss during hold.** Tearing down the media bridge stops the
//!    far party's audio from transiting the caller leg's recording tap, so the
//!    hold window of the stereo WAV loses that party's voice. Contract after
//!    the fix: during hold the recorder's B channel switches to the callee
//!    leg's ingress (the agent mic's original RTP), so both parties' live
//!    speech stays in the recording for the whole call.
//! 2. **Choppy hold audio.** Switching the held leg's egress from the rewrite
//!    relay to the paced pipeline used to adopt only the wire RTP *timestamp*,
//!    not the *sequence number* — hold music went out with a stale sequence on
//!    the same SSRC and receivers dropped it as stale retransmissions.
//!    Contract after the fix: the pipeline continues the relay's sequence
//!    space across hold and unhold (no backward jumps / big gaps).
//!
//! Media design: both parties stream constant pure tones (caller 400 Hz,
//! callee 800 Hz — both integer cycles per 160-sample PCMU frame so every
//! frame payload is identical), which lets the assertions discriminate the
//! stereo channels by dominant frequency.

use crate::common::audio_analysis::{dominant_frequency, rms};
use crate::common::cdr_capture::CdrExpectation;
use crate::common::e2e_test_server::E2eTestServer;
use crate::common::rtp_utils::{RtpPacket, extract_media_endpoint};
use crate::common::test_ua::{TestUaEvent, create_test_sdp};
use anyhow::Result;
use audio_codec::{CodecType, create_decoder, create_encoder};
use rustpbx::config::{MediaProxyMode, ProxyConfig, RecordingPolicy};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::sleep;

const CALLER_TONE_HZ: f64 = 400.0;
const CALLEE_TONE_HZ: f64 = 800.0;
const CANDIDATE_HZ: [f64; 2] = [CALLER_TONE_HZ, CALLEE_TONE_HZ];
/// Tone amplitude 8000 → rms ≈ 5600; hold-window comfort noise (−35 dBFS)
/// stays far below this floor.
const RMS_FLOOR: f64 = 1500.0;
const FREQ_TOLERANCE_HZ: f64 = 50.0;
const SAMPLE_RATE: u32 = 8000;
/// 20 ms frames; 600 packets = 12 s of continuous tone, longer than the call.
const TONE_PACKETS: usize = 600;
const RUSTPBX_COMMAND_CT: &str = "application/vnd.rustpbx+json";

/// Build `TONE_PACKETS` PCMU packets carrying a pure tone whose period divides
/// the 160-sample frame exactly, so every packet reuses the same payload.
fn tone_packets(freq_hz: f64, ssrc: u32) -> Vec<RtpPacket> {
    let samples_per_frame = (SAMPLE_RATE / 50) as usize; // 160 @ 8 kHz
    let pcm: Vec<i16> = (0..samples_per_frame)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            (8000.0 * (2.0 * std::f64::consts::PI * freq_hz * t).sin()) as i16
        })
        .collect();
    let payload = create_encoder(CodecType::PCMU).encode(&pcm).to_vec();
    (0..TONE_PACKETS)
        .map(|i| {
            RtpPacket::new(
                0,
                (i + 1) as u16,
                (i * samples_per_frame) as u32,
                ssrc,
                payload.clone(),
            )
        })
        .collect()
}

/// Minimal RIFF walker: returns (audio_format, channels, sample_rate, data).
fn parse_wav(path: &str) -> Result<(u16, u16, u32, Vec<u8>)> {
    let bytes = std::fs::read(path)?;
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut pos = 12usize;
    let mut fmt = (0u16, 0u16, 0u32);
    let mut data = Vec::new();
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        match id {
            b"fmt " => {
                let format = u16::from_le_bytes(body[0..2].try_into()?);
                let channels = u16::from_le_bytes(body[2..4].try_into()?);
                let rate = u32::from_le_bytes(body[4..8].try_into()?);
                fmt = (format, channels, rate);
            }
            b"data" => data = body.to_vec(),
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    Ok((fmt.0, fmt.1, fmt.2, data))
}

/// Decode a μ-law WAV data chunk into interleaved samples.
fn decode_mulaw(data: &[u8]) -> Vec<i16> {
    create_decoder(CodecType::PCMU).decode(data)
}

/// Half-open sample range [start, end) of `channel`, by file fraction.
fn window<'a>(channel: &'a [i16], from: f64, to: f64) -> &'a [i16] {
    let len = channel.len();
    let start = ((len as f64) * from) as usize;
    let end = ((len as f64) * to) as usize;
    &channel[start..end.max(start)]
}

fn assert_tone(channel: &[i16], rate: u32, freq: f64, label: &str) {
    assert!(
        channel.len() > (rate as usize) / 10,
        "{label}: window too small ({} samples)",
        channel.len()
    );
    let level = rms(channel);
    assert!(
        level > RMS_FLOOR,
        "{label}: silent window (rms {level:.1} ≤ {RMS_FLOOR}) — audio lost from the recording"
    );
    let (dominant, _) = dominant_frequency(channel, rate, &CANDIDATE_HZ);
    assert!(
        (dominant - freq).abs() <= FREQ_TOLERANCE_HZ,
        "{label}: dominant frequency {dominant:.0} Hz != expected {freq:.0} Hz (rms {level:.1}) \
         — wrong party's audio in this channel"
    );
}

/// The recorded call timeline (wall clock from call establishment):
///   0.0s talk → 1.5s HOLD(callee) → 4.0s UNHOLD → 6.5s hangup.
/// Windows are expressed as file fractions, each well inside its phase:
/// the file trails the hangup by a finalize margin (the callee side keeps
/// receiving packets briefly after the BYE, and the flush zero-pads the
/// caller channel to match), so the post-hold window stays below 75%.
const PRE_WINDOW: (f64, f64) = (0.04, 0.14);
const HOLD_WINDOW: (f64, f64) = (0.28, 0.44);
const POST_WINDOW: (f64, f64) = (0.58, 0.72);

/// Minimal RTP link used by both parties: one socket that streams a tone
/// toward the PBX and tracks sequence-number continuity of everything it
/// receives (the held leg's downlink crosses relay → music → relay).
struct RtpLink {
    socket: Arc<tokio::net::UdpSocket>,
    gaps: Arc<std::sync::Mutex<Vec<(u16, u16)>>>,
    received: Arc<AtomicUsize>,
}

impl RtpLink {
    async fn bind() -> Result<Self> {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        let gaps = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = Arc::new(AtomicUsize::new(0));
        let (rx_socket, rx_gaps, rx_received) = (
            Arc::clone(&socket),
            Arc::clone(&gaps),
            Arc::clone(&received),
        );
        rustpbx::utils::spawn(async move {
            let mut buf = vec![0u8; 1500];
            // Sequence continuity is tracked per payload type: the same
            // socket also receives non-audio flows (RTCP/control, other PTs)
            // whose independent sequence spaces must not pollute the audio
            // continuity check.
            let mut last_audio_seq: Option<u16> = None;
            loop {
                match rx_socket.recv_from(&mut buf).await {
                    Ok((len, _)) => {
                        let Ok(packet) = RtpPacket::decode(&buf[..len]) else {
                            continue;
                        };
                        rx_received.fetch_add(1, Ordering::Relaxed);
                        if packet.payload_type == 0 {
                            if let Some(last) = last_audio_seq {
                                let expected = last.wrapping_add(1);
                                if packet.sequence_number != expected {
                                    rx_gaps
                                        .lock()
                                        .unwrap()
                                        .push((expected, packet.sequence_number));
                                }
                            }
                            last_audio_seq = Some(packet.sequence_number);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            socket,
            gaps,
            received,
        })
    }

    fn local_port(&self) -> Result<u16> {
        Ok(self.socket.local_addr()?.port())
    }

    /// Stream a finite packet list at the given cadence (20 ms = one ptime).
    fn start_tone(&self, target: SocketAddr, packets: Vec<RtpPacket>, interval_ms: u64) {
        let socket = Arc::clone(&self.socket);
        rustpbx::utils::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            for packet in packets {
                interval.tick().await;
                if socket.send_to(&packet.encode(), target).await.is_err() {
                    break;
                }
            }
        });
    }

    fn received_count(&self) -> usize {
        self.received.load(Ordering::Relaxed)
    }

    fn seq_gaps(&self) -> Vec<(u16, u16)> {
        self.gaps.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn test_hold_keeps_both_parties_in_recording_and_rtp_continuous() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let record_dir = tempfile::tempdir()?;
    let record_path = record_dir.path().to_string_lossy().to_string();

    let proxy_config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        recording: Some(RecordingPolicy {
            enabled: Some(true),
            auto_start: Some(true),
            path: Some(record_path.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let server = Arc::new(E2eTestServer::start_with_config(proxy_config).await?);

    // Caller media endpoint (streams the 400 Hz tone toward the PBX).
    let caller_media = RtpLink::bind().await?;
    // Callee media endpoint (streams the 800 Hz tone; its receive side tracks
    // the held leg's downlink sequence continuity).
    let callee_media = RtpLink::bind().await?;

    let alice = Arc::new(server.create_ua("alice").await?);
    let bob = server.create_ua("bob").await?;
    sleep(Duration::from_millis(150)).await;

    let caller_sdp = format!(
        "v=0\r\n\
         o=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        caller_media.local_port()?
    );

    let caller_handle = rustpbx::utils::spawn({
        let a = alice.clone();
        let sdp = caller_sdp.clone();
        async move { a.make_call("bob", Some(sdp)).await }
    });

    // Bob answers with his own media endpoint; capture the PBX-side media
    // address for his RTP from the incoming INVITE offer.
    let mut bob_dialog = None;
    let mut pbx_callee_endpoint: Option<SocketAddr> = None;
    for _ in 0..60 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, offer) = event {
                pbx_callee_endpoint = offer.as_deref().and_then(extract_media_endpoint);
                let answer = create_test_sdp("127.0.0.1", callee_media.local_port()?, false);
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
    let _bob_dialog = bob_dialog.expect("bob must receive the call");
    let pbx_callee_endpoint =
        pbx_callee_endpoint.expect("INVITE offer must carry the PBX callee media endpoint");

    let alice_dialog = tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .expect("call setup timed out")
        .expect("alice make_call failed")
        .expect("call must be established");

    // PBX caller-leg media address from the negotiated answer.
    let answer_sdp = alice
        .get_negotiated_answer_sdp(&alice_dialog)
        .await
        .expect("caller answer SDP must be negotiated");
    let pbx_caller_endpoint =
        extract_media_endpoint(&answer_sdp).expect("caller answer has no media endpoint");

    // Both parties start streaming their tones.
    caller_media.start_tone(
        pbx_caller_endpoint,
        tone_packets(CALLER_TONE_HZ, 0x1111_0001),
        20,
    );
    callee_media.start_tone(
        pbx_callee_endpoint,
        tone_packets(CALLEE_TONE_HZ, 0x2222_0002),
        20,
    );
    sleep(Duration::from_millis(1500)).await;

    // ── HOLD the callee (the CTI `call.hold` default leg) via SIP INFO ────
    alice
        .send_info(
            &alice_dialog,
            RUSTPBX_COMMAND_CT,
            br#"{"action":"hold","params":{"leg_id":"callee"}}"#.to_vec(),
        )
        .await?;
    sleep(Duration::from_millis(2200)).await;

    // ── UNHOLD ────────────────────────────────────────────────────────────
    alice
        .send_info(
            &alice_dialog,
            RUSTPBX_COMMAND_CT,
            br#"{"action":"unhold","params":{"leg_id":"callee"}}"#.to_vec(),
        )
        .await?;
    sleep(Duration::from_millis(2500)).await;

    alice.hangup(&alice_dialog).await?;
    sleep(Duration::from_millis(300)).await;

    // Recording finalized with the CDR.
    let all_records = server
        .cdr_capture
        .wait_for_records(1, Duration::from_secs(5))
        .await
        .expect("CDR must be generated");
    let record = &all_records[0];
    let expected = CdrExpectation::default().with_recording(true);
    let result = crate::common::cdr_capture::validate_cdr(record, &expected);
    assert!(
        result.is_valid,
        "CDR validation failed: {:?}",
        result.errors
    );
    let wav_path = record.recorder[0].path.clone();
    assert!(!wav_path.is_empty());

    // ── Recording content: both parties present in every phase ───────────
    let (format, channels, rate, data) = parse_wav(&wav_path)?;
    assert_eq!(format, 7, "auto-start WAV must be μ-law (PCMU)");
    assert_eq!(channels, 2, "auto-start recording must be stereo");
    let samples = decode_mulaw(&data);
    let per_channel = samples.len() / channels as usize;
    assert!(
        per_channel > (rate as usize), // ≥ 1 s of audio
        "recording too short: {per_channel} samples/channel"
    );

    let caller_channel: Vec<i16> = samples.iter().step_by(channels as usize).copied().collect();
    let callee_channel: Vec<i16> = samples
        .iter()
        .skip(1)
        .step_by(channels as usize)
        .copied()
        .collect();

    let caller_windows = [
        window(&caller_channel, PRE_WINDOW.0, PRE_WINDOW.1),
        window(&caller_channel, HOLD_WINDOW.0, HOLD_WINDOW.1),
        window(&caller_channel, POST_WINDOW.0, POST_WINDOW.1),
    ];
    let callee_windows = [
        window(&callee_channel, PRE_WINDOW.0, PRE_WINDOW.1),
        window(&callee_channel, HOLD_WINDOW.0, HOLD_WINDOW.1),
        window(&callee_channel, POST_WINDOW.0, POST_WINDOW.1),
    ];
    for (phase, w) in caller_windows.iter().enumerate() {
        let label = format!("caller channel phase {phase} (rms {:.0})", rms(w));
        assert_tone(w, rate, CALLER_TONE_HZ, &label);
    }
    for (phase, w) in callee_windows.iter().enumerate() {
        let label = if phase == 1 {
            format!("callee channel DURING HOLD (rms {:.0})", rms(w))
        } else {
            format!("callee channel phase {phase} (rms {:.0})", rms(w))
        };
        assert_tone(w, rate, CALLEE_TONE_HZ, &label);
    }

    // ── RTP timeline continuity on the held leg's downlink ───────────────
    // The callee receives relayed caller audio (pre/post) and hold music
    // (during). The server-side sequence space must be continuous across
    // both transitions — no stale-sequence restart at hold, no jump at unhold.
    let stats_gaps = callee_media.seq_gaps();
    assert!(
        callee_media.received_count() > 100,
        "callee must receive downlink RTP (got {})",
        callee_media.received_count()
    );
    for (expected, actual) in &stats_gaps {
        let forward = actual.wrapping_sub(*expected);
        let backward = expected.wrapping_sub(*actual);
        // Allow tiny forward loss/reorder (≤ 16) and tiny backward reordering.
        // A restart of the server's sequence space shows up as a large value
        // on one of the two sides.
        assert!(
            forward <= 16 || backward <= 16,
            "RTP sequence discontinuity at the held leg: expected {expected}, got {actual} \
             (forward {forward}, backward {backward}) — hold/unhold must continue the \
             relay's sequence timeline"
        );
    }

    server.stop();
    Ok(())
}
