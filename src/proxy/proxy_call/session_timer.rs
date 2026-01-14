use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

pub const HEADER_SESSION_EXPIRES: &str = "Session-Expires";
pub const HEADER_MIN_SE: &str = "Min-SE";
pub const HEADER_SUPPORTED: &str = "Supported";
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
    pub enabled: bool,
    pub session_interval: Duration,
    #[allow(dead_code)]
    pub min_se: Duration,
    pub refresher: SessionRefresher,
    pub active: bool,
    pub refreshing: bool,
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
    pub fn should_refresh(&self) -> bool {
        if !self.active || self.refreshing {
            return false;
        }
        // RFC 4028: Refresher should send refresh at half the interval
        self.last_refresh.elapsed() >= self.session_interval / 2
    }

    pub fn is_expired(&self) -> bool {
        if !self.active {
            return false;
        }
        // RFC 4028: If no refresh received within interval, session is expired
        self.last_refresh.elapsed() >= self.session_interval
    }

    pub fn update_refresh(&mut self) {
        self.last_refresh = Instant::now();
    }
}

pub fn get_header_value(headers: &rsip::Headers, name: &str) -> Option<String> {
    headers.iter().find_map(|h| match h {
        rsip::Header::Other(n, v) if n.eq_ignore_ascii_case(name) => Some(v.clone()),
        rsip::Header::Supported(h) if name.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            Some(h.to_string())
        }
        _ => None,
    })
}

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

pub fn parse_min_se(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

pub fn has_timer_support(headers: &rsip::Headers) -> bool {
    headers.iter().any(|h| match h {
        rsip::Header::Supported(s) => s.to_string().split(',').any(|v| v.trim() == TIMER_TAG),
        rsip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            v.split(',').any(|v| v.trim() == TIMER_TAG)
        }
        _ => false,
    })
}
