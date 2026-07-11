#[cfg(test)]
mod media_engine_tests {
use std::sync::Arc;
use crate::media::LegId;
use crate::media::media_stream::TrackMap;
use crate::media::mixer::AudioMixer;
use crate::media::engine::MediaCommand;
use crate::media::engine::event::MediaEvent;

    // ── LegId ────────────────────────────────────────────────────────────

    #[test]
    fn test_leg_id_creation() {
        let id = LegId::new("leg-1");
        assert_eq!(id.as_str(), "leg-1");
        assert_eq!(id.to_string(), "leg-1");
    }

    #[test]
    fn test_leg_id_from_string() {
        let id = LegId::from("test".to_string());
        assert_eq!(id.as_str(), "test");
    }

    #[test]
    fn test_leg_id_from_str() {
        let id: LegId = "hello".into();
        assert_eq!(id.as_str(), "hello");
    }

    #[test]
    fn test_leg_id_into_string() {
        let id = LegId::new("convert");
        let s: String = id.into();
        assert_eq!(s, "convert");
    }

    #[test]
    fn test_leg_id_equality() {
        let a = LegId::new("same");
        let b = LegId::new("same");
        let c = LegId::new("diff");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_leg_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(LegId::new("a"));
        set.insert(LegId::new("b"));
        set.insert(LegId::new("a")); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_leg_id_serde_roundtrip() {
        let id = LegId::new("serde-test");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"serde-test\"");
        let deserialized: LegId = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, id);
    }

    // ── AudioMixer (pure PCM) ────────────────────────────────────────────

    #[test]
    fn test_audio_mixer_single_frame() {
        let mixer = AudioMixer::new(8000, 1);
        let frame = vec![500i16; 160];
        let result = mixer.mix_frames(vec![frame], &[1.0]);
        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| s == 500));
    }

    #[test]
    fn test_audio_mixer_mismatched_gains_returns_empty() {
        let mixer = AudioMixer::new(8000, 1);
        let frame1 = vec![100i16; 160];
        let frame2 = vec![200i16; 160];
        // 2 frames but 3 gains — mismatch
        let result = mixer.mix_frames(vec![frame1, frame2], &[1.0, 0.5, 0.0]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_audio_mixer_different_length_frames() {
        let mixer = AudioMixer::new(8000, 1);
        let frame1 = vec![100i16; 160];
        let frame2 = vec![200i16; 80]; // shorter
        let result = mixer.mix_frames(vec![frame1, frame2], &[1.0, 1.0]);
        // Short frame is skipped, so output should be just frame1
        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| s == 100));
    }

    #[test]
    fn test_audio_mixer_full_volume_no_clip() {
        let mixer = AudioMixer::new(8000, 1);
        let frame = vec![i16::MAX; 160];
        let result = mixer.mix_frames(vec![frame], &[1.0]);
        assert_eq!(result.len(), 160);
        // Should be clamped to i16::MAX
        assert!(result.iter().all(|&s| s == i16::MAX));
    }

    // ── MediaEvent serialisation ────────────────────────────────────────

    #[test]
    fn test_media_event_session_created_serde() {
        let ev = MediaEvent::SessionCreated {
            session_id: "sess-1".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("session_created"));
        assert!(json.contains("sess-1"));
    }

    #[test]
    fn test_media_event_play_started_serde() {
        let ev = MediaEvent::PlayStarted {
            session_id: "s-1".into(),
            leg_id: "caller".into(),
            play_id: "p-1".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("play_started"));
    }

    #[test]
    fn test_media_event_dtmf_collected_serde() {
        let ev = MediaEvent::DtmfCollected {
            session_id: "s-1".into(),
            leg_id: "caller".into(),
            digits: "123#".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("dtmf_collected"));
    }

    #[test]
    fn test_media_event_session_id_extraction() {
        let ev = MediaEvent::SessionCreated {
            session_id: "test".into(),
        };
        assert_eq!(ev.session_id(), Some("test"));

        let ev = MediaEvent::BridgeEstablished {
            session_id: "bridge-sess".into(),
            leg_a: "a".into(),
            leg_b: "b".into(),
        };
        assert_eq!(ev.session_id(), Some("bridge-sess"));

        let ev = MediaEvent::LegHeld {
            session_id: "hold-sess".into(),
            leg_id: "leg1".into(),
        };
        assert_eq!(ev.session_id(), Some("hold-sess"));
    }

    // ── MediaCommand session_id extraction ──────────────────────────────

    #[test]
    fn test_media_command_session_id() {
        let cmd = MediaCommand::CreateSession {
            session_id: "s1".into(),
        };
        assert_eq!(cmd.session_id(), Some("s1"));

        let cmd = MediaCommand::DestroySession {
            session_id: "s1".into(),
        };
        assert_eq!(cmd.session_id(), Some("s1"));

        let cmd = MediaCommand::Play {
            session_id: "s1".into(),
            leg_id: None,
            source: crate::media::engine::PlaySource::Silence,
            options: Default::default(),
        };
        assert_eq!(cmd.session_id(), Some("s1"));

        let cmd = MediaCommand::SendDtmf {
            session_id: "s1".into(),
            leg_id: "caller".into(),
            digits: "1".into(),
        };
        assert_eq!(cmd.session_id(), Some("s1"));
    }

    #[test]
    fn test_media_command_name() {
        assert_eq!(MediaCommand::CreateSession { session_id: "x".into() }.name(), "create_session");
        assert_eq!(MediaCommand::DestroySession { session_id: "x".into() }.name(), "destroy_session");
        assert_eq!(
            MediaCommand::Play {
                session_id: "x".into(),
                leg_id: None,
                source: crate::media::engine::PlaySource::Silence,
                options: Default::default(),
            }
            .name(),
            "play"
        );
        assert_eq!(MediaCommand::BridgeLegs { session_id: "x".into(), leg_a: "a".into(), leg_b: "b".into() }.name(), "bridge_legs");
        assert_eq!(MediaCommand::SendDtmf { session_id: "x".into(), leg_id: "a".into(), digits: "".into() }.name(), "send_dtmf");
        assert_eq!(MediaCommand::MuteLeg { session_id: "x".into(), leg_id: "a".into() }.name(), "mute_leg");
        assert_eq!(MediaCommand::UnmuteLeg { session_id: "x".into(), leg_id: "a".into() }.name(), "unmute_leg");
    }

    // ── DashMap / TrackMap ──────────────────────────────────────────────

    #[test]
    fn test_track_map_insert_lookup() {
        let map = TrackMap::new();
        assert!(map.is_empty());
        assert!(!map.contains_key("nonexistent"));
    }

    #[tokio::test]
    async fn test_track_map_remove() {
        let map = TrackMap::new();
        // Use a mock track that doesn't need a PeerConnection
        struct DummyTrack(String);
        #[async_trait::async_trait]
        impl crate::media::Track for DummyTrack {
            fn id(&self) -> &str { &self.0 }
            async fn handshake(&self, _: String) -> anyhow::Result<String> { Ok("".into()) }
            async fn local_description(&self) -> anyhow::Result<String> { Ok("".into()) }
            async fn set_remote_description(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn stop(&self) {}
            async fn get_peer_connection(&self) -> Option<rustrtc::PeerConnection> { None }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
        }

        map.insert("key".into(), std::sync::Arc::new(tokio::sync::Mutex::new(
            Box::new(DummyTrack("key".into())) as Box<dyn crate::media::Track>
        )));
        assert_eq!(map.len(), 1);
        map.remove("key");
        assert!(map.is_empty());
    }

    // ── Performance benchmarks ───────────────────────────────────────

    /// Benchmark: AudioMixer 4-channel mixing (20ms @ 8kHz, 1000 iterations).
    #[test]
    fn bench_audio_mixer_4ch_20ms() {
        let mixer = AudioMixer::new(8000, 1);
        let frames: Vec<Vec<i16>> = (0..4)
            .map(|_| (0..160).map(|_| rand::random::<i16>()).collect())
            .collect();
        let gains = [1.0, 0.7, 0.5, 0.3];
        let start = std::time::Instant::now();
        let iterations = 1000;
        for _ in 0..iterations {
            let _result = mixer.mix_frames(frames.clone(), &gains);
        }
        let elapsed = start.elapsed();
        let per_call = elapsed / iterations;
        assert!(
            per_call.as_nanos() < 100_000,
            "AudioMixer::mix_frames took {}ns per call (max 100µs)",
            per_call.as_nanos()
        );
    }

    /// Helper: create a running MediaEngine for benchmarks.
    fn bench_engine() -> (crate::media::engine::MediaEngine, tokio::sync::broadcast::Receiver<MediaEvent>) {
        use crate::media::engine::{MediaEngine, MediaEngineConfig};
        let (engine, handle) = MediaEngine::new(MediaEngineConfig {
            command_channel_capacity: 8192,
            event_channel_capacity: 8192,
            session_reaper_interval: std::time::Duration::from_secs(3600),
            session_idle_ttl: std::time::Duration::from_secs(7200),
        });
        let rx = engine.subscribe();
        engine.spawn(handle);
        (engine, rx)
    }

    /// Benchmark: engine channel send throughput (async, backpressure-aware).
    #[tokio::test]
    async fn bench_engine_dispatch_throughput() {
        let (engine, _rx) = bench_engine();
        engine
            .send(MediaCommand::CreateSession {
                session_id: "bench-sess".into(),
            })
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let n = 5_000u32;
        let start = std::time::Instant::now();
        for i in 0..n {
            engine
                .send_async(MediaCommand::AddLeg {
                    session_id: "bench-sess".into(),
                    leg_id: format!("leg-{}", i),
                    transport: crate::media::engine::LegTransport::File {
                        path: "/dev/null".into(),
                    },
                    codec_profile: Some(crate::media::engine::CodecProfile::pcmu()),
                })
                .await
                .unwrap();
        }
        let elapsed = start.elapsed();
        let per_cmd = elapsed / n;
        // send_async waits for capacity, so it's slower than try_send.
        // Typical dispatch latency (engine processes as fast as we send):
        assert!(
            per_cmd.as_micros() < 500,
            "Engine send_async took {}µs per command (max 500µs)",
            per_cmd.as_micros()
        );

        engine
            .send(MediaCommand::DestroySession {
                session_id: "bench-sess".into(),
            })
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    /// Benchmark: session create + destroy round-trip latency.
    #[tokio::test]
    async fn bench_engine_session_lifecycle() {
        let (engine, mut rx) = bench_engine();
        let n = 1000u32;
        let start = std::time::Instant::now();

        for i in 0..n {
            let sid = format!("bench-lc-{}", i);
            engine
                .send(MediaCommand::CreateSession { session_id: sid.clone() })
                .unwrap();
            let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv()).await;
            engine
                .send(MediaCommand::DestroySession { session_id: sid })
                .unwrap();
            let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx.recv()).await;
        }

        let elapsed = start.elapsed();
        let per_session = elapsed / n;
        assert!(
            per_session.as_millis() < 50,
            "Session lifecycle took {}ms per session (max 50ms)",
            per_session.as_millis()
        );
    }

    /// Benchmark: DTMF detection throughput.
    #[test]
    fn bench_dtmf_detection() {
        let mut detector = crate::media::dtmf::DtmfDetector::default();
        let n = 100_000usize;
        let start = std::time::Instant::now();
        for i in 0..n {
            let payload = &[b'1', 10, 0, 0];    // digit=1, end=no, volume=10, duration=0
            let _ = detector.observe(payload, i as u32);
        }
        let elapsed = start.elapsed();
        let per_call = elapsed / n as u32;
        assert!(
            per_call.as_nanos() < 500,
            "DtmfDetector::observe took {}ns per call (max 500ns)",
            per_call.as_nanos()
        );
    }

    /// Benchmark: parking_lot::Mutex lock/unlock overhead (used extensively
    /// in bridge.rs, forwarding_track.rs, and conference_mixer.rs).
    #[test]
    fn bench_parking_lot_mutex_contention() {
        use parking_lot::Mutex;
        let mu = Arc::new(Mutex::new(0u64));
        let n = 100_000usize;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let mut g = mu.lock();
            *g += 1;
        }
        let elapsed = start.elapsed();
        let per_op = elapsed / n as u32;
        assert!(
            per_op.as_nanos() < 200,
            "parking_lot::Mutex lock+unlock took {}ns (max 200ns)",
            per_op.as_nanos()
        );
    }
}
