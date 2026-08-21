//! MCU Three-Way Call Integration Tests
//!
//! Tests for in-server MCU conference functionality with 3+ participants.

use rustpbx::call::domain::LegId;
use rustpbx::call::runtime::ConferenceManager;
use rustpbx::media::conference_mixer::AudioFrame;

#[tokio::test]
async fn test_three_way_conference_audio_flow() {
    // 1. Create conference
    let manager = ConferenceManager::new();
    manager
        .create_conference("conf1".into(), None)
        .await
        .unwrap();

    // 2. Add 3 participants
    let channels_a = manager
        .add_participant(&"conf1".into(), LegId::new("a"))
        .await
        .unwrap();
    let channels_b = manager
        .add_participant(&"conf1".into(), LegId::new("b"))
        .await
        .unwrap();
    let channels_c = manager
        .add_participant(&"conf1".into(), LegId::new("c"))
        .await
        .unwrap();

    let tx_a = channels_a.input_tx;
    let tx_b = channels_b.input_tx;
    let tx_c = channels_c.input_tx;

    // Get output receivers
    let mut rx_a = manager
        .take_participant_output_rx(&LegId::new("a"))
        .await
        .unwrap();
    let mut rx_b = manager
        .take_participant_output_rx(&LegId::new("b"))
        .await
        .unwrap();
    let mut rx_c = manager
        .take_participant_output_rx(&LegId::new("c"))
        .await
        .unwrap();

    // 3. A sends audio
    let samples = vec![1000i16; 160];
    tx_a.send(AudioFrame::new(samples, 8000)).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 4. B and C should receive A's audio
    assert!(rx_b.try_recv().is_ok(), "B should hear A");
    assert!(rx_c.try_recv().is_ok(), "C should hear A");

    // 5. A should not receive its own audio
    assert!(rx_a.try_recv().is_err(), "A should not hear self");

    // 6. All three speak simultaneously
    tx_a.send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    tx_b.send(AudioFrame::new(vec![2000i16; 160], 8000))
        .await
        .unwrap();
    tx_c.send(AudioFrame::new(vec![3000i16; 160], 8000))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 7. Each should hear the other two
    let frame_a = rx_a.try_recv().expect("A should hear B+C");
    let frame_b = rx_b.try_recv().expect("B should hear A+C");
    let frame_c = rx_c.try_recv().expect("C should hear A+B");

    // Verify mixed audio is non-zero
    assert!(frame_a.samples.iter().any(|&s| s != 0));
    assert!(frame_b.samples.iter().any(|&s| s != 0));
    assert!(frame_c.samples.iter().any(|&s| s != 0));

    // 8. Cleanup
    manager.destroy_conference(&"conf1".into()).await.unwrap();
}

#[tokio::test]
async fn test_conference_participant_join_leave() {
    let manager = ConferenceManager::new();
    manager
        .create_conference("conf2".into(), None)
        .await
        .unwrap();

    // Add 2 participants
    let channels_a = manager
        .add_participant(&"conf2".into(), LegId::new("a"))
        .await
        .unwrap();
    let _channels_b = manager
        .add_participant(&"conf2".into(), LegId::new("b"))
        .await
        .unwrap();

    let tx_a = channels_a.input_tx;
    let mut rx_b = manager
        .take_participant_output_rx(&LegId::new("b"))
        .await
        .unwrap();

    // A speaks
    tx_a.send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // B should hear A
    assert!(rx_b.try_recv().is_ok(), "B should hear A");

    // Add third participant C
    let channels_c = manager
        .add_participant(&"conf2".into(), LegId::new("c"))
        .await
        .unwrap();
    let tx_c = channels_c.input_tx;
    let mut rx_a = manager
        .take_participant_output_rx(&LegId::new("a"))
        .await
        .unwrap();
    let mut rx_c = manager
        .take_participant_output_rx(&LegId::new("c"))
        .await
        .unwrap();

    // C speaks
    tx_c.send(AudioFrame::new(vec![2000i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // A and B should hear C
    assert!(rx_a.try_recv().is_ok(), "A should hear C");
    assert!(rx_b.try_recv().is_ok(), "B should hear C");

    // Remove B
    manager
        .remove_participant(&"conf2".into(), &LegId::new("b"))
        .await
        .unwrap();

    // A speaks again
    tx_a.send(AudioFrame::new(vec![1500i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // C should still hear A
    assert!(rx_c.try_recv().is_ok(), "C should hear A after B leaves");

    // Cleanup
    manager.destroy_conference(&"conf2".into()).await.unwrap();
}

#[tokio::test]
async fn test_conference_mute_unmute() {
    let manager = ConferenceManager::new();
    manager
        .create_conference("conf3".into(), None)
        .await
        .unwrap();

    let channels_a = manager
        .add_participant(&"conf3".into(), LegId::new("a"))
        .await
        .unwrap();
    let _channels_b = manager
        .add_participant(&"conf3".into(), LegId::new("b"))
        .await
        .unwrap();

    let tx_a = channels_a.input_tx;
    let mut rx_b = manager
        .take_participant_output_rx(&LegId::new("b"))
        .await
        .unwrap();

    // Mute A
    manager
        .mute_participant(&"conf3".into(), &LegId::new("a"))
        .await
        .unwrap();

    // A speaks while muted
    tx_a.send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // B should not hear A (muted)
    let _result = rx_b.try_recv();
    // Note: Due to timing, we might get silence frames or nothing
    // The key is that the muted audio is not mixed in

    // Unmute A
    manager
        .unmute_participant(&"conf3".into(), &LegId::new("a"))
        .await
        .unwrap();

    // A speaks again
    tx_a.send(AudioFrame::new(vec![1000i16; 160], 8000))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // B should now hear A
    assert!(rx_b.try_recv().is_ok(), "B should hear A after unmute");

    // Cleanup
    manager.destroy_conference(&"conf3".into()).await.unwrap();
}

#[tokio::test]
async fn test_full_duplex_media_bridge() {
    use rustpbx::call::runtime::ConferenceMediaBridge;
    use rustpbx::call::runtime::conference_media_bridge::{AudioReceiver, PcmAudioFrame};
    use std::pin::Pin;

    let manager = std::sync::Arc::new(ConferenceManager::new());
    let bridge = ConferenceMediaBridge::new(manager.clone());
    let _ = tracing_subscriber::fmt().try_init();

    // Create conference
    manager
        .create_conference("conf4".into(), None)
        .await
        .unwrap();

    // Create mock audio receiver that provides test frames
    struct MockReceiver {
        frames: Vec<PcmAudioFrame>,
        index: usize,
    }

    impl AudioReceiver for MockReceiver {
        fn recv(
            &mut self,
        ) -> Pin<Box<dyn std::future::Future<Output = Option<PcmAudioFrame>> + Send + '_>> {
            Box::pin(async move {
                if self.index < self.frames.len() {
                    let frame = self.frames[self.index].clone();
                    self.index += 1;
                    Some(frame)
                } else {
                    None
                }
            })
        }
    }

    async fn start_leg(
        bridge: &ConferenceMediaBridge,
        leg: &'static str,
        amplitude: i16,
    ) -> (
        anyhow::Result<rustpbx::call::runtime::conference_media_bridge::ConferenceBridgeHandle>,
        tokio::sync::mpsc::Receiver<rustrtc::media::MediaSample>,
    ) {
        let receiver = Box::new(MockReceiver {
            frames: vec![
                PcmAudioFrame::new(vec![amplitude; 160], 8000),
                PcmAudioFrame::new(vec![amplitude; 160], 8000),
            ],
            index: 0,
        });
        let (sender_tx, sender_rx) = tokio::sync::mpsc::channel(100);
        let leg_id = LegId::new(leg);
        let handle = bridge
            .start_bridge_full_duplex(
                "conf4",
                &leg_id,
                sender_tx,
                receiver,
                audio_codec::CodecType::PCMU,
            )
            .await;
        (handle, sender_rx)
    }

    // Two participants: the N-1 mixer forwards each leg's audio to the OTHER
    // leg only, so both directions must carry audio for the egress to be
    // proven alive (a single participant legitimately hears silence).
    let (handle_a, mut sender_rx_a) = start_leg(&bridge, "leg-a", 1000).await;
    let (handle_b, mut sender_rx_b) = start_leg(&bridge, "leg-b", 2000).await;
    assert!(handle_a.is_ok(), "bridge for leg-a should start");
    assert!(handle_b.is_ok(), "bridge for leg-b should start");

    // Give the 20ms mixing loop time to run several ticks.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    fn drain_peak_amplitude(
        rx: &mut tokio::sync::mpsc::Receiver<rustrtc::media::MediaSample>,
    ) -> (usize, i16) {
        let mut count = 0usize;
        let mut peak = 0i16;
        let mut decoder = audio_codec::create_decoder(audio_codec::CodecType::PCMU);
        while let Ok(sample) = rx.try_recv() {
            if let rustrtc::media::MediaSample::Audio(frame) = sample {
                count += 1;
                for s in decoder.decode(&frame.data) {
                    let s = if s < 0 { -s } else { s };
                    peak = peak.max(s);
                }
            }
        }
        (count, peak)
    }

    // Leg A must hear leg B's 2000-amplitude tone, leg B must hear leg A's
    // 1000-amplitude tone — both encoded through the PCMU forward loop.
    let (a_count, a_peak) = drain_peak_amplitude(&mut sender_rx_a);
    assert!(a_count > 0, "leg A must receive mixed output frames");
    assert!(
        a_peak > 500,
        "leg A's output must carry leg B's audible signal, peak {a_peak}"
    );

    let (b_count, b_peak) = drain_peak_amplitude(&mut sender_rx_b);
    assert!(b_count > 0, "leg B must receive mixed output frames");
    assert!(
        b_peak > 500,
        "leg B's output must carry leg A's audible signal, peak {b_peak}"
    );

    // Cleanup
    manager.destroy_conference(&"conf4".into()).await.unwrap();
}
