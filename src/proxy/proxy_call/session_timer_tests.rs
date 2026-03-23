#[cfg(test)]
mod tests {
    use crate::proxy::proxy_call::session_timer::{
        SessionRefresher, SessionTimerState, parse_min_se, parse_session_expires,
    };
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn test_session_timer_state_memory() {
        let timer = Arc::new(Mutex::new(SessionTimerState {
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now(),
            refreshing: false,
        }));

        let timer_clone = timer.clone();
        assert_eq!(Arc::strong_count(&timer), 2);

        drop(timer_clone);
        assert_eq!(Arc::strong_count(&timer), 1);
    }

    #[test]
    fn test_should_refresh_at_half_interval() {
        let mut timer = SessionTimerState::default();
        timer.enabled = true;
        timer.active = true;
        timer.refreshing = false;
        timer.session_interval = Duration::from_secs(1800); // 30 minutes
        timer.last_refresh = Instant::now() - Duration::from_secs(900); // exactly half

        // At exactly half interval, should_refresh should return true (RFC 4028: refresh at half)
        assert!(
            timer.should_refresh(),
            "Should refresh when elapsed >= half interval"
        );
    }

    #[test]
    fn test_should_refresh_just_past_half_interval() {
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.refreshing = false;
        timer.session_interval = Duration::from_secs(1800);

        // Just past half (901 seconds out of 1800)
        timer.last_refresh = Instant::now() - Duration::from_secs(901);
        assert!(timer.should_refresh());
    }

    #[test]
    fn test_should_not_refresh_before_half_interval() {
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.refreshing = false;
        timer.session_interval = Duration::from_secs(1800);

        // Just before half (899 seconds out of 1800)
        timer.last_refresh = Instant::now() - Duration::from_secs(899);
        assert!(
            !timer.should_refresh(),
            "Should NOT refresh before half interval"
        );
    }

    #[test]
    fn test_should_not_refresh_when_refreshing_flag_is_set() {
        let mut timer = SessionTimerState::default();
        timer.active = true;
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
        let mut timer = SessionTimerState::default();
        timer.active = false; // Not active
        timer.refreshing = false;
        timer.session_interval = Duration::from_secs(1800);
        timer.last_refresh = Instant::now() - Duration::from_secs(1000);

        assert!(
            !timer.should_refresh(),
            "Should NOT refresh when timer is not active"
        );
    }

    #[test]
    fn test_is_expired_after_full_interval() {
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.session_interval = Duration::from_secs(100);

        // Just past full interval
        timer.last_refresh = Instant::now() - Duration::from_secs(101);
        assert!(timer.is_expired(), "Should be expired after full interval");
    }

    #[test]
    fn test_is_not_expired_before_full_interval() {
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.session_interval = Duration::from_secs(100);

        // Just before full interval
        timer.last_refresh = Instant::now() - Duration::from_secs(99);
        assert!(
            !timer.is_expired(),
            "Should NOT be expired before full interval"
        );
    }

    #[test]
    fn test_is_not_expired_when_inactive() {
        let mut timer = SessionTimerState::default();
        timer.active = false;
        timer.session_interval = Duration::from_secs(100);
        timer.last_refresh = Instant::now() - Duration::from_secs(200);

        assert!(
            !timer.is_expired(),
            "Should NOT be expired when timer is not active"
        );
    }

    #[test]
    fn test_update_refresh_sets_last_refresh_to_now() {
        let mut timer = SessionTimerState::default();
        timer.last_refresh = Instant::now() - Duration::from_secs(1000);

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
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.refreshing = false;
        timer.session_interval = Duration::from_secs(1800);
        timer.last_refresh = Instant::now() - Duration::from_secs(1000);

        // should_refresh returns true when past half interval, regardless of refresher role
        assert!(
            timer.should_refresh(),
            "should_refresh returns true when past half interval"
        );
    }

    #[test]
    fn test_should_refresh_with_uac_refresher() {
        // UAC refresher - the UAC is responsible for sending refresh
        // But should_refresh() doesn't check this - the caller does
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.refreshing = false;
        timer.refresher = SessionRefresher::Uac;
        timer.session_interval = Duration::from_secs(1800);
        timer.last_refresh = Instant::now() - Duration::from_secs(1000);

        // should_refresh only checks time, not who the refresher is
        assert!(timer.should_refresh());
    }

    #[test]
    fn test_should_refresh_with_uas_refresher() {
        // UAS refresher - the UAS is responsible for sending refresh
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.refreshing = false;
        timer.refresher = SessionRefresher::Uas;
        timer.session_interval = Duration::from_secs(1800);
        timer.last_refresh = Instant::now() - Duration::from_secs(1000);

        // should_refresh only checks time, not who the refresher is
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
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now() - Duration::from_secs(100),
            refreshing: false,
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
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now() - Duration::from_secs(46), // Just past half (45s)
            refreshing: false,
        };

        // Logic from handle_server_events
        let mut next_time = None;
        if timer.active {
            if timer.refresher == SessionRefresher::Uas {
                let refresh_at = timer.last_refresh + (timer.session_interval / 2);
                next_time = Some(refresh_at);
            }
        }

        if let Some(next_refresh) = next_time {
            if Instant::now() >= next_refresh {
                // Simulate refresh
                timer.last_refresh = Instant::now();
            }
        }

        // Should be refreshed now
        assert!(Instant::now() < timer.last_refresh + Duration::from_secs(1));
    }

    #[test]
    fn test_session_timer_methods() {
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.session_interval = Duration::from_secs(100);
        timer.last_refresh = Instant::now() - Duration::from_secs(51);

        assert!(timer.should_refresh());
        assert!(!timer.is_expired());

        timer.last_refresh = Instant::now() - Duration::from_secs(101);
        assert!(timer.is_expired());
    }

    // ==================== RFC 4028 Compliance Tests ====================

    #[test]
    fn test_rfc4028_refresh_at_half_interval() {
        // RFC 4028: The refresher MUST send a refresh at half the session interval
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.refreshing = false;

        // Test with various intervals
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
        let mut timer = SessionTimerState::default();
        timer.active = true;
        timer.session_interval = Duration::from_secs(1800);

        // At exactly full interval
        timer.last_refresh = Instant::now() - Duration::from_secs(1800);
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
