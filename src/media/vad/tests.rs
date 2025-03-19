use super::*;
use crate::media::processor::{AudioFrame, Processor};
use crate::media::stream::{EventSender, MediaStreamEvent};
use tokio::sync::broadcast;

#[tokio::test]
async fn test_webrtc_vad() {
    let (event_sender, mut event_receiver) = broadcast::channel(16);
    let track_id = "test_track".to_string();
    let ssrc = 12345;

    let mut vad = VadProcessor::new(track_id.clone(), ssrc, VadType::WebRTC, event_sender);

    // Initialize VAD
    vad.init().await.unwrap();

    // Test with silence (all zeros)
    let silence_frame = AudioFrame {
        samples: vec![0.0; 480], // 30ms at 16kHz
        sample_rate: 16000,
        channels: 1,
    };
    vad.process_frame(silence_frame).await.unwrap();

    // Should receive silence event
    if let Ok(MediaStreamEvent::Silence(id, s)) = event_receiver.try_recv() {
        assert_eq!(id, track_id);
        assert_eq!(s, ssrc);
    } else {
        panic!("Expected silence event");
    }

    // Test with speech (sine wave)
    let speech_frame = AudioFrame {
        samples: (0..480).map(|i| (i as f32 * 0.1).sin() * 0.5).collect(),
        sample_rate: 16000,
        channels: 1,
    };
    vad.process_frame(speech_frame).await.unwrap();

    // Should receive speech event
    if let Ok(MediaStreamEvent::StartSpeaking(id, s)) = event_receiver.try_recv() {
        assert_eq!(id, track_id);
        assert_eq!(s, ssrc);
    } else {
        panic!("Expected speech event");
    }
}

#[tokio::test]
async fn test_voice_activity_vad() {
    let (event_sender, mut event_receiver) = broadcast::channel(16);
    let track_id = "test_track".to_string();
    let ssrc = 12345;

    let mut vad = VadProcessor::new(track_id.clone(), ssrc, VadType::VoiceActivity, event_sender);

    // Initialize VAD
    vad.init().await.unwrap();

    // Test with silence (all zeros)
    let silence_frame = AudioFrame {
        samples: vec![0.0; 480], // 30ms at 16kHz
        sample_rate: 16000,
        channels: 1,
    };
    vad.process_frame(silence_frame).await.unwrap();

    // Should receive silence event
    if let Ok(MediaStreamEvent::Silence(id, s)) = event_receiver.try_recv() {
        assert_eq!(id, track_id);
        assert_eq!(s, ssrc);
    } else {
        panic!("Expected silence event");
    }

    // Test with speech (sine wave)
    let speech_frame = AudioFrame {
        samples: (0..480).map(|i| (i as f32 * 0.1).sin() * 0.5).collect(),
        sample_rate: 16000,
        channels: 1,
    };
    vad.process_frame(speech_frame).await.unwrap();

    // Should receive speech event
    if let Ok(MediaStreamEvent::StartSpeaking(id, s)) = event_receiver.try_recv() {
        assert_eq!(id, track_id);
        assert_eq!(s, ssrc);
    } else {
        panic!("Expected speech event");
    }
}

#[tokio::test]
async fn test_vad_type_switching() {
    let (event_sender, _) = broadcast::channel(16);
    let track_id = "test_track".to_string();
    let ssrc = 12345;

    // Create VAD with WebRTC type
    let mut vad = VadProcessor::new(
        track_id.clone(),
        ssrc,
        VadType::WebRTC,
        event_sender.clone(),
    );
    vad.init().await.unwrap();

    // Create VAD with VoiceActivity type
    let mut vad2 = VadProcessor::new(track_id, ssrc, VadType::VoiceActivity, event_sender);
    vad2.init().await.unwrap();

    // Test that both can process frames
    let frame = AudioFrame {
        samples: vec![0.0; 480],
        sample_rate: 16000,
        channels: 1,
    };

    vad.process_frame(frame.clone()).await.unwrap();
    vad2.process_frame(frame).await.unwrap();
}
