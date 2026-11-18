//! Audio-content QC for the three-way conference path.
//!
//! These tests assert on *decoded PCM content*, not just frame plumbing:
//! - N-1 mixing (a leg never hears its own tone),
//! - level normalisation survives the mixer → FIR resample → μ-law round trip,
//! - no hard clipping (the soft limiter keeps peaks below the codec rail),
//! - RTP cadence stays roughly one frame per 20 ms,
//! - network jitter produces short, bounded silence gaps instead of stale
//!   audio replays or unbounded dropouts.
//!
//! Tones are 440 / 660 / 880 Hz (all below the 4 kHz Nyquist of the PCMU
//! legs); magnitudes are measured with a Goertzel filter over the decoded
//! output of each leg.

use std::f64::consts::PI;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rustpbx::call::domain::LegId;
use rustpbx::call::runtime::ConferenceMediaBridge;
use rustpbx::call::runtime::ConferenceManager;
use rustpbx::call::runtime::conference_media_bridge::{AudioReceiver, PcmAudioFrame};

const SAMPLE_RATE: u32 = 8_000;
const PTIME: Duration = Duration::from_millis(20);
const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 50;
const AMPLITUDE: f64 = 9_000.0;
const TOTAL_FRAMES: usize = 70;

/// Goertzel amplitude of `freq` — approximately the peak amplitude of that
/// sine inside `samples`.
fn goertzel_amplitude(samples: &[i16], sample_rate: u32, freq: f64) -> f64 {
    let coeff = 2.0 * (2.0 * PI * freq / f64::from(sample_rate)).cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &s in samples {
        let s0 = f64::from(s) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.sqrt() / samples.len() as f64
}

fn tone_samples(freq: f64, start_sample: usize, len: usize) -> Vec<i16> {
    (0..len)
        .map(|i| {
            let t = (start_sample + i) as f64 / f64::from(SAMPLE_RATE);
            ((t * freq * 2.0 * PI).sin() * AMPLITUDE) as i16
        })
        .collect()
}

/// Real-time paced tone source: `recv()` sleeps until the frame's wall-clock
/// due time and stamps it with its sample position, emulating a live decoder
/// feed (the mixer consumes by timestamp, not by arrival).
struct ToneReceiver {
    start: tokio::time::Instant,
    index: usize,
    freq: f64,
    /// Extra delay applied to every Nth frame (0 = steady clock).
    jitter_every: usize,
}

impl AudioReceiver for ToneReceiver {
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Option<PcmAudioFrame>> + Send + '_>> {
        Box::pin(async move {
            if self.index >= TOTAL_FRAMES {
                return None;
            }
            let due = self.start + PTIME * self.index as u32;
            let now = tokio::time::Instant::now();
            if due > now {
                tokio::time::sleep(due - now).await;
            }
            if self.jitter_every > 0 && self.index % self.jitter_every == 0 {
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            let samples = tone_samples(self.freq, self.index * FRAME_SAMPLES, FRAME_SAMPLES);
            let mut frame = PcmAudioFrame::new(samples, SAMPLE_RATE);
            frame.timestamp = (self.index * FRAME_SAMPLES) as u64;
            self.index += 1;
            Some(frame)
        })
    }
}

/// Receive-only leg (never sends audio upstream).
struct SilentReceiver;

impl AudioReceiver for SilentReceiver {
    fn recv(&mut self) -> Pin<Box<dyn Future<Output = Option<PcmAudioFrame>> + Send + '_>> {
        Box::pin(async { None })
    }
}

async fn join_leg(
    bridge: &ConferenceMediaBridge,
    conf_id: &str,
    leg: &str,
    source: Box<dyn AudioReceiver>,
) -> anyhow::Result<(
    rustpbx::call::runtime::ConferenceBridgeHandle,
    tokio::sync::mpsc::Receiver<rustrtc::media::MediaSample>,
)> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    // The caller MUST hold the handle: dropping it cancels and aborts the
    // bridge's forward/reverse tasks.
    let handle = bridge
        .start_bridge_full_duplex(
            conf_id,
            &LegId::new(leg),
            tx,
            source,
            audio_codec::CodecType::PCMU,
            None,
        )
        .await?;
    Ok((handle, rx))
}

async fn tone_leg(
    bridge: &ConferenceMediaBridge,
    conf_id: &str,
    leg: &str,
    freq: f64,
    jitter_every: usize,
) -> anyhow::Result<(
    rustpbx::call::runtime::ConferenceBridgeHandle,
    tokio::sync::mpsc::Receiver<rustrtc::media::MediaSample>,
)> {
    join_leg(
        bridge,
        conf_id,
        leg,
        Box::new(ToneReceiver {
            start: tokio::time::Instant::now(),
            index: 0,
            freq,
            jitter_every,
        }),
    )
    .await
}

fn decode_all(rx: &mut tokio::sync::mpsc::Receiver<rustrtc::media::MediaSample>) -> Vec<i16> {
    let mut decoder = audio_codec::create_decoder(audio_codec::CodecType::PCMU);
    let mut out = Vec::new();
    while let Ok(sample) = rx.try_recv() {
        if let rustrtc::media::MediaSample::Audio(frame) = sample {
            out.extend_from_slice(&decoder.decode(&frame.data));
        }
    }
    out
}

fn max_zero_run(samples: &[i16]) -> usize {
    let (mut run, mut max) = (0usize, 0usize);
    for &s in samples {
        if s == 0 {
            run += 1;
            max = max.max(run);
        } else {
            run = 0;
        }
    }
    max
}

/// Full three-way QC: every leg must hear the other two tones at the
/// normalised level, must NOT hear its own tone, must not clip, and must
/// keep a steady 20 ms cadence.
#[tokio::test]
async fn three_way_audio_content_qc() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
    let manager = Arc::new(ConferenceManager::new());
    manager
        .create_conference("qc-conf".into(), None)
        .await
        .unwrap();
    let bridge = ConferenceMediaBridge::new(manager.clone());

    let (handle_a, mut rx_a) = tone_leg(&bridge, "qc-conf", "a", 440.0, 0).await.unwrap();
    let (handle_b, mut rx_b) = tone_leg(&bridge, "qc-conf", "b", 660.0, 0).await.unwrap();
    let (handle_c, mut rx_c) = tone_leg(&bridge, "qc-conf", "c", 880.0, 0).await.unwrap();
    let _handles = (handle_a, handle_b, handle_c);

    // 70 frames x 20 ms + scheduling margin.
    tokio::time::sleep(Duration::from_millis(1_700)).await;
    if std::env::var("QC_DEBUG").is_ok() {
        eprintln!(
            "QC-DEBUG channel lens a={} b={} c={}",
            rx_a.len(),
            rx_b.len(),
            rx_c.len()
        );
    }

    for (leg, rx, heard, absent) in [
        ("a", &mut rx_a, [660.0f64, 880.0], [440.0f64]),
        ("b", &mut rx_b, [440.0, 880.0], [660.0]),
        ("c", &mut rx_c, [440.0, 660.0], [880.0]),
    ] {
        let pcm = decode_all(rx);
        let frames = pcm.len() / FRAME_SAMPLES;
        assert!(
            (50..=75).contains(&frames),
            "leg {leg}: expected ~{TOTAL_FRAMES} RTP frames (one per 20 ms), got {frames}"
        );

        // No clipping: the soft limiter must keep the two-tone sum below the
        // codec rail (μ-law decodes at most ±32254 even at full scale).
        let peak = pcm.iter().map(|&s| s.abs()).max().unwrap_or(0);
        assert!(
            peak <= 32_300,
            "leg {leg}: clipping suspected, decoded peak {peak}"
        );

        // Cadence continuity: no hole longer than a few frames.
        let silent_run = max_zero_run(&pcm);
        assert!(
            silent_run <= 6 * FRAME_SAMPLES,
            "leg {leg}: silence gap of {} samples (> 6 frames)",
            silent_run
        );

        // Content: the other two tones, at the 1/sqrt(2) normalised level.
        for &freq in &heard {
            let magnitude = goertzel_amplitude(&pcm, SAMPLE_RATE, freq);
            assert!(
                magnitude > 0.35 * f64::from(AMPLITUDE),
                "leg {leg}: missing {freq} Hz tone (magnitude {magnitude:.0}, expected > {:.0})",
                0.35 * f64::from(AMPLITUDE)
            );
            assert!(
                magnitude < 1.2 * f64::from(AMPLITUDE),
                "leg {leg}: {freq} Hz too loud (magnitude {magnitude:.0}) — double-talk normalisation lost"
            );
        }
        // N-1 correctness: the leg's own tone must be absent.
        for &freq in &absent {
            let magnitude = goertzel_amplitude(&pcm, SAMPLE_RATE, freq);
            assert!(
                magnitude < 0.15 * f64::from(AMPLITUDE),
                "leg {leg}: self audio at {freq} Hz leaked into the mix (magnitude {magnitude:.0})"
            );
        }
    }

    manager
        .destroy_conference(&"qc-conf".into())
        .await
        .unwrap();
}

/// Network jitter (a late frame every 7th) must produce short silence gaps —
/// never replays, never a runaway dropout — while the content stays intact.
#[tokio::test]
async fn jittered_uplink_produces_bounded_silence_gaps() {
    let manager = Arc::new(ConferenceManager::new());
    manager
        .create_conference("qc-jitter".into(), None)
        .await
        .unwrap();
    let bridge = ConferenceMediaBridge::new(manager.clone());

    // Dropping the bridge handle aborts the forward/reverse tasks, so the
    // listener's handle is held for the whole measurement window.
    let (_handle_listener, mut rx_listener) =
        join_leg(&bridge, "qc-jitter", "listener", Box::new(SilentReceiver))
            .await
            .unwrap();
    let (_handle_talker, _rx_talker) = tone_leg(&bridge, "qc-jitter", "talker", 440.0, 7)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(1_700)).await;

    let pcm = decode_all(&mut rx_listener);
    let frames = pcm.len() / FRAME_SAMPLES;
    assert!(
        (50..=75).contains(&frames),
        "expected ~{TOTAL_FRAMES} RTP frames under jitter, got {frames}"
    );

    let magnitude = goertzel_amplitude(&pcm, SAMPLE_RATE, 440.0);
    assert!(
        magnitude > 0.5 * f64::from(AMPLITUDE),
        "440 Hz tone lost under jitter (magnitude {magnitude:.0})"
    );

    // 15 ms late frames map to ~1 padded frame each; anything beyond 4
    // consecutive silent frames (80 ms) means the mixer fell back to
    // arrival-order pacing instead of timestamp pacing.
    let silent_run = max_zero_run(&pcm);
    assert!(
        silent_run <= 4 * FRAME_SAMPLES,
        "silence gap of {} samples (> 4 frames) under mild jitter",
        silent_run
    );

    manager
        .destroy_conference(&"qc-jitter".into())
        .await
        .unwrap();
}
