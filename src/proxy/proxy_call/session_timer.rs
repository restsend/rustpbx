//! Session Timer implementation (RFC 4028)
//!
//! This module implements SIP Session Timers as defined in RFC 4028.
//! Session timers are used to detect and recover from hung SIP sessions
//! by requiring periodic session refresh requests.

use crate::config::SessionTimerMode;
use anyhow::{Result, anyhow};
use std::time::Duration;
use std::time::Instant;

// Session timer header constants
pub const HEADER_SESSION_EXPIRES: &str = "Session-Expires";
pub const HEADER_MIN_SE: &str = "Min-SE";
pub const HEADER_SUPPORTED: &str = "Supported";
#[cfg(test)]
pub const HEADER_REQUIRE: &str = "Require";
pub const TIMER_TAG: &str = "timer";

/// Default session expiration interval (30 minutes per RFC 4028 recommendation)
pub const DEFAULT_SESSION_EXPIRES: u64 = 1800;

/// Minimum acceptable session expiration interval (90 seconds per RFC 4028)
pub const MIN_MIN_SE: u64 = 90;

/// Which endpoint is responsible for refreshing the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRefresher {
    Local,
    Remote,
}

impl std::fmt::Display for SessionRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionRefresher::Local => write!(f, "local"),
            SessionRefresher::Remote => write!(f, "remote"),
        }
    }
}

/// Transaction-relative refresher value in a Session-Expires header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRefresherParam {
    Uac,
    Uas,
}

/// Parsed and generated Session-Expires header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionExpires {
    pub interval: Duration,
    pub refresher: Option<SessionRefresherParam>,
}

impl SessionExpires {
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(';');
        let interval = Duration::from_secs(parts.next()?.trim().parse::<u64>().ok()?);
        let mut refresher = None;

        for part in parts {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("refresher") {
                refresher = match value.trim().to_ascii_lowercase().as_str() {
                    "uac" => Some(SessionRefresherParam::Uac),
                    "uas" => Some(SessionRefresherParam::Uas),
                    _ => None,
                };
            }
        }

        Some(Self {
            interval,
            refresher,
        })
    }

    pub fn value(self) -> String {
        match self.refresher {
            Some(SessionRefresherParam::Uac) => {
                format!("{};refresher=uac", self.interval.as_secs())
            }
            Some(SessionRefresherParam::Uas) => {
                format!("{};refresher=uas", self.interval.as_secs())
            }
            None => self.interval.as_secs().to_string(),
        }
    }

    pub fn into_header(self) -> rsipstack::sip::Header {
        rsipstack::sip::Header::Other(HEADER_SESSION_EXPIRES.to_string(), self.value())
    }
}

/// Session timer state machine
#[derive(Debug, Clone)]
pub struct SessionTimerState {
    /// Session timer policy for this dialog
    pub mode: SessionTimerMode,
    /// Timer is enabled (negotiated via Session-Expires header)
    pub enabled: bool,
    /// Session expiration interval
    pub session_interval: Duration,
    /// Minimum session expiration (from Min-SE header)
    pub min_se: Duration,
    /// Who is responsible for refreshing (local or remote endpoint)
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
            mode: SessionTimerMode::Off,
            enabled: false,
            session_interval: Duration::from_secs(DEFAULT_SESSION_EXPIRES),
            min_se: Duration::from_secs(MIN_MIN_SE),
            refresher: SessionRefresher::Local,
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
            mode: SessionTimerMode::Supported,
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
            if exp > now { exp - now } else { Duration::ZERO }
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

    /// Check if we are responsible for refreshing this dialog
    pub fn should_we_refresh(&self) -> bool {
        self.refresher == SessionRefresher::Local
    }

    /// Get the next wakeup timeout for our role on this dialog
    pub fn next_timeout(&self) -> Option<Duration> {
        if !self.active || !self.enabled {
            return None;
        }

        if !self.refreshing && self.should_we_refresh() {
            self.time_until_refresh()
        } else {
            self.time_until_expiration()
        }
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

    /// Generate Min-SE header value
    pub fn get_min_se_value(&self) -> String {
        self.min_se.as_secs().to_string()
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
    headers
        .iter()
        .find(|header| header.name().eq_ignore_ascii_case(name))
        .map(|header| header.value().to_string())
}

/// Check if the message has timer support (Supported: timer header)
pub fn has_timer_support(headers: &rsipstack::sip::Headers) -> bool {
    headers.iter().any(|h| match h {
        rsipstack::sip::Header::Supported(s) => s.value().split(',').any(|v| v.trim() == TIMER_TAG),
        rsipstack::sip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_SUPPORTED) => {
            v.split(',').any(|v| v.trim() == TIMER_TAG)
        }
        _ => false,
    })
}

/// Parse Min-SE header value
pub fn parse_min_se(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

pub fn select_server_timer_refresher(
    peer_supports_timer: bool,
    session_expires_present: bool,
    requested_refresher: Option<SessionRefresherParam>,
) -> SessionRefresher {
    if let Some(refresher) = requested_refresher {
        match refresher {
            SessionRefresherParam::Uac => SessionRefresher::Remote,
            SessionRefresherParam::Uas => SessionRefresher::Local,
        }
    } else if peer_supports_timer && session_expires_present {
        SessionRefresher::Remote
    } else {
        SessionRefresher::Local
    }
}

pub fn select_client_timer_refresher(
    response_refresher: Option<SessionRefresherParam>,
) -> SessionRefresher {
    match response_refresher {
        Some(SessionRefresherParam::Uas) => SessionRefresher::Remote,
        Some(SessionRefresherParam::Uac) | None => SessionRefresher::Local,
    }
}

pub fn apply_session_timer_headers(
    timer: &mut SessionTimerState,
    headers: &rsipstack::sip::Headers,
) -> Result<()> {
    if let Some(se_value) = get_header_value(headers, HEADER_SESSION_EXPIRES)
        && let Some(session_expires) = SessionExpires::parse(&se_value)
    {
        if session_expires.interval < timer.min_se {
            return Err(anyhow!(
                "Session-Expires too small: {} < {}",
                session_expires.interval.as_secs(),
                timer.min_se.as_secs()
            ));
        }

        timer.session_interval = session_expires.interval;
        if let Some(refresher) = session_expires.refresher {
            timer.refresher = match refresher {
                SessionRefresherParam::Uac => SessionRefresher::Remote,
                SessionRefresherParam::Uas => SessionRefresher::Local,
            };
        }
    }

    Ok(())
}

pub fn apply_refresh_response(
    timer: &mut SessionTimerState,
    headers: &rsipstack::sip::Headers,
) -> Result<()> {
    if get_header_value(headers, HEADER_SESSION_EXPIRES).is_none() {
        timer.complete_refresh();
        if timer.mode.is_always() {
            // Keep the local side responsible for refreshes when always mode is forcing
            // session timers but the peer omits Session-Expires in a successful refresh.
            timer.refresher = SessionRefresher::Local;
        } else {
            timer.enabled = false;
            timer.active = false;
        }
        return Ok(());
    }

    if let Some(se_value) = get_header_value(headers, HEADER_SESSION_EXPIRES)
        && let Some(session_expires) = SessionExpires::parse(&se_value)
    {
        if session_expires.interval < timer.min_se {
            timer.fail_refresh();
            return Err(anyhow!(
                "Session-Expires too small: {} < {}",
                session_expires.interval.as_secs(),
                timer.min_se.as_secs()
            ));
        }

        timer.session_interval = session_expires.interval;
        if let Some(refresher) = session_expires.refresher {
            timer.refresher = select_client_timer_refresher(Some(refresher));
        }
    }

    timer.complete_refresh();
    Ok(())
}

fn build_timer_headers(
    session_expires: SessionExpires,
    min_se: String,
    include_content_type: bool,
) -> Vec<rsipstack::sip::Header> {
    let mut headers = Vec::new();
    if include_content_type {
        headers.push(rsipstack::sip::Header::ContentType(
            "application/sdp".into(),
        ));
    }
    headers.push(session_expires.into_header());
    headers.push(rsipstack::sip::Header::Other(
        HEADER_MIN_SE.to_string(),
        min_se,
    ));
    headers.push(rsipstack::sip::Header::Supported(
        rsipstack::sip::headers::Supported::from(TIMER_TAG),
    ));
    headers
}

pub fn build_default_session_timer_headers(
    session_expires: u64,
    min_se: u64,
) -> Vec<rsipstack::sip::Header> {
    build_timer_headers(
        SessionExpires {
            interval: Duration::from_secs(session_expires),
            refresher: None,
        },
        min_se.to_string(),
        false,
    )
}

pub fn build_session_timer_headers(
    timer: &SessionTimerState,
    include_content_type: bool,
) -> Vec<rsipstack::sip::Header> {
    build_timer_headers(
        SessionExpires {
            interval: timer.session_interval,
            refresher: Some(match timer.refresher {
                SessionRefresher::Local => SessionRefresherParam::Uac,
                SessionRefresher::Remote => SessionRefresherParam::Uas,
            }),
        },
        timer.get_min_se_value(),
        include_content_type,
    )
}

pub fn build_session_timer_response_headers(
    timer: &SessionTimerState,
) -> Vec<rsipstack::sip::Header> {
    let mut headers = vec![SessionExpires {
        interval: timer.session_interval,
        refresher: Some(match timer.refresher {
            SessionRefresher::Local => SessionRefresherParam::Uas,
            SessionRefresher::Remote => SessionRefresherParam::Uac,
        }),
    }
    .into_header()];

    if timer.refresher == SessionRefresher::Remote {
        headers.push(rsipstack::sip::Header::Require(
            rsipstack::sip::headers::Require::from(TIMER_TAG),
        ));
    }

    headers
}

/// Check if timer is required (Require: timer header)
#[cfg(test)]
pub fn is_timer_required(headers: &rsipstack::sip::Headers) -> bool {
    headers.iter().any(|h| match h {
        rsipstack::sip::Header::Require(s) => s.value().split(',').any(|v| v.trim() == TIMER_TAG),
        rsipstack::sip::Header::Other(n, v) if n.eq_ignore_ascii_case(HEADER_REQUIRE) => {
            v.split(',').any(|v| v.trim() == TIMER_TAG)
        }
        _ => false,
    })
}

/// Build Session-Expires header
#[cfg(test)]
pub fn build_session_expires_header(
    interval: Duration,
    refresher: SessionRefresherParam,
) -> rsipstack::sip::Header {
    SessionExpires {
        interval,
        refresher: Some(refresher),
    }
    .into_header()
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
