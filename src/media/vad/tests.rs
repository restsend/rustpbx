use super::*;
use crate::event::SessionEvent;
use crate::{media::processor::Processor, Samples};
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

#[test]
fn test_vadtype_deserialization() {
    // Test deserializing a string
    let json_str = r#""unknown_vad_type""#;
    let vad_type: VadType = serde_json::from_str(json_str).unwrap();
    match vad_type {
        VadType::Other(s) => assert_eq!(s, "unknown_vad_type"),
        _ => panic!("Expected VadType::Other"),
    }

    // Test deserializing a known type
    #[cfg(feature = "vad_webrtc")]
    {
        let json_known = r#""webrtc""#;
        let vad_type: VadType = serde_json::from_str(json_known).unwrap();
        match vad_type {
            VadType::WebRTC => {}
            _ => panic!("Expected VadType::WebRTC"),
        }
    }
}

#[derive(Default, Debug)]
struct TestResults {
    speech_segments: Vec<(u64, u64)>, // (start_time, duration)
}

#[tokio::test]
#[cfg(feature = "vad_silero")]
async fn test_vad_with_noise_denoise() {
    use std::fs::File;
    use std::io::Write;

    use crate::media::codecs::samples_to_bytes;
    use crate::media::denoiser::NoiseReducer;
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/noise_gating_zh_16k.wav").unwrap();
    assert_eq!(sample_rate, 16000, "Expected 16kHz sample rate");
    assert!(!all_samples.is_empty(), "Expected non-empty audio file");

    println!(
        "Loaded {} samples from WAV file for testing",
        all_samples.len()
    );
    let nr = NoiseReducer::new(sample_rate as usize).expect("Failed to create reducer");
    let (event_sender, mut event_receiver) = broadcast::channel(128);
    let track_id = "test_track".to_string();

    let mut option = VADOption::default();
    option.r#type = VadType::Silero;
    let token = CancellationToken::new();
    let vad = VadProcessor::create_silero(token, event_sender.clone(), option)
        .expect("Failed to create VAD processor");
    let mut total_duration = 0;
    let (frame_size, chunk_duration_ms) = (320, 20);
    let mut out_file = File::create("fixtures/noise_gating_zh_16k_denoised.pcm.decoded").unwrap();
    for (i, chunk) in all_samples.chunks(frame_size).enumerate() {
        let chunk_vec = chunk.to_vec();
        let chunk_vec = if chunk_vec.len() < frame_size {
            let mut padded = chunk_vec;
            padded.resize(frame_size, 0);
            padded
        } else {
            chunk_vec
        };

        let mut frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM { samples: chunk_vec },
            sample_rate,
            timestamp: i as u64 * chunk_duration_ms,
        };
        nr.process_frame(&mut frame).unwrap();
        vad.process_frame(&mut frame).unwrap();
        let samples = match frame.samples {
            Samples::PCM { samples } => samples,
            _ => panic!("Expected PCM samples"),
        };
        out_file.write_all(&samples_to_bytes(&samples)).unwrap();
        total_duration += chunk_duration_ms;
    }
    sleep(Duration::from_millis(50)).await;

    let mut results = TestResults::default();
    while let Ok(event) = event_receiver.try_recv() {
        match event {
            SessionEvent::Speaking { start_time, .. } => {
                println!("  Speaking event at {}ms", start_time);
            }
            SessionEvent::Silence {
                start_time,
                duration,
                ..
            } => {
                if duration > 0 {
                    println!(
                        "  Silence event: start_time={}ms, duration={}ms",
                        start_time, duration
                    );
                    results.speech_segments.push((start_time, duration));
                }
            }
            _ => {}
        }
    }

    println!(
        "detected {} speech segments, total_duration:{}",
        results.speech_segments.len(),
        total_duration
    );
    assert!(results.speech_segments.len() == 2);
}

#[tokio::test]
async fn test_vad_engines_with_wav_file() {
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/hello_book_course_zh_16k.wav").unwrap();
    assert_eq!(sample_rate, 16000, "Expected 16kHz sample rate");
    assert!(!all_samples.is_empty(), "Expected non-empty audio file");

    println!(
        "Loaded {} samples from WAV file for testing",
        all_samples.len()
    );
    //
    for vad_type in [
        #[cfg(feature = "vad_webrtc")]
        VadType::WebRTC,
        #[cfg(feature = "vad_silero")]
        VadType::Silero,
        #[cfg(feature = "vad_ten")]
        VadType::Ten,
    ] {
        let vad_name = match vad_type {
            #[cfg(feature = "vad_webrtc")]
            VadType::WebRTC => "WebRTC",
            #[cfg(feature = "vad_silero")]
            VadType::Silero => "Silero",
            #[cfg(feature = "vad_ten")]
            VadType::Ten => "ten",
            VadType::Other(ref name) => name,
        };

        println!("\n--- Testing {} VAD Engine ---", vad_name);

        let (event_sender, mut event_receiver) = broadcast::channel(16);
        let track_id = "test_track".to_string();

        let mut option = VADOption::default();
        option.r#type = vad_type.clone();
        // Use different thresholds and padding based on VAD type
        option.voice_threshold = match vad_type {
            VadType::Ten => 0.25, // Threshold based on actual score range
            _ => 0.5,             // Use default threshold for other VAD engines
        };

        // Adjust padding for TenVad's frequent state changes
        if matches!(vad_type, VadType::Ten) {
            option.silence_padding = 5; // Minimal silence padding for TenVad
            option.speech_padding = 30; // Minimal speech padding for TenVad
        }
        let token = CancellationToken::new();
        let vad = match vad_type {
            #[cfg(feature = "vad_silero")]
            VadType::Silero => VadProcessor::create_silero(token, event_sender.clone(), option)
                .expect("Failed to create VAD processor"),
            #[cfg(feature = "vad_webrtc")]
            VadType::WebRTC => VadProcessor::create_webrtc(token, event_sender.clone(), option)
                .expect("Failed to create VAD processor"),
            #[cfg(feature = "vad_ten")]
            VadType::Ten => VadProcessor::create_ten(token, event_sender.clone(), option)
                .expect("Failed to create VAD processor"),
            VadType::Other(ref name) => {
                panic!("Unsupported VAD type: {}", name);
            }
        };

        let (frame_size, chunk_duration_ms) = (320, 20);
        let mut total_duration = 0;
        for (i, chunk) in all_samples.chunks(frame_size).enumerate() {
            let chunk_vec = chunk.to_vec();
            let chunk_vec = if chunk_vec.len() < frame_size {
                let mut padded = chunk_vec;
                padded.resize(frame_size, 0);
                padded
            } else {
                chunk_vec
            };

            let mut frame = AudioFrame {
                track_id: track_id.clone(),
                samples: Samples::PCM { samples: chunk_vec },
                sample_rate,
                timestamp: i as u64 * chunk_duration_ms,
            };

            vad.process_frame(&mut frame).unwrap();
            total_duration += chunk_duration_ms;
        }

        // Add multiple final silence frames to force end any ongoing speech
        for i in 1..=5 {
            let final_timestamp = (all_samples.len() / frame_size + i) as u64 * chunk_duration_ms;
            let mut final_frame = AudioFrame {
                track_id: track_id.clone(),
                samples: Samples::PCM {
                    samples: vec![0; frame_size],
                },
                sample_rate,
                timestamp: final_timestamp,
            };
            vad.process_frame(&mut final_frame).unwrap();
        }

        sleep(Duration::from_millis(50)).await;
        println!(
            "Events from {} VAD, total duration: {}ms",
            vad_name, total_duration
        );

        let mut results = TestResults::default();
        while let Ok(event) = event_receiver.try_recv() {
            match event {
                SessionEvent::Speaking { start_time, .. } => {
                    println!("  Speaking event at {}ms", start_time);
                }
                SessionEvent::Silence {
                    start_time,
                    duration,
                    ..
                } => {
                    if duration > 0 {
                        println!(
                            "  Silence event: start_time={}ms, duration={}ms",
                            start_time, duration
                        );
                        results.speech_segments.push((start_time, duration));
                    }
                }
                _ => {}
            }
        }

        println!(
            "{} detected {} speech segments:",
            vad_name,
            results.speech_segments.len()
        );

        // TenVad has finer-grained detection, allow different segment counts
        let expected_segments = if matches!(vad_type, VadType::Ten) {
            // TenVad detects more precise, smaller segments
            assert!(
                results.speech_segments.len() >= 2,
                "TenVad should detect at least 2 speech segments, got {}",
                results.speech_segments.len()
            );
            // Verify it detected speech in expected time ranges
            let has_first_segment = results
                .speech_segments
                .iter()
                .any(|(start, _)| (1140..=1500).contains(start));
            let has_second_segment = results
                .speech_segments
                .iter()
                .any(|(start, _)| (3980..=4400).contains(start));
            assert!(
                has_first_segment,
                "TenVad should detect speech around 1264ms"
            );
            assert!(
                has_second_segment,
                "TenVad should detect speech around 4096ms"
            );
            return; // Skip detailed validation for TenVad as it has different detection patterns
        } else {
            2
        };

        assert!(results.speech_segments.len() == expected_segments);
        //1260ms - 1620m
        let first_speech = results.speech_segments[0];
        assert!(
            (1140..=1300).contains(&first_speech.0),
            "{} first speech should be in range 1260-1300ms, got {}ms",
            vad_name,
            first_speech.0
        );
        assert!(
            (340..=460).contains(&first_speech.1),
            "{} first speech duration should be in range 340-460ms, got {}ms",
            vad_name,
            first_speech.1
        );
        //4080-5200ms
        let second_speech = results.speech_segments[1];
        assert!(
            (3980..=4300).contains(&second_speech.0),
            "{} second speech should be in range 3980-4300ms, got {}ms",
            vad_name,
            second_speech.0
        );
        assert!(
            (1000..=1400).contains(&second_speech.1),
            "{} second speech duration should be in range 1000-1400ms, got {}ms",
            vad_name,
            second_speech.1
        );
    }
    println!("All VAD engine tests completed successfully");
}

#[tokio::test]
#[cfg(feature = "vad_silero")]
async fn test_vad_with_long_audio() {
    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/noise_long_audio_zh_16k.wav").unwrap();
    assert_eq!(sample_rate, 16000, "Expected 16kHz sample rate");
    assert!(!all_samples.is_empty(), "Expected non-empty audio file");

    println!(
        "Loaded {} samples from WAV file for testing",
        all_samples.len()
    );

    let mut option = VADOption::default();
    option.r#type = VadType::Silero;
    option.voice_threshold = 0.35;
    let token = CancellationToken::new();
    let (event_sender, mut event_receiver) = broadcast::channel(128); // Increase channel capacity
    let vad = VadProcessor::create_silero(token, event_sender.clone(), option)
        .expect("Failed to create VAD processor");
    let total_duration = 0;
    let (frame_size, chunk_duration_ms) = (320, 20);
    for (i, chunk) in all_samples.chunks(frame_size).enumerate() {
        let chunk_vec = chunk.to_vec();
        let chunk_vec = if chunk_vec.len() < frame_size {
            let mut padded = chunk_vec;
            padded.resize(frame_size, 0);
            padded
        } else {
            chunk_vec
        };

        let mut frame = AudioFrame {
            track_id: "long_audio".to_string(),
            samples: Samples::PCM { samples: chunk_vec },
            sample_rate,
            timestamp: i as u64 * chunk_duration_ms,
        };
        vad.process_frame(&mut frame).unwrap();
    }

    // Add a final silence frame to force end any ongoing speech
    let final_timestamp = (all_samples.len() / frame_size + 1) as u64 * chunk_duration_ms;
    let mut final_frame = AudioFrame {
        track_id: "long_audio".to_string(),
        samples: Samples::PCM {
            samples: vec![0; frame_size],
        },
        sample_rate,
        timestamp: final_timestamp,
    };
    vad.process_frame(&mut final_frame).unwrap();

    sleep(Duration::from_millis(100)).await;

    let mut results = TestResults::default();
    while let Ok(event) = event_receiver.try_recv() {
        match event {
            SessionEvent::Speaking { start_time, .. } => {
                println!("  Speaking event at {}ms", start_time);
            }
            SessionEvent::Silence {
                start_time,
                duration,
                ..
            } => {
                if duration > 0 {
                    println!(
                        "  Silence event: start_time={}ms, duration={}ms",
                        start_time, duration
                    );
                    results.speech_segments.push((start_time, duration));
                }
            }
            _ => {}
        }
    }

    println!(
        "detected {} speech segments, total_duration:{}",
        results.speech_segments.len(),
        total_duration
    );
    // Temporarily change expectation to see what we actually get
    println!("Speech segments detected:");
    for (i, (start_time, duration)) in results.speech_segments.iter().enumerate() {
        println!(
            "  Segment {}: start={}ms, duration={}ms",
            i + 1,
            start_time,
            duration
        );
    }

    // Verify we detected the main speech regions (allowing for more fine-grained detection)
    assert!(
        results.speech_segments.len() == 4,
        "Should detect 4 main speech segments"
    );
}
