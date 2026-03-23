//! Session Timer implementation (RFC 4028)
//! 
//! Note: This module provides Session Timer support but is currently
//! not fully integrated into SipSession. The infrastructure
//! is in place but timer refresh logic needs to be added to the
//! session processing loop.

use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

// Session timer header constants
#[allow(dead_code)]
pub const HEADER_SESSION_EXPIRES: &str = "Session-Expires";
#[allow(dead_code)]
pub const HEADER_MIN_SE: &str = "Min-SE";
#[allow(dead_code)]
pub const HEADER_SUPPORTED: &str = "Supported";
#[allow(dead_code)]
pub const TIMER_TAG: &str = "timer";

#[cfg(test)]
#[path = "session_timer_tests.rs"]
mod session_timer_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRefresher {
    Uac,
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

#[derive(Debug, Clone)]
pub struct SessionTimerState {
    /// Timer is enabled ( negotiated via Session-Expires header)
    #[allow(dead_code)]
    pub enabled: bool,
    /// Session expiration interval
    pub session_interval: Duration,
    /// Minimum session expiration (from Min-SE header)
    #[allow(dead_code)]
    pub min_se: Duration,
    /// Who is responsible for refreshing (UAC or UAS)
    #[allow(dead_code)]
    pub refresher: SessionRefresher,
    /// Timer is actively running
    pub active: bool,
    /// Currently in the process of refreshing
    pub refreshing: bool,
    /// Last time the session was refreshed
    pub last_refresh: Instant,
}

impl Default for SessionTimerState {
    fn default() -> Self {
        Self {
            enabled: false,
            session_interval: Duration::from_secs(1800),
            min_se: Duration::from_secs(90),
            refresher: SessionRefresher::Uac,
            active: false,
            refreshing: false,
            last_refresh: Instant::now(),
        }
    }
}

impl SessionTimerState {
    /// Check if a refresh should be sent (RFC 4028)
    /// Returns true if we are the refresher and it's time to send a refresh
    #[allow(dead_code)]
    pub fn should_refresh(&self) -> bool {
        if !self.active || self.refreshing {
            return false;
        }
        // RFC 4028: Refresher should send refresh at half the interval
        self.last_refresh.elapsed() >= self.session_interval / 2
    }

    /// Check if the session has expired (no refresh received)
    #[allow(dead_code)]
    pub fn is_expired(&self) -> bool {
        if !self.active {
            return false;
        }
        // RFC 4028: If no refresh received within interval, session is expired
        self.last_refresh.elapsed() >= self.session_interval
    }

    /// Update the last refresh time when a refresh is sent/received
    #[allow(dead_code)]
    pub fn update_refresh(&mut self) {
        self.last_refresh = Instant::now();
    }
}

/// Get header value by name (case-insensitive)
#[allow(dead_code)]
pub fn get_header_value(headers: &rsip::Headers, name: &str) -> Option<String> {
    headers.iter().find_map(|h| match h {
        rsip::Header::Other(n, v) if n.eq_ignore_ascii_case(name) => Some(v.clone()),
        rsip::Header::Supported(h) if name.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            Some(h.to_string())
        }
        _ => None,
    })
}

/// Parse Session-Expires header value
/// Returns (interval, refresher) where refresher is optional
#[allow(dead_code)]
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

/// Parse Min-SE header value
#[allow(dead_code)]
pub fn parse_min_se(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Check if the message has timer support (Supported: timer header)
#[allow(dead_code)]
pub fn has_timer_support(headers: &rsip::Headers) -> bool {
    headers.iter().any(|h| match h {
        rsip::Header::Supported(s) => s.to_string().split(',').any(|v| v.trim() == TIMER_TAG),
        rsip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            v.split(',').any(|v| v.trim() == TIMER_TAG)
        }
        _ => false,
    })
}
