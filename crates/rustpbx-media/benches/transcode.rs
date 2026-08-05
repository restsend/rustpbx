//! Micro-benchmarks for the media transcoder hot path (incl. Opus↔PCMU).
//!
//! Run: `cargo bench -p rustpbx-media --bench transcode`

use audio_codec::{CodecType, create_encoder};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rustpbx_media::transcoder::Transcoder;
use rustrtc::media::AudioFrame;

fn make_frame(data: Vec<u8>, pt: u8, clock: u32, ts: u32) -> AudioFrame {
    AudioFrame {
        rtp_timestamp: ts,
        clock_rate: clock,
        data: Bytes::from(data),
        sequence_number: Some(1),
        payload_type: Some(pt),
        marker: false,
        header_extension: None,
        raw_packet: None,
        source_addr: None,
    }
}

fn codec_pt_clock(codec: CodecType) -> (u8, u32) {
    match codec {
        CodecType::PCMU => (0, 8000),
        CodecType::PCMA => (8, 8000),
        CodecType::G722 => (9, 8000),
        CodecType::G729 => (18, 8000),
        CodecType::Opus => (111, 48000),
        _ => (0, 8000),
    }
}

/// Encode a non-flat 20 ms PCM buffer with `codec` so the transcode input is valid.
fn encoded_frame_for(codec: CodecType) -> AudioFrame {
    let rate = codec.samplerate();
    let n = rate / 50; // 20 ms
    let pcm: Vec<i16> = (0..n).map(|i| (((i as i32 * 7) % 10000) - 5000) as i16).collect();
    let mut enc = create_encoder(codec);
    let data = enc.encode(&pcm);
    let (pt, clock) = codec_pt_clock(codec);
    make_frame(data, pt, clock, 0)
}

fn bench_transcode(c: &mut Criterion) {
    let mut g = c.benchmark_group("transcode_20ms");
    for (name, source, target) in [
        ("pcmu_to_pcma", CodecType::PCMU, CodecType::PCMA),
        ("pcmu_to_g722", CodecType::PCMU, CodecType::G722),
        ("pcmu_to_g729", CodecType::PCMU, CodecType::G729),
        ("pcmu_to_opus", CodecType::PCMU, CodecType::Opus),
        ("opus_to_pcmu", CodecType::Opus, CodecType::PCMU),
        ("opus_to_pcma", CodecType::Opus, CodecType::PCMA),
        ("g729_to_pcmu", CodecType::G729, CodecType::PCMU),
    ] {
        let frame = encoded_frame_for(source);
        let (pt, _clock) = codec_pt_clock(target);
        g.throughput(Throughput::Bytes(frame.data.len() as u64));
        g.bench_with_input(BenchmarkId::new(name, frame.data.len()), &frame, |b, f| {
            b.iter(|| {
                let mut t = Transcoder::new(source, target, pt);
                let out = t.transcode(black_box(f));
                black_box(out.data.len());
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench_transcode);
criterion_main!(benches);
