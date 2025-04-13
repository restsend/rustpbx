use crate::media::codecs::samples_to_bytes;
use crate::{
    media::{denoiser::NoiseReducer, processor::Processor},
    AudioFrame, Samples,
};
use std::{fs::File, io::Write};

#[test]
fn test_basic_processing() {
    let reducer = NoiseReducer::new(16000).expect("Failed to create reducer");
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/noise_gating_zh_16k.wav").unwrap();
    let mut out_file = File::create("fixtures/noise_gating_zh_16k_denoised.pcm.decoded").unwrap();
    for chunk in all_samples.chunks(320) {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: chunk.to_vec(),
            },
            sample_rate,
            track_id: "test".to_string(),
            timestamp: 0,
        };
        reducer.process_frame(&mut frame).unwrap();
        let samples = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => panic!("Expected PCM samples"),
        };
        out_file.write_all(&samples_to_bytes(&samples)).unwrap();
    }
    println!("ffplay -f s16le -ar 16000 fixtures/noise_gating_zh_16k_denoised.pcm.decoded");
}
