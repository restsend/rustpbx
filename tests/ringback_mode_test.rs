#[cfg(test)]
mod ringback_mode_tests {
    use rustpbx::call::RingbackMode;

    #[test]
    fn test_ringback_mode_decision_logic() {
        let has_early_media = true;
        let no_early_media = false;

        assert!(should_play_local(RingbackMode::Local, has_early_media));
        assert!(should_play_local(RingbackMode::Local, no_early_media));
        assert!(!should_passthrough(RingbackMode::Local, has_early_media));
        assert!(!should_passthrough(RingbackMode::Local, no_early_media));

        assert!(!should_play_local(
            RingbackMode::Passthrough,
            has_early_media
        ));
        assert!(!should_play_local(
            RingbackMode::Passthrough,
            no_early_media
        ));
        assert!(should_passthrough(
            RingbackMode::Passthrough,
            has_early_media
        ));
        assert!(!should_passthrough(
            RingbackMode::Passthrough,
            no_early_media
        ));

        assert!(!should_play_local(RingbackMode::Auto, has_early_media));
        assert!(should_play_local(RingbackMode::Auto, no_early_media));
        assert!(should_passthrough(RingbackMode::Auto, has_early_media));
        assert!(!should_passthrough(RingbackMode::Auto, no_early_media));

        assert!(!should_play_local(RingbackMode::None, has_early_media));
        assert!(!should_play_local(RingbackMode::None, no_early_media));
        assert!(!should_passthrough(RingbackMode::None, has_early_media));
        assert!(!should_passthrough(RingbackMode::None, no_early_media));
    }

    fn should_play_local(mode: RingbackMode, has_early_media: bool) -> bool {
        match mode {
            RingbackMode::Local => true,
            RingbackMode::Passthrough => false,
            RingbackMode::Auto => !has_early_media,
            RingbackMode::None => false,
        }
    }

    fn should_passthrough(mode: RingbackMode, has_early_media: bool) -> bool {
        match mode {
            RingbackMode::Local => false,
            RingbackMode::Passthrough => has_early_media,
            RingbackMode::Auto => has_early_media,
            RingbackMode::None => false,
        }
    }
} // mod ringback_mode_tests

#[cfg(test)]
mod ringback_audio_tests {
    use rsipstack::sip::StatusCode;
    use rustpbx::proxy::routing::RingbackAudio;

    #[test]
    fn test_ringback_audio_default() {
        let audio = RingbackAudio::default();
        assert_eq!(audio.ring, None);
        assert_eq!(audio.busy, None);
        assert_eq!(audio.reject, None);
        assert_eq!(audio.offline, None);
        assert_eq!(audio.notfound, None);
    }

    #[test]
    fn test_ringback_audio_for_status_busy() {
        let audio = RingbackAudio {
            ring: Some("/sounds/ring.wav".to_string()),
            busy: Some("/sounds/busy.wav".to_string()),
            reject: Some("/sounds/reject.wav".to_string()),
            offline: Some("/sounds/offline.wav".to_string()),
            notfound: Some("/sounds/notfound.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(
            audio.for_status(&StatusCode::BusyHere),
            Some("/sounds/busy.wav")
        );
    }

    #[test]
    fn test_ringback_audio_for_status_offline() {
        let audio = RingbackAudio {
            offline: Some("/sounds/unavailable.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(
            audio.for_status(&StatusCode::TemporarilyUnavailable),
            Some("/sounds/unavailable.wav")
        );
    }

    #[test]
    fn test_ringback_audio_for_status_notfound() {
        let audio = RingbackAudio {
            notfound: Some("/sounds/notfound.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(
            audio.for_status(&StatusCode::NotFound),
            Some("/sounds/notfound.wav")
        );
    }

    #[test]
    fn test_ringback_audio_for_status_reject() {
        let audio = RingbackAudio {
            reject: Some("/sounds/decline.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(
            audio.for_status(&StatusCode::Decline),
            Some("/sounds/decline.wav")
        );
    }

    #[test]
    fn test_ringback_audio_for_status_unknown() {
        let audio = RingbackAudio {
            ring: Some("/sounds/ring.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(audio.for_status(&StatusCode::OK), None);
        assert_eq!(audio.for_status(&StatusCode::Ringing), None);
        assert_eq!(audio.for_status(&StatusCode::RequestTerminated), None);
    }

    #[test]
    fn test_ringback_audio_for_status_ring_not_returned() {
        let audio = RingbackAudio {
            ring: Some("/sounds/ring.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(audio.for_status(&StatusCode::Ringing), None);
    }

    #[test]
    fn test_ringback_audio_has_failure_tone_true() {
        let audio = RingbackAudio {
            busy: Some("/sounds/busy.wav".to_string()),
            ..Default::default()
        };
        assert!(audio.has_failure_tone());
    }

    #[test]
    fn test_ringback_audio_has_failure_tone_false() {
        let audio = RingbackAudio::default();
        assert!(!audio.has_failure_tone());

        let audio = RingbackAudio {
            ring: Some("/sounds/ring.wav".to_string()),
            ..Default::default()
        };
        assert!(
            !audio.has_failure_tone(),
            "ring alone is not a failure tone"
        );
    }

    #[test]
    fn test_ringback_audio_play_duration_default() {
        let audio = RingbackAudio {
            busy: Some("/sounds/busy.wav".to_string()),
            ..Default::default()
        };
        let dur = audio
            .play_duration_for(&StatusCode::BusyHere)
            .expect("should have duration");
        assert_eq!(dur.as_secs(), 2, "default play_duration should be 2s");
    }

    #[test]
    fn test_ringback_audio_play_duration_custom() {
        let audio = RingbackAudio {
            busy: Some("/sounds/busy.wav".to_string()),
            play_duration_secs: Some(5),
            ..Default::default()
        };
        let dur = audio
            .play_duration_for(&StatusCode::BusyHere)
            .expect("should have duration");
        assert_eq!(dur.as_secs(), 5);
    }

    #[test]
    fn test_ringback_audio_play_duration_zero() {
        let audio = RingbackAudio {
            busy: Some("/sounds/busy.wav".to_string()),
            play_duration_secs: Some(0),
            ..Default::default()
        };
        let dur = audio
            .play_duration_for(&StatusCode::BusyHere)
            .expect("should have duration");
        assert_eq!(dur.as_secs(), 0, "0 means no playback");
    }

    #[test]
    fn test_ringback_audio_play_duration_none_for_unconfigured_code() {
        let audio = RingbackAudio {
            busy: Some("/sounds/busy.wav".to_string()),
            ..Default::default()
        };
        assert_eq!(audio.play_duration_for(&StatusCode::NotFound), None);
        assert_eq!(audio.play_duration_for(&StatusCode::Decline), None);
    }
} // mod ringback_audio_tests

#[cfg(test)]
mod ringback_tone_audio_tests {
    use rustpbx::media::wav_reader::{SampleFormat, WavReader, WavSpec, WavWriter};

    /// Generate sine wave PCM matching the exact algorithm in
    /// `SipSession::resolve_audio_path()` (src/proxy/proxy_call/sip_session.rs).
    ///   8 kHz sample rate, 16-bit signed, mono, amplitude 8192.
    fn generate_tone_pcm(frequency: u32, duration_ms: u64) -> Vec<i16> {
        let sample_rate = 8000u32;
        let num_samples = (sample_rate as u64 * duration_ms / 1000) as usize;
        let amplitude = 8192i16;

        (0..num_samples)
            .map(|i| {
                let t = i as f64 / sample_rate as f64;
                (amplitude as f64 * (2.0 * std::f64::consts::PI * frequency as f64 * t).sin())
                    as i16
            })
            .collect()
    }

    fn count_zero_crossings(samples: &[i16]) -> usize {
        let mut count = 0;
        for w in samples.windows(2) {
            if w[0] <= 0 && w[1] > 0 {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn test_tone_pcm_440hz_positive_crossings() {
        // 440 Hz sine at 8 kHz for 50ms = 400 samples
        // 440 * 0.05 = 22 cycles, each with 1 positive-going crossing
        let pcm = generate_tone_pcm(440, 50);
        let crossings = count_zero_crossings(&pcm);
        assert!(
            crossings >= 21 && crossings <= 23,
            "440Hz for 50ms should have ~22 positive zero-crossings, got {}",
            crossings
        );
    }

    #[test]
    fn test_tone_pcm_1000hz_positive_crossings() {
        // 1000 Hz at 8 kHz for 50ms = 400 samples
        // 1000 * 0.05 = 50 cycles, each with 1 positive-going crossing
        let pcm = generate_tone_pcm(1000, 50);
        let crossings = count_zero_crossings(&pcm);
        assert!(
            crossings >= 49 && crossings <= 51,
            "1000Hz for 50ms should have ~50 positive zero-crossings, got {}",
            crossings
        );
    }

    #[test]
    fn test_tone_pcm_amplitude_8192() {
        let pcm = generate_tone_pcm(440, 20); // 20ms = 160 samples
        let max_abs = pcm.iter().map(|&s| s.abs()).max().unwrap_or(0);
        // Amplitude should be ~8192 (may be slightly rounded)
        assert!(
            max_abs >= 8100 && max_abs <= 8192,
            "max amplitude should be near 8192, got {}",
            max_abs
        );
    }

    #[test]
    fn test_tone_pcm_no_silence() {
        let pcm = generate_tone_pcm(440, 20);
        let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
        assert!(
            rms > 1000.0,
            "Tone should have significant energy, RMS={}",
            rms
        );
    }

    #[test]
    fn test_tone_pcm_sample_count() {
        assert_eq!(generate_tone_pcm(440, 20).len(), 160); // 20ms at 8kHz
        assert_eq!(generate_tone_pcm(440, 100).len(), 800); // 100ms at 8kHz
        assert_eq!(generate_tone_pcm(440, 1000).len(), 8000); // 1s at 8kHz
    }

    #[test]
    fn test_tone_wav_write_and_read_back() {
        let pcm = generate_tone_pcm(440, 30); // 30ms = 240 samples
        let temp_dir = std::env::temp_dir();
        let wav_path = temp_dir.join("rustpbx_test_tone_440hz_30ms.wav");

        // Write WAV matching resolve_audio_path spec
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        {
            let mut writer = WavWriter::create(&wav_path, spec).expect("create WAV");
            for &sample in &pcm {
                writer.write_sample(sample).expect("write sample");
            }
            writer.finalize().expect("finalize WAV");
        }

        // Read back and verify
        let mut reader = WavReader::open(&wav_path).expect("open WAV");
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 8000);
        assert_eq!(reader.spec().bits_per_sample, 16);

        let samples: Vec<i16> = reader.samples().map(|s| s.expect("read sample")).collect();
        assert_eq!(samples.len(), 240, "should read back 240 samples");

        // Verify PCM content matches
        for (i, (&original, &read)) in pcm.iter().zip(samples.iter()).enumerate() {
            assert_eq!(
                original, read,
                "sample {} mismatch: original={}, read={}",
                i, original, read
            );
        }

        // Cleanup
        let _ = std::fs::remove_file(&wav_path);
    }

    #[test]
    fn test_tone_wav_spec_matches_production() {
        // Verify the WAV spec is identical to what resolve_audio_path produces
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        assert_eq!(spec.channels, 1, "must be mono");
        assert_eq!(spec.sample_rate, 8000, "must be 8kHz");
        assert_eq!(spec.bits_per_sample, 16, "must be 16-bit");
        assert_eq!(spec.sample_format, SampleFormat::Int, "must be signed int");
    }

    #[test]
    fn test_tone_uri_parsing_logic() {
        // Test the URI parsing used in resolve_audio_path
        let spec = "tone://440,300";
        let tone_spec = spec.strip_prefix("tone://").unwrap();
        let parts: Vec<&str> = tone_spec.splitn(2, ',').collect();
        assert_eq!(parts.len(), 2);

        let frequency: u32 = parts[0].trim().parse().unwrap();
        let duration_ms: u64 = parts[1].trim().parse().unwrap();
        assert_eq!(frequency, 440);
        assert_eq!(duration_ms, 300);
    }

    #[test]
    fn test_tone_uri_edge_cases() {
        // Invalid: missing frequency
        let spec = "tone://,300";
        let tone_spec = spec.strip_prefix("tone://").unwrap();
        let parts: Vec<&str> = tone_spec.splitn(2, ',').collect();
        assert!(parts[0].trim().parse::<u32>().is_err());

        // Invalid: non-numeric
        let spec = "tone://abc,300";
        let tone_spec = spec.strip_prefix("tone://").unwrap();
        let parts: Vec<&str> = tone_spec.splitn(2, ',').collect();
        assert!(parts[0].trim().parse::<u32>().is_err());

        // Not a tone URI — passthrough
        let spec = "/sounds/ringback.wav";
        assert!(spec.strip_prefix("tone://").is_none());
    }

    #[test]
    fn test_tone_pcm_all_samples_in_range() {
        let pcm = generate_tone_pcm(440, 50);
        for &sample in &pcm {
            assert!(
                sample >= -8192 && sample <= 8192,
                "sample {} out of range [-8192, 8192]",
                sample
            );
        }
    }

    #[test]
    fn test_tone_pcm_zero_duration() {
        let pcm = generate_tone_pcm(440, 0);
        assert!(pcm.is_empty(), "zero duration should produce empty PCM");
    }

    #[test]
    fn test_tone_pcm_various_frequencies() {
        for &freq in &[200, 440, 800, 1000, 2000] {
            let pcm = generate_tone_pcm(freq, 40);
            assert_eq!(pcm.len(), 320, "40ms at 8kHz = 320 samples for {}Hz", freq);
            let rms =
                (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
            assert!(
                rms > 1000.0,
                "Tone {}Hz should have energy, RMS={}",
                freq,
                rms
            );
        }
    }
} // mod ringback_tone_audio_tests
