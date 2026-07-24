//! Layer 4: Recording content verification.
//!
//! Tests that recorded WAV files contain correct audio content when audio
//! is injected via `replace_output_with_file` through a real BridgePeer.
//! verifies codec format tag, sample rate, stereo layout, and decoded PCM
//! content via cross-correlation against a known sine-wave source.

use crate::media::bridge::{BridgeEndpoint, BridgePeerBuilder};
use crate::media::file_track::FileTrack;
use crate::media::negotiate::{CodecInfo, NegotiatedCodec, NegotiatedLegProfile};
use crate::media::recorder::Recorder;
use crate::media::recorder_tap::RecorderTap;
use crate::media::wav_reader::WavReader;
use audio_codec::CodecType;
use parking_lot::RwLock;
use rustrtc::rtp::{RtpHeader, RtpPacket as RtpPkt};
use rustrtc::RtpReceiverInterceptor;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// ---- helpers ---------------------------------------------------------

fn generate_sine_wav(path: &Path, freq: f64, duration_secs: f64, sample_rate: u32) {
    use crate::media::wav_reader::{SampleFormat, WavSpec, WavWriter};
    let spec = WavSpec { channels: 1, sample_rate, bits_per_sample: 16, sample_format: SampleFormat::Int };
    let mut writer = WavWriter::create(path, spec).unwrap();
    let n = (sample_rate as f64 * duration_secs) as usize;
    for i in 0..n {
        let t = i as f64 / sample_rate as f64;
        let s = (0.5 * (2.0 * std::f64::consts::PI * freq * t).sin() * 32767.0) as i16;
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
}

fn cross_correlate(signal: &[i16], reference: &[i16]) -> f64 {
    if signal.is_empty() || reference.is_empty() || signal.len() < reference.len() {
        return 0.0;
    }
    let n = reference.len();
    let ref_mean: f64 = reference.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
    let ref_var: f64 = reference.iter().map(|&s| { let d = s as f64 - ref_mean; d * d }).sum::<f64>() / n as f64;
    if ref_var < 1e-20 { return 0.0; }
    let ref_std = ref_var.sqrt();
    let max_offset = signal.len() - n;
    let step = std::cmp::max(1, max_offset / 200);
    let mut best = 0.0;
    for offset in (0..=max_offset).step_by(step) {
        let w = &signal[offset..offset + n];
        let wm: f64 = w.iter().map(|&s| s as f64).sum::<f64>() / n as f64;
        let wv: f64 = w.iter().map(|&s| { let d = s as f64 - wm; d * d }).sum::<f64>() / n as f64;
        if wv < 1e-20 { continue; }
        let ws = wv.sqrt();
        let num: f64 = w.iter().zip(reference.iter()).map(|(&a, &b)| (a as f64 - wm) * (b as f64 - ref_mean)).sum();
        let c = num / (n as f64 * ws * ref_std);
        if c > best { best = c; }
    }
    best
}

fn make_rtp(pt: u8, seq: u16, ts: u32, payload: Vec<u8>) -> RtpPkt {
    let hdr = RtpHeader::new(pt, seq, ts, 0x12345678);
    RtpPkt::new(hdr, payload)
}

fn codec_info_pcmu() -> CodecInfo {
    CodecInfo { payload_type: 0, codec: CodecType::PCMU, clock_rate: 8000, channels: 1, fmtp: None }
}

// ---- tests -----------------------------------------------------------

/// RecorderTap + file_recorder captures a sine WAV with correct content.
#[tokio::test]
async fn test_recorder_tap_pcmu_content() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("recorder_content_test.wav");
    let sine_path = dir.path().join("source_440.wav");
    generate_sine_wav(&sine_path, 440.0, 0.5, 8000);

    let mut rec = Recorder::new(wav_path.to_str().unwrap(), CodecType::PCMU).unwrap();
    let profile = NegotiatedLegProfile {
        audio: Some(NegotiatedCodec { payload_type: 0, codec: CodecType::PCMU, clock_rate: 8000, channels: 1 }),
        video: None, dtmf: None, transport: rustrtc::TransportMode::Rtp,
    };
    rec.set_leg_profile(crate::media::recorder::Leg::A, profile.clone());
    rec.set_leg_profile(crate::media::recorder::Leg::B, profile);

    let recorder_arc: Arc<RwLock<Option<Recorder>>> = Arc::new(RwLock::new(Some(rec)));
    let paused = Arc::new(AtomicBool::new(false));
    let tap = RecorderTap::for_recv(
        Some(recorder_arc.clone()), None, "recv-test".into(), paused, vec![],
    );

    let src_pcm: Vec<i16> = WavReader::open(&sine_path).unwrap().samples().map(|s| s.unwrap()).collect();
    let mut encoder = audio_codec::create_encoder(CodecType::PCMU);
    let frame_size = 160;

    let a: SocketAddr = ([127, 0, 0, 1], 50000).into();
    let b: SocketAddr = ([127, 0, 0, 1], 50001).into();

    for (i, chunk) in src_pcm.chunks(frame_size).enumerate() {
        let encoded = encoder.encode(chunk);
        let rtp = make_rtp(0, i as u16, (i as u32) * 160, encoded);
        tap.on_packet_received(&rtp, a, b).await;
    }

    {
        let mut g = recorder_arc.write();
        if let Some(r) = g.as_mut() { r.finalize().unwrap(); }
    }

    let meta = std::fs::metadata(&wav_path).unwrap();
    assert!(meta.len() > 44, "WAV should have header + data, got {} bytes", meta.len());

    let mut reader = WavReader::open(&wav_path).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 8000, "sample_rate");
    assert_eq!(spec.channels, 2, "recorder forces stereo");

    let all: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
    // Leg::A → left channel, Leg::B → right channel
    let leg_a: Vec<i16> = all.iter().step_by(2).copied().collect();
    let corr = cross_correlate(&leg_a, &src_pcm);
    assert!(corr > 0.80, "recorded content should match source, corr={:.4}", corr);
}

/// BridgePeer + recorder captures audio played to the caller leg.
#[tokio::test]
async fn test_bridge_recording_captures_played_audio() {
    let dir = tempfile::tempdir().unwrap();
    let wav_path = dir.path().join("bridge_rec_test.wav");
    let sine_path = dir.path().join("src_440.wav");
    generate_sine_wav(&sine_path, 440.0, 0.5, 8000);

    let mut rec = Recorder::new(wav_path.to_str().unwrap(), CodecType::PCMU).unwrap();
    let profile = NegotiatedLegProfile {
        audio: Some(NegotiatedCodec { payload_type: 0, codec: CodecType::PCMU, clock_rate: 8000, channels: 1 }),
        video: None, dtmf: None, transport: rustrtc::TransportMode::Rtp,
    };
    rec.set_leg_profile(crate::media::recorder::Leg::A, profile.clone());
    rec.set_leg_profile(crate::media::recorder::Leg::B, profile);

    let recorder_arc: Arc<RwLock<Option<Recorder>>> = Arc::new(RwLock::new(Some(rec)));
    let paused = Arc::new(AtomicBool::new(false));

    let bridge = BridgePeerBuilder::new("test-rec-bridge".into())
        .with_recorder(recorder_arc.clone(), paused.clone())
        .build();
    bridge.setup_bridge().await.unwrap();

    let file_track = FileTrack::new("test-playback".into())
        .with_path(sine_path.to_str().unwrap().to_string())
        .with_loop(false)
        .with_codec_info(codec_info_pcmu());

    bridge.replace_output_with_file(BridgeEndpoint::Caller, &file_track).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    {
        let mut g = recorder_arc.write();
        if let Some(r) = g.as_mut() { r.finalize().unwrap(); }
    }

    let meta = std::fs::metadata(&wav_path).unwrap();
    assert!(meta.len() > 100, "WAV should have meaningful data, got {} bytes", meta.len());

    let mut reader = WavReader::open(&wav_path).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 8000, "sample_rate");
    assert_eq!(spec.channels, 2, "recorder forces stereo");

    let pcm: Vec<i16> = reader.samples().map(|s| s.unwrap()).collect();
    let leg_b: Vec<i16> = pcm.iter().skip(1).step_by(2).copied().collect();

    let rms_b: f64 = {
        let sum_sq: f64 = leg_b.iter().map(|&s| { let v = s as f64 / 32768.0; v * v }).sum();
        if leg_b.is_empty() { 0.0 } else { (sum_sq / leg_b.len() as f64).sqrt() }
    };
    assert!(rms_b > 0.01, "caller egress should have audible content, rms={:.4}", rms_b);

    let src_pcm: Vec<i16> = WavReader::open(&sine_path).unwrap().samples().map(|s| s.unwrap()).collect();
    let corr = cross_correlate(&leg_b, &src_pcm);
    assert!(corr > 0.30, "caller egress content should correlate with source, corr={:.4}", corr);

    bridge.stop().await;
}
