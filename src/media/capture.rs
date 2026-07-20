//! Unified capture pipeline for recording and sipflow.
//!
//! Design:
//! - `CaptureSink` is a lightweight cloneable handle held by the bridge /
//!   ForwardingTrack. At every caller-track recv/send chokepoint it does a
//!   non-blocking `try_send` to two bounded mpsc channels:
//!     1. `file_tx`  → consumed by `FileRecorderTask` (needs full MediaSample)
//!     2. `sipflow_tx` → consumed by the sipflow drain (needs only marshaled RTP bytes)
//! - For sipflow, raw RTP bytes are marshaled directly from `&sample` at
//!   capture time — no `Arc<MediaSample>` clone needed.
//! - `FileRecorderTask` owns the `Recorder`, uses non-blocking batch drain,
//!   flushes every `flush_interval` (default 200 ms), and finalises when the
//!   channel closes (sender dropped on session end).

use crate::media::ReceiveTimestampClock;
use crate::media::recorder::{Leg, Recorder};
use bytes::Bytes;
use rustrtc::media::MediaSample;
use rustrtc::rtp::{RtpHeader, RtpPacket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub const DEFAULT_CAPTURE_CAPACITY: usize = 512;
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 200;

const BATCH_MAX: usize = 64;

/// One captured RTP sample for the file recorder.
#[derive(Clone, Debug)]
pub struct CaptureSample {
    pub leg: Leg,
    pub sample: Arc<MediaSample>,
    pub timestamp_us: u64,
}

/// Pre-marshaled RTP packet for sipflow — no MediaSample clone needed.
#[derive(Clone, Debug)]
pub struct SipflowSample {
    pub leg: Leg,
    /// Serialised RTP bytes (header + payload), ready for storage.
    pub rtp_bytes: Bytes,
    pub timestamp_us: u64,
}

/// Cloneable, zero-allocation-on-hot-path capture handle.
///
/// `Clone` is cheap: two `Option<Sender>` clones + one `Arc` bump.
/// For samples with `raw_packet`, sipflow captures by marshaling the RTP bytes
/// directly — no `Arc<MediaSample>` allocation for the sipflow path.
#[derive(Clone)]
pub struct CaptureSink {
    file_tx: Option<tokio::sync::mpsc::Sender<CaptureSample>>,
    sipflow_tx: Option<tokio::sync::mpsc::Sender<SipflowSample>>,
    paused: Arc<AtomicBool>,
    clock: ReceiveTimestampClock,
}

impl CaptureSink {
    pub fn disabled() -> Self {
        Self {
            file_tx: None,
            sipflow_tx: None,
            paused: Arc::new(AtomicBool::new(false)),
            clock: ReceiveTimestampClock::new(),
        }
    }

    pub fn new(
        file_tx: Option<tokio::sync::mpsc::Sender<CaptureSample>>,
        sipflow_tx: Option<tokio::sync::mpsc::Sender<SipflowSample>>,
        paused: Arc<AtomicBool>,
    ) -> Self {
        Self {
            file_tx,
            sipflow_tx,
            paused,
            clock: ReceiveTimestampClock::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.file_tx.is_some() || self.sipflow_tx.is_some()
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// Marshal raw RTP bytes from an AudioFrame, or synthesise them for
    /// locally-generated samples that carry no raw_packet.
    #[inline]
    fn marshal_for_sipflow(frame: &rustrtc::media::frame::AudioFrame) -> Option<Bytes> {
        match &frame.raw_packet {
            Some(pkt) => pkt.marshal().ok().map(Bytes::from),
            None => {
                // Locally-generated audio (IVR, hold music) — synthesise.
                let header = RtpHeader::new(
                    frame.payload_type.unwrap_or(0),
                    frame.sequence_number.unwrap_or(0),
                    frame.rtp_timestamp,
                    0,
                );
                let pkt = RtpPacket::new(header, frame.data.to_vec());
                pkt.marshal().ok().map(Bytes::from)
            }
        }
    }

    #[inline]
    pub fn tap(&self, leg: Leg, sample: &MediaSample) {
        if !self.is_active() {
            return;
        }
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        let frame = match sample {
            MediaSample::Audio(f) => f,
            _ => return,
        };
        let ts = self.clock.now_micros();

        if let Some(tx) = &self.file_tx {
            let _ = tx.try_send(CaptureSample {
                leg,
                sample: Arc::new(sample.clone()),
                timestamp_us: ts,
            });
        }
        if let Some(tx) = &self.sipflow_tx {
            if let Some(bytes) = Self::marshal_for_sipflow(frame) {
                let _ = tx.try_send(SipflowSample {
                    leg,
                    rtp_bytes: bytes,
                    timestamp_us: ts,
                });
            }
        }
    }
}

/// File recorder drain task — bound to session lifetime.
///
/// Lifecycle:
/// - Spawned when recording starts (or session created).
/// - Reads `CaptureSample`s via `recv` + `recv_many`, writes to `Recorder`.
/// - Periodic flush every `flush_interval`.
/// - When the last sender drops → `rx.recv()` returns `None` → final
///   `write_batch` + `finalize` + exit.  No cancel-token needed.
pub struct FileRecorderTask {
    tx: tokio::sync::mpsc::Sender<CaptureSample>,
}

impl FileRecorderTask {
    /// Spawn the task and return a sender for the capture sink.
    pub fn spawn(
        recorder: Arc<parking_lot::RwLock<Option<Recorder>>>,
        flush_interval: Duration,
        channel_capacity: usize,
    ) -> Self {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<CaptureSample>(channel_capacity.max(1));

        crate::utils::spawn(async move {
            let mut buf: Vec<CaptureSample> = Vec::with_capacity(BATCH_MAX);

            loop {
                tokio::select! {
                    biased; // prefer data over timer
                    pkt = rx.recv() => {
                        match pkt {
                            Some(p) => {
                                buf.push(p);
                                // Non-blocking drain of any remaining buffered
                                // messages — avoids blocking the periodic
                                // flush timer.
                                while buf.len() < BATCH_MAX {
                                    match rx.try_recv() {
                                        Ok(p) => buf.push(p),
                                        Err(_) => break,
    }
                                }
                                Self::write_batch(&recorder, &mut buf);
                            }
                            None => {
                                Self::write_batch(&recorder, &mut buf);
                                let mut g = recorder.write();
                                if let Some(r) = g.as_mut() {
                                    let _ = r.finalize();
                                }
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(flush_interval) => {
                        if !buf.is_empty() {
                            Self::write_batch(&recorder, &mut buf);
                        } else if let Some(r) = recorder.write().as_mut() {
                            let _ = r.flush();
                        }
                    }
                }
            }
        });

        Self { tx }
    }

    /// Get a sender for wiring into `CaptureSink`.
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<CaptureSample> {
        self.tx.clone()
    }
}

impl FileRecorderTask {
    fn write_batch(
        recorder: &Arc<parking_lot::RwLock<Option<Recorder>>>,
        buf: &mut Vec<CaptureSample>,
    ) {
        if buf.is_empty() {
            return;
        }
        let mut guard = recorder.write();
        let Some(r) = guard.as_mut() else {
            buf.clear();
            return;
        };
        for pkt in buf.drain(..) {
            if let Err(e) = r.write_sample(pkt.leg, &pkt.sample, None, None, None) {
                warn!("recorder write_sample error: {e}");
            }
        }
        let _ = r.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::recorder::Recorder;
    use audio_codec::CodecType;
    use rustrtc::media::frame::AudioFrame;

    fn make_pcmu_sample(seq: u16, ts: u32, data: Vec<u8>, pt: u8) -> MediaSample {
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: ts,
            clock_rate: 8000,
            data: bytes::Bytes::from(data),
            sequence_number: Some(seq),
            payload_type: Some(pt),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        })
    }

    fn pcmu_profile() -> crate::media::negotiate::NegotiatedLegProfile {
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: None,
            transport: rustrtc::TransportMode::Rtp,
        }
    }

    #[tokio::test]
    async fn test_capture_sink_filters_non_audio() {
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sink = CaptureSink::new(Some(tx), None, paused);

        let sample = make_pcmu_sample(0, 0, vec![0u8; 160], 0);
        sink.tap(Leg::A, &sample);
        assert!(rx.recv().await.is_some(), "audio should be captured");
    }

    #[tokio::test]
    async fn test_capture_sink_paused() {
        let paused = Arc::new(AtomicBool::new(true));
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sink = CaptureSink::new(Some(tx), None, paused);

        let sample = make_pcmu_sample(0, 0, vec![0u8; 160], 0);
        sink.tap(Leg::A, &sample);
        let result = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(result.is_err(), "paused sink should not capture");
    }

    #[tokio::test]
    async fn test_capture_sink_tees_to_both_channels() {
        let paused = Arc::new(AtomicBool::new(false));
        let (file_tx, mut file_rx) = tokio::sync::mpsc::channel(10);
        let (sf_tx, mut sf_rx) = tokio::sync::mpsc::channel(10);
        let sink = CaptureSink::new(Some(file_tx), Some(sf_tx), paused);

        let sample = make_pcmu_sample(0, 0, vec![0u8; 160], 0);
        sink.tap(Leg::A, &sample);

        let f = file_rx.recv().await.unwrap();
        assert_eq!(f.leg, Leg::A);
        let s = sf_rx.recv().await.unwrap();
        assert_eq!(s.leg, Leg::A);
    }

    #[test]
    fn test_capture_sink_disabled() {
        let sink = CaptureSink::disabled();
        assert!(!sink.is_active());
        let sample = make_pcmu_sample(0, 0, vec![0u8; 160], 0);
        sink.tap(Leg::A, &sample);
    }

    #[tokio::test]
    async fn test_capture_sink_clone_and_tap() {
        let paused = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sink = CaptureSink::new(Some(tx), None, paused);
        let sink2 = sink.clone();

        let sample = make_pcmu_sample(0, 0, vec![0u8; 160], 0);
        sink.tap(Leg::A, &sample);
        sink2.tap(Leg::B, &sample);

        let p1 = rx.recv().await.unwrap();
        assert_eq!(p1.leg, Leg::A);
        let p2 = rx.recv().await.unwrap();
        assert_eq!(p2.leg, Leg::B);
    }

    #[tokio::test]
    async fn test_file_recorder_writes_and_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        {
            let mut g = arc.write();
            let p = pcmu_profile();
            g.as_mut().unwrap().set_leg_profile(Leg::A, p.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, p);
        }

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        // Send 10 PCMU samples (200ms of audio)
        for i in 0..10u16 {
            let sample = make_pcmu_sample(i, i as u32 * 160, vec![0x55u8; 160], 0);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(sample),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }

        // Drop sender → triggers final flush + finalize
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(
            metadata.len() > 44,
            "WAV should have header + data, got {} bytes",
            metadata.len()
        );
    }

    #[tokio::test]
    async fn test_file_recorder_flush_on_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flush_test.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        {
            let mut g = arc.write();
            let p = pcmu_profile();
            g.as_mut().unwrap().set_leg_profile(Leg::A, p.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, p);
        }

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_millis(50), 64);

        let sample = make_pcmu_sample(0, 0, vec![0x55u8; 160], 0);
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(sample),
                timestamp_us: 0,
            })
            .await
            .unwrap();

        // Wait for periodic flush
        tokio::time::sleep(Duration::from_millis(200)).await;

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(
            metadata.len() > 44,
            "WAV should have flushed data, got {} bytes",
            metadata.len()
        );

        drop(task);
    }

    #[tokio::test]
    async fn test_file_recorder_dtmf_rendered_as_tone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_test.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));

        // Profile with DTMF PT=101
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: Some(NegotiatedCodec {
                payload_type: 101,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::Rtp,
        };
        {
            let mut g = arc.write();
            g.as_mut().unwrap().set_leg_profile(Leg::A, profile.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, profile);
        }

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        // DTMF digit '5' (event code 5), duration 160 ticks (20ms @ 8kHz)
        let dtmf_payload = vec![5u8, 0x00, 0x00, 0xA0];
        let dtmf_sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 0,
            clock_rate: 8000,
            data: bytes::Bytes::from(dtmf_payload),
            sequence_number: Some(0),
            payload_type: Some(101),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(dtmf_sample),
                timestamp_us: 0,
            })
            .await
            .unwrap();

        // End-of-event packet (byte 1 has end bit = 0x80)
        let dtmf_end = vec![5u8, 0x80, 0x00, 0xA0];
        let dtmf_end_sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 0,
            clock_rate: 8000,
            data: bytes::Bytes::from(dtmf_end),
            sequence_number: Some(1),
            payload_type: Some(101),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(dtmf_end_sample),
                timestamp_us: 20000,
            })
            .await
            .unwrap();

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(
            metadata.len() > 44,
            "WAV with DTMF should have data, got {} bytes",
            metadata.len()
        );
    }

    // ── helpers for advanced tests ────────────────────────────────────

    use audio_codec::create_encoder;

    /// Generate a sine-wave tone as i16 PCM samples.
    struct Tone {
        samples: Vec<i16>,
    }

    impl Tone {
        fn generate(freq: f32, sample_rate: u32, duration_ms: u32) -> Self {
            let n = (sample_rate as usize * duration_ms as usize / 1000).max(1);
            let samples: Vec<i16> = (0..n)
                .map(|i| {
                    ((2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin()
                        * 15000.0) as i16
                })
                .collect();
            Self { samples }
        }

        /// Encode this tone into a codec-specific byte array via audio_codec.
        fn encode(&self, codec: CodecType) -> Vec<u8> {
            match codec {
                CodecType::PCMU | CodecType::PCMA | CodecType::G722 | CodecType::G729 => {
                    let mut enc = create_encoder(codec);
                    enc.encode(&self.samples)
                }
                _ => {
                    // Raw PCM 16-bit little-endian
                    let bytes: Vec<u8> =
                        self.samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                    bytes
                }
            }
        }

        fn as_media_sample(
            &self,
            codec: CodecType,
            seq: u16,
            rtp_ts: u32,
        ) -> MediaSample {
            let pt = codec.payload_type();
            let clock_rate = codec.clock_rate();
            let data = self.encode(codec);
            raw_audio_sample(seq, rtp_ts, data, pt, clock_rate.max(1))
        }
    }

    /// Decode WAV audio data back to PCM i16.
    /// Returns (channel_count, sample_rate, left_samples, right_samples).
    fn decode_wav(path: &std::path::Path) -> (u16, u32, Vec<i16>, Vec<i16>) {
        let raw = std::fs::read(path).unwrap();
        assert!(raw.len() > 44, "WAV too short: {}", raw.len());
        assert_eq!(&raw[0..4], b"RIFF");
        assert_eq!(&raw[8..12], b"WAVE");

        let format_tag = u16::from_le_bytes([raw[20], raw[21]]);
        let channels = u16::from_le_bytes([raw[22], raw[23]]);
        let sample_rate = u32::from_le_bytes([raw[24], raw[25], raw[26], raw[27]]);

        let codec: CodecType = match format_tag {
            1 => CodecType::PCMU, // PCM → treat as raw (decoder handles)
            6 => CodecType::PCMA,
            7 => CodecType::PCMU,
            0x0065 => CodecType::G722,
            0x0083 => CodecType::G729,
            _ => CodecType::PCMU,
        };

        let body = &raw[44..];

        if codec == CodecType::PCMU || codec == CodecType::PCMA {
            let mut decoder = audio_codec::create_decoder(codec);
            let pcm = decoder.decode(body);
            split_stereo(pcm, channels, sample_rate)
        } else if codec == CodecType::G722 {
            let mut decoder = audio_codec::create_decoder(codec);
            let pcm = decoder.decode(body);
            split_stereo(pcm, channels, sample_rate)
        } else if codec == CodecType::G729 {
            let mut decoder = audio_codec::create_decoder(codec);
            let mut all_pcm = Vec::new();
            for chunk in body.chunks(10) {
                if chunk.len() == 10 {
                    let pcm = decoder.decode(chunk);
                    all_pcm.extend_from_slice(&pcm);
                }
            }
            split_stereo(all_pcm, channels, sample_rate)
        } else {
            let pcm: Vec<i16> = body
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            split_stereo(pcm, channels, sample_rate)
        }
    }

    fn split_stereo(pcm: Vec<i16>, channels: u16, sample_rate: u32) -> (u16, u32, Vec<i16>, Vec<i16>) {
        if channels >= 2 {
            let mut left = Vec::with_capacity(pcm.len() / 2);
            let mut right = Vec::with_capacity(pcm.len() / 2);
            for pair in pcm.chunks(2) {
                left.push(pair.first().copied().unwrap_or(0));
                right.push(pair.get(1).copied().unwrap_or(0));
            }
            (channels, sample_rate, left, right)
        } else {
            (channels, sample_rate, pcm, Vec::new())
        }
    }

    /// Energy (sum of absolute values) — quick check that signal isn't silent.
    fn energy(samples: &[i16]) -> u64 {
        samples.iter().map(|s| s.abs() as u64).sum()
    }

    /// Create a MediaSample with raw_packet carrying the given payload bytes.
    fn raw_audio_sample(seq: u16, ts: u32, data: Vec<u8>, pt: u8, clock_rate: u32) -> MediaSample {
        let header = rustrtc::rtp::RtpHeader::new(pt, seq, ts, 1);
        let packet = rustrtc::rtp::RtpPacket::new(header, data.clone());
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: ts,
            clock_rate,
            data: bytes::Bytes::from(data),
            sequence_number: Some(seq),
            payload_type: Some(pt),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: Some(packet),
        })
    }

    fn setup_recorder(
        path: &str,
        codec: CodecType,
        audio_pt: u8,
        dtmf_pt: Option<u8>,
    ) -> Arc<parking_lot::RwLock<Option<Recorder>>> {
        let rec = Recorder::new(path, codec).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: audio_pt,
                codec,
                clock_rate: codec.clock_rate().max(1),
                channels: 1,
            }),
            video: None,
            dtmf: dtmf_pt.map(|pt| NegotiatedCodec {
                payload_type: pt,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::Rtp,
        };
        let mut g = arc.write();
        g.as_mut().unwrap().set_leg_profile(Leg::A, profile.clone());
        g.as_mut().unwrap().set_leg_profile(Leg::B, profile);
        drop(g);
        arc
    }

    // ── ptime 20 ms — PCMU throughput ──────────────────────────────────

    #[tokio::test]
    async fn ptime20_pcmu_both_legs_stereo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ptime20.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, CodecType::PCMU.payload_type(), None);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone_a = Tone::generate(440.0, 8000, 20);
        let tone_b = Tone::generate(220.0, 8000, 20);

        // 25 frames × 20 ms = 500 ms of audio
        for i in 0..25u16 {
            let a = tone_a.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(a),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
            let b = tone_b.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::B,
                    sample: Arc::new(b),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        // 25 frames × 160 samples × 2 legs × 1 byte each (PCMU interleaved) + 44 header
        assert!(size >= 44 + (25 * 160 * 2), "stereo ptime20 size={size}");
    }

    // ── jitter ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn jitter_irregular_timestamps_does_not_corrupt_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jitter.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, CodecType::PCMU.payload_type(), None);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone = Tone::generate(440.0, 8000, 20);
        // Deliberately irregular timestamps: early, late, reordered
        let schedule: Vec<(u32, u64)> = vec![
            (0, 0), (160, 20000), (320, 40000), // normal
            (640, 60000), // skip 480
            (480, 80000), // late arrival for 480 (30ms delay)
            (800, 100000), (960, 120000), // normal
            (1280, 130000), (1120, 140000), // reordered
        ];
        for (rtp_ts, wall_us) in schedule {
            let s = tone.as_media_sample(CodecType::PCMU, 0, rtp_ts);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: wall_us,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 44, "jitter WAV size={size}");
    }

    // ── codec transcoding ────────────────────────────────────────────

    #[tokio::test]
    async fn opus_transcoded_to_pcmu_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opus2pcmu.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::Opus).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));

        // Profile with Opus audio PT=111, DTMF PT=101
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 111,
                codec: CodecType::Opus,
                clock_rate: 48000,
                channels: 1,
            }),
            video: None,
            dtmf: Some(NegotiatedCodec {
                payload_type: 101,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::WebRtc,
        };
        {
            let mut g = arc.write();
            g.as_mut().unwrap().set_leg_profile(Leg::A, profile.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, profile);
        }

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);
        

        // Opus samples at ptime 20ms, 48kHz → 960 PCM samples per frame
        let opus_tone = Tone::generate(440.0, 48000, 20);

        for i in 0..25u16 {
            let s = opus_tone.as_media_sample(CodecType::Opus, i, i as u32 * 960);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 44, "opus→pcmu WAV size={size}");
    }

    // ── DTMF through pipeline ──────────────────────────────────────

    #[tokio::test]
    async fn dtmf_tone_appears_in_wav_for_both_legs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_both.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: Some(NegotiatedCodec {
                payload_type: 101,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::Rtp,
        };
        {
            let mut g = arc.write();
            g.as_mut().unwrap().set_leg_profile(Leg::A, profile.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, profile);
        }

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        // Send audio, then DTMF, then audio for both legs
        let tone = Tone::generate(440.0, 8000, 20);
        let dtmf_payload = |digit: u8, end: bool| -> Vec<u8> {
            vec![digit, if end { 0x80 } else { 0x00 }, 0x00, 0xA0] // 160 ticks duration
        };

        for leg in [Leg::A, Leg::B] {
        // Audio before DTMF
        for i in 0..5u16 {
            let s = Tone::generate(440.0, 8000, 20).as_media_sample(CodecType::PCMU, i, i as u32 * 160);
                task.sender()
                    .send(CaptureSample {
                        leg,
                        sample: Arc::new(s),
                        timestamp_us: i as u64 * 20000,
                    })
                    .await
                    .unwrap();
            }
            // DTMF digit '1' (start + end)
            let dtmf_start = MediaSample::Audio(AudioFrame {
                rtp_timestamp: 800,
                clock_rate: 8000,
                data: bytes::Bytes::from(dtmf_payload(1, false)),
                sequence_number: Some(5),
                payload_type: Some(101),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            let dtmf_end = MediaSample::Audio(AudioFrame {
                rtp_timestamp: 800,
                clock_rate: 8000,
                data: bytes::Bytes::from(dtmf_payload(1, true)),
                sequence_number: Some(6),
                payload_type: Some(101),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender()
                .send(CaptureSample {
                    leg,
                    sample: Arc::new(dtmf_start),
                    timestamp_us: 100000,
                })
                .await
                .unwrap();
            task.sender()
                .send(CaptureSample {
                    leg,
                    sample: Arc::new(dtmf_end),
                    timestamp_us: 120000,
                })
                .await
                .unwrap();
            // Audio after DTMF
            for i in 0..5u16 {
                let s = tone.as_media_sample(CodecType::PCMU, i + 10, 1600 + i as u32 * 160);
                task.sender()
                    .send(CaptureSample {
                        leg,
                        sample: Arc::new(s),
                        timestamp_us: 140000 + i as u64 * 20000,
                    })
                    .await
                    .unwrap();
            }
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 44, "DTMF both legs WAV size={size}");
    }

    // ── DTMF through pipeline with Opus codec ──────────────────────────

    #[tokio::test]
    async fn dtmf_tone_in_opus_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_opus.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::Opus).unwrap();
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 111,
                codec: CodecType::Opus,
                clock_rate: 48000,
                channels: 1,
            }),
            video: None,
            dtmf: Some(NegotiatedCodec {
                payload_type: 101,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::WebRtc,
        };
        {
            let mut g = arc.write();
            g.as_mut().unwrap().set_leg_profile(Leg::A, profile.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, profile);
        }

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        // Opus audio for 2 frames
        let opus_tone = Tone::generate(440.0, 48000, 20);
        for i in 0..5u16 {
            let s = opus_tone.as_media_sample(CodecType::Opus, i, i as u32 * 960);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }

        // DTMF '#' with end bit
        let dtmf_end = vec![11u8, 0x80, 0x01, 0x60]; // '#' end, duration 352 ticks
        let dtmf_sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 4800,
            clock_rate: 8000,
            data: bytes::Bytes::from(dtmf_end),
            sequence_number: Some(5),
            payload_type: Some(101),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(dtmf_sample),
                timestamp_us: 100000,
            })
            .await
            .unwrap();

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 44, "DTMF Opus WAV size={size}");
    }

    // ── dual consumer: file + sipflow ──────────────────────────────

    #[tokio::test]
    async fn dual_consumer_file_and_sipflow_receive_same_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dual.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, CodecType::PCMU.payload_type(), None);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let (sf_tx, mut sf_rx) = tokio::sync::mpsc::channel::<SipflowSample>(64);
        let paused = Arc::new(AtomicBool::new(false));
        let sink = CaptureSink::new(Some(task.sender()), Some(sf_tx), paused);

        let tone = Tone::generate(440.0, 8000, 20);
        for i in 0..5u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            sink.tap(if i % 2 == 0 { Leg::A } else { Leg::B }, &s);
        }

        // Drain sipflow — verify each sample arrives with marshaled bytes
        let mut sf_count = 0usize;
        while let Ok(pkt) = tokio::time::timeout(Duration::from_millis(200), sf_rx.recv()).await {
            match pkt {
                Some(p) => {
                    sf_count += 1;
                    assert!(p.leg == Leg::A || p.leg == Leg::B);
                    assert!(!p.rtp_bytes.is_empty(), "sipflow should have marshaled RTP bytes");
                }
                None => break,
            }
        }

        assert_eq!(sf_count, 5, "sipflow should receive all 5 samples");

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(std::fs::metadata(&path).unwrap().len() > 44, "WAV should have data");
    }

    // ── backpressure: channel at capacity ─────────────────────────

    #[tokio::test]
    async fn backpressure_channel_full_drops_non_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bp.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, CodecType::PCMU.payload_type(), None);
        // Tiny channel capacity
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_millis(50), 2);

        let tone = Tone::generate(440.0, 8000, 20);
        // Flood the channel faster than the drain can process
        for i in 0..50u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            let _ = task
                .sender()
                .try_send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: i as u64 * 20000,
                });
            // Don't await — this will fill the channel and start dropping
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        // Should have written SOME data, just not all 50 frames
        assert!(size > 44, "backpressure WAV size={size}");
    }

    // ── pause / resume ──────────────────────────────────────────────

    #[tokio::test]
    async fn pause_stops_capture_resume_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pause.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, CodecType::PCMU.payload_type(), None);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_millis(50), 64);

        let paused = Arc::new(AtomicBool::new(false));
        let sink = CaptureSink::new(Some(task.sender()), None, Arc::clone(&paused));
        let tone = Tone::generate(440.0, 8000, 20);

        // Send 5 samples while active
        for i in 0..5u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            sink.tap(Leg::A, &s);
        }

        // Pause
        sink.set_paused(true);

        // These should be dropped
        for i in 5..10u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            sink.tap(Leg::A, &s);
        }

        // Resume
        sink.set_paused(false);

        for i in 10..15u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i + 10, i as u32 * 160);
            sink.tap(Leg::A, &s);
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        // Should have data from first 5 + last 5 frames (10 total), not middle 5
        assert!(size > 44, "pause WAV size={size}");
    }

    // ── multi-leg interleaving ──────────────────────────────────────

    #[tokio::test]
    async fn interleaved_ab_same_timestamp_writes_stereo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interleaved.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, CodecType::PCMU.payload_type(), None);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone_a = Tone::generate(440.0, 8000, 20);
        let tone_b = Tone::generate(220.0, 8000, 20);

        for i in 0..20u16 {
            let a = tone_a.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            let b = tone_b.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(a),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
            task.sender()
                .send(CaptureSample {
                    leg: Leg::B,
                    sample: Arc::new(b),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let data = std::fs::read(&path).unwrap();
        assert!(data.len() > 44, "interleaved WAV data len={}", data.len());
        // PCMU stereo: every byte pair should have both legs (interleaved)
        // Even bytes [44..] = Leg A, odd bytes = Leg B
        if data.len() > 100 {
            let body = &data[44..];
            // Spot-check: both channels should not be all-silence
            let mut a_energy: u64 = 0;
            let mut b_energy: u64 = 0;
            for (_i, chunk) in body.chunks(2).enumerate().take(100) {
                if chunk.len() == 2 {
                    a_energy += chunk[0] as u64;
                    b_energy += chunk[1] as u64;
                }
            }
            assert!(a_energy > 0, "Leg A should have audio energy");
            assert!(b_energy > 0, "Leg B should have audio energy");
        }
    }

    // ── cross-codec roundtrip verification ─────────────────────────────

    /// Helper: send tone through the pipeline and decode WAV back.
    /// Returns (channels, sample_rate, left_pcm, right_pcm).
    async fn roundtrip(
        codec: CodecType,
        audio_pt: u8,
        dtmf_pt: Option<u8>,
        frames: u16,
    ) -> (u16, u32, Vec<i16>, Vec<i16>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.wav");
        let arc = setup_recorder(path.to_str().unwrap(), codec, audio_pt, dtmf_pt);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone = if codec == CodecType::Opus {
            Tone::generate(440.0, 48000, 20)
        } else if codec == CodecType::G722 {
            Tone::generate(440.0, 16000, 20)
        } else {
            Tone::generate(440.0, 8000, 20)
        };

        let samples_per_frame = codec.clock_rate().max(1) as u32 / 50;

        for i in 0..frames {
            let s = tone.as_media_sample(codec, i, i as u32 * samples_per_frame);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        decode_wav(&path)
    }

    #[tokio::test]
    async fn pcmu_roundtrip_preserves_tone_energy() {
        let (ch, _sr, left, _right) =
            roundtrip(CodecType::PCMU, 0, None, 25).await;
        assert_eq!(ch, 2, "stereo");
        assert!(left.len() >= 400, "got {} samples", left.len());
        let e = energy(&left);
        assert!(e > 5000, "PCMU tone energy should be high, got {e}");
    }

    #[tokio::test]
    async fn pcma_roundtrip_preserves_tone_energy() {
        let (ch, _sr, left, _right) =
            roundtrip(CodecType::PCMA, 8, None, 25).await;
        assert_eq!(ch, 2, "stereo");
        let e = energy(&left);
        assert!(e > 5000, "PCMA tone energy should be high, got {e}");
    }

    #[tokio::test]
    async fn raw_pcm_roundtrip_preserves_tone_energy() {
        let (_ch, _sr, left, _right) =
            roundtrip(CodecType::PCMU, 0, None, 25).await;
        let e = energy(&left);
        assert!(e > 5000, "raw PCM energy got {e}");
    }

    #[tokio::test]
    async fn opus_to_pcmu_roundtrip_preserves_tone_energy() {
        let (_ch, _sr, left, _right) =
            roundtrip(CodecType::Opus, 111, None, 25).await;
        let e = energy(&left);
        assert!(e > 5000, "Opus→PCMU tone energy should be high, got {e}");
    }

    #[tokio::test]
    async fn dtmf_pcmu_roundtrip_produces_audible_tone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_rt.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, 0, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let dtmf_payload = vec![5u8, 0x80, 0x00, 0xA0];
        let dtmf_sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 0,
            clock_rate: 8000,
            data: bytes::Bytes::from(dtmf_payload),
            sequence_number: Some(0),
            payload_type: Some(101),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(dtmf_sample),
                timestamp_us: 0,
            })
            .await
            .unwrap();

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, _right) = decode_wav(&path);
        let e = energy(&left);
        assert!(e > 100, "DTMF tone energy got {e}");
    }

    #[tokio::test]
    async fn dtmf_opus_roundtrip_produces_audible_tone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_opus_rt.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::Opus, 111, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let dtmf_payload = vec![5u8, 0x80, 0x00, 0xA0];
        let dtmf_sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 0,
            clock_rate: 8000,
            data: bytes::Bytes::from(dtmf_payload),
            sequence_number: Some(0),
            payload_type: Some(101),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(dtmf_sample),
                timestamp_us: 0,
            })
            .await
            .unwrap();

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, _right) = decode_wav(&path);
        let e = energy(&left);
        assert!(e > 100, "DTMF Opus tone energy got {e}");
    }

    // ── stereo_swap ───────────────────────────────────────────────────

    #[tokio::test]
    async fn stereo_swap_puts_callee_on_left_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swap.wav");
        let mut rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();
        rec.stereo_swap = true;
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        {
            let p = pcmu_profile();
            let mut g = arc.write();
            g.as_mut().unwrap().set_leg_profile(Leg::A, p.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, p);
        }
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        // Leg A: 440 Hz tone (high energy)
        let tone_a = Tone::generate(440.0, 8000, 20);
        // Leg B: much quieter tone (low energy)
        let tone_b = Tone::generate(440.0, 8000, 20);
        let quiet_b: Vec<i16> = tone_b.samples.iter().map(|s| s / 10).collect();

        for i in 0..25u16 {
            let a = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 160,
                clock_rate: 8000,
                data: bytes::Bytes::from(tone_a.encode(CodecType::PCMU)),
                sequence_number: Some(i),
                payload_type: Some(0),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(a),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();

            let b = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 160,
                clock_rate: 8000,
                data: bytes::Bytes::from(create_encoder(CodecType::PCMU).encode(&quiet_b)),
                sequence_number: Some(i),
                payload_type: Some(0),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender()
                .send(CaptureSample {
                    leg: Leg::B,
                    sample: Arc::new(b),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, right) = decode_wav(&path);
        let left_energy = energy(&left);
        let right_energy = energy(&right);

        // With stereo_swap: left = Leg B (quiet), right = Leg A (loud)
        assert!(
            right_energy > left_energy * 3,
            "stereo_swap: right (leg A) should be much louder than left (leg B), got left={left_energy} right={right_energy}"
        );
    }

    #[tokio::test]
    async fn no_stereo_swap_keeps_caller_on_left_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noswap.wav");
        let rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();
        // stereo_swap = false (default)
        let arc = Arc::new(parking_lot::RwLock::new(Some(rec)));
        {
            let p = pcmu_profile();
            let mut g = arc.write();
            g.as_mut().unwrap().set_leg_profile(Leg::A, p.clone());
            g.as_mut().unwrap().set_leg_profile(Leg::B, p);
        }
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone_a = Tone::generate(440.0, 8000, 20);
        let quiet_b: Vec<i16> = tone_a.samples.iter().map(|s| s / 10).collect();

        for i in 0..25u16 {
            let a = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 160,
                clock_rate: 8000,
                data: bytes::Bytes::from(tone_a.encode(CodecType::PCMU)),
                sequence_number: Some(i),
                payload_type: Some(0),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(a),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();

            let b = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 160,
                clock_rate: 8000,
                data: bytes::Bytes::from(create_encoder(CodecType::PCMU).encode(&quiet_b)),
                sequence_number: Some(i),
                payload_type: Some(0),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender()
                .send(CaptureSample {
                    leg: Leg::B,
                    sample: Arc::new(b),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, right) = decode_wav(&path);
        let left_energy = energy(&left);
        let right_energy = energy(&right);

        // Without stereo_swap: left = Leg A (loud), right = Leg B (quiet)
        assert!(
            left_energy > right_energy * 3,
            "no swap: left (leg A) should be much louder than right (leg B), got left={left_energy} right={right_energy}"
        );
    }

    // ── regression: start_offset must not create huge silence gaps ────

    #[tokio::test]
    async fn delayed_first_sample_does_not_produce_huge_silence() {
        // With wall-clock positioning, a delay before the first sample
        // simply means the recording starts later — no silence gap.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delayed.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, 0, None);

        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_millis(50), 64);
        let tone = Tone::generate(440.0, 8000, 20);

        // Wait 2 seconds before sending the first sample
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Send just 5 frames (100ms of audio)
        for i in 0..5u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: 2_000_000 + i as u64 * 20_000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let size = std::fs::metadata(&path).unwrap().len();
        // Wall-clock: first sample at t=2s, last at t=2.1s → 0.1s of audio
        // WAV should be small (~2KB), NOT 2+ seconds of silence
        assert!(
            size < 5_000,
            "delayed WAV should be small (<5KB), got {size} bytes — wall-clock positioning bug!"
        );
    }

    #[tokio::test]
    async fn red_wrapped_opus_is_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("red.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::Opus, 111, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone = Tone::generate(440.0, 48000, 20);
        let opus_data = tone.encode(CodecType::Opus);

        // --- Test BOTH single-block and multi-block RED ---

        for i in 0..25u16 {
            let red_data = if i % 2 == 0 {
                // Single-block RED: [0|111, opus_payload...]
                let mut d = vec![0x6Fu8];
                d.extend_from_slice(&opus_data);
                d
            } else {
                // Multi-block RED (2 blocks):
                // [F=1|111] [ts_off_hi] [ts_off_lo|len_hi] [len_lo] [F=0|111] [redundant] [primary]
                let redundant = &opus_data;
                let rlen = redundant.len();
                let mut d = vec![0u8; 5 + rlen + opus_data.len()];
                d[0] = 0x80 | 111;     // F=1, PT=111 (redundant block)
                d[1] = 0x00;            // timestamp offset high
                d[2] = 0x00;            // ts_off_lo=0, len_hi=0
                d[3] = (rlen & 0xFF) as u8; // len_lo
                d[4] = 0x6F;            // F=0, PT=111 (primary block)
                d[5..5 + rlen].copy_from_slice(redundant);
                d[5 + rlen..].copy_from_slice(&opus_data);
                d
            };

            let sample = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 960,
                clock_rate: 48000,
                data: bytes::Bytes::from(red_data),
                sequence_number: Some(i),
                payload_type: Some(63),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(sample),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }
        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, _right) = decode_wav(&path);
        let e = energy(&left);
        assert!(
            e > 5000,
            "RED-wrapped Opus (single+multi block) should produce audio, got energy={e}"
        );
    }

    #[tokio::test]
    async fn dtmf_telephone_event_recorded_on_correct_leg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_leg.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, 0, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        // Send some PCMU audio on Leg A
        let tone = Tone::generate(440.0, 8000, 20);
        for i in 0..10u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: i as u64 * 20000,
                })
                .await
                .unwrap();
        }

        // Send DTMF '5' (end-of-event) on Leg A with PT=101
        let dtmf_end = vec![5u8, 0x80, 0x00, 0xA0];
        let dtmf_sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 1600,
            clock_rate: 8000,
            data: bytes::Bytes::from(dtmf_end),
            sequence_number: Some(10),
            payload_type: Some(101),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender()
            .send(CaptureSample {
                leg: Leg::A,
                sample: Arc::new(dtmf_sample),
                timestamp_us: 200000,
            })
            .await
            .unwrap();

        // Send more audio after DTMF
        for i in 0..10u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i + 20, 3200 + i as u32 * 160);
            task.sender()
                .send(CaptureSample {
                    leg: Leg::A,
                    sample: Arc::new(s),
                    timestamp_us: 220000 + i as u64 * 20000,
                })
                .await
                .unwrap();
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, _right) = decode_wav(&path);
        // Left should have: audio(200ms) + DTMF tone(~20ms) + audio(200ms)
        // Total ~420ms → ~3360 samples at 8kHz
        assert!(left.len() > 2000, "DTMF test: left too short, got {} samples", left.len());
        let e = energy(&left);
        assert!(e > 10000, "DTMF test: energy too low, got {e}");
    }

    /// Full simulation: both legs Opus/48kHz, caller sends multi-block RED,
    /// IVR sends raw Opus, DTMF in the middle, interleaved batches.
    /// Verifies: (1) caller audio present, (2) IVR audio present,
    /// (3) DTMF tone present, (4) no huge silence gaps.
    #[tokio::test]
    async fn full_opus_scenario_caller_red_ivr_raw_dtmf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full_opus.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::Opus, 111, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_millis(50), 256);

        let _caller_tone = Tone::generate(440.0, 48000, 20);
        let _ivr_tone = Tone::generate(300.0, 48000, 20);

        // Generate a CONTINUOUS 4-second tone and encode each 20ms segment
        // separately — reusing the same Opus frame causes DTX silence.
        let continuous: Vec<i16> = (0..48000 * 4)
            .map(|i| ((2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48000.0).sin() * 15000.0) as i16)
            .collect();
        let mut opus_frames = Vec::new();
        let mut enc = create_encoder(CodecType::Opus);
        for chunk in continuous.chunks(960) {
            opus_frames.push(enc.encode(chunk));
        }

        let ivr_continuous: Vec<i16> = (0..48000 * 4)
            .map(|i| ((2.0 * std::f32::consts::PI * 300.0 * i as f32 / 48000.0).sin() * 15000.0) as i16)
            .collect();
        let mut ivr_opus_frames = Vec::new();
        let mut ivr_enc = create_encoder(CodecType::Opus);
        for chunk in ivr_continuous.chunks(960) {
            ivr_opus_frames.push(ivr_enc.encode(chunk));
        }

        // Phase 1: IVR greeting (3 seconds = 150 frames)
        for i in 0..150u16 {
            let frame_idx = i as usize;
            let opus_caller = &opus_frames[frame_idx % opus_frames.len()];
            let opus_ivr = &ivr_opus_frames[frame_idx % ivr_opus_frames.len()];

            // Leg A: multi-block RED
            let rlen = opus_caller.len();
            let mut red = vec![0u8; 5 + rlen + opus_caller.len()];
            red[0] = 0x80 | 111;
            red[1] = 0x00;
            red[2] = 0x00;
            red[3] = (rlen & 0xFF) as u8;
            red[4] = 0x6F;
            red[5..5 + rlen].copy_from_slice(opus_caller);
            red[5 + rlen..].copy_from_slice(opus_caller);
            let leg_a = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 960,
                clock_rate: 48000,
                data: bytes::Bytes::from(red),
                sequence_number: Some(i),
                payload_type: Some(63),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            // Leg B: raw Opus
            let leg_b = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 960,
                clock_rate: 48000,
                data: bytes::Bytes::from(opus_ivr.clone()),
                sequence_number: Some(i),
                payload_type: Some(111),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            // Interleave: A, B, A, B...
            task.sender().send(CaptureSample {
                leg: Leg::A, sample: Arc::new(leg_a), timestamp_us: i as u64 * 20000,
            }).await.unwrap();
            task.sender().send(CaptureSample {
                leg: Leg::B, sample: Arc::new(leg_b), timestamp_us: i as u64 * 20000,
            }).await.unwrap();
        }

        // Phase 2: DTMF '1' on Leg A (end-of-event)
        let dtmf_end = vec![1u8, 0x80, 0x00, 0xA0]; // digit 1, end, duration 160
        let dtmf = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 150 * 960,
            clock_rate: 48000,
            data: bytes::Bytes::from(dtmf_end),
            sequence_number: Some(150),
            payload_type: Some(101), // telephone-event PT
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        });
        task.sender().send(CaptureSample {
            leg: Leg::A, sample: Arc::new(dtmf), timestamp_us: 3_000_000,
        }).await.unwrap();

        // Phase 3: Connected phase — both legs active (50 frames = 1s)
        for i in 0..50u16 {
            let frame_idx = 151 + i as usize;
            let opus_a = &opus_frames[frame_idx % opus_frames.len()];
            let opus_b = &ivr_opus_frames[frame_idx % ivr_opus_frames.len()];

            let leg_a = MediaSample::Audio(AudioFrame {
                rtp_timestamp: (151 + i) as u32 * 960,
                clock_rate: 48000,
                data: bytes::Bytes::from(opus_a.clone()),
                sequence_number: Some(151 + i),
                payload_type: Some(111),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            let leg_b = MediaSample::Audio(AudioFrame {
                rtp_timestamp: (151 + i) as u32 * 960,
                clock_rate: 48000,
                data: bytes::Bytes::from(opus_b.clone()),
                sequence_number: Some(151 + i),
                payload_type: Some(111),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            });
            task.sender().send(CaptureSample {
                leg: Leg::A, sample: Arc::new(leg_a), timestamp_us: 3_020_000 + i as u64 * 20000,
            }).await.unwrap();
            task.sender().send(CaptureSample {
                leg: Leg::B, sample: Arc::new(leg_b), timestamp_us: 3_020_000 + i as u64 * 20000,
            }).await.unwrap();
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let (_ch, _sr, left, right) = decode_wav(&path);

        // --- Assertions ---

        // 1. Both legs have audio
        let le = energy(&left);
        let re = energy(&right);
        assert!(le > 5000, "Leg A (caller RED Opus) should have audio, got energy={le}");
        assert!(re > 5000, "Leg B (IVR Opus) should have audio, got energy={re}");

        // 2. Duration is reasonable (~4s = 32000 samples at 8kHz)
        let expected_min = 16000; // 2 seconds minimum
        assert!(left.len() >= expected_min,
            "Leg A too short: {} samples (expected >= {})", left.len(), expected_min);
        assert!(right.len() >= expected_min,
            "Leg B too short: {} samples (expected >= {})", right.len(), expected_min);

        // 3. No periodic silence gaps (check every 100ms window in first 2 seconds)
        let mut consecutive_silence = 0;
        let mut max_silence_run = 0;
        for ms100 in 0..20 { // 20 × 100ms = 2 seconds
            let start_sample = ms100 * 800; // 100ms = 800 samples at 8kHz
            let end_sample = (start_sample + 800).min(left.len());
            if start_sample >= left.len() { break; }
            let chunk = &left[start_sample..end_sample];
            let e = energy(chunk);
            if (e as f64 / chunk.len() as f64) < 20.0 {
                consecutive_silence += 1;
                max_silence_run = max_silence_run.max(consecutive_silence);
            } else {
                consecutive_silence = 0;
            }
        }
        // Allow up to 2 consecutive silent windows (200ms), not more
        assert!(max_silence_run <= 2,
            "Leg A has {max_silence_run} consecutive silence windows (100ms each) — choppy audio!");

        // Same for right channel
        consecutive_silence = 0;
        max_silence_run = 0;
        for ms100 in 0..20 {
            let start_sample = ms100 * 800;
            let end_sample = (start_sample + 800).min(right.len());
            if start_sample >= right.len() { break; }
            let chunk = &right[start_sample..end_sample];
            let e = energy(chunk);
            if (e as f64 / chunk.len() as f64) < 20.0 {
                consecutive_silence += 1;
                max_silence_run = max_silence_run.max(consecutive_silence);
            } else {
                consecutive_silence = 0;
            }
        }
        assert!(max_silence_run <= 2,
            "Leg B has {max_silence_run} consecutive silence windows (100ms each) — choppy IVR!");
    }

    // ── flush fix: stalled leg should not cause silence gaps ────────────

    #[tokio::test]
    async fn stalled_leg_b_no_silence_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stall.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, 0, None);
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);
        let tone = Tone::generate(440.0, 8000, 20);

        // 10x both legs
        for i in 0..10u16 {
            let a = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            let b = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(a), timestamp_us: i as u64 * 20000 }).await.unwrap();
            task.sender().send(CaptureSample { leg: Leg::B, sample: Arc::new(b), timestamp_us: i as u64 * 20000 }).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 5x Leg A ONLY (Leg B stalled)
        for i in 10..15u16 {
            let a = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(a), timestamp_us: i as u64 * 20000 }).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 5x BOTH (Leg B catches up)
        for i in 10..15u16 {
            let a = tone.as_media_sample(CodecType::PCMU, i + 5, (i + 5) as u32 * 160);
            let b = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(a), timestamp_us: i as u64 * 20000 }).await.unwrap();
            task.sender().send(CaptureSample { leg: Leg::B, sample: Arc::new(b), timestamp_us: i as u64 * 20000 }).await.unwrap();
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, _left, right) = decode_wav(&path);
        let chunk = &right[1600..2400.min(right.len())];
        let avg = energy(chunk) as f64 / chunk.len() as f64;
        assert!(avg > 200.0,
            "Leg B silenced at 1600-2400: avg={avg:.1} — flush should wait for stalled leg");
    }

    // ── DTMF fallback: alternate PT by payload shape ──────────────────

    #[tokio::test]
    async fn dtmf_alternate_pt_shape_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf_alt.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::PCMU, 0, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone = Tone::generate(440.0, 8000, 20);
        for i in 0..5u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i, i as u32 * 160);
            task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(s), timestamp_us: i as u64 * 20000 }).await.unwrap();
        }

        let dtmf = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 800, clock_rate: 8000,
            data: bytes::Bytes::from(vec![5u8, 0x80, 0x00, 0xA0]),
            sequence_number: Some(5), payload_type: Some(126),
            marker: false, header_extension: None, source_addr: None, raw_packet: None,
        });
        task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(dtmf), timestamp_us: 100000 }).await.unwrap();

        for i in 0..5u16 {
            let s = tone.as_media_sample(CodecType::PCMU, i + 10, (10 + i) as u32 * 160);
            task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(s), timestamp_us: 120000 + i as u64 * 20000 }).await.unwrap();
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, _right) = decode_wav(&path);
        let e = energy(&left);
        assert!(e > 20000, "DTMF with PT=126 should be recorded via shape detection, got energy={e}");
    }

    // ── dynamic Opus PT ───────────────────────────────────────────────

    #[tokio::test]
    async fn dynamic_opus_pt_96_decoded_via_red() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opus96.wav");
        let arc = setup_recorder(path.to_str().unwrap(), CodecType::Opus, 96, Some(101));
        let task = FileRecorderTask::spawn(Arc::clone(&arc), Duration::from_secs(10), 64);

        let tone = Tone::generate(440.0, 48000, 20);
        let opus_data = tone.encode(CodecType::Opus);

        for i in 0..25u16 {
            // Single-block RED wrapping Opus with PT=96
            let mut red = vec![0x60u8]; // F=0, PT=96
            red.extend_from_slice(&opus_data);
            let sample = MediaSample::Audio(AudioFrame {
                rtp_timestamp: i as u32 * 960, clock_rate: 48000,
                data: bytes::Bytes::from(red),
                sequence_number: Some(i), payload_type: Some(63),
                marker: false, header_extension: None, source_addr: None, raw_packet: None,
            });
            task.sender().send(CaptureSample { leg: Leg::A, sample: Arc::new(sample), timestamp_us: i as u64 * 20000 }).await.unwrap();
        }

        drop(task);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let (_ch, _sr, left, _right) = decode_wav(&path);
        let e = energy(&left);
        assert!(e > 5000, "Opus PT=96 via RED should be decoded, got energy={e}");
    }
}
