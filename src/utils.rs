use sea_orm::sea_query::{Func, IntoCondition, SimpleExpr};
use std::net::IpAddr;
use std::sync::atomic::{AtomicI64, Ordering};

/// Strip control characters (`\r`, `\n`, `\0`, etc.) from a header value
/// to prevent HTTP response splitting / header injection.
pub fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() && *c != '\r' && *c != '\n')
        .collect()
}

/// Validate that a domain name contains only DNS-safe characters.
/// Returns `true` if valid, `false` if it contains `..`, `/`, `\`, or other dangerous chars.
pub fn validate_domain(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > 253 {
        return false;
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return false;
    }
    domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Check whether a URL points to a private / loopback / link-local IP address
/// to prevent Server-Side Request Forgery (SSRF) to internal networks.
pub fn is_url_ssrf_safe(url: &str) -> bool {
    let url_lower = url.trim().to_lowercase();
    if !url_lower.starts_with("http://") && !url_lower.starts_with("https://") {
        return false;
    }

    // Strip protocol
    let rest = url_lower
        .strip_prefix("https://")
        .or_else(|| url_lower.strip_prefix("http://"))
        .unwrap_or(&url_lower);

    // Extract hostname (up to first /, ?, or :)
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    if host.is_empty() {
        return false;
    }

    // Block hostnames that look like internal addresses
    if host == "localhost" || host == "localhost6" || host == "127.0.0.1" || host == "::1" {
        return false;
    }

    // Block internal TLDs
    if host.ends_with(".local") || host.ends_with(".internal") || host.ends_with(".lan") {
        return false;
    }

    // Try parsing as IP address
    if let Ok(ip) = host.parse::<IpAddr>() {
        return !is_private_ip(&ip);
    }

    true
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // 10.0.0.0/8
            v4.octets()[0] == 10
                // 172.16.0.0/12
                || (v4.octets()[0] == 172 && (v4.octets()[1] & 0xf0) == 16)
                // 192.168.0.0/16
                || (v4.octets()[0] == 192 && v4.octets()[1] == 168)
                // 127.0.0.0/8
                || v4.is_loopback()
                // 169.254.0.0/16 (link-local)
                || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
                // 0.0.0.0/8
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                    // fe80::/10 link-local
                    || (v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80)
                    // fc00::/7 unique-local
                    || (v6.octets()[0] & 0xfe) == 0xfc
        }
    }
}

pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            '~' | ',' | '|' | '.' | '/' | '[' | '{' | '}' | ']' | '=' | '&' | '%' | '$' | '\\'
            | '"' | '\'' | '`' | '<' | '>' | '?' | ':' | ';' | '*' | '+' | '#' => '_',
            _ => c,
        })
        .collect()
}

/// Database query helper: `COUNT(CASE WHEN condition THEN 1 END)`.
pub fn count_when<C>(condition: C) -> SimpleExpr
where
    C: IntoCondition,
{
    Func::count(sea_orm::sea_query::Expr::case(
        condition,
        sea_orm::sea_query::Expr::val(1),
    ))
    .into()
}

/// Global active task counter (atomic, no lock contention).
pub static GLOBAL_TASK_COUNT: AtomicI64 = AtomicI64::new(0);

pub struct TaskGuard;

impl TaskGuard {
    pub fn new(_loc: String) -> Self {
        GLOBAL_TASK_COUNT.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        GLOBAL_TASK_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

#[track_caller]
pub fn spawn<T>(future: T) -> tokio::task::JoinHandle<T::Output>
where
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    let location = std::panic::Location::caller();
    let loc = format!("{}:{}", location.file(), location.line());
    let _guard = TaskGuard::new(loc);
    tokio::spawn(async move {
        let _guard = _guard;
        future.await
    })
}

/// Get current active task count
pub fn active_task_count() -> usize {
    GLOBAL_TASK_COUNT.load(Ordering::Relaxed) as usize
}

/// Get task count by location prefix (stub — always returns 0, kept for test compat)
pub fn active_task_count_by_prefix(_prefix: &str) -> usize {
    0
}

/// Get detailed task metrics (stub — returns empty map, kept for compat)
pub fn task_metrics_snapshot() -> std::collections::HashMap<String, usize> {
    std::collections::HashMap::new()
}

/// Reset all metrics (useful for tests)
pub fn reset_task_metrics() {
    GLOBAL_TASK_COUNT.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("session~123"), "session_123");
        assert_eq!(sanitize_id("leg|456,"), "leg_456_");
        assert_eq!(sanitize_id("path/to/id"), "path_to_id");
        assert_eq!(sanitize_id("id.with.dots"), "id_with_dots");
        assert_eq!(sanitize_id("brackets[{}]"), "brackets____");
        assert_eq!(sanitize_id("symbols=&%$"), "symbols____");
        assert_eq!(sanitize_id("safe-id_123"), "safe-id_123");
        assert_eq!(sanitize_id("more:;*+#"), "more_____");
    }
}
