use crate::{
    media::recorder::{Recorder, RecorderOption},
    AudioFrame, PcmBuf, Sample, Samples,
};
use anyhow::Result;
use std::{path::Path, sync::Arc};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_recorder() -> Result<()> {
    // Setup
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_recording.wav");
    let file_path_clone = file_path.clone(); // Clone for the spawned task
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(cancel_token.clone(), config));

    // Create channels for testing
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording in the background
    let recorder_clone = recorder.clone();
    let recorder_hanadle = tokio::spawn(async move {
        let r = recorder_clone.process_recording(&file_path_clone, rx).await;
        println!("recorder: {:?}", r);
    });

    // Create test frames
    let left_channel_id = "left".to_string();
    let right_channel_id = "right".to_string();

    // Generate some sample audio data (sine wave)
    let sample_count = 1600; // 100ms of audio at 16kHz

    for i in 0..5 {
        // Send 5 frames (500ms total)
        // Left channel (lower frequency sine wave)
        let left_samples: PcmBuf = (0..sample_count)
            .map(|j| {
                let t = (i * sample_count + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        // Right channel (higher frequency sine wave)
        let right_samples: PcmBuf = (0..sample_count)
            .map(|j| {
                let t = (i * sample_count + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        let left_frame = AudioFrame {
            track_id: left_channel_id.clone(),
            samples: Samples::PCM {
                samples: left_samples,
            },
            timestamp: (i * 100), // Increment timestamp by 100ms
            sample_rate: 16000,
        };

        let right_frame = AudioFrame {
            track_id: right_channel_id.clone(),
            samples: Samples::PCM {
                samples: right_samples,
            },
            timestamp: (i * 100), // Same timestamp for synchronized channels
            sample_rate: 16000,
        };

        // Send frames
        tx.send(left_frame)?;
        tx.send(right_frame)?;

        // Wait a bit to simulate real-time recording
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    recorder.stop_recording()?;
    recorder_hanadle.await?;
    // Verify the file exists
    assert!(file_path.exists());
    println!("file_path: {:?}", file_path.to_str());
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

#[tokio::test]
async fn test_recorder_intermittent_data() -> Result<()> {
    // Test for intermittent data handling and clipping detection
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_intermittent.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(cancel_token.clone(), config));
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording
    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    let track_id = "test_track".to_string();

    // Test 1: Send intermittent small frames (less than chunk_size)
    for i in 0..10 {
        let small_samples: PcmBuf = (0..50) // Small frame, much less than chunk_size (320)
            .map(|j| {
                let t = (i * 50 + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 16384.0) as Sample
            })
            .collect();

        let frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM {
                samples: small_samples,
            },
            timestamp: (i * 50) as u64,
            sample_rate: 16000,
        };

        tx.send(frame)?;

        // Simulate intermittent arrival with gaps
        if i % 3 == 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }

    // Test 2: Send frames with clipping values
    let clipped_samples: PcmBuf = vec![32767, -32768, 32767, -32768, 0, 0, 0, 0]; // Clipped values
    let clipped_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: clipped_samples,
        },
        timestamp: 1000,
        sample_rate: 16000,
    };
    tx.send(clipped_frame)?;

    // Test 3: Send frames with constant values (freeze detection)
    let constant_samples: PcmBuf = vec![1000; 20]; // 20 identical values
    let constant_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: constant_samples,
        },
        timestamp: 1100,
        sample_rate: 16000,
    };
    tx.send(constant_frame)?;

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Stop recording
    recorder.stop_recording()?;
    recorder_handle.await??;

    // Verify file exists and is valid
    assert!(file_path.exists());
    verify_wav_file(&file_path)?;

    println!("Intermittent data test completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_constant_value_detection() -> Result<()> {
    // Test the improved constant value detection logic
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_constant_detection.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default();

    let recorder = Arc::new(Recorder::new(cancel_token.clone(), config));
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording
    let recorder_clone = recorder.clone();
    let recorder_handle =
        tokio::spawn(async move { recorder_clone.process_recording(&file_path_clone, rx).await });

    let track_id = "test_track".to_string();

    // Test 1: Short silence (should NOT trigger warning)
    let short_silence: PcmBuf = vec![0; 15]; // Less than 20 samples
    let frame1 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: short_silence,
        },
        timestamp: 0,
        sample_rate: 16000,
    };
    tx.send(frame1)?;

    // Test 2: Medium silence (should NOT trigger warning as it's normal)
    let medium_silence: PcmBuf = vec![0; 40]; // Less than 50 samples
    let frame2 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: medium_silence,
        },
        timestamp: 100,
        sample_rate: 16000,
    };
    tx.send(frame2)?;

    // Test 3: Large silence buffer (should trigger warning)
    let large_silence: PcmBuf = vec![0; 100]; // More than 50 samples, all zeros
    let frame3 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: large_silence,
        },
        timestamp: 200,
        sample_rate: 16000,
    };
    tx.send(frame3)?;

    // Test 4: Non-zero constant values (should trigger warning)
    let constant_non_zero: PcmBuf = vec![1000; 50]; // 50 identical non-zero values
    let frame4 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: constant_non_zero,
        },
        timestamp: 300,
        sample_rate: 16000,
    };
    tx.send(frame4)?;

    // Test 5: Normal audio (should NOT trigger warning)
    let normal_audio: PcmBuf = (0..50)
        .map(|i| ((i as f32 * 0.1).sin() * 1000.0) as Sample)
        .collect();
    let frame5 = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM {
            samples: normal_audio,
        },
        timestamp: 400,
        sample_rate: 16000,
    };
    tx.send(frame5)?;

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Stop recording
    recorder.stop_recording()?;
    recorder_handle.await??;

    // Verify file exists and is valid
    assert!(file_path.exists());
    verify_wav_file(&file_path)?;

    println!("Constant value detection test completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_recorder_200ms_timing() -> Result<()> {
    // Test with the new 200ms timing and improved buffer handling
    let temp_dir = tempdir()?;
    let file_path = temp_dir.path().join("test_200ms_recording.wav");
    let file_path_clone = file_path.clone();
    let cancel_token = CancellationToken::new();
    let config = RecorderOption::default(); // Uses 200ms ptime now

    let recorder = Arc::new(Recorder::new(cancel_token.clone(), config));
    let (tx, rx) = mpsc::unbounded_channel();

    // Start recording
    let recorder_clone = recorder.clone();
    let recorder_handle = tokio::spawn(async move {
        let r = recorder_clone.process_recording(&file_path_clone, rx).await;
        println!("recorder result: {:?}", r);
    });

    let track_id_1 = "track_1".to_string();
    let track_id_2 = "track_2".to_string();

    // Send smaller, irregular frames to test the improved pop mechanism
    for i in 0..10 {
        // Varying frame sizes to test the new buffer handling
        let frame_size = 100 + (i * 50); // Variable frame sizes: 100, 150, 200, ..., 550

        let samples_1: PcmBuf = (0..frame_size)
            .map(|j| {
                let t = (i * frame_size + j) as f32 / 16000.0;
                ((t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as Sample
            })
            .collect();

        let samples_2: PcmBuf = (0..frame_size)
            .map(|j| {
                let t = (i * frame_size + j) as f32 / 16000.0;
                ((t * 880.0 * 2.0 * std::f32::consts::PI).sin() * 8000.0) as Sample
            })
            .collect();

        let frame_1 = AudioFrame {
            track_id: track_id_1.clone(),
            samples: Samples::PCM { samples: samples_1 },
            timestamp: (i * 200) as u64, // 200ms intervals
            sample_rate: 16000,
        };

        let frame_2 = AudioFrame {
            track_id: track_id_2.clone(),
            samples: Samples::PCM { samples: samples_2 },
            timestamp: (i * 200) as u64,
            sample_rate: 16000,
        };

        tx.send(frame_1)?;
        // Send second frame with slight delay to test buffer handling
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        tx.send(frame_2)?;

        // Wait for the 200ms interval
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    }

    // Let it process for a bit longer
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Stop recording - this should trigger the buffer flush
    recorder.stop_recording()?;
    recorder_handle.await?;

    // Verify the file exists and is valid
    assert!(file_path.exists());
    verify_wav_file(&file_path)?;

    println!("200ms timing test completed successfully");
    Ok(())
}
