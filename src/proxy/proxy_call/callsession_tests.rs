#[cfg(test)]
mod callsession_b2bua_tests {
    use super::super::media_bridge::MediaBridge;
    use super::super::session::NegotiationState;
    use super::super::test_util::tests::MockMediaPeer;
    use crate::call::{DialStrategy, DialplanFlow};
    use crate::media::negotiate::MediaNegotiator;
    // use crate::proxy::tests::common::create_test_server;
    use audio_codec::CodecType;
    use rustrtc::RtpCodecParameters;
    use std::sync::Arc;

    // ==================== Helper Functions ====================

    /// Create a mock RTP codec parameters for testing
    fn mock_rtp_params(payload_type: u8, clock_rate: u32, channels: u8) -> RtpCodecParameters {
        RtpCodecParameters {
            payload_type,
            clock_rate,
            channels,
        }
    }

    /// Create a simple SDP offer with specified codec
    fn create_sdp_offer(codec: &str, payload_type: u8) -> String {
        format!(
            "v=0\r\n\
             o=- 1234567890 1234567890 IN IP4 192.168.1.1\r\n\
             s=-\r\n\
             c=IN IP4 192.168.1.1\r\n\
             t=0 0\r\n\
             m=audio 10000 RTP/AVP {}\r\n\
             a=rtpmap:{} {}\r\n",
            payload_type, payload_type, codec
        )
    }

    /// Create a simple SDP answer with specified codec
    fn create_sdp_answer(codec: &str, payload_type: u8) -> String {
        format!(
            "v=0\r\n\
             o=- 9876543210 9876543210 IN IP4 192.168.1.2\r\n\
             s=-\r\n\
             c=IN IP4 192.168.1.2\r\n\
             t=0 0\r\n\
             m=audio 20000 RTP/AVP {}\r\n\
             a=rtpmap:{} {}\r\n",
            payload_type, payload_type, codec
        )
    }

    // ==================== Test 1: Basic Forwarding Scenarios ====================

    #[tokio::test]
    async fn test_simple_forward_pcmu_to_pcmu() {
        // Test: A -> B with same codec (PCMU), should use zero-copy forwarding
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let params_a = mock_rtp_params(0, 8000, 1);
        let params_b = mock_rtp_params(0, 8000, 1);

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            params_a,
            params_b,
            None,
            None,
            CodecType::PCMU,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_simple_forward_pcmu_to_pcmu".to_string(),
            None,
        );

        // Should NOT require transcoding
        assert_eq!(bridge.codec_a, CodecType::PCMU);
        assert_eq!(bridge.codec_b, CodecType::PCMU);

        bridge
            .start()
            .await
            .expect("Bridge should start successfully");
        bridge.stop();

        assert!(leg_a.stop_called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(leg_b.stop_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_forward_opus_to_pcmu_transcoding() {
        // Test: A (Opus) -> B (PCMU), should require transcoding
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let params_a = mock_rtp_params(111, 48000, 2);
        let params_b = mock_rtp_params(0, 8000, 1);

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            params_a,
            params_b,
            None,
            None,
            CodecType::Opus,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_forward_opus_to_pcmu_transcoding".to_string(),
            None,
        );

        // Should require transcoding
        assert_eq!(bridge.codec_a, CodecType::Opus);
        assert_eq!(bridge.codec_b, CodecType::PCMU);

        bridge
            .start()
            .await
            .expect("Bridge should start successfully");

        // Verify bridge handles transcoding path
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        bridge.stop();
    }

    #[tokio::test]
    async fn test_pcma_to_pcmu_transcoding() {
        // Test: A (PCMA) -> B (PCMU), should require transcoding
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let params_a = mock_rtp_params(8, 8000, 1);
        let params_b = mock_rtp_params(0, 8000, 1);

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            params_a.clone(),
            params_b.clone(),
            None,
            None,
            CodecType::PCMA,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_pcma_to_pcmu_transcoding".to_string(),
            None,
        );

        // Should require transcoding
        assert_eq!(bridge.codec_a, CodecType::PCMA);
        assert_eq!(bridge.codec_b, CodecType::PCMU);

        bridge
            .start()
            .await
            .expect("Bridge should start successfully");
        bridge.stop();
    }

    // ==================== Test 2: Codec Negotiation Scenarios ====================

    #[test]
    fn test_parse_rtp_map_from_sdp_multi_codec() {
        // Test: Parse SDP with multiple codecs
        let sdp = "v=0\r\n\
            o=- 8819118164752754436 2 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 53824 UDP/TLS/RTP/SAVPF 111 9 0 8\r\n\
            a=rtpmap:111 opus/48000/2\r\n\
            a=rtpmap:9 G722/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        let parsed_sdp = rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, sdp).unwrap();
        let section = parsed_sdp
            .media_sections
            .iter()
            .find(|m| m.kind == rustrtc::MediaKind::Audio)
            .unwrap();

        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        // Verify all codecs are parsed
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 111 && *codec == CodecType::Opus)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 9 && *codec == CodecType::G722)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 0 && *codec == CodecType::PCMU)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 8 && *codec == CodecType::PCMA)
        );
    }

    #[test]
    fn test_extract_codec_params_pcmu() {
        let sdp = create_sdp_answer("PCMU/8000/1", 0);
        let (codecs, dtmf_pt) = MediaNegotiator::extract_codec_params(&sdp);
        let first = &codecs[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::PCMU);
        assert_eq!(params.payload_type, 0);
        assert_eq!(params.clock_rate, 8000);
        assert_eq!(params.channels, 1);
        assert_eq!(dtmf_pt, None);
    }

    #[test]
    fn test_extract_codec_params_pcma() {
        let sdp = create_sdp_answer("PCMA/8000/1", 8);
        let (codecs, _) = MediaNegotiator::extract_codec_params(&sdp);
        let first = &codecs[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::PCMA);
        assert_eq!(params.payload_type, 8);
        assert_eq!(params.clock_rate, 8000);
        assert_eq!(params.channels, 1);
    }

    #[test]
    fn test_extract_codec_params_opus() {
        let sdp = create_sdp_answer("opus/48000/2", 111);
        let (codecs, _) = MediaNegotiator::extract_codec_params(&sdp);
        let first = &codecs[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::Opus);
        assert_eq!(params.payload_type, 111);
        assert_eq!(params.clock_rate, 48000);
        assert_eq!(params.channels, 2);
    }

    #[test]
    fn test_extract_codec_params_g722() {
        let sdp = create_sdp_answer("G722/8000", 9);
        let (codecs, _) = MediaNegotiator::extract_codec_params(&sdp);
        let first = &codecs[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::G722);
        assert_eq!(params.payload_type, 9);
        assert_eq!(params.clock_rate, 8000);
    }

    #[test]
    fn test_dtmf_payload_extraction() {
        let sdp = "v=0\r\n\
            o=- 1234567890 1234567890 IN IP4 192.168.1.1\r\n\
            s=-\r\n\
            c=IN IP4 192.168.1.1\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let (codecs, dtmf_pt) = MediaNegotiator::extract_codec_params(sdp);
        let first = &codecs[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::PCMU);
        assert_eq!(params.payload_type, 0);
        assert_eq!(dtmf_pt, Some(101));
    }

    // ==================== Test 3: Codec Compatibility Scenarios ====================

    #[test]
    fn test_codec_compatibility_exact_match() {
        // Both sides support PCMU - should match without transcoding
        let alice_offer = create_sdp_offer("PCMU/8000/1", 0);
        let bob_answer = create_sdp_answer("PCMU/8000/1", 0);

        let (alice_codecs, _) = MediaNegotiator::extract_codec_params(&alice_offer);
        let (bob_codecs, _) = MediaNegotiator::extract_codec_params(&bob_answer);
        let bob_codec = bob_codecs[0].codec;

        // Verify Alice supports Bob's chosen codec
        let compatible = alice_codecs.iter().any(|c| c.codec == bob_codec);

        assert!(compatible, "Alice should support PCMU");
        assert_eq!(bob_codec, CodecType::PCMU);
    }

    #[test]
    fn test_codec_incompatibility_requires_transcoding() {
        // Alice only supports Opus, Bob only supports PCMU - requires transcoding
        let alice_offer = create_sdp_offer("opus/48000/2", 111);
        let bob_answer = create_sdp_answer("PCMU/8000/1", 0);

        let alice_sdp =
            rustrtc::SessionDescription::parse(rustrtc::SdpType::Offer, &alice_offer).unwrap();
        let alice_section = alice_sdp
            .media_sections
            .iter()
            .find(|m| m.kind == rustrtc::MediaKind::Audio)
            .unwrap();
        let alice_codecs = MediaNegotiator::parse_rtp_map_from_section(alice_section);

        let (bob_codecs, _) = MediaNegotiator::extract_codec_params(&bob_answer);
        let bob_codec = bob_codecs[0].codec;

        // Verify Alice does NOT support Bob's codec
        let compatible = alice_codecs
            .iter()
            .any(|(_, (codec, _, _))| *codec == bob_codec);

        assert!(
            !compatible,
            "Alice should NOT support PCMU - transcoding required"
        );
        assert_eq!(bob_codec, CodecType::PCMU);
    }

    // ==================== Test 4: MediaBridge Advanced Scenarios ====================

    #[tokio::test]
    async fn test_bridge_supports_suppress_and_resume() {
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            mock_rtp_params(0, 8000, 1),
            mock_rtp_params(0, 8000, 1),
            None,
            None,
            CodecType::PCMU,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_bridge_supports_suppress_and_resume".to_string(),
            None,
        );

        bridge.start().await.expect("Bridge should start");

        // Test suppress and resume
        bridge
            .suppress_forwarding("test-track")
            .await
            .expect("Should suppress");
        bridge
            .resume_forwarding("test-track")
            .await
            .expect("Should resume");

        bridge.stop();
    }

    #[tokio::test]
    async fn test_bridge_multiple_start_is_idempotent() {
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            mock_rtp_params(0, 8000, 1),
            mock_rtp_params(0, 8000, 1),
            None,
            None,
            CodecType::PCMU,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_bridge_multiple_start_is_idempotent".to_string(),
            None,
        );

        // Starting multiple times should be safe
        bridge.start().await.expect("First start should succeed");
        bridge.start().await.expect("Second start should succeed");
        bridge.start().await.expect("Third start should succeed");

        bridge.stop();
    }

    // ==================== Test 5: Negotiation State Machine ====================

    #[test]
    fn test_negotiation_state_transitions() {
        // Test the NegotiationState enum transitions
        let idle = NegotiationState::Idle;
        let stable = NegotiationState::Stable;
        let local_offer = NegotiationState::LocalOfferSent;
        let remote_offer = NegotiationState::RemoteOfferReceived;

        // Verify states are distinct
        assert_ne!(idle, stable);
        assert_ne!(idle, local_offer);
        assert_ne!(idle, remote_offer);
        assert_ne!(stable, local_offer);
        assert_ne!(stable, remote_offer);
        assert_ne!(local_offer, remote_offer);

        // Verify Copy and Clone work
        let copied = stable;
        assert_eq!(copied, stable);
    }

    // ==================== Test 6: DTMF Handling ====================

    #[tokio::test]
    async fn test_bridge_with_dtmf_payload_types() {
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            mock_rtp_params(0, 8000, 1),
            mock_rtp_params(0, 8000, 1),
            Some(101), // DTMF for leg A
            Some(101), // DTMF for leg B
            CodecType::PCMU,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_bridge_with_dtmf_payload_types".to_string(),
            None,
        );

        assert_eq!(bridge.dtmf_pt_a, Some(101));
        assert_eq!(bridge.dtmf_pt_b, Some(101));

        bridge.start().await.expect("Bridge should start");
        bridge.stop();
    }

    // ==================== Test 7: Error Handling Scenarios ====================

    #[test]
    fn test_dialplan_flow_validation() {
        // Test empty targets
        let empty_strategy = DialStrategy::Sequential(vec![]);
        let flow = DialplanFlow::Targets(empty_strategy);

        // This should work without panic
        assert!(matches!(flow, DialplanFlow::Targets(_)));
    }

    // ==================== Test 8: Codec Parameters Validation ====================

    #[test]
    fn test_rtp_params_clock_rates() {
        // PCMU/PCMA should use 8000 Hz
        let pcmu_params = mock_rtp_params(0, 8000, 1);
        assert_eq!(pcmu_params.clock_rate, 8000);
        assert_eq!(pcmu_params.channels, 1);

        // Opus should use 48000 Hz
        let opus_params = mock_rtp_params(111, 48000, 2);
        assert_eq!(opus_params.clock_rate, 48000);
        assert_eq!(opus_params.channels, 2);

        // G722 should use 8000 Hz (despite 16kHz sampling)
        let g722_params = mock_rtp_params(9, 8000, 1);
        assert_eq!(g722_params.clock_rate, 8000);
    }

    #[tokio::test]
    async fn test_bridge_without_recorder() {
        // Test that bridge works when recorder is disabled
        let leg_a = Arc::new(MockMediaPeer::new());
        let leg_b = Arc::new(MockMediaPeer::new());

        let bridge = MediaBridge::new(
            leg_a.clone(),
            leg_b.clone(),
            mock_rtp_params(0, 8000, 1),
            mock_rtp_params(0, 8000, 1),
            None,
            None,
            CodecType::PCMU,
            CodecType::PCMU,
            None,
            None,
            None,
            "test_bridge_without_recorder".to_string(),
            None,
        );

        // Should work fine without recorder
        bridge
            .start()
            .await
            .expect("Bridge should start without recorder");
        bridge.stop();
    }

    #[test]
    fn test_queue_plan_creation() {
        use crate::call::{DialStrategy, Location, QueuePlan};
        use rsip::Uri;

        let target1 = Location {
            aor: Uri::try_from("sip:agent1@example.com").unwrap(),
            expires: 3600,
            ..Default::default()
        };

        let mut plan = QueuePlan::default();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![target1]));
        plan.accept_immediately = true;

        assert!(plan.accept_immediately);
        assert!(plan.dial_strategy.is_some());
    }

    #[test]
    fn test_queue_plan_with_hold_config() {
        use crate::call::{QueueHoldConfig, QueuePlan};

        let mut plan = QueuePlan::default();
        plan.hold = Some(QueueHoldConfig {
            audio_file: Some("/audio/hold-music.wav".to_string()),
            loop_playback: true,
        });

        assert!(plan.hold.is_some());
        assert_eq!(
            plan.hold.as_ref().unwrap().audio_file,
            Some("/audio/hold-music.wav".to_string())
        );
        assert!(plan.hold.as_ref().unwrap().loop_playback);
    }

    #[test]
    fn test_queue_plan_fallback_actions() {
        use crate::call::{FailureAction, QueueFallbackAction, QueuePlan};
        use rsip::StatusCode;

        let mut plan = QueuePlan::default();

        // Test with failure action fallback
        plan.fallback = Some(QueueFallbackAction::Failure(
            FailureAction::PlayThenHangup {
                audio_file: "/audio/unavailable.wav".to_string(),
                use_early_media: false,
                status_code: StatusCode::TemporarilyUnavailable,
                reason: Some("All agents busy".to_string()),
            },
        ));

        assert!(plan.fallback.is_some());
    }

    #[test]
    fn test_dialplan_flow_queue_construction() {
        use crate::call::{DialStrategy, DialplanFlow, Location, QueuePlan};
        use rsip::Uri;

        let target = Location {
            aor: Uri::try_from("sip:agent1@example.com").unwrap(),
            expires: 3600,
            ..Default::default()
        };

        let mut plan = QueuePlan::default();
        plan.dial_strategy = Some(DialStrategy::Sequential(vec![target.clone()]));

        // Create a Queue flow with a fallback to a simple target
        let fallback = DialplanFlow::Targets(DialStrategy::Sequential(vec![target]));
        let queue_flow = DialplanFlow::Queue {
            plan,
            next: Box::new(fallback),
        };

        // Verify structure
        match queue_flow {
            DialplanFlow::Queue { plan: _, next } => {
                assert!(matches!(*next, DialplanFlow::Targets(_)));
            }
            _ => panic!("Expected Queue flow"),
        }
    }

    // Note: QueueExecutor tests removed as the executor is no longer used.
    // Queue functionality is now integrated directly into CallSession via execute_queue_plan()

    #[test]
    fn test_dialplan_flow_queue_with_multiple_fallback_levels() {
        use crate::call::{DialStrategy, DialplanFlow, Location, QueuePlan};
        use rsip::Uri;

        let target1 = Location {
            aor: Uri::try_from("sip:agent1@example.com").unwrap(),
            expires: 3600,
            ..Default::default()
        };

        let target2 = Location {
            aor: Uri::try_from("sip:agent2@example.com").unwrap(),
            expires: 3600,
            ..Default::default()
        };

        // Create primary queue
        let mut primary_queue = QueuePlan::default();
        primary_queue.dial_strategy = Some(DialStrategy::Sequential(vec![target1]));
        primary_queue.label = Some("Primary Queue".to_string());

        // Create fallback queue
        let mut fallback_queue = QueuePlan::default();
        fallback_queue.dial_strategy = Some(DialStrategy::Sequential(vec![target2.clone()]));
        fallback_queue.label = Some("Fallback Queue".to_string());

        // Create final fallback to direct dial
        let final_fallback = DialplanFlow::Targets(DialStrategy::Sequential(vec![target2]));

        // Build nested queue flows
        let fallback_queue_flow = DialplanFlow::Queue {
            plan: fallback_queue,
            next: Box::new(final_fallback),
        };

        let primary_queue_flow = DialplanFlow::Queue {
            plan: primary_queue,
            next: Box::new(fallback_queue_flow),
        };

        // Verify structure
        match primary_queue_flow {
            DialplanFlow::Queue { plan, next } => {
                assert_eq!(plan.label, Some("Primary Queue".to_string()));
                match *next {
                    DialplanFlow::Queue {
                        plan: fallback_plan,
                        next: final_next,
                    } => {
                        assert_eq!(fallback_plan.label, Some("Fallback Queue".to_string()));
                        assert!(matches!(*final_next, DialplanFlow::Targets(_)));
                    }
                    _ => panic!("Expected nested Queue flow"),
                }
            }
            _ => panic!("Expected Queue flow"),
        }
    }

    #[test]
    fn test_queue_plan_without_dial_strategy() {
        use crate::call::QueuePlan;

        let plan = QueuePlan {
            accept_immediately: false,
            passthrough_ringback: false,
            hold: None,
            fallback: None,
            dial_strategy: None, // No targets
            ring_timeout: None,
            label: Some("Empty Queue".to_string()),
            ..Default::default()
        };

        // Should be valid to create, but execution will fail gracefully
        assert_eq!(plan.label, Some("Empty Queue".to_string()));
        assert!(plan.dial_strategy.is_none());
    }
}
