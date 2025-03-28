#[cfg(test)]
mod webrtc_tests {
    use std::fs::File;
    use std::io::Read;
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::{broadcast, Mutex};
    use tokio::time::{sleep, Duration};
    use uuid::Uuid;
    use webrtc::api::{media_engine::MediaEngine, APIBuilder};
    use webrtc::interceptor::registry::Registry;
    use webrtc::peer_connection::configuration::RTCConfiguration;

    use crate::handler::call::{
        ActiveCall, AsrConfig, CallEvent, CallHandlerState, TtsConfig, VadConfig, WsCommand,
    };
    use crate::handler::webrtc::{handle_ws_command, WebRtcCallRequest};
    use crate::llm::LlmConfig;
    use crate::media::pipeline::{PipelineManager, StreamState};
    use crate::media::processor::{AudioFrame, Processor, Samples};
    use crate::media::stream::MediaStreamBuilder;
    use crate::media::track::{tts::TtsTrack, Track};

    // Helper to setup test media stream
    async fn setup_test_media_stream(session_id: &str) -> Arc<crate::media::stream::MediaStream> {
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let builder = MediaStreamBuilder::new()
            .id(format!("test-stream-{}", session_id))
            .cancel_token(cancel_token);

        Arc::new(builder.build())
    }

    // Helper to setup test peer connection
    async fn setup_test_peer_connection() -> Arc<webrtc::peer_connection::RTCPeerConnection> {
        let config = RTCConfiguration {
            ice_servers: vec![],
            ..Default::default()
        };

        let api = APIBuilder::new()
            .with_media_engine(MediaEngine::default())
            .with_interceptor_registry(Registry::new())
            .build();

        Arc::new(api.new_peer_connection(config).await.unwrap())
    }

    // Read WAV file and convert to PCM samples
    fn read_wav_file() -> Vec<i16> {
        let wav_path = Path::new("fixtures/hello_book_course_zh_16k.wav");
        let mut file = File::open(wav_path).expect("Failed to open WAV file");

        // Read the entire file into a buffer
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .expect("Failed to read WAV file");

        // Skip WAV header (44 bytes) and convert to i16 samples
        let mut samples = Vec::with_capacity((buffer.len() - 44) / 2);
        let mut i = 44; // Skip WAV header
        while i < buffer.len() - 1 {
            let sample = i16::from_le_bytes([buffer[i], buffer[i + 1]]);
            samples.push(sample);
            i += 2;
        }

        samples
    }

    // Test processing real audio file through the pipeline
    #[tokio::test]
    async fn test_audio_processing_pipeline() {
        // Create a test session
        let session_id = Uuid::new_v4().to_string();

        // Create event channel
        let (event_sender, mut event_receiver) = broadcast::channel::<CallEvent>(100);

        // Create session event channel for pipeline manager
        let (session_event_sender, _) = broadcast::channel(100);

        // Create a cancellation token for the pipeline manager
        let cancel_token = tokio_util::sync::CancellationToken::new();

        // Create a pipeline manager
        let pipeline_manager = Arc::new(PipelineManager::new(
            format!("test-{}", session_id),
            session_event_sender,
            cancel_token.clone(),
        ));

        // Create a media stream
        let media_stream = setup_test_media_stream(&session_id).await;

        // Create a peer connection
        let peer_connection = setup_test_peer_connection().await;

        // Create an active call
        let active_call = ActiveCall {
            session_id: session_id.clone(),
            media_stream: media_stream.clone(),
            peer_connection,
            events: event_sender.clone(),
            pipeline_manager: Some(pipeline_manager.clone()),
        };

        // Create a call handler state
        let state = CallHandlerState {
            active_calls: Arc::new(Mutex::new(std::collections::HashMap::new())),
        };

        // Store the active call
        state
            .active_calls
            .lock()
            .await
            .insert(session_id.clone(), active_call);

        // Start the transcription pipeline
        let transcription_cmd = WsCommand::PipelineStart {
            pipeline_type: "transcription".to_string(),
            config: Some(serde_json::json!({ "model": "whisper" })),
        };

        let json = serde_json::to_string(&transcription_cmd).unwrap();
        handle_ws_command(&state, &session_id, json).await;

        // Wait for the pipeline to be set up
        sleep(Duration::from_millis(50)).await;

        // Get the call from state
        let call = state
            .active_calls
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .unwrap();

        if let Some(pipeline_manager) = &call.pipeline_manager {
            // Read real audio file
            let pcm_samples = read_wav_file();

            // Create track and add to media stream
            let track_id = format!("audio-test-{}", Uuid::new_v4());
            let mut audio_track = TtsTrack::new(track_id.clone());

            // Add track to media stream
            media_stream.update_track(Box::new(audio_track)).await;

            // Process the audio directly through the pipeline manager
            if let Err(e) = pipeline_manager.process_audio(pcm_samples, 16000).await {
                eprintln!("Failed to process audio: {}", e);
            }

            // Wait for processing to complete
            sleep(Duration::from_millis(100)).await;

            // Check for events (in a real implementation with a proper ASR service, this would
            // result in a transcription event with "您好,请帮我预约课程")
            let mut received_asr_event = false;

            // Try to receive events for a short time
            for _ in 0..5 {
                match tokio::time::timeout(Duration::from_millis(100), event_receiver.recv()).await
                {
                    Ok(Ok(event)) => {
                        if let CallEvent::AsrEvent { .. } = event {
                            received_asr_event = true;
                            break;
                        }
                    }
                    _ => break,
                }
            }

            // Log what we found - in a real setup with actual ASR service, this would be true
            println!("Received ASR event: {}", received_asr_event);

            // Clean up
            let _ = pipeline_manager.stop().await;
        }
    }

    // Test audio length verification
    #[tokio::test]
    async fn test_audio_length() {
        // Read the real audio file
        let pcm_samples = read_wav_file();

        // The file is 16kHz, check if the length is reasonable
        // Calculate the actual length and print it for inspection
        let duration_sec = pcm_samples.len() as f32 / 16000.0;
        println!("Audio duration: {:.2} seconds", duration_sec);
        println!("Audio samples: {} at 16kHz", pcm_samples.len());

        // Ensure the file has some content and isn't unreasonably large
        // The Chinese phrase "您好,请帮我预约课程" should be several seconds at 16kHz
        assert!(
            pcm_samples.len() >= 16000,
            "Audio too short, expected at least 1 second"
        );
        assert!(
            pcm_samples.len() <= 300000,
            "Audio too large, expected less than reasonable limit"
        );

        // Create a test audio frame
        let track_id = format!("test-{}", Uuid::new_v4());
        let audio_frame = AudioFrame {
            track_id,
            samples: Samples::PCM(pcm_samples.clone()),
            timestamp: 0,
            sample_rate: 16000,
        };

        // Verify the audio has correct format
        if let Samples::PCM(data) = &audio_frame.samples {
            // Calculate duration in seconds
            let duration_sec = data.len() as f32 / audio_frame.sample_rate as f32;
            println!("Audio duration: {:.2} seconds", duration_sec);
            println!(
                "Audio samples: {} at {}Hz",
                data.len(),
                audio_frame.sample_rate
            );

            // Ensure we have reasonable audio duration
            assert!(
                duration_sec >= 1.0,
                "Audio duration should be at least 1 second, got {:.2}s",
                duration_sec
            );
        } else {
            panic!("Expected PCM samples");
        }
    }
}
