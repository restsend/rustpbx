use crate::{
    media::{denoiser::NoiseReducer, processor::Processor},
    AudioFrame, Samples,
};

#[test]
fn test_noise_reducer_frame_sizes() {
    let reducer = NoiseReducer::new();

    // Test different frame sizes
    for size in [160, 320, 480, 960] {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: vec![0; size],
            },
            ..Default::default()
        };
        reducer.process_frame(&mut frame).unwrap();
        assert!(matches!(
            frame.samples,
            Samples::PCM { samples } if samples.len() == size
        ));
    }
}

#[test]
fn test_noise_reducer_sample_rates() {
    let reducer = NoiseReducer::new();

    // Test different sample rates
    for rate in [8000, 16000, 32000, 48000] {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: vec![0; 480],
            },
            sample_rate: rate,
            ..Default::default()
        };
        reducer.process_frame(&mut frame).unwrap();
        assert_eq!(frame.sample_rate, rate);
    }
}
