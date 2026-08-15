//! Micro-benchmarks for the media audio mixer hot path.
//!
//! Run: `cargo bench -p rustpbx-media --bench mixer`

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rustpbx_media::mixer::AudioMixer;

fn bench_mixer(c: &mut Criterion) {
    let mixer = AudioMixer;
    // 20 ms of 16-bit PCM at 8 kHz.
    let frame: Vec<i16> = (0..160).map(|i| (i as i16 * 7) % 10000).collect();

    let mut g = c.benchmark_group("mix_20ms");
    for participants in [2usize, 4, 8, 16] {
        g.throughput(Throughput::Elements(160));
        g.bench_with_input(
            BenchmarkId::new("n_participants", participants),
            &participants,
            |b, &n| {
                b.iter(|| {
                    let frames = vec![frame.clone(); n];
                    let gains = vec![1.0 / n as f32; n];
                    let out = mixer.mix_frames(black_box(frames), black_box(&gains));
                    black_box(out.len());
                })
            },
        );
    }
    g.finish();
}

criterion_group!(benches, bench_mixer);
criterion_main!(benches);
