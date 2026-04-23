#[cfg(test)]
mod tests {
    use crate::config::SessionTimerMode;
    use crate::proxy::proxy_call::session_timer::*;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // ==================== Basic State Tests ====================

    #[test]
    fn test_session_timer_state_default() {
        let timer = SessionTimerState::default();
        assert!(!timer.enabled);
        assert!(!timer.active);
        assert!(!timer.refreshing);
        assert_eq!(
            timer.session_interval,
            Duration::from_secs(DEFAULT_SESSION_EXPIRES)
        );
        assert_eq!(timer.min_se, Duration::from_secs(MIN_MIN_SE));
        assert_eq!(timer.refresh_count, 0);
        assert_eq!(timer.failed_refreshes, 0);
    }

    #[test]
    fn test_session_timer_state_new() {
        let interval = Duration::from_secs(600);
        let min_se = Duration::from_secs(120);
        let timer = SessionTimerState::new(interval, min_se, SessionRefresher::Uas);

        assert!(timer.enabled);
        assert!(timer.active);
        assert!(!timer.refreshing);
        assert_eq!(timer.session_interval, interval);
        assert_eq!(timer.min_se, min_se);
        assert_eq!(timer.refresher, SessionRefresher::Uas);
    }

    #[test]
    fn test_session_timer_state_memory() {
        let timer = Arc::new(Mutex::new(SessionTimerState {
            mode: SessionTimerMode::Supported,
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now(),
            refreshing: false,
            session_start: Instant::now(),
            refresh_count: 0,
            failed_refreshes: 0,
        }));

        let timer_clone = timer.clone();
        assert_eq!(Arc::strong_count(&timer), 2);

        drop(timer_clone);
        assert_eq!(Arc::strong_count(&timer), 1);
    }

    // ==================== Refresh Logic Tests ====================

    #[test]
    fn test_should_refresh_when_inactive() {
        let timer = SessionTimerState {
            enabled: true,
            active: false,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        assert!(!timer.should_refresh());
    }

    #[test]
    fn test_should_refresh_when_disabled() {
        let timer = SessionTimerState {
            enabled: false,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        assert!(!timer.should_refresh());
    }

    #[test]
    fn test_should_refresh_when_refreshing() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.last_refresh = Instant::now() - Duration::from_secs(60); // Past half interval

        assert!(!timer.should_refresh());
    }

    #[test]
    fn test_should_refresh_before_half_interval() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.last_refresh = Instant::now() - Duration::from_secs(40); // Before half (50s)

        assert!(!timer.should_refresh());
    }

    #[test]
    fn test_should_refresh_at_half_interval() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.last_refresh = Instant::now() - Duration::from_secs(51); // Past half (50s)

        assert!(timer.should_refresh());
    }

    // ==================== Expiration Tests ====================

    #[test]
    fn test_is_expired_when_inactive() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: false,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.last_refresh = Instant::now() - Duration::from_secs(150); // Past interval

        assert!(!timer.is_expired());
    }

    #[test]
    fn test_is_expired_before_interval() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.last_refresh = Instant::now() - Duration::from_secs(50); // Before 100s

        assert!(!timer.is_expired());
    }

    #[test]
    fn test_is_expired_at_interval() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.last_refresh = Instant::now() - Duration::from_secs(101); // Past 100s

        assert!(timer.is_expired());
    }

    #[test]
    fn test_should_refresh_just_past_half_interval() {
        let timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: false,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(901),
            ..Default::default()
        };
        assert!(timer.should_refresh());
    }

    #[test]
    fn test_should_not_refresh_before_half_interval() {
        let timer = SessionTimerState {
            active: true,
            refreshing: false,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(899),
            ..Default::default()
        };
        assert!(
            !timer.should_refresh(),
            "Should NOT refresh before half interval"
        );
    }

    #[test]
    fn test_should_not_refresh_when_refreshing_flag_is_set() {
        let mut timer = SessionTimerState {
            active: true,
            ..Default::default()
        };
        timer.refreshing = true; // Already refreshing
        timer.session_interval = Duration::from_secs(1800);
        timer.last_refresh = Instant::now() - Duration::from_secs(1000); // Well past half

        // Even though past half interval, should NOT refresh because already refreshing
        assert!(
            !timer.should_refresh(),
            "Should NOT refresh when refreshing flag is true"
        );
    }

    #[test]
    fn test_should_not_refresh_when_inactive() {
        let timer = SessionTimerState {
            active: false,
            refreshing: false,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(1000),
            ..Default::default()
        };

        assert!(
            !timer.should_refresh(),
            "Should NOT refresh when timer is not active"
        );
    }

    #[test]
    fn test_is_expired_after_full_interval() {
        let timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now() - Duration::from_secs(101),
            ..Default::default()
        };
        assert!(timer.is_expired(), "Should be expired after full interval");
    }

    #[test]
    fn test_is_not_expired_before_full_interval() {
        let timer = SessionTimerState {
            active: true,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now() - Duration::from_secs(99),
            ..Default::default()
        };
        assert!(
            !timer.is_expired(),
            "Should NOT be expired before full interval"
        );
    }

    #[test]
    fn test_is_not_expired_when_inactive() {
        let timer = SessionTimerState {
            active: false,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now() - Duration::from_secs(200),
            ..Default::default()
        };
        assert!(
            !timer.is_expired(),
            "Should NOT be expired when timer is not active"
        );
    }

    #[test]
    fn test_update_refresh_sets_last_refresh_to_now() {
        let mut timer = SessionTimerState {
            last_refresh: Instant::now() - Duration::from_secs(1000),
            ..Default::default()
        };
        let before_update = Instant::now();
        timer.update_refresh();
        let after_update = Instant::now();

        // last_refresh should be updated to approximately now
        assert!(
            timer.last_refresh >= before_update && timer.last_refresh <= after_update,
            "update_refresh should set last_refresh to approximately now"
        );
    }

    #[test]
    fn test_should_refresh_regardless_of_refresher_role() {
        // Note: should_refresh() does NOT check who the refresher is.
        // The caller (run_server_events_loop) checks refresher role before calling should_refresh().
        // This method only checks if the time has passed since last refresh.
        let timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: false,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(1000),
            ..Default::default()
        };
        assert!(
            timer.should_refresh(),
            "should_refresh returns true when past half interval"
        );
    }

    #[test]
    fn test_should_refresh_with_uac_refresher() {
        // UAC refresher - the UAC is responsible for sending refresh
        // But should_refresh() doesn't check this - the caller does
        let timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: false,
            refresher: SessionRefresher::Uac,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(1000),
            ..Default::default()
        };
        assert!(timer.should_refresh());
    }

    #[test]
    fn test_should_refresh_with_uas_refresher() {
        // UAS refresher - the UAS is responsible for sending refresh
        let timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: false,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(1000),
            ..Default::default()
        };
        assert!(timer.should_refresh());
    }

    // ==================== Header Parsing Tests ====================

    #[test]
    fn test_parse_session_expires_with_refresher() {
        let result = parse_session_expires("1800;refresher=uac");
        assert!(result.is_some());

        let (duration, refresher) = result.unwrap();
        assert_eq!(duration, Duration::from_secs(1800));
        assert_eq!(refresher, Some(SessionRefresher::Uac));
    }

    #[test]
    fn test_parse_session_expires_with_uas_refresher() {
        let result = parse_session_expires("3600;refresher=uas");
        assert!(result.is_some());

        let (duration, refresher) = result.unwrap();
        assert_eq!(duration, Duration::from_secs(3600));
        assert_eq!(refresher, Some(SessionRefresher::Uas));
    }

    #[test]
    fn test_parse_session_expires_without_refresher() {
        let result = parse_session_expires("1800");
        assert!(result.is_some());

        let (duration, refresher) = result.unwrap();
        assert_eq!(duration, Duration::from_secs(1800));
        assert_eq!(refresher, None);
    }

    #[test]
    fn test_parse_session_expires_invalid() {
        assert!(parse_session_expires("").is_none());
        assert!(parse_session_expires("invalid").is_none());
        assert!(parse_session_expires("-100").is_none());
    }

    #[test]
    fn test_parse_min_se() {
        assert_eq!(parse_min_se("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_min_se("300"), Some(Duration::from_secs(300)));
    }

    #[test]
    fn test_parse_min_se_invalid() {
        assert!(parse_min_se("").is_none());
        assert!(parse_min_se("invalid").is_none());
    }

    #[test]
    fn test_session_refresher_from_str() {
        assert_eq!(SessionRefresher::from_str("uac"), Ok(SessionRefresher::Uac));
        assert_eq!(SessionRefresher::from_str("UAC"), Ok(SessionRefresher::Uac));
        assert_eq!(SessionRefresher::from_str("uas"), Ok(SessionRefresher::Uas));
        assert_eq!(SessionRefresher::from_str("UAS"), Ok(SessionRefresher::Uas));
        assert!(SessionRefresher::from_str("invalid").is_err());
    }

    #[test]
    fn test_session_refresher_display() {
        assert_eq!(SessionRefresher::Uac.to_string(), "uac");
        assert_eq!(SessionRefresher::Uas.to_string(), "uas");
    }

    // ==================== Integration Tests ====================

    #[test]
    fn test_session_timer_expiration_logic() {
        let mut timer = SessionTimerState {
            mode: SessionTimerMode::Supported,
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now() - Duration::from_secs(100),
            refreshing: false,
            session_start: Instant::now(),
            refresh_count: 0,
            failed_refreshes: 0,
        };

        // Check if expired
        let is_expired = Instant::now() >= timer.last_refresh + timer.session_interval;
        assert!(is_expired);

        // Update refresh time
        timer.last_refresh = Instant::now();
        let is_expired_now = Instant::now() >= timer.last_refresh + timer.session_interval;
        assert!(!is_expired_now);
    }

    #[test]
    fn test_session_timer_refresh_logic() {
        let mut timer = SessionTimerState {
            mode: SessionTimerMode::Supported,
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now() - Duration::from_secs(46), // Just past half (45s)
            refreshing: false,
            session_start: Instant::now(),
            refresh_count: 0,
            failed_refreshes: 0,
        };

        // Logic from handle_server_events
        if timer.active
            && timer.refresher == SessionRefresher::Uas
            && let Some(next_refresh) = Some(timer.last_refresh + (timer.session_interval / 2))
            && Instant::now() >= next_refresh
        {
            // Simulate refresh
            timer.last_refresh = Instant::now();
        }

        // Should be refreshed now
        assert!(Instant::now() < timer.last_refresh + Duration::from_secs(1));
    }

    #[test]
    fn test_session_timer_methods() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now() - Duration::from_secs(51),
            ..Default::default()
        };
        assert!(timer.should_refresh());
        assert!(!timer.is_expired());

        timer.last_refresh = Instant::now() - Duration::from_secs(101);
        assert!(timer.is_expired());
    }

    // ==================== Timer Action Tests ====================

    #[test]
    fn test_start_refresh() {
        let mut timer = SessionTimerState::default();

        // First start should succeed
        assert!(timer.start_refresh());
        assert!(timer.refreshing);

        // Second start should fail (already refreshing)
        assert!(!timer.start_refresh());
    }

    #[test]
    fn test_complete_refresh() {
        let mut timer = SessionTimerState {
            active: true,
            refreshing: true,
            last_refresh: Instant::now() - Duration::from_secs(60),
            ..Default::default()
        };
        let old_refresh_time = timer.last_refresh;
        timer.complete_refresh();

        assert!(!timer.refreshing);
        assert_eq!(timer.refresh_count, 1);
        assert!(timer.last_refresh > old_refresh_time);
    }

    #[test]
    fn test_fail_refresh() {
        let mut timer = SessionTimerState {
            refreshing: true,
            ..Default::default()
        };
        timer.fail_refresh();

        assert!(!timer.refreshing);
        assert_eq!(timer.failed_refreshes, 1);
    }

    #[test]
    fn test_update_refresh() {
        let mut timer = SessionTimerState {
            active: true,
            last_refresh: Instant::now() - Duration::from_secs(60),
            ..Default::default()
        };
        let old_refresh_time = timer.last_refresh;
        timer.update_refresh();

        assert!(timer.last_refresh > old_refresh_time);
        assert_eq!(timer.refresh_count, 1);
    }

    // ==================== Time Calculation Tests ====================

    #[test]
    fn test_next_refresh_time_when_inactive() {
        let timer = SessionTimerState::default();
        assert!(timer.next_refresh_time().is_none());
    }

    #[test]
    fn test_next_refresh_time_when_active() {
        let mut timer = SessionTimerState {
            active: true,
            enabled: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        let now = Instant::now();
        timer.last_refresh = now;

        let next = timer.next_refresh_time();
        assert!(next.is_some());
        // Should be at half interval (50s)
        let expected = now + Duration::from_secs(50);
        let diff = if next.unwrap() > expected {
            next.unwrap() - expected
        } else {
            expected - next.unwrap()
        };
        assert!(diff < Duration::from_millis(10));
    }

    #[test]
    fn test_expiration_time_when_inactive() {
        let timer = SessionTimerState::default();
        assert!(timer.expiration_time().is_none());
    }

    #[test]
    fn test_expiration_time_when_active() {
        let mut timer = SessionTimerState {
            active: true,
            enabled: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        let now = Instant::now();
        timer.last_refresh = now;

        let exp = timer.expiration_time();
        assert!(exp.is_some());
        // Should be at full interval (100s)
        let expected = now + Duration::from_secs(100);
        let diff = if exp.unwrap() > expected {
            exp.unwrap() - expected
        } else {
            expected - exp.unwrap()
        };
        assert!(diff < Duration::from_millis(10));
    }

    #[test]
    fn test_time_until_expiration() {
        let timer = SessionTimerState {
            active: true,
            enabled: true,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now(),
            ..Default::default()
        };
        let remaining = timer.time_until_expiration();
        assert!(remaining.is_some());
        // Should be close to 100s
        assert!(remaining.unwrap() <= Duration::from_secs(100));
        assert!(remaining.unwrap() > Duration::from_secs(99));
    }

    #[test]
    fn test_time_until_expiration_when_expired() {
        let timer = SessionTimerState {
            active: true,
            enabled: true,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now() - Duration::from_secs(150),
            ..Default::default()
        };
        let remaining = timer.time_until_expiration();
        assert!(remaining.is_some());
        assert_eq!(remaining.unwrap(), Duration::ZERO);
    }

    #[test]
    fn test_time_until_refresh() {
        let timer = SessionTimerState {
            active: true,
            enabled: true,
            session_interval: Duration::from_secs(100),
            last_refresh: Instant::now(),
            ..Default::default()
        };
        let remaining = timer.time_until_refresh();
        assert!(remaining.is_some());
        // Should be close to 50s (half interval)
        assert!(remaining.unwrap() <= Duration::from_secs(50));
        assert!(remaining.unwrap() > Duration::from_secs(49));
    }

    // ==================== Header Building Tests ====================

    #[test]
    fn test_build_session_expires_header() {
        let header = build_session_expires_header(Duration::from_secs(1800), SessionRefresher::Uac);
        match header {
            rsipstack::sip::Header::Other(name, value) => {
                assert_eq!(name, HEADER_SESSION_EXPIRES);
                assert_eq!(value, "1800;refresher=uac");
            }
            _ => panic!("Expected Other header"),
        }
    }

    #[test]
    fn test_build_min_se_header() {
        let header = build_min_se_header(Duration::from_secs(90));
        match header {
            rsipstack::sip::Header::Other(name, value) => {
                assert_eq!(name, HEADER_MIN_SE);
                assert_eq!(value, "90");
            }
            _ => panic!("Expected Other header"),
        }
    }

    // ==================== Timer State Management Tests ====================

    #[test]
    fn test_activate() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: false,
            ..Default::default()
        };
        let old_refresh = timer.last_refresh;
        timer.activate();

        assert!(timer.active);
        assert!(timer.last_refresh > old_refresh);
    }

    #[test]
    fn test_activate_when_disabled() {
        let mut timer = SessionTimerState {
            enabled: false,
            active: false,
            ..Default::default()
        };
        timer.activate();

        // Should not activate if disabled
        assert!(!timer.active);
    }

    #[test]
    fn test_deactivate() {
        let mut timer = SessionTimerState {
            active: true,
            ..Default::default()
        };
        timer.deactivate();

        assert!(!timer.active);
    }

    #[test]
    fn test_reset() {
        let mut timer = SessionTimerState {
            active: true,
            refreshing: true,
            session_interval: Duration::from_secs(100),
            refresher: SessionRefresher::Uac,
            last_refresh: Instant::now() - Duration::from_secs(50),
            ..Default::default()
        };
        timer.reset(Duration::from_secs(200), SessionRefresher::Uas);

        assert_eq!(timer.session_interval, Duration::from_secs(200));
        assert_eq!(timer.refresher, SessionRefresher::Uas);
        assert!(!timer.refreshing);
        assert!(timer.last_refresh > Instant::now() - Duration::from_secs(1));
    }

    #[test]
    fn test_get_session_expires_value() {
        let mut timer = SessionTimerState {
            session_interval: Duration::from_secs(1800),
            refresher: SessionRefresher::Uac,
            ..Default::default()
        };
        assert_eq!(timer.get_session_expires_value(), "1800;refresher=uac");

        timer.refresher = SessionRefresher::Uas;
        assert_eq!(timer.get_session_expires_value(), "1800;refresher=uas");
    }

    #[test]
    fn test_get_min_se_value() {
        let timer = SessionTimerState {
            min_se: Duration::from_secs(90),
            ..Default::default()
        };
        assert_eq!(timer.get_min_se_value(), "90");
    }

    #[test]
    fn test_require_timer() {
        let mut timer = SessionTimerState::default();

        // Not enabled and not active
        assert!(!timer.require_timer());

        // Enabled but not active
        timer.enabled = true;
        assert!(!timer.require_timer());

        // Enabled and active
        timer.active = true;
        assert!(timer.require_timer());
    }

    #[test]
    fn test_session_duration() {
        let timer = SessionTimerState::default();
        // Wait a tiny bit
        std::thread::sleep(Duration::from_millis(10));
        let duration = timer.session_duration();
        assert!(duration >= Duration::from_millis(10));
    }

    // ==================== Stats Tests ====================

    #[test]
    fn test_stats() {
        let timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: false,
            session_interval: Duration::from_secs(1800),
            min_se: Duration::from_secs(90),
            refresher: SessionRefresher::Uac,
            refresh_count: 5,
            failed_refreshes: 2,
            ..Default::default()
        };
        let stats = timer.stats();

        assert!(stats.enabled);
        assert!(stats.active);
        assert!(!stats.refreshing);
        assert_eq!(stats.session_interval_secs, 1800);
        assert_eq!(stats.min_se_secs, 90);
        assert_eq!(stats.refresher, "uac");
        assert_eq!(stats.refresh_count, 5);
        assert_eq!(stats.failed_refreshes, 2);
        assert!(stats.time_until_refresh_secs.is_some());
        assert!(stats.time_until_expiration_secs.is_some());
    }

    // ==================== Negotiation Tests ====================

    #[test]
    fn test_negotiate_session_interval_success() {
        let requested = Duration::from_secs(1800);
        let local_min_se = Duration::from_secs(90);

        let result = negotiate_session_interval(requested, local_min_se);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), requested);
    }

    #[test]
    fn test_negotiate_session_interval_too_small() {
        let requested = Duration::from_secs(60);
        let local_min_se = Duration::from_secs(90);

        let result = negotiate_session_interval(requested, local_min_se);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), local_min_se);
    }

    #[test]
    fn test_negotiate_session_interval_exact_min() {
        let requested = Duration::from_secs(90);
        let local_min_se = Duration::from_secs(90);

        let result = negotiate_session_interval(requested, local_min_se);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), requested);
    }

    // ==================== Header Value Extraction Tests ====================

    #[test]
    fn test_get_header_value_session_expires() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_SESSION_EXPIRES.to_string(),
            "1800;refresher=uac".to_string(),
        )]);

        let value = get_header_value(&headers, HEADER_SESSION_EXPIRES);
        assert_eq!(value, Some("1800;refresher=uac".to_string()));
    }

    #[test]
    fn test_get_header_value_typed_session_expires() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::SessionExpires(
            rsipstack::sip::headers::SessionExpires::new("1800;refresher=uac"),
        )]);

        let value = get_header_value(&headers, HEADER_SESSION_EXPIRES);
        assert_eq!(value, Some("1800;refresher=uac".to_string()));
    }

    #[test]
    fn test_get_header_value_min_se() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_MIN_SE.to_string(),
            "90".to_string(),
        )]);

        let value = get_header_value(&headers, HEADER_MIN_SE);
        assert_eq!(value, Some("90".to_string()));
    }

    #[test]
    fn test_get_header_value_typed_min_se() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::MinSE(
            rsipstack::sip::headers::MinSE::new("90"),
        )]);

        let value = get_header_value(&headers, HEADER_MIN_SE);
        assert_eq!(value, Some("90".to_string()));
    }

    #[test]
    fn test_get_header_value_supported() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Supported(
            rsipstack::sip::headers::Supported::from("timer,100rel"),
        )]);

        let value = get_header_value(&headers, HEADER_SUPPORTED);
        assert!(value.is_some());
        assert!(value.unwrap().contains("timer"));
    }

    #[test]
    fn test_get_header_value_not_found() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        )]);

        let value = get_header_value(&headers, HEADER_SESSION_EXPIRES);
        assert!(value.is_none());
    }

    // ==================== Timer Support Detection Tests ====================

    #[test]
    fn test_has_timer_support_with_supported_header() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_SUPPORTED.to_string(),
            "timer,100rel".to_string(),
        )]);

        assert!(has_timer_support(&headers));
    }

    #[test]
    fn test_has_timer_support_with_other_header() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_SUPPORTED.to_string(),
            "timer,100rel".to_string(),
        )]);

        assert!(has_timer_support(&headers));
    }

    #[test]
    fn test_has_timer_support_without_timer() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Supported(
            rsipstack::sip::headers::Supported::from("100rel"),
        )]);

        assert!(!has_timer_support(&headers));
    }

    #[test]
    fn test_has_timer_support_no_supported_header() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        )]);

        assert!(!has_timer_support(&headers));
    }

    #[test]
    fn test_is_timer_required_with_require_header() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_REQUIRE.to_string(),
            "timer".to_string(),
        )]);

        assert!(is_timer_required(&headers));
    }

    #[test]
    fn test_is_timer_required_with_other_header() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_REQUIRE.to_string(),
            "timer".to_string(),
        )]);

        assert!(is_timer_required(&headers));
    }

    #[test]
    fn test_is_timer_required_without_timer() {
        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Require(
            rsipstack::sip::headers::Require::from("100rel"),
        )]);

        assert!(!is_timer_required(&headers));
    }

    #[test]
    fn test_build_session_timer_response_headers_do_not_require_timer_for_uac_refresher() {
        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uac;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::from(build_session_timer_response_headers(&timer));

        assert!(!is_timer_required(&headers));
        assert_eq!(
            get_header_value(&headers, HEADER_SESSION_EXPIRES),
            Some("1800;refresher=uac".to_string())
        );
    }

    #[test]
    fn test_build_session_timer_response_headers_no_require_for_uas_refresher() {
        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uas;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::from(build_session_timer_response_headers(&timer));

        assert!(!is_timer_required(&headers));
    }

    #[test]
    fn test_build_session_timer_response_headers_no_require_without_peer_support() {
        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uac;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::from(build_session_timer_response_headers(&timer));

        assert!(!is_timer_required(&headers));
    }

    #[test]
    fn test_build_default_session_timer_headers_include_supported_timer() {
        let headers = rsipstack::sip::Headers::from(build_default_session_timer_headers(1800, 90));

        assert!(has_timer_support(&headers));
        assert_eq!(
            get_header_value(&headers, HEADER_SESSION_EXPIRES),
            Some("1800".to_string())
        );
        assert_eq!(
            get_header_value(&headers, HEADER_MIN_SE),
            Some("90".to_string())
        );
    }

    #[test]
    fn test_build_session_timer_response_headers_omit_min_se() {
        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refresher = SessionRefresher::Uas;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::from(build_session_timer_response_headers(&timer));

        assert_eq!(
            get_header_value(&headers, HEADER_SESSION_EXPIRES),
            Some("1800;refresher=uas".to_string())
        );
        assert_eq!(get_header_value(&headers, HEADER_MIN_SE), None);
        assert!(!has_timer_support(&headers));
    }

    #[test]
    fn test_select_server_timer_refresher_defaults_to_uas_without_timer_support() {
        assert_eq!(
            select_server_timer_refresher(false, true, None),
            SessionRefresher::Uas
        );
        assert_eq!(
            select_server_timer_refresher(false, true, Some(SessionRefresher::Uac)),
            SessionRefresher::Uac
        );
    }

    #[test]
    fn test_select_server_timer_refresher_uses_request_when_peer_supports_timer() {
        assert_eq!(
            select_server_timer_refresher(true, true, None),
            SessionRefresher::Uac
        );
        assert_eq!(
            select_server_timer_refresher(true, true, Some(SessionRefresher::Uas)),
            SessionRefresher::Uas
        );
    }

    #[test]
    fn test_select_server_timer_refresher_defaults_to_uas_without_session_expires() {
        assert_eq!(
            select_server_timer_refresher(true, false, None),
            SessionRefresher::Uas
        );
    }

    #[test]
    fn test_select_client_timer_refresher_defaults_to_uac() {
        assert_eq!(select_client_timer_refresher(None), SessionRefresher::Uac);
        assert_eq!(
            select_client_timer_refresher(Some(SessionRefresher::Uas)),
            SessionRefresher::Uas
        );
    }

    #[test]
    fn test_apply_refresh_response_disables_timer_without_session_expires() {
        let mut timer = SessionTimerState::default();
        timer.mode = SessionTimerMode::Supported;
        timer.enabled = true;
        timer.active = true;
        timer.refreshing = true;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::default();
        let result = apply_refresh_response(&mut timer, &headers, false);

        assert!(result.is_ok());
        assert!(!timer.enabled);
        assert!(!timer.active);
        assert!(!timer.refreshing);
        assert_eq!(timer.refresh_count, 1);
    }

    #[test]
    fn test_apply_refresh_response_keeps_local_uas_without_session_expires_in_always_mode() {
        let mut timer = SessionTimerState::default();
        timer.mode = SessionTimerMode::Always;
        timer.enabled = true;
        timer.active = true;
        timer.refreshing = true;
        timer.refresher = SessionRefresher::Uas;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::default();
        let result = apply_refresh_response(&mut timer, &headers, false);

        assert!(result.is_ok());
        assert!(timer.enabled);
        assert!(timer.active);
        assert!(!timer.refreshing);
        assert_eq!(timer.refresher, SessionRefresher::Uas);
        assert_eq!(timer.refresh_count, 1);
    }

    #[test]
    fn test_apply_refresh_response_keeps_local_uac_without_session_expires_in_always_mode() {
        let mut timer = SessionTimerState::default();
        timer.mode = SessionTimerMode::Always;
        timer.enabled = true;
        timer.active = true;
        timer.refreshing = true;
        timer.refresher = SessionRefresher::Uas;
        timer.session_interval = Duration::from_secs(1800);

        let headers = rsipstack::sip::Headers::default();
        let result = apply_refresh_response(&mut timer, &headers, true);

        assert!(result.is_ok());
        assert!(timer.enabled);
        assert!(timer.active);
        assert!(!timer.refreshing);
        assert_eq!(timer.refresher, SessionRefresher::Uac);
        assert_eq!(timer.refresh_count, 1);
    }

    #[test]
    fn test_apply_refresh_response_keeps_timer_active_with_session_expires() {
        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refreshing = true;
        timer.session_interval = Duration::from_secs(1800);
        timer.min_se = Duration::from_secs(90);

        let headers = rsipstack::sip::Headers::from(vec![rsipstack::sip::Header::Other(
            HEADER_SESSION_EXPIRES.to_string(),
            "900;refresher=uas".to_string(),
        )]);
        let result = apply_refresh_response(&mut timer, &headers, false);

        assert!(result.is_ok());
        assert!(timer.enabled);
        assert!(timer.active);
        assert!(!timer.refreshing);
        assert_eq!(timer.session_interval, Duration::from_secs(900));
        assert_eq!(timer.refresher, SessionRefresher::Uas);
        assert_eq!(timer.refresh_count, 1);
    }

    // ==================== Integration Tests ====================

    #[test]
    fn test_full_refresh_cycle() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.refresher = SessionRefresher::Uas; // We are the refresher

        // Initial state - should not need refresh yet
        assert!(!timer.should_refresh());
        assert!(!timer.refreshing);

        // Simulate time passing past half interval
        timer.last_refresh = Instant::now() - Duration::from_secs(51);

        // Now should need refresh
        assert!(timer.should_refresh());

        // Start refresh
        assert!(timer.start_refresh());
        assert!(timer.refreshing);

        // Should not need refresh while refreshing
        assert!(!timer.should_refresh());

        // Complete refresh
        timer.complete_refresh();
        assert!(!timer.refreshing);
        assert_eq!(timer.refresh_count, 1);

        // Should not need refresh immediately after
        assert!(!timer.should_refresh());
    }

    #[test]
    fn test_refresh_failure_recovery() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            refresher: SessionRefresher::Uas,
            last_refresh: Instant::now() - Duration::from_secs(51),
            ..Default::default()
        };
        assert!(timer.start_refresh());
        assert!(timer.refreshing);

        // Fail the refresh
        timer.fail_refresh();
        assert!(!timer.refreshing);
        assert_eq!(timer.failed_refreshes, 1);

        // Can start another refresh
        assert!(timer.start_refresh());
        assert!(timer.refreshing);
    }

    #[test]
    fn test_remote_refresh_update() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(100),
            ..Default::default()
        };
        timer.refresher = SessionRefresher::Uac; // Remote is refresher
        timer.last_refresh = Instant::now() - Duration::from_millis(100);

        let old_refresh_time = timer.last_refresh;

        // Simulate receiving refresh from remote
        timer.update_refresh();

        assert!(timer.last_refresh > old_refresh_time);
        assert_eq!(timer.refresh_count, 1);
    }

    #[test]
    fn test_session_expires_various_intervals() {
        let test_cases = vec![
            (90, SessionRefresher::Uac, "90;refresher=uac"),
            (1800, SessionRefresher::Uas, "1800;refresher=uas"),
            (3600, SessionRefresher::Uac, "3600;refresher=uac"),
            (600, SessionRefresher::Uas, "600;refresher=uas"),
        ];

        for (secs, refresher, expected) in test_cases {
            let timer = SessionTimerState {
                session_interval: Duration::from_secs(secs),
                refresher,
                ..Default::default()
            };
            assert_eq!(timer.get_session_expires_value(), expected);
        }
    }

    #[test]
    fn test_concurrent_timer_access() {
        let timer = Arc::new(Mutex::new(SessionTimerState::default()));
        timer.lock().unwrap().enabled = true;
        timer.lock().unwrap().active = true;
        timer.lock().unwrap().session_interval = Duration::from_secs(100);

        // Simulate concurrent access
        let timer1 = timer.clone();
        let timer2 = timer.clone();

        // Thread 1: Check if refresh needed
        let _needs_refresh = {
            let t = timer1.lock().unwrap();
            t.should_refresh()
        };

        // Thread 2: Update refresh time
        {
            let mut t = timer2.lock().unwrap();
            t.update_refresh();
        }

        // Verify state
        let t = timer.lock().unwrap();
        assert_eq!(t.refresh_count, 1);
    }

    // ==================== RFC 4028 Compliance Tests ====================

    #[test]
    fn test_rfc4028_refresh_at_half_interval() {
        // RFC 4028: The refresher MUST send a refresh at half the session interval
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            refreshing: false,
            ..Default::default()
        };
        for interval_secs in [300, 600, 900, 1800, 3600] {
            timer.session_interval = Duration::from_secs(interval_secs);

            // At exactly half, should refresh
            timer.last_refresh = Instant::now() - Duration::from_secs(interval_secs / 2);
            assert!(
                timer.should_refresh(),
                "Should refresh at exactly half of {}s interval",
                interval_secs
            );

            // Just before half, should not refresh
            timer.last_refresh = Instant::now() - Duration::from_secs(interval_secs / 2 - 1);
            assert!(
                !timer.should_refresh(),
                "Should NOT refresh just before half of {}s interval",
                interval_secs
            );
        }
    }

    #[test]
    fn test_rfc4028_session_expires_if_no_refresh() {
        // RFC 4028: If no refresh is received within the session interval, session is expired
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            session_interval: Duration::from_secs(1800),
            last_refresh: Instant::now() - Duration::from_secs(1800),
            ..Default::default()
        };
        assert!(
            timer.is_expired(),
            "Should be expired at exactly full interval"
        );

        // Past full interval
        timer.last_refresh = Instant::now() - Duration::from_secs(2000);
        assert!(timer.is_expired(), "Should be expired past full interval");

        // Before full interval
        timer.last_refresh = Instant::now() - Duration::from_secs(1799);
        assert!(
            !timer.is_expired(),
            "Should NOT be expired before full interval"
        );
    }
}
