#[cfg(test)]
mod audio_feature_tests {
    use audio_codec::CodecType;
    use rustpbx::media::{FileTrack, Track, audio_source::AudioSourceManager};
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_file_track_supports_multiple_formats() {
        let temp_dir = std::env::temp_dir();

        let wav_path = temp_dir.join("test_audio.wav");
        let mp3_path = temp_dir.join("test_audio.mp3");
        let raw_path = temp_dir.join("test_audio.pcmu");

        create_dummy_wav(&wav_path).unwrap();
        create_dummy_mp3(&mp3_path).unwrap();
        create_dummy_raw(&raw_path).unwrap();

        let track_wav = FileTrack::new("wav-test".to_string())
            .with_path(wav_path.to_string_lossy().to_string())
            .with_loop(false);

        let track_mp3 = FileTrack::new("mp3-test".to_string())
            .with_path(mp3_path.to_string_lossy().to_string())
            .with_loop(false);

        let track_raw = FileTrack::new("raw-test".to_string())
            .with_path(raw_path.to_string_lossy().to_string())
            .with_loop(false);

        assert!(
            track_wav.local_description().await.is_ok(),
            "Wav format should be supported"
        );
        assert!(
            track_mp3.local_description().await.is_ok(),
            "Mp3 format should be supported"
        );
        assert!(
            track_raw.local_description().await.is_ok(),
            "Raw PCM is should be supported"
        );

        std::fs::remove_file(wav_path).ok();
        std::fs::remove_file(mp3_path).ok();
        std::fs::remove_file(raw_path).ok();
    }

    #[tokio::test]
    async fn test_file_track_loop_playback() {
        let temp_dir = std::env::temp_dir();
        let audio_path = temp_dir.join("loop_test.pcmu");
        create_dummy_raw(&audio_path).unwrap();

        let track_loop = FileTrack::new("loop-test".to_string())
            .with_path(audio_path.to_string_lossy().to_string())
            .with_loop(true);

        let track_no_loop = FileTrack::new("no-loop-test".to_string())
            .with_path(audio_path.to_string_lossy().to_string())
            .with_loop(false);

        assert!(
            track_loop.local_description().await.is_ok(),
            "Must support looped playback"
        );
        assert!(
            track_no_loop.local_description().await.is_ok(),
            "Must support non-looped playback"
        );

        std::fs::remove_file(audio_path).ok();
    }

    #[tokio::test]
    async fn test_file_track_codec_preference() {
        let temp_dir = std::env::temp_dir();
        let audio_path = temp_dir.join("codec_test.pcmu");
        create_dummy_raw(&audio_path).unwrap();

        let track_pcmu = FileTrack::new("pcmu-test".to_string())
            .with_path(audio_path.to_string_lossy().to_string())
            .with_codec_preference(vec![CodecType::PCMU]);

        let track_pcma = FileTrack::new("pcma-test".to_string())
            .with_path(audio_path.to_string_lossy().to_string())
            .with_codec_preference(vec![CodecType::PCMA]);

        let offer_pcmu = track_pcmu.local_description().await.unwrap();
        let offer_pcma = track_pcma.local_description().await.unwrap();

        assert!(
            offer_pcmu.contains("PCMU") || offer_pcmu.contains("0"),
            "PCMU must be in the offer"
        );
        assert!(
            offer_pcma.contains("PCMA") || offer_pcma.contains("8"),
            "PCMA must be in the offer"
        );

        std::fs::remove_file(audio_path).ok();
    }

    #[tokio::test]
    async fn test_audio_source_manager_file_switching() {
        let temp_dir = std::env::temp_dir();
        let audio1_path = temp_dir.join("switch_test1.pcmu");
        let audio2_path = temp_dir.join("switch_test2.pcmu");

        create_dummy_raw(&audio1_path).unwrap();
        create_dummy_raw(&audio2_path).unwrap();

        let manager = AudioSourceManager::new(8000);

        assert!(
            manager
                .switch_to_file(audio1_path.to_string_lossy().to_string(), false)
                .is_ok(),
            "Must be able to switch to first audio file (no loop)"
        );

        assert!(
            manager
                .switch_to_file(audio2_path.to_string_lossy().to_string(), true)
                .is_ok(),
            "Must be able to switch to second audio file (with loop)"
        );

        std::fs::remove_file(audio1_path).ok();
        std::fs::remove_file(audio2_path).ok();
    }

    fn create_dummy_wav(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(path, spec)?;

        for _ in 0..1600 {
            writer.write_sample(0i16)?;
        }

        writer.finalize()?;
        Ok(())
    }

    fn create_dummy_mp3(path: &std::path::Path) -> std::io::Result<()> {
        let mut file = File::create(path)?;
        let mp3_header = vec![
            0xFF, 0xFB, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        file.write_all(&mp3_header)?;
        for _ in 0..100 {
            file.write_all(&[0u8; 32])?;
        }
        Ok(())
    }

    fn create_dummy_raw(path: &std::path::Path) -> std::io::Result<()> {
        let mut file = File::create(path)?;
        let silence: Vec<u8> = vec![0xFF; 1600];
        file.write_all(&silence)?;
        Ok(())
    }
}
