use anyhow::Result;
use std::path::Path;
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    media::recorder::{Recorder, RecorderConfig},
    AudioFrame, Samples,
};

#[tokio::test]
async fn test_recorder() -> Result<()> {
    // Setup
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_recording.wav");
    let file_path_clone = file_path.clone(); // Clone for the spawned task
    let cancel_token = CancellationToken::new();
    let config = RecorderConfig { sample_rate: 16000 };

    let recorder = Recorder::new(cancel_token.clone(), config);

    // Create channels for testing
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording in the background
    let recording_handle =
        tokio::spawn(async move { recorder.process_recording(&file_path_clone, rx).await });

    // Create test frames
    let left_channel_id = "left".to_string();
    let right_channel_id = "right".to_string();

    // Generate some sample audio data (sine wave)
    let sample_count = 1600; // 100ms of audio at 16kHz

    for i in 0..5 {
        // Send 5 frames (500ms total)
        // Left channel (lower frequency sine wave)
        let left_samples: Vec<i16> = (0..sample_count)
            .map(|j| {
                let t = (i * sample_count + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as i16
            })
            .collect();

        // Right channel (higher frequency sine wave)
        let right_samples: Vec<i16> = (0..sample_count)
            .map(|j| {
                let t = (i * sample_count + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as i16
            })
            .collect();

        let left_frame = AudioFrame {
            track_id: left_channel_id.clone(),
            samples: Samples::PCM(left_samples),
            timestamp: (i * 100), // Increment timestamp by 100ms
            sample_rate: 16000,
        };

        let right_frame = AudioFrame {
            track_id: right_channel_id.clone(),
            samples: Samples::PCM(right_samples),
            timestamp: (i * 100), // Same timestamp for synchronized channels
            sample_rate: 16000,
        };

        // Send frames
        tx.send(left_frame)?;
        tx.send(right_frame)?;

        // Wait a bit to simulate real-time recording
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Stop recording
    drop(tx);
    recording_handle.await??;

    // Verify the file exists
    assert!(file_path.exists());

    // Verify the file is a valid WAV file with expected content
    verify_wav_file(&file_path)?;

    Ok(())
}

fn verify_wav_file(path: &Path) -> Result<()> {
    // Open the WAV file
    let reader = hound::WavReader::open(path)?;
    let spec = reader.spec();

    // Verify format
    assert_eq!(spec.channels, 2); // Stereo
    assert_eq!(spec.sample_rate, 16000);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);

    // Verify the file has some samples
    let samples_count = reader.len();
    assert!(samples_count > 0, "WAV file has no samples");

    Ok(())
}
