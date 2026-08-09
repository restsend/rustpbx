#[cfg(test)]
mod audio_feature_tests {
    use rustpbx::media::audio_source::{AudioSource, FileAudioSource};
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_file_audio_source_supports_multiple_formats() {
        let temp_dir = std::env::temp_dir();

        let wav_path = temp_dir.join("test_audio.wav");
        let mp3_path = temp_dir.join("test_audio.mp3");
        let raw_path = temp_dir.join("test_audio.pcmu");

        create_dummy_wav(&wav_path).unwrap();
        create_dummy_mp3(&mp3_path).unwrap();
        create_dummy_raw(&raw_path).unwrap();

        assert!(
            FileAudioSource::new(wav_path.to_string_lossy().to_string(), false)
                .await
                .is_ok(),
            "Wav format should be supported"
        );
        assert!(
            FileAudioSource::new(mp3_path.to_string_lossy().to_string(), false)
                .await
                .is_ok(),
            "Mp3 format should be supported"
        );
        assert!(
            FileAudioSource::new(raw_path.to_string_lossy().to_string(), false)
                .await
                .is_ok(),
            "Raw PCM should be supported"
        );

        std::fs::remove_file(wav_path).ok();
        std::fs::remove_file(mp3_path).ok();
        std::fs::remove_file(raw_path).ok();
    }

    #[tokio::test]
    async fn test_file_audio_source_loop_playback() {
        let temp_dir = std::env::temp_dir();
        let audio_path = temp_dir.join("loop_test.pcmu");
        create_dummy_raw(&audio_path).unwrap();

        // Non-looped source exhausts and returns 0.
        let mut source =
            FileAudioSource::new(audio_path.to_string_lossy().to_string(), false)
                .await
                .unwrap();
        let mut buf = [0i16; 3200];
        let first = source.read_samples(&mut buf);
        assert!(first > 0, "first read should return samples");
        let second = source.read_samples(&mut buf);
        assert_eq!(second, 0, "non-looped source must exhaust");

        // Looped source keeps serving samples.
        let mut looped =
            FileAudioSource::new(audio_path.to_string_lossy().to_string(), true)
                .await
                .unwrap();
        let mut total = 0;
        for _ in 0..3 {
            total += looped.read_samples(&mut buf);
        }
        assert!(total > first, "looped source must keep producing samples");

        std::fs::remove_file(audio_path).ok();
    }

    fn create_dummy_wav(path: &std::path::Path) -> anyhow::Result<()> {
        use rustpbx::media::wav_reader::{SampleFormat, WavSpec, WavWriter};

        let spec = WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        let mut writer = WavWriter::create(path, spec)?;

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
