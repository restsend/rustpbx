#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use crate::proxy::proxy_call::session_timer::{SessionTimerState, SessionRefresher};

    #[test]
    fn test_session_timer_state_memory() {
        let timer = Arc::new(Mutex::new(SessionTimerState {
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now(),
        }));

        let timer_clone = timer.clone();
        assert_eq!(Arc::strong_count(&timer), 2);

        drop(timer_clone);
        assert_eq!(Arc::strong_count(&timer), 1);
    }

    #[test]
    fn test_session_timer_expiration_logic() {
        let mut timer = SessionTimerState {
            enabled: true,
            active: true,
            refresher: SessionRefresher::Uas,
            session_interval: Duration::from_secs(90),
            min_se: Duration::from_secs(90),
            last_refresh: Instant::now() - Duration::from_secs(100),
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
}
