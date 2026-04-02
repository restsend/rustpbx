//! Session Timer implementation (RFC 4028)
//!
//! This module implements SIP Session Timers as defined in RFC 4028.
//! Session timers are used to detect and recover from hung SIP sessions
//! by requiring periodic session refresh requests.

use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

// Session timer header constants
pub const HEADER_SESSION_EXPIRES: &str = "Session-Expires";
pub const HEADER_MIN_SE: &str = "Min-SE";
pub const HEADER_SUPPORTED: &str = "Supported";
pub const HEADER_REQUIRE: &str = "Require";
pub const TIMER_TAG: &str = "timer";

/// Default session expiration interval (30 minutes per RFC 4028 recommendation)
pub const DEFAULT_SESSION_EXPIRES: u64 = 1800;

/// Minimum acceptable session expiration interval (90 seconds per RFC 4028)
pub const MIN_MIN_SE: u64 = 90;

/// Refresher role - determines which party is responsible for session refresh
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRefresher {
    /// User Agent Client (caller) refreshes
    Uac,
    /// User Agent Server (callee) refreshes
    Uas,
}

impl std::fmt::Display for SessionRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionRefresher::Uac => write!(f, "uac"),
            SessionRefresher::Uas => write!(f, "uas"),
        }
    }
}

impl FromStr for SessionRefresher {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "uac" => Ok(SessionRefresher::Uac),
            "uas" => Ok(SessionRefresher::Uas),
            _ => Err(()),
        }
    }
}

impl SessionRefresher {
    /// Check if we are the refresher based on our role
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn is_our_role(&self, we_are_uac: bool) -> bool {
        matches!(
            (self, we_are_uac),
            (SessionRefresher::Uac, true) | (SessionRefresher::Uas, false)
        )
    }
}

/// Session timer state machine
#[derive(Debug, Clone)]
pub struct SessionTimerState {
    /// Timer is enabled (negotiated via Session-Expires header)
    pub enabled: bool,
    /// Session expiration interval
    pub session_interval: Duration,
    /// Minimum session expiration (from Min-SE header)
    pub min_se: Duration,
    /// Who is responsible for refreshing (UAC or UAS)
    pub refresher: SessionRefresher,
    /// Timer is actively running
    pub active: bool,
    /// Currently in the process of refreshing
    pub refreshing: bool,
    /// Last time the session was refreshed
    pub last_refresh: Instant,
    /// Session start time (used for testing)
    #[cfg(test)]
    pub session_start: Instant,
    /// Number of successful refreshes
    pub refresh_count: u32,
    /// Number of failed refresh attempts
    pub failed_refreshes: u32,
}

impl Default for SessionTimerState {
    fn default() -> Self {
        Self {
            enabled: false,
            session_interval: Duration::from_secs(DEFAULT_SESSION_EXPIRES),
            min_se: Duration::from_secs(MIN_MIN_SE),
            refresher: SessionRefresher::Uac,
            active: false,
            refreshing: false,
            last_refresh: Instant::now(),
            #[cfg(test)]
            session_start: Instant::now(),
            refresh_count: 0,
            failed_refreshes: 0,
        }
    }
}

impl SessionTimerState {
    /// Create a new session timer state with specific interval
    #[cfg(test)]
    pub fn new(session_interval: Duration, min_se: Duration, refresher: SessionRefresher) -> Self {
        Self {
            enabled: true,
            session_interval,
            min_se,
            refresher,
            active: true,
            refreshing: false,
            last_refresh: Instant::now(),
            session_start: Instant::now(),
            refresh_count: 0,
            failed_refreshes: 0,
        }
    }

    /// Check if a refresh should be sent (RFC 4028)
    /// Returns true if we are the refresher and it's time to send a refresh
    pub fn should_refresh(&self) -> bool {
        if !self.active || !self.enabled || self.refreshing {
            return false;
        }
        // RFC 4028: Refresher should send refresh at half the interval
        self.last_refresh.elapsed() >= self.session_interval / 2
    }

    /// Check if the session has expired (no refresh received)
    pub fn is_expired(&self) -> bool {
        if !self.active || !self.enabled {
            return false;
        }
        // RFC 4028: If no refresh received within interval, session is expired
        self.last_refresh.elapsed() >= self.session_interval
    }

    /// Get the time when next refresh should be sent
    pub fn next_refresh_time(&self) -> Option<Instant> {
        if !self.active || !self.enabled {
            return None;
        }
        Some(self.last_refresh + self.session_interval / 2)
    }

    /// Get the time when session will expire
    pub fn expiration_time(&self) -> Option<Instant> {
        if !self.active || !self.enabled {
            return None;
        }
        Some(self.last_refresh + self.session_interval)
    }

    /// Get remaining time until expiration
    pub fn time_until_expiration(&self) -> Option<Duration> {
        self.expiration_time().map(|exp| {
            let now = Instant::now();
            if exp > now {
                exp - now
            } else {
                Duration::ZERO
            }
        })
    }

    /// Get remaining time until next refresh is needed
    pub fn time_until_refresh(&self) -> Option<Duration> {
        self.next_refresh_time().map(|next| {
            let now = Instant::now();
            if next > now {
                next - now
            } else {
                Duration::ZERO
            }
        })
    }

    /// Start a refresh operation
    pub fn start_refresh(&mut self) -> bool {
        if self.refreshing {
            return false;
        }
        self.refreshing = true;
        true
    }

    /// Complete a successful refresh
    pub fn complete_refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.refreshing = false;
        self.refresh_count += 1;
    }

    /// Mark refresh as failed
    pub fn fail_refresh(&mut self) {
        self.refreshing = false;
        self.failed_refreshes += 1;
    }

    /// Update the last refresh time when a refresh is received from remote
    pub fn update_refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.refresh_count += 1;
    }

    /// Generate Session-Expires header value
    pub fn get_session_expires_value(&self) -> String {
        format!("{};refresher={}", self.session_interval.as_secs(), self.refresher)
    }

    /// Generate Min-SE header value
    pub fn get_min_se_value(&self) -> String {
        self.min_se.as_secs().to_string()
    }

    /// Activate the timer
    #[cfg(test)]
    pub fn activate(&mut self) {
        if self.enabled {
            self.active = true;
            self.last_refresh = Instant::now();
        }
    }

    /// Deactivate the timer
    #[cfg(test)]
    pub fn deactivate(&mut self) {
        self.active = false;
    }

    /// Reset the timer with new parameters
    #[cfg(test)]
    pub fn reset(&mut self, interval: Duration, refresher: SessionRefresher) {
        self.session_interval = interval;
        self.refresher = refresher;
        self.last_refresh = Instant::now();
        self.refreshing = false;
    }

    /// Check if we need to include timer in Require header
    #[cfg(test)]
    pub fn require_timer(&self) -> bool {
        self.enabled && self.active
    }

    /// Get session duration
    #[cfg(test)]
    pub fn session_duration(&self) -> Duration {
        self.session_start.elapsed()
    }

    /// Get timer statistics
    #[cfg(test)]
    pub fn stats(&self) -> TimerStats {
        TimerStats {
            enabled: self.enabled,
            active: self.active,
            refreshing: self.refreshing,
            session_interval_secs: self.session_interval.as_secs(),
            min_se_secs: self.min_se.as_secs(),
            refresher: format!("{}", self.refresher),
            refresh_count: self.refresh_count,
            failed_refreshes: self.failed_refreshes,
            session_duration_secs: self.session_duration().as_secs(),
            time_until_refresh_secs: self.time_until_refresh().map(|d| d.as_secs()),
            time_until_expiration_secs: self.time_until_expiration().map(|d| d.as_secs()),
        }
    }
}

/// Timer statistics for diagnostics
#[derive(Debug, Clone, serde::Serialize)]
#[cfg(test)]
pub struct TimerStats {
    pub enabled: bool,
    pub active: bool,
    pub refreshing: bool,
    pub session_interval_secs: u64,
    pub min_se_secs: u64,
    pub refresher: String,
    pub refresh_count: u32,
    pub failed_refreshes: u32,
    pub session_duration_secs: u64,
    pub time_until_refresh_secs: Option<u64>,
    pub time_until_expiration_secs: Option<u64>,
}

/// Get header value by name (case-insensitive)
pub fn get_header_value(headers: &rsipstack::sip::Headers, name: &str) -> Option<String> {
    headers.iter().find_map(|h| match h {
        rsipstack::sip::Header::Other(n, v) if n.eq_ignore_ascii_case(name) => Some(v.clone()),
        rsipstack::sip::Header::Supported(h) if name.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            Some(h.to_string())
        }
        rsipstack::sip::Header::Require(h) if name.eq_ignore_ascii_case(HEADER_REQUIRE) => {
            Some(h.to_string())
        }
        _ => None,
    })
}

/// Parse Session-Expires header value
/// Returns (interval, refresher) where refresher is optional
pub fn parse_session_expires(value: &str) -> Option<(Duration, Option<SessionRefresher>)> {
    let parts: Vec<&str> = value.split(';').collect();
    if parts.is_empty() {
        return None;
    }

    let seconds = parts[0].trim().parse::<u64>().ok()?;
    let mut refresher = None;

    for part in parts.iter().skip(1) {
        let part = part.trim();
        if part.starts_with("refresher=") {
            let val = part.trim_start_matches("refresher=");
            refresher = SessionRefresher::from_str(val).ok();
        }
    }

    Some((Duration::from_secs(seconds), refresher))
}

/// Check if the message has timer support (Supported: timer header)
pub fn has_timer_support(headers: &rsipstack::sip::Headers) -> bool {
    headers.iter().any(|h| match h {
        rsipstack::sip::Header::Supported(s) => s.to_string().split(',').any(|v| v.trim() == TIMER_TAG),
        rsipstack::sip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            v.split(',').any(|v| v.trim() == TIMER_TAG)
        }
        _ => false,
    })
}

/// Parse Min-SE header value
#[cfg(test)]
pub fn parse_min_se(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Check if timer is required (Require: timer header)
#[cfg(test)]
pub fn is_timer_required(headers: &rsipstack::sip::Headers) -> bool {
    headers.iter().any(|h| match h {
        rsipstack::sip::Header::Require(s) => s.to_string().split(',').any(|v| v.trim() == TIMER_TAG),
        rsipstack::sip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_REQUIRE) => {
            v.split(',').any(|v| v.trim() == TIMER_TAG)
        }
        _ => false,
    })
}

/// Build Session-Expires header
#[cfg(test)]
pub fn build_session_expires_header(interval: Duration, refresher: SessionRefresher) -> rsipstack::sip::Header {
    let value = format!("{};refresher={}", interval.as_secs(), refresher);
    rsipstack::sip::Header::Other(HEADER_SESSION_EXPIRES.to_string(), value)
}

/// Build Min-SE header
#[cfg(test)]
pub fn build_min_se_header(min_se: Duration) -> rsipstack::sip::Header {
    rsipstack::sip::Header::Other(HEADER_MIN_SE.to_string(), min_se.as_secs().to_string())
}

/// Calculate the appropriate session interval based on negotiation
/// Returns Ok(interval) or Err(min_se) if the requested interval is too small
#[cfg(test)]
pub fn negotiate_session_interval(
    requested: Duration,
    local_min_se: Duration,
) -> Result<Duration, Duration> {
    if requested < local_min_se {
        Err(local_min_se)
    } else {
        Ok(requested)
    }
}

#[cfg(test)]
#[path = "session_timer_tests.rs"]
mod session_timer_tests;
