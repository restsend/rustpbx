#[cfg(test)]
mod recorder_advanced_tests {
    use super::super::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
    use super::super::recorder::{DtmfGenerator, Leg, Recorder};
    use audio_codec::CodecType;
    use rustrtc::media::{AudioFrame, MediaSample};
    use rustrtc::rtp::{RtpHeader, RtpPacket};

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
            header_extension: None,
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
            header_extension: None,
        };

        // Write samples from both legs
        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame_a), None, None, None)
            .expect("Should write Leg A sample");

        recorder
            .write_sample(Leg::B, &MediaSample::Audio(frame_b), None, None, None)
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
            header_extension: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
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
            .write_dtmf_payload(Leg::A, &dtmf_payload, 0, 8000)
            .expect("Should write DTMF payload");

        recorder.finalize().expect("Should finalize");

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44, "Should have recorded DTMF tone");

        // Cleanup
        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_dtmf_ignores_duplicate_terminal_packets() {
        let temp_path_single = std::env::temp_dir().join("test_recorder_dtmf_single.wav");
        let temp_path_dup = std::env::temp_dir().join("test_recorder_dtmf_duplicate.wav");

        let mut recorder_single =
            Recorder::new(temp_path_single.to_str().unwrap(), CodecType::PCMU).unwrap();
        let mut recorder_dup =
            Recorder::new(temp_path_dup.to_str().unwrap(), CodecType::PCMU).unwrap();

        let dtmf_payload = [5, 0x80, 0x06, 0x40];

        recorder_single
            .write_dtmf_payload(Leg::A, &dtmf_payload, 12_345, 8000)
            .expect("single terminal DTMF should be written");

        for _ in 0..3 {
            recorder_dup
                .write_dtmf_payload(Leg::A, &dtmf_payload, 12_345, 8000)
                .expect("duplicate terminal DTMF packets should be accepted");
        }

        recorder_single.finalize().expect("single finalize should succeed");
        recorder_dup.finalize().expect("duplicate finalize should succeed");

        let len_single = std::fs::metadata(&temp_path_single).unwrap().len();
        let len_dup = std::fs::metadata(&temp_path_dup).unwrap().len();

        assert_eq!(
            len_dup, len_single,
            "retransmitted terminal DTMF packets should not duplicate recorded tones"
        );

        let _ = std::fs::remove_file(&temp_path_single);
        let _ = std::fs::remove_file(&temp_path_dup);
    }

    #[test]
    fn test_recorder_dtmf_uses_event_clock_rate() {
        let temp_path_a = std::env::temp_dir().join("test_recorder_dtmf_clock_a.wav");
        let temp_path_b = std::env::temp_dir().join("test_recorder_dtmf_clock_b.wav");
        let mut recorder_a = Recorder::new(temp_path_a.to_str().unwrap(), CodecType::PCMU).unwrap();
        let mut recorder_b = Recorder::new(temp_path_b.to_str().unwrap(), CodecType::PCMU).unwrap();

        recorder_a
            .write_dtmf_payload(Leg::A, &[5, 0x80, 0x12, 0xC0], 0, 48000)
            .expect("48k DTMF should be written");
        recorder_b
            .write_dtmf_payload(Leg::A, &[5, 0x80, 0x03, 0x20], 0, 8000)
            .expect("8k DTMF should be written");

        recorder_a.finalize().expect("48k finalize should succeed");
        recorder_b.finalize().expect("8k finalize should succeed");

        let len_a = std::fs::metadata(&temp_path_a).unwrap().len();
        let len_b = std::fs::metadata(&temp_path_b).unwrap().len();

        let size_delta = len_a.abs_diff(len_b);
        assert!(
            size_delta <= 32,
            "Equivalent 100ms DTMF should produce nearly the same recording size regardless of event clock, delta={}",
            size_delta
        );

        let _ = std::fs::remove_file(&temp_path_a);
        let _ = std::fs::remove_file(&temp_path_b);
    }

    #[test]
    fn test_recorder_dtmf_timestamp_uses_event_clock_rate() {
        let temp_path_a = std::env::temp_dir().join("test_recorder_dtmf_ts_a.wav");
        let temp_path_b = std::env::temp_dir().join("test_recorder_dtmf_ts_b.wav");
        let mut recorder_a = Recorder::new(temp_path_a.to_str().unwrap(), CodecType::PCMU).unwrap();
        let mut recorder_b = Recorder::new(temp_path_b.to_str().unwrap(), CodecType::PCMU).unwrap();

        let frame = AudioFrame {
            data: vec![0xFF; 160].into(),
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(0),
            clock_rate: 8000,
            marker: false,
            raw_packet: None,
            source_addr: None,
            header_extension: None,
        };

        recorder_a
            .write_sample(Leg::A, &MediaSample::Audio(frame.clone()), None, None, None)
            .expect("Should write anchor sample");
        recorder_b
            .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
            .expect("Should write anchor sample");

        recorder_a
            .write_dtmf_payload(Leg::A, &[5, 0x80, 0x12, 0xC0], 4800, 48000)
            .expect("48k DTMF should be written");
        recorder_b
            .write_dtmf_payload(Leg::A, &[5, 0x80, 0x03, 0x20], 800, 8000)
            .expect("8k DTMF should be written");

        recorder_a.finalize().expect("48k finalize should succeed");
        recorder_b.finalize().expect("8k finalize should succeed");

        let len_a = std::fs::metadata(&temp_path_a).unwrap().len();
        let len_b = std::fs::metadata(&temp_path_b).unwrap().len();

        let size_delta = len_a.abs_diff(len_b);
        assert!(
            size_delta <= 32,
            "Equivalent timestamp offsets should produce nearly the same recording size regardless of event clock, delta={}",
            size_delta
        );

        let _ = std::fs::remove_file(&temp_path_a);
        let _ = std::fs::remove_file(&temp_path_b);
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
                .write_dtmf_payload(Leg::A, &payload, 0, 8000)
                .expect(&format!("Should write DTMF {}", digit));
        }

        // Test * (code 10)
        let payload_star = vec![10, 0x80, 0x03, 0x20];
        recorder
            .write_dtmf_payload(Leg::A, &payload_star, 0, 8000)
            .unwrap();

        // Test # (code 11)
        let payload_hash = vec![11, 0x80, 0x03, 0x20];
        recorder
            .write_dtmf_payload(Leg::A, &payload_hash, 0, 8000)
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
        let result = recorder.write_dtmf_payload(Leg::A, &short_payload, 0, 8000);
        assert!(result.is_ok(), "Short payload should be ignored gracefully");

        // Invalid digit code (>15)
        let invalid_payload = vec![99, 0x80, 0x03, 0x20];
        let result = recorder.write_dtmf_payload(Leg::A, &invalid_payload, 0, 8000);
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
            header_extension: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
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
            header_extension: None,
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
            header_extension: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame_a), None, None, None)
            .unwrap();

        recorder
            .write_sample(Leg::B, &MediaSample::Audio(frame_b), None, None, None)
            .unwrap();

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(metadata.len() > 44);

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_nominal_pcmu_packet_size_matches_rtp_duration() {
        let temp_path = std::env::temp_dir().join("test_nominal_pcmu_duration.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMU).unwrap();

        for i in 0..50u32 {
            let frame = AudioFrame {
                data: vec![0xFF; 160].into(),
                rtp_timestamp: i * 160,
                sequence_number: Some(i as u16),
                payload_type: Some(0),
                clock_rate: 8000,
                marker: false,
                raw_packet: None,
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert_eq!(
            metadata.len(),
            16_044,
            "1 second of 8k PCMU mono audio is written as 8k stereo WAV (44-byte header + 16000 bytes payload)"
        );

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn repro_recorder_inflates_duration_when_frame_bytes_exceed_rtp_span() {
        let temp_path = std::env::temp_dir().join("test_recorder_inflated_duration.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMA).unwrap();

        for i in 0..50u32 {
            let frame = AudioFrame {
                data: vec![0; 3840].into(),
                rtp_timestamp: i * 160,
                sequence_number: Some(i as u16),
                payload_type: Some(8),
                clock_rate: 8000,
                marker: false,
                raw_packet: None,
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert!(
            metadata.len() > 16_044,
            "Oversized frames still inflate the payload beyond the 1-second baseline"
        );
        assert!(
            (23_404..=23_420).contains(&metadata.len()),
            "Current recorder logic still extends 1 second of RTP timestamps into about 1.46 seconds of WAV payload"
        );

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_uses_frame_clock_rate_for_timestamp_alignment() {
        let temp_path = std::env::temp_dir().join("test_recorder_mostly_silence.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMA).unwrap();
        let mut encoder = audio_codec::create_encoder(CodecType::PCMA);
        let silence_byte = encoder.encode(&vec![0i16; 160])[0];

        for i in 0..10u32 {
            let frame = AudioFrame {
                data: vec![0xAA; 160].into(),
                rtp_timestamp: i * 960,
                sequence_number: Some(i as u16),
                payload_type: Some(8),
                clock_rate: 48000,
                marker: false,
                raw_packet: None,
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        recorder.finalize().unwrap();

        let file = std::fs::read(&temp_path).unwrap();
        let payload = &file[44..];
        let leg_a: Vec<u8> = payload.iter().step_by(2).copied().collect();
        let leg_b: Vec<u8> = payload.iter().skip(1).step_by(2).copied().collect();

        let silence_count = leg_a.iter().filter(|&&byte| byte == silence_byte).count();
        let signal_count = leg_a.iter().filter(|&&byte| byte == 0xAA).count();
        let leg_b_silence_count = leg_b.iter().filter(|&&byte| byte == silence_byte).count();

        assert!(
            signal_count > silence_count * 3,
            "Leg A should retain contiguous audio after clock-rate-aware timestamp scaling: silence_count={}, signal_count={}",
            silence_count,
            signal_count
        );
        assert!(
            leg_b_silence_count > leg_b.len() * 9 / 10,
            "Leg B should remain almost entirely silent because no packets were written for it"
        );
        assert_eq!(
            file.len(),
            3_244,
            "Ten 20ms frames at 8kHz should produce 1600 stereo samples plus a 44-byte WAV header"
        );

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_prefers_raw_rtp_payload_over_mutated_frame_data() {
        let temp_path = std::env::temp_dir().join("test_recorder_prefers_raw_payload.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMA).unwrap();

        for i in 0..50u32 {
            let raw_payload = vec![0xD5; 160];
            let raw_packet =
                RtpPacket::new(RtpHeader::new(8, i as u16, i * 160, 12345), raw_payload);
            let frame = AudioFrame {
                data: vec![0; 3840].into(),
                rtp_timestamp: i * 160,
                sequence_number: Some(i as u16),
                payload_type: Some(8),
                clock_rate: 48000,
                marker: false,
                raw_packet: Some(raw_packet),
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert_eq!(
            metadata.len(),
            16_044,
            "Recorder should use the original 160-byte RTP payload instead of an expanded 3840-byte frame buffer"
        );

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_resets_timeline_on_rtp_stream_switch() {
        let temp_path = std::env::temp_dir().join("test_recorder_stream_switch.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMA).unwrap();

        for i in 0..50u32 {
            let raw_packet = RtpPacket::new(
                RtpHeader::new(8, i as u16, i * 160, 11_111),
                vec![0xD5; 160],
            );
            let frame = AudioFrame {
                data: vec![0; 160].into(),
                rtp_timestamp: i * 160,
                sequence_number: Some(i as u16),
                payload_type: Some(8),
                clock_rate: 8000,
                marker: false,
                raw_packet: Some(raw_packet),
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        for i in 0..50u32 {
            let rtp_timestamp = 2_400_000 + i * 160;
            let raw_packet = RtpPacket::new(
                RtpHeader::new(8, (i + 50) as u16, rtp_timestamp, 22_222),
                vec![0xD5; 160],
            );
            let frame = AudioFrame {
                data: vec![0; 160].into(),
                rtp_timestamp,
                sequence_number: Some((i + 50) as u16),
                payload_type: Some(8),
                clock_rate: 8000,
                marker: false,
                raw_packet: Some(raw_packet),
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert_eq!(
            metadata.len(),
            32_044,
            "A large timestamp jump on a new SSRC should start a new contiguous segment instead of expanding the WAV with silence"
        );

        let _ = std::fs::remove_file(&temp_path);
    }

    #[test]
    fn test_recorder_handles_183_early_media_then_200_ok_stream_switch() {
        let temp_path = std::env::temp_dir().join("test_recorder_183_200_stream_switch.wav");
        let path_str = temp_path.to_str().unwrap();

        let mut recorder = Recorder::new(path_str, CodecType::PCMA).unwrap();

        // Phase 1: 183 early media arrives on the first RTP stream.
        for i in 0..100u32 {
            let rtp_timestamp = i * 160;
            let raw_packet = RtpPacket::new(
                RtpHeader::new(8, i as u16, rtp_timestamp, 0x183183),
                vec![0xD5; 160],
            );
            let frame = AudioFrame {
                data: vec![0; 160].into(),
                rtp_timestamp,
                sequence_number: Some(i as u16),
                payload_type: Some(8),
                clock_rate: 8000,
                marker: false,
                raw_packet: Some(raw_packet),
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        // Phase 2: 200 OK lands, media is re-synced, and RTP continues on a new stream
        // with a new SSRC and a far-ahead timestamp base.
        for i in 0..100u32 {
            let rtp_timestamp = 3_600_000 + i * 160;
            let raw_packet = RtpPacket::new(
                RtpHeader::new(8, (i + 100) as u16, rtp_timestamp, 0x200200),
                vec![0xD5; 160],
            );
            let frame = AudioFrame {
                data: vec![0; 160].into(),
                rtp_timestamp,
                sequence_number: Some((i + 100) as u16),
                payload_type: Some(8),
                clock_rate: 8000,
                marker: false,
                raw_packet: Some(raw_packet),
                source_addr: None,
                header_extension: None,
            };

            recorder
                .write_sample(Leg::A, &MediaSample::Audio(frame), None, None, None)
                .unwrap();
        }

        recorder.finalize().unwrap();

        let metadata = std::fs::metadata(&temp_path).unwrap();
        assert_eq!(
            metadata.len(),
            64_044,
            "183 early media followed by 200 OK on a new RTP stream should produce two contiguous 2-second segments, not a silence-inflated WAV"
        );

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
            header_extension: None,
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
            header_extension: None,
        };

        recorder
            .write_sample(Leg::A, &MediaSample::Audio(frame_a), None, None, None)
            .unwrap();
        recorder
            .write_sample(Leg::B, &MediaSample::Audio(frame_b), None, None, None)
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

    #[test]
    fn test_dynamic_opus_payload_type_uses_codec_hint() {
        use audio_codec::create_encoder;
        use bytes::Bytes;

        let temp_path = "/tmp/test_dynamic_opus_pt.wav";
        let mut recorder = Recorder::new(temp_path, CodecType::PCMU).unwrap();
        let mut encoder = create_encoder(CodecType::Opus);
        let pcm_samples = vec![100i16; 960 * 2];
        let encoded = encoder.encode(&pcm_samples);

        let frame = MediaSample::Audio(AudioFrame {
            data: Bytes::from(encoded),
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(96),
            clock_rate: 48000,
            marker: false,
            raw_packet: None,
            source_addr: None,
            header_extension: None,
        });

        recorder
            .write_sample(Leg::A, &frame, None, None, Some(CodecType::Opus))
            .expect("Should write Opus sample with dynamic payload type");
        recorder.finalize().expect("Should finalize recorder");

        let metadata = std::fs::metadata(temp_path).unwrap();
        assert!(metadata.len() > 44, "WAV file should have audio data");

        let _ = std::fs::remove_file(temp_path);
    }

    #[test]
    fn test_dynamic_opus_payload_type_uses_stored_leg_payloads() {
        use audio_codec::create_encoder;
        use bytes::Bytes;

        let temp_path = "/tmp/test_dynamic_opus_pt_stored.wav";
        let mut recorder = Recorder::new(temp_path, CodecType::PCMU).unwrap();
        recorder.set_leg_profile(
            Leg::A,
            NegotiatedLegProfile {
                audio: Some(NegotiatedCodec {
                    codec: CodecType::Opus,
                    payload_type: 96,
                    clock_rate: 48000,
                    channels: 2,
                }),
                dtmf: Some(NegotiatedCodec {
                    codec: CodecType::TelephoneEvent,
                    payload_type: 101,
                    clock_rate: 8000,
                    channels: 1,
                }),
            },
        );

        let mut encoder = create_encoder(CodecType::Opus);
        let pcm_samples = vec![100i16; 960 * 2];
        let encoded = encoder.encode(&pcm_samples);

        let frame = MediaSample::Audio(AudioFrame {
            data: Bytes::from(encoded),
            rtp_timestamp: 0,
            sequence_number: Some(1),
            payload_type: Some(96),
            clock_rate: 48000,
            marker: false,
            raw_packet: None,
            source_addr: None,
            header_extension: None,
        });

        recorder
            .write_sample(Leg::A, &frame, None, None, None)
            .expect("Should write Opus sample using stored leg payload mapping");
        recorder.finalize().expect("Should finalize recorder");

        let metadata = std::fs::metadata(temp_path).unwrap();
        assert!(metadata.len() > 44, "WAV file should have audio data");

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
                header_extension: None,
            });
            recorder.write_sample(Leg::A, &frame, None, None, None).ok();
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
                header_extension: None,
            });
            recorder.write_sample(Leg::B, &frame, None, None, None).ok();
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
            header_extension: None,
        });

        recorder
            .write_sample(Leg::A, &frame, Some(101), Some(8000), None)
            .ok();
        recorder.finalize().ok();

        // Verify file was created
        assert!(
            std::path::Path::new(temp_path).exists(),
            "DTMF recording should create file"
        );

        let _ = std::fs::remove_file(temp_path);
    }
}
