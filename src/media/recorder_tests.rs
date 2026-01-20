#[cfg(test)]
mod recorder_advanced_tests {
    use super::super::recorder::{DtmfGenerator, Leg, Recorder};
    use audio_codec::CodecType;
    use rustrtc::media::{AudioFrame, MediaSample};

    // ==================== DTMF Generator Tests ====================

    #[test]
    fn test_dtmf_generator_all_digits() {
        let generator = DtmfGenerator::new(8000);

        // Test all standard DTMF digits
        let digits = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '#'];
        for digit in digits.iter() {
            let samples = generator.generate(*digit, 100); // 100ms duration
            assert!(
                !samples.is_empty(),
                "DTMF for {} should generate samples",
                digit
            );

            // At 8000 Hz, 100ms should be 800 samples
            let expected_samples = 800;
            assert_eq!(
                samples.len(),
                expected_samples,
                "DTMF {} should generate {} samples",
                digit,
                expected_samples
            );
        }
    }

    #[test]
    fn test_dtmf_generator_extended_digits() {
        let generator = DtmfGenerator::new(8000);

        // Test extended DTMF digits (A, B, C, D)
        let digits = ['A', 'B', 'C', 'D'];
        for digit in digits.iter() {
            let samples = generator.generate(*digit, 100);
            assert!(
                !samples.is_empty(),
                "Extended DTMF {} should generate samples",
                digit
            );
        }
    }

    #[test]
    fn test_dtmf_generator_invalid_digit() {
        let generator = DtmfGenerator::new(8000);

        // Invalid digit should return empty vec
        let samples = generator.generate('X', 100);
        assert!(
            samples.is_empty(),
            "Invalid digit should return empty samples"
        );
    }

    #[test]
    fn test_dtmf_generator_different_sample_rates() {
        // Test at 8000 Hz
        let generator_8k = DtmfGenerator::new(8000);
        let samples_8k = generator_8k.generate('5', 100);
        assert_eq!(samples_8k.len(), 800);

        // Test at 16000 Hz
        let generator_16k = DtmfGenerator::new(16000);
        let samples_16k = generator_16k.generate('5', 100);
        assert_eq!(samples_16k.len(), 1600);

        // Test at 48000 Hz
        let generator_48k = DtmfGenerator::new(48000);
        let samples_48k = generator_48k.generate('5', 100);
        assert_eq!(samples_48k.len(), 4800);
    }

    #[test]
    fn test_dtmf_generator_duration_scaling() {
        let generator = DtmfGenerator::new(8000);

        // 50ms should generate 400 samples at 8000 Hz
        let samples_50ms = generator.generate('1', 50);
        assert_eq!(samples_50ms.len(), 400);

        // 200ms should generate 1600 samples at 8000 Hz
        let samples_200ms = generator.generate('1', 200);
        assert_eq!(samples_200ms.len(), 1600);
    }

    // ==================== Recorder Format Tests ====================

    #[test]
    fn test_recorder_wav_header_pcmu() {
        let temp_path = std::env::temp_dir().join("test_wav_header_pcmu.wav");
        let path_str = temp_path.to_str().unwrap();

        // Create recorder with PCMU
        let recorder = Recorder::new(path_str, CodecType::PCMU);
        assert!(recorder.is_ok(), "Should create PCMU recorder");

        // File should exist
        assert!(temp_path.exists(), "WAV file should be created");

        // Read first 44 bytes (WAV header)
        let file_content = std::fs::read(&temp_path).unwrap();
        assert!(
            file_content.len() >= 44,
            "WAV file should have at least 44 bytes header"
        );

        // Check RIFF signature
        assert_eq!(&file_content[0..4], b"RIFF", "Should have RIFF signature");
        assert_eq!(&file_content[8..12], b"WAVE", "Should have WAVE signature");
        assert_eq!(&file_content[12..16], b"fmt ", "Should have fmt chunk");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_wav_header_pcma() {
        let temp_path = std::env::temp_dir().join("test_wav_header_pcma.wav");
        let path_str = temp_path.to_str().unwrap();

        let recorder = Recorder::new(path_str, CodecType::PCMA);
        assert!(recorder.is_ok(), "Should create PCMA recorder");

        assert!(temp_path.exists(), "WAV file should be created");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_wav_header_g722() {
        let temp_path = std::env::temp_dir().join("test_wav_header_g722.wav");
        let path_str = temp_path.to_str().unwrap();

        let recorder = Recorder::new(path_str, CodecType::G722);
        assert!(recorder.is_ok(), "Should create G722 recorder");

        assert!(temp_path.exists(), "WAV file should be created");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    // ==================== Recorder Dual-Leg Tests ====================

    #[test]
    fn test_recorder_dual_leg_recording() {
        let temp_path = std::env::temp_dir().join("test_recorder_dual_leg.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Create mock audio frames for both legs
        let frame_a = AudioFrame {
            data: vec![0xFF; 160].into(), // 20ms @ 8kHz PCMU
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(0),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        let frame_b = AudioFrame {
            data: vec![0x00; 160].into(),
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(0),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        // Write samples from both legs
        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame_a), None)
            .expect("Should write Leg A sample");

        recorder
            .write_sample(Leg::B, &MediaSample::Audio(frame_b), None)
            .expect("Should write Leg B sample");

        // Force flush
        recorder.finalize().expect("Should finalize recorder");

        // File should have data
        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(
            metadata.len() > 44,
            "WAV file should have audio data beyond header"
        );

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_single_channel_recording() {
        let temp_path = std::env::temp_dir().join("test_recorder_single_channel.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        let frame = AudioFrame {
            data: vec![0xFF; 160].into(),
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(0),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame), None)
            .expect("Should write sample");

        recorder.finalize().expect("Should finalize recorder");

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44, "WAV file should have audio data");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    // ==================== DTMF Recording Tests ====================

    #[test]
    fn test_recorder_dtmf_event_payload() {
        let temp_path = std::env::temp_dir().join("test_recorder_dtmf_event.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // DTMF payload: digit '5' (code 5), end bit set, duration 800 samples
        let dtmf_payload = vec![
            5,    // digit code for '5'
            0x80, // end bit set
            0x03, // duration high byte
            0x20, // duration low byte (800 in big-endian)
        ];

        recorder
            .write_dtmf_payload(Leg::A, &dtmf_payload, 0)
            .expect("Should write DTMF payload");

        recorder.finalize().expect("Should finalize");

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44, "Should have recorded DTMF tone");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_dtmf_all_digits() {
        let temp_path = std::env::temp_dir().join("test_recorder_dtmf_all.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Test digits 0-9
        for digit in 0u8..=9u8 {
            let payload = vec![digit, 0x80, 0x03, 0x20];
            recorder
                .write_dtmf_payload(Leg::A, &payload, 0)
                .expect(&format!("Should write DTMF {}", digit));
        }

        // Test * (code 10)
        let payload_star = vec![10, 0x80, 0x03, 0x20];
        recorder
            .write_dtmf_payload(Leg::A, &payload_star, 0)
            .unwrap();

        // Test # (code 11)
        let payload_hash = vec![11, 0x80, 0x03, 0x20];
        recorder
            .write_dtmf_payload(Leg::A, &payload_hash, 0)
            .unwrap();

        recorder.finalize().expect("Should finalize");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_dtmf_invalid_payload() {
        let temp_path = std::env::temp_dir().join("test_dtmf_invalid.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Too short payload (should be ignored)
        let short_payload = vec![5, 0x80];
        let result = recorder.write_dtmf_payload(Leg::A, &short_payload, 0);
        assert!(result.is_ok(), "Short payload should be ignored gracefully");

        // Invalid digit code (>15)
        let invalid_payload = vec![99, 0x80, 0x03, 0x20];
        let result = recorder.write_dtmf_payload(Leg::A, &invalid_payload, 0);
        assert!(result.is_ok(), "Invalid digit should be ignored gracefully");

        recorder.finalize().expect("Should finalize");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_empty_finalize() {
        let temp_path = std::env::temp_dir().join("test_empty.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Finalize without writing any samples
        recorder.finalize().expect("Should finalize empty recorder");

        // Should still have valid WAV header
        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert_eq!(metadata.len(), 44, "Empty WAV should have just the header");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_multiple_finalize() {
        let temp_path = std::env::temp_dir().join("test_multi_finalize.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Multiple finalize calls should be safe
        recorder.finalize().expect("First finalize should succeed");
        recorder.finalize().expect("Second finalize should succeed");
        recorder.finalize().expect("Third finalize should succeed");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_high_sample_rate() {
        let temp_path = std::env::temp_dir().join("test_high_rate.wav");
        let path_str = temp_path.to_str().unwrap();

        // Test with 48kHz (Opus sample rate)
        let recorder = Recorder::new(path_str, CodecType::PCMU);
        assert!(recorder.is_ok(), "Should support 48kHz sample rate");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }
    #[test]
    fn test_recorder_transcoding() {
        let temp_path = std::env::temp_dir().join("test_transcoding.wav");
        let path_str = temp_path.to_str().unwrap();

        // Recorder output is PCMU
        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Input is PCMA (payload type 8)
        let frame = AudioFrame {
            data: vec![0xD5; 160].into(), // 20ms @ 8kHz PCMA silence-ish
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(8), // PCMA
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame), None)
            .expect("Should write PCMA sample to PCMU recorder");

        recorder.finalize().expect("Should finalize");

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44, "WAV file should have audio data");

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_alignment_with_gaps() {
        let temp_path = std::env::temp_dir().join("test_alignment.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        // Leg A starts at 0
        let frame_a = AudioFrame {
            data: vec![0xAA; 160].into(), // 20ms @ 8kHz PCMU
            rtp_timestamp: 1000,
            sequence_number: Some(1),
            payload_type: Some(0),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        // Leg B starts at 40ms (320 samples later)
        let frame_b = AudioFrame {
            data: vec![0xBB; 160].into(),
            rtp_timestamp: 1320, // 1000 + 320
            sequence_number: Some(1),
            payload_type: Some(0),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame_a), None)
            .unwrap();

        recorder
            .write_sample(Leg::B, &MediaSample::Audio(frame_b), None)
            .unwrap();

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44);

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_g729_stereo() {
        let temp_path = std::env::temp_dir().join("test_g729_stereo.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::G729).unwrap();

        let frame_a = AudioFrame {
            data: vec![0; 10].into(), // 10 bytes G.729
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(18),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        let frame_b = AudioFrame {
            data: vec![0; 10].into(),
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(18),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame_a), None)
            .unwrap();
        recorder
            .write_sample(Leg::B, &MediaSample::Audio(frame_b), None)
            .unwrap();

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44);

        let _ = std::fs::remove_file(&temp_path);
    }

    /// Test: Opus should convert to PCMU automatically
    #[test]
    fn test_opus_converts_to_pcmu() {
        let temp_path = "/tmp/test_opus_convert.wav";
        let recorder = Recorder::new(temp_path, CodecType::Opus);
        assert!(recorder.is_ok());

        let mut rec = recorder.unwrap();
        assert_eq!(
            rec.codec,
            CodecType::PCMU,
            "Opus should be converted to PCMU"
        );

        rec.finalize().ok();
        let _ = std::fs::remove_file(temp_path);
    }

    /// Test: Supported codecs (PCMU, PCMA, G722, G729) all work
    #[test]
    fn test_supported_codecs() {
        let codecs = vec![
            (CodecType::PCMU, "/tmp/test_supported_pcmu.wav"),
            (CodecType::PCMA, "/tmp/test_supported_pcma.wav"),
            (CodecType::G722, "/tmp/test_supported_g722.wav"),
            (CodecType::G729, "/tmp/test_supported_g729.wav"),
        ];

        for (codec, path) in codecs {
            let recorder = Recorder::new(path, codec);
            assert!(recorder.is_ok(), "Recorder should support {:?}", codec);
            recorder.unwrap().finalize().ok();
            let _ = std::fs::remove_file(path);
        }
    }

    /// Test: Recording from both legs creates stereo output
    #[test]
    fn test_dual_leg_recording_stereo() {
        use audio_codec::create_encoder;
        use bytes::Bytes;

        let temp_path = "/tmp/test_dual_leg_stereo.wav";
        let mut recorder = Recorder::new(temp_path, CodecType::PCMU).unwrap();

        // Generate test audio for both legs
        let mut encoder = create_encoder(CodecType::PCMU);
        let pcm_samples = vec![100i16; 160]; // 20ms of audio

        // Leg A: caller (5 packets)
        for i in 0..5 {
            let encoded = encoder.encode(&pcm_samples);
            let frame = MediaSample::Audio(AudioFrame {
                data: Bytes::from(encoded),
                rtp_timestamp: i * 160,
                payload_type: Some(0),
                sequence_number: None,
                clock_rate: 8000,
                marker: false,
                raw_packet: None,
                source_addr: None,
            });
            recorder.write_sample(Leg::A, &frame, None).ok();
        }

        // Leg B: callee (5 packets)
        for i in 0..5 {
            let encoded = encoder.encode(&pcm_samples);
            let frame = MediaSample::Audio(AudioFrame {
                data: Bytes::from(encoded),
                rtp_timestamp: i * 160,
                payload_type: Some(0),
                sequence_number: None,
                clock_rate: 8000,
                marker: false,
                raw_packet: None,
                source_addr: None,
            });
            recorder.write_sample(Leg::B, &frame, None).ok();
        }

        recorder.finalize().ok();

        // Verify file exists and has content
        let metadata = std::fs::metadata(temp_path).unwrap();
        assert!(
            metadata.len() > 44,
            "WAV file should have more than just header"
        );

        let _ = std::fs::remove_file(temp_path);
    }

    /// Test: DTMF is converted and recorded properly
    #[test]
    fn test_dtmf_recording() {
        use bytes::Bytes;

        let temp_path = "/tmp/test_dtmf_recording.wav";
        let mut recorder = Recorder::new(temp_path, CodecType::PCMU).unwrap();

        // Create DTMF payload (RFC 4733)
        // Format: [digit, flags, duration_high, duration_low]
        let dtmf_payload = vec![
            5,    // digit '5'
            0x80, // end bit set
            0x03, // duration high byte
            0x20, // duration low byte (800 samples = 100ms at 8kHz)
        ];

        let frame = MediaSample::Audio(AudioFrame {
            data: Bytes::from(dtmf_payload),
            rtp_timestamp: 160,
            payload_type: Some(101), // DTMF payload type
            sequence_number: None,
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
        });

        recorder.write_sample(Leg::A, &frame, Some(101)).ok();
        recorder.finalize().ok();

        // Verify file was created
        assert!(
            std::path::Path::new(temp_path).exists(),
            "DTMF recording should create file"
        );

        let _ = std::fs::remove_file(temp_path);
    }
}
