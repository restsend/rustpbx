use super::*;
use crate::{media::processor::Processor, Samples};
use tokio::sync::broadcast;

#[tokio::test]
async fn test_webrtc_vad() {
    let (event_sender, mut event_receiver) = broadcast::channel(16);
    let track_id = "test_track".to_string();

    let vad = VadProcessor::new(VadType::WebRTC, event_sender.clone(), VADConfig::default());

    // Test with silence (all zeros)
    let mut silence_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(vec![0; 480]), // 30ms at 16kHz
        sample_rate: 16000,
        timestamp: 0,
    };
    vad.process_frame(&mut silence_frame).unwrap();

    // Should receive silence event
    if let Ok(SessionEvent::Silence {
        track_id: id,
        timestamp: ts,
    }) = event_receiver.try_recv()
    {
        assert_eq!(id, track_id);
        assert_eq!(ts, 0);
    } else {
        panic!("Expected silence event");
    }

    // Test with speech (sine wave)
    let mut speech_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(
            (0..480)
                .map(|i| ((i as f32 * 0.1).sin() * 16000.0) as i16)
                .collect(),
        ),
        sample_rate: 16000,
        timestamp: 1,
    };
    vad.process_frame(&mut speech_frame).unwrap();

    // Should receive speech event
    if let Ok(SessionEvent::StartSpeaking {
        track_id: id,
        timestamp: ts,
    }) = event_receiver.try_recv()
    {
        assert_eq!(id, track_id);
        assert_eq!(ts, 1);
    } else {
        panic!("Expected speech event");
    }
}

#[tokio::test]
async fn test_voice_activity_vad() {
    let (event_sender, mut event_receiver) = broadcast::channel(16);
    let track_id = "test_track".to_string();

    let vad = VadProcessor::new(VadType::Silero, event_sender.clone(), VADConfig::default());

    // Test with silence (all zeros)
    let mut silence_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(vec![0; 512]), // Use 512 samples for VoiceActivityVad (32ms at 16kHz)
        sample_rate: 16000,
        timestamp: 0,
    };
    vad.process_frame(&mut silence_frame).unwrap();

    // Should receive silence event
    if let Ok(SessionEvent::Silence {
        track_id: id,
        timestamp: ts,
    }) = event_receiver.try_recv()
    {
        assert_eq!(id, track_id);
        assert_eq!(ts, 0);
    } else {
        panic!("Expected silence event");
    }

    // Test with speech frame (doesn't matter what we send since we're in test mode)
    let speech_samples: Vec<i16> = (0..512)
        .map(|i| ((i as f32 * 0.1).sin() * 25000.0) as i16)
        .collect();

    let mut speech_frame = AudioFrame {
        track_id: track_id.clone(),
        samples: Samples::PCM(speech_samples),
        sample_rate: 16000,
        timestamp: 1,
    };
    vad.process_frame(&mut speech_frame).unwrap();

    // Check what event we got
    match event_receiver.try_recv() {
        Ok(SessionEvent::StartSpeaking {
            track_id: id,
            timestamp: ts,
        }) => {
            assert_eq!(id, track_id);
            assert_eq!(ts, 1);
        }
        Ok(other_event) => {
            panic!("Expected speech event, but got {:?}", other_event);
        }
        Err(e) => {
            panic!("Expected speech event, but got error: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_vad_type_switching() {
    let (event_sender, _) = broadcast::channel(16);
    let track_id = "test_track".to_string();

    // Create VAD with WebRTC type
    let vad = VadProcessor::new(VadType::WebRTC, event_sender.clone(), VADConfig::default());

    // Create VAD with VoiceActivity type
    let vad2 = VadProcessor::new(VadType::Silero, event_sender.clone(), VADConfig::default());

    // Test that both can process frames
    let mut frame = AudioFrame {
        track_id,
        samples: Samples::PCM(vec![0; 512]), // Use 512 samples (32ms at 16kHz)
        sample_rate: 16000,
        timestamp: 0,
    };

    vad.process_frame(&mut frame).unwrap();

    // Clone the frame for the second processor
    let mut frame2 = frame.clone();
    vad2.process_frame(&mut frame2).unwrap();
}
