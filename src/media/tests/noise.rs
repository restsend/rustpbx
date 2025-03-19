use crate::media::{
    noise::NoiseReducer,
    processor::{AudioFrame, Processor},
};

#[test]
fn test_noise_reducer() {
    let mut reducer = NoiseReducer::new();

    // Test with silence (all zeros)
    let silence_frame = AudioFrame {
        track_id: "test".to_string(),
        samples: vec![0.0; 480], // 30ms at 16kHz
        timestamp: 0,
        sample_rate: 16000,
        channels: 1,
    };
    let processed = reducer.process_frame(silence_frame).unwrap();

    // Output should be mostly zeros for silence
    assert!(processed.samples.iter().all(|&x| x.abs() < 0.01));

    // Test with sine wave (clean signal)
    let clean_frame = AudioFrame {
        track_id: "test".to_string(),
        samples: (0..480).map(|i| (i as f32 * 0.1).sin() * 0.5).collect(),
        timestamp: 20,
        sample_rate: 16000,
        channels: 1,
    };
    let processed = reducer.process_frame(clean_frame.clone()).unwrap();

    // Clean signal should be mostly preserved
    for (orig, proc) in clean_frame.samples.iter().zip(processed.samples.iter()) {
        assert!((orig - proc).abs() < 0.1);
    }

    // Test with noisy signal (sine wave + noise)
    let noisy_frame = AudioFrame {
        track_id: "test".to_string(),
        samples: (0..480)
            .map(|i| {
                let signal = (i as f32 * 0.1).sin() * 0.5;
                let noise = rand::random::<f32>() * 0.1;
                signal + noise
            })
            .collect(),
        timestamp: 40,
        sample_rate: 16000,
        channels: 1,
    };
    let processed = reducer.process_frame(noisy_frame.clone()).unwrap();

    // Noise should be reduced
    let original_noise = noisy_frame
        .samples
        .iter()
        .zip(clean_frame.samples.iter())
        .map(|(n, c)| (n - c).abs())
        .sum::<f32>();

    let processed_noise = processed
        .samples
        .iter()
        .zip(clean_frame.samples.iter())
        .map(|(p, c)| (p - c).abs())
        .sum::<f32>();

    assert!(processed_noise < original_noise);
}

#[test]
fn test_noise_reducer_frame_sizes() {
    let mut reducer = NoiseReducer::new();

    // Test different frame sizes
    for size in [160, 320, 480, 960] {
        let frame = AudioFrame {
            track_id: "test".to_string(),
            samples: vec![0.0; size],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        };
        let processed = reducer.process_frame(frame).unwrap();
        assert_eq!(processed.samples.len(), size);
    }
}

#[test]
fn test_noise_reducer_sample_rates() {
    let mut reducer = NoiseReducer::new();

    // Test different sample rates
    for rate in [8000, 16000, 32000, 48000] {
        let frame = AudioFrame {
            track_id: "test".to_string(),
            samples: vec![0.0; 480],
            timestamp: 0,
            sample_rate: rate,
            channels: 1,
        };
        let processed = reducer.process_frame(frame).unwrap();
        assert_eq!(processed.sample_rate, rate);
    }
}

#[test]
fn test_noise_reducer_channels() {
    let mut reducer = NoiseReducer::new();

    // Test mono and stereo
    for channels in [1, 2] {
        let frame = AudioFrame {
            track_id: "test".to_string(),
            samples: vec![0.0; 480 * channels as usize],
            timestamp: 0,
            sample_rate: 16000,
            channels,
        };
        let processed = reducer.process_frame(frame).unwrap();
        assert_eq!(processed.channels, channels);
    }
}
