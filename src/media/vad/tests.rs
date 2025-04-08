use super::*;
use crate::event::SessionEvent;
use crate::{media::processor::Processor, Samples};
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_vad_engines_with_wav_file() {
    #[derive(Default, Debug)]
    struct TestResults {
        speech_segments: Vec<(u64, u64)>, // (start_time, duration)
    }

    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/hello_book_course_zh_16k.wav").unwrap();
    assert_eq!(sample_rate, 16000, "Expected 16kHz sample rate");
    assert!(!all_samples.is_empty(), "Expected non-empty audio file");

    println!(
        "Loaded {} samples from WAV file for testing",
        all_samples.len()
    );

    for vad_type in [VadType::WebRTC, VadType::Silero] {
        let vad_name = match vad_type {
            VadType::WebRTC => "WebRTC",
            VadType::Silero => "Silero",
        };

        println!("\n--- Testing {} VAD Engine ---", vad_name);

        let (event_sender, mut event_receiver) = broadcast::channel(16);
        let track_id = "test_track".to_string();

        let config = VADConfig::default();
        let vad = VadProcessor::new(vad_type, event_sender.clone(), config)
            .expect("Failed to create VAD processor");

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
        assert!(results.speech_segments.len() == 2);
        //1260ms - 1620m
        let first_speech = results.speech_segments[0];
        assert!(
            (1140..=1260).contains(&first_speech.0),
            "{} first speech should be in range 1260-1300ms, got {}ms",
            vad_name,
            first_speech.0
        );
        assert!(
            (380..=500).contains(&first_speech.1),
            "{} first speech duration should be in range 380-500ms, got {}ms",
            vad_name,
            first_speech.1
        );
        //4080-5200ms
    }
    println!("All VAD engine tests completed successfully");
}
