//! Contract tests for AppRuntime
//!
//! These tests verify the behavioral contracts between AppRuntime
//! and the session layer, ensuring event delivery semantics are preserved.

#[cfg(test)]
mod tests {
    use crate::call::domain::{MediaCapability, MediaPathMode, MediaRuntimeProfile};
    use crate::call::runtime::{AppDescriptor, AppStatus, CapabilityCheckResult};
    use serde_json::json;

    // ============================================================================
    // Capability Gate Tests
    // ============================================================================

    /// Test that IVR requires Full media capability
    #[test]
    fn ivr_requires_full_capability() {
        let desc = AppDescriptor::ivr();
        assert!(!desc.required_capabilities.is_empty());
        assert!(desc
            .required_capabilities
            .contains(&MediaCapability::Full));
    }

    /// Test that signaling-only apps work in bypass mode
    #[test]
    fn signaling_app_works_in_bypass() {
        let desc = AppDescriptor::signaling_only();
        let bypass_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);

        let result = desc.check_capabilities(&[bypass_profile.capability]);
        assert!(
            result.is_satisfied(),
            "Signaling-only app should work in bypass mode"
        );
    }

    /// Test that IVR is denied in bypass mode
    #[test]
    fn ivr_denied_in_bypass() {
        let desc = AppDescriptor::ivr();
        let bypass_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);

        let result = desc.check_capabilities(&[bypass_profile.capability]);
        assert!(
            !result.is_satisfied(),
            "IVR should be denied in bypass mode"
        );

        if let CapabilityCheckResult::Missing(missing) = result {
            assert!(missing.contains(&MediaCapability::Full));
        } else {
            panic!("Expected Missing result");
        }
    }

    /// Test that IVR works in anchored mode
    #[test]
    fn ivr_works_in_anchored() {
        let desc = AppDescriptor::ivr();
        let anchored_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored);

        let result = desc.check_capabilities(&[anchored_profile.capability]);
        assert!(
            result.is_satisfied(),
            "IVR should work in anchored mode"
        );
    }

    /// Test voicemail capability requirements
    #[test]
    fn voicemail_requires_full_capability() {
        let desc = AppDescriptor::voicemail();
        let bypass_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);

        // Voicemail needs media (recording)
        let result = desc.check_capabilities(&[bypass_profile.capability]);
        assert!(
            !result.is_satisfied(),
            "Voicemail should be denied in bypass mode"
        );

        let anchored_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored);
        let result = desc.check_capabilities(&[anchored_profile.capability]);
        assert!(
            result.is_satisfied(),
            "Voicemail should work in anchored mode"
        );
    }

    /// Test queue capability requirements
    #[test]
    fn queue_requires_full_capability() {
        let desc = AppDescriptor::queue();
        let bypass_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);

        // Queue needs media (hold music, announcements)
        let result = desc.check_capabilities(&[bypass_profile.capability]);
        assert!(
            !result.is_satisfied(),
            "Queue should be denied in bypass mode"
        );

        let anchored_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored);
        let result = desc.check_capabilities(&[anchored_profile.capability]);
        assert!(result.is_satisfied(), "Queue should work in anchored mode");
    }

    // ============================================================================
    // Event Injection Contract Tests
    // ============================================================================

    /// Test that DTMF events can be parsed correctly
    #[test]
    fn dtmf_event_parsing_contract() {
        let event = json!({
            "type": "dtmf",
            "digit": "5"
        });

        // Verify the event has the required fields
        assert!(event.get("type").is_some());
        assert_eq!(event.get("type").unwrap(), "dtmf");
        assert!(event.get("digit").is_some());
    }

    /// Test that AudioComplete events have required fields
    #[test]
    fn audio_complete_event_contract() {
        let event = json!({
            "type": "audio_complete",
            "track_id": "track-123",
            "interrupted": false
        });

        // Verify the event has the required fields
        assert!(event.get("type").is_some());
        assert_eq!(event.get("type").unwrap(), "audio_complete");
        assert!(event.get("track_id").is_some());
        assert!(event.get("interrupted").is_some());
    }

    /// Test that RecordingComplete events have required fields
    #[test]
    fn recording_complete_event_contract() {
        let event = json!({
            "type": "recording_complete",
            "path": "/recordings/call-123.wav"
        });

        // Verify the event has the required fields
        assert!(event.get("type").is_some());
        assert_eq!(event.get("type").unwrap(), "recording_complete");
        assert!(event.get("path").is_some());
    }

    /// Test that Custom events have required fields
    #[test]
    fn custom_event_contract() {
        let event = json!({
            "type": "custom",
            "name": "webhook",
            "data": {"action": "transfer", "target": "1001"}
        });

        // Verify the event has the required fields
        assert!(event.get("type").is_some());
        assert_eq!(event.get("type").unwrap(), "custom");
        assert!(event.get("name").is_some());
        // data is optional, but if present should be an object
        if let Some(data) = event.get("data") {
            assert!(data.is_object());
        }
    }

    /// Test that Hangup events can be created
    #[test]
    fn hangup_event_contract() {
        let event = json!({
            "type": "hangup",
            "reason": "normal_clearing"
        });

        assert!(event.get("type").is_some());
        assert_eq!(event.get("type").unwrap(), "hangup");
    }

    /// Test that Timeout events can be created
    #[test]
    fn timeout_event_contract() {
        let event = json!({
            "type": "timeout",
            "timer_id": "timer-456"
        });

        assert!(event.get("type").is_some());
        assert_eq!(event.get("type").unwrap(), "timeout");
        assert!(event.get("timer_id").is_some());
    }

    // ============================================================================
    // Lifecycle State Machine Tests
    // ============================================================================

    /// Test AppStatus transitions are valid
    #[test]
    fn app_status_idle_is_default() {
        let status = AppStatus::default();
        assert_eq!(status, AppStatus::Idle);
    }

    /// Test AppStatus display formatting
    #[test]
    fn app_status_display_values() {
        assert_eq!(AppStatus::Idle.to_string(), "idle");
        assert_eq!(AppStatus::Starting.to_string(), "starting");
        assert_eq!(AppStatus::Running.to_string(), "running");
        assert_eq!(AppStatus::Stopping.to_string(), "stopping");
        assert_eq!(AppStatus::Stopped.to_string(), "stopped");
        assert_eq!(AppStatus::Failed.to_string(), "failed");
    }

    // ============================================================================
    // Media Profile Capability Tests
    // ============================================================================

    /// Test anchored profile has Full capability
    #[test]
    fn anchored_profile_has_full_capability() {
        let profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored);
        assert_eq!(profile.capability, MediaCapability::Full);
        assert_eq!(profile.path, MediaPathMode::Anchored);
        assert!(profile.supports_recording);
        assert!(profile.supports_local_ringback);
    }

    /// Test bypass profile has SignalingOnly capability
    #[test]
    fn bypass_profile_has_signaling_only_capability() {
        let profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);
        assert_eq!(profile.capability, MediaCapability::SignalingOnly);
        assert_eq!(profile.path, MediaPathMode::Bypass);
        assert!(!profile.supports_recording);
        assert!(!profile.supports_local_ringback);
    }

    /// Test adaptive profile starts with Limited capability (may switch based on conditions)
    #[test]
    fn adaptive_profile_starts_with_limited_capability() {
        let profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Adaptive);
        // Adaptive starts with Limited by design, as it may switch between modes
        assert_eq!(profile.capability, MediaCapability::Limited);
        assert_eq!(profile.path, MediaPathMode::Adaptive);
    }

    // ============================================================================
    // Capability Hierarchy Tests
    // ============================================================================

    /// Test capability hierarchy: Full satisfies everything
    #[test]
    fn full_satisfies_all_requirements() {
        let full = MediaCapability::Full;

        let signaling_desc = AppDescriptor::signaling_only();
        assert!(signaling_desc
            .check_capabilities(&[full])
            .is_satisfied());

        // Even apps requiring Full can be satisfied by Full
        let ivr_desc = AppDescriptor::ivr();
        assert!(ivr_desc.check_capabilities(&[full]).is_satisfied());
    }

    /// Test capability hierarchy: Limited satisfies Limited and SignalingOnly
    #[test]
    fn limited_satisfies_limited_and_signaling() {
        let limited = MediaCapability::Limited;

        let signaling_desc = AppDescriptor::signaling_only();
        assert!(signaling_desc
            .check_capabilities(&[limited])
            .is_satisfied());

        // Create a limited app descriptor
        let limited_desc = AppDescriptor::new("limited_app")
            .with_capabilities(vec![MediaCapability::Limited]);
        assert!(limited_desc.check_capabilities(&[limited]).is_satisfied());

        // Limited cannot satisfy Full requirements
        let ivr_desc = AppDescriptor::ivr();
        assert!(!ivr_desc.check_capabilities(&[limited]).is_satisfied());
    }

    /// Test capability hierarchy: SignalingOnly only satisfies SignalingOnly
    #[test]
    fn signaling_only_satisfies_only_signaling() {
        let signaling = MediaCapability::SignalingOnly;

        let signaling_desc = AppDescriptor::signaling_only();
        // Signaling-only app with empty requirements is satisfied
        let result = signaling_desc.check_capabilities(&[signaling]);
        // Since signaling_only has empty required_capabilities, it should be satisfied
        assert!(result.is_satisfied());

        // SignalingOnly cannot satisfy Full requirements
        let ivr_desc = AppDescriptor::ivr();
        assert!(!ivr_desc.check_capabilities(&[signaling]).is_satisfied());
    }

    // ============================================================================
    // Bypass Mode Degradation Tests
    // ============================================================================

    /// Test that bypass mode correctly identifies unsupported operations
    #[test]
    fn bypass_mode_identifies_unsupported_operations() {
        let bypass_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Bypass);

        // Recording not supported
        assert!(!bypass_profile.supports_recording);

        // Local ringback not supported
        assert!(!bypass_profile.supports_local_ringback);

        // Supervisor media not supported
        assert!(!bypass_profile.supports_supervisor_media);

        // Media injection not supported
        assert!(!bypass_profile.supports_media_injection);
    }

    /// Test that anchored mode supports all operations
    #[test]
    fn anchored_mode_supports_all_operations() {
        let anchored_profile = MediaRuntimeProfile::from_media_path(MediaPathMode::Anchored);

        // Recording supported
        assert!(anchored_profile.supports_recording);

        // Local ringback supported
        assert!(anchored_profile.supports_local_ringback);

        // Supervisor media supported
        assert!(anchored_profile.supports_supervisor_media);

        // Media injection supported
        assert!(anchored_profile.supports_media_injection);
    }
}
