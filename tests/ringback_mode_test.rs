#[cfg(test)]
mod ringback_mode_tests {
    use rustpbx::call::{RingbackConfig, RingbackMode};

    #[test]
    fn test_ringback_mode_enum() {
        assert_eq!(RingbackMode::default(), RingbackMode::Auto);

        assert_eq!(RingbackMode::Local, RingbackMode::Local);
        assert_eq!(RingbackMode::Passthrough, RingbackMode::Passthrough);
        assert_eq!(RingbackMode::Auto, RingbackMode::Auto);
        assert_eq!(RingbackMode::None, RingbackMode::None);
    }

    #[test]
    fn test_ringback_config_default() {
        let config = RingbackConfig::new();

        assert_eq!(config.mode, RingbackMode::Auto);
        assert_eq!(config.audio_file, None);
        assert_eq!(config.loop_playback, true);
        assert_eq!(config.wait_for_completion, false);
    }

    #[test]
    fn test_ringback_config_builder_local_mode() {
        let config = RingbackConfig::new()
            .with_mode(RingbackMode::Local)
            .with_audio_file("/sounds/ringback.mp3".to_string())
            .with_loop(true);

        assert_eq!(config.mode, RingbackMode::Local);
        assert_eq!(config.audio_file, Some("/sounds/ringback.mp3".to_string()));
        assert_eq!(config.loop_playback, true);
    }

    #[test]
    fn test_ringback_config_builder_passthrough_mode() {
        let config = RingbackConfig::new().with_mode(RingbackMode::Passthrough);

        assert_eq!(config.mode, RingbackMode::Passthrough);
        assert_eq!(config.audio_file, None);
    }

    #[test]
    fn test_ringback_config_builder_auto_mode() {
        let config = RingbackConfig::new()
            .with_mode(RingbackMode::Auto)
            .with_audio_file("/sounds/default.wav".to_string());

        assert_eq!(config.mode, RingbackMode::Auto);
        assert_eq!(config.audio_file, Some("/sounds/default.wav".to_string()));
    }

    #[test]
    fn test_ringback_config_builder_none_mode() {
        let config = RingbackConfig::new().with_mode(RingbackMode::None);

        assert_eq!(config.mode, RingbackMode::None);
    }

    #[test]
    fn test_ringback_config_serde() {
        let config = RingbackConfig::new()
            .with_mode(RingbackMode::Local)
            .with_audio_file("https://cdn.example.com/ringback.mp3".to_string())
            .with_loop(false);

        let toml = toml::to_string(&config).expect("Failed to serialize");

        assert!(toml.contains("mode = \"local\""));
        assert!(toml.contains("audio_file"));
        assert!(toml.contains("loop_playback = false"));
    }

    #[test]
    fn test_ringback_config_deserialize_local() {
        let toml = r#"
            mode = "local"
            audio_file = "/sounds/company.mp3"
            loop_playback = true
        "#;

        let config: RingbackConfig = toml::from_str(toml).expect("Failed to deserialize");

        assert_eq!(config.mode, RingbackMode::Local);
        assert_eq!(config.audio_file, Some("/sounds/company.mp3".to_string()));
        assert_eq!(config.loop_playback, true);
    }

    #[test]
    fn test_ringback_config_deserialize_passthrough() {
        let toml = r#"
            mode = "passthrough"
        "#;

        let config: RingbackConfig = toml::from_str(toml).expect("Failed to deserialize");

        assert_eq!(config.mode, RingbackMode::Passthrough);
    }

    #[test]
    fn test_ringback_config_deserialize_auto() {
        let toml = r#"
            mode = "auto"
            audio_file = "/sounds/default.wav"
        "#;

        let config: RingbackConfig = toml::from_str(toml).expect("Failed to deserialize");

        assert_eq!(config.mode, RingbackMode::Auto);
        assert_eq!(config.audio_file, Some("/sounds/default.wav".to_string()));
    }

    #[test]
    fn test_ringback_config_deserialize_none() {
        let toml = r#"
            mode = "none"
        "#;

        let config: RingbackConfig = toml::from_str(toml).expect("Failed to deserialize");

        assert_eq!(config.mode, RingbackMode::None);
    }

    #[test]
    fn test_ringback_config_deserialize_default_values() {
        let toml = r#"
            audio_file = "/sounds/test.mp3"
        "#;

        let config: RingbackConfig = toml::from_str(toml).expect("Failed to deserialize");

        assert_eq!(config.mode, RingbackMode::Auto);
        assert_eq!(config.loop_playback, true);
        assert_eq!(config.wait_for_completion, false);
    }

    #[test]
    fn test_ringback_mode_decision_logic() {
        let has_early_media = true;
        let no_early_media = false;

        // Local mode: always play local
        assert!(should_play_local(RingbackMode::Local, has_early_media));
        assert!(should_play_local(RingbackMode::Local, no_early_media));
        assert!(!should_passthrough(RingbackMode::Local, has_early_media));
        assert!(!should_passthrough(RingbackMode::Local, no_early_media));

        // Passthrough mode: passthrough if has early media
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

        // Auto mode: passthrough if has early media, else local
        assert!(!should_play_local(RingbackMode::Auto, has_early_media));
        assert!(should_play_local(RingbackMode::Auto, no_early_media));
        assert!(should_passthrough(RingbackMode::Auto, has_early_media));
        assert!(!should_passthrough(RingbackMode::Auto, no_early_media));

        // None mode: no audio at all
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

    #[test]
    fn test_ringback_config_with_http_url() {
        let config = RingbackConfig::new()
            .with_mode(RingbackMode::Local)
            .with_audio_file("https://cdn.example.com/vip_ringback.mp3".to_string());

        assert_eq!(
            config.audio_file,
            Some("https://cdn.example.com/vip_ringback.mp3".to_string())
        );
        assert!(config.audio_file.as_ref().unwrap().starts_with("https://"));
    }

    #[test]
    fn test_ringback_config_loop_variants() {
        let config_loop = RingbackConfig::new().with_loop(true);
        let config_no_loop = RingbackConfig::new().with_loop(false);

        assert_eq!(config_loop.loop_playback, true);
        assert_eq!(config_no_loop.loop_playback, false);
    }
}
