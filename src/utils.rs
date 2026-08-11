use sea_orm::sea_query::{Func, IntoCondition, SimpleExpr};
use std::net::IpAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::runtime::Handle;

use dashmap::DashMap;

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

/// Per-location active task counter for leak diagnostics.
static TASK_LOCATIONS: std::sync::LazyLock<DashMap<String, AtomicI64>> =
    std::sync::LazyLock::new(DashMap::new);

pub struct TaskGuard {
    loc: String,
}

impl TaskGuard {
    pub fn new(loc: String) -> Self {
        GLOBAL_TASK_COUNT.fetch_add(1, Ordering::Relaxed);
        if let Some(entry) = TASK_LOCATIONS.get(&loc) {
            entry.fetch_add(1, Ordering::Relaxed);
        } else {
            TASK_LOCATIONS
                .entry(loc.clone())
                .or_insert_with(|| AtomicI64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
        Self { loc }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        GLOBAL_TASK_COUNT.fetch_sub(1, Ordering::Relaxed);
        if let Some(entry) = TASK_LOCATIONS.get(&self.loc) {
            entry.fetch_sub(1, Ordering::Relaxed);
        }
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

// ---------------------------------------------------------------------------
// Media runtime isolation: a dedicated tokio runtime for RTP/media tasks.
// Set once at startup via set_media_runtime().  All media-layer tokio::spawn
// calls should go through media_spawn() so they land on the media runtime
// instead of the SIP runtime, preventing RTP load from starving SIP timers.
// ---------------------------------------------------------------------------
static MEDIA_RUNTIME: OnceLock<Handle> = OnceLock::new();

/// Atomically set the global media runtime handle.  Must be called exactly
/// once at startup, before any media task is spawned.
pub fn set_media_runtime(handle: Handle) {
    MEDIA_RUNTIME
        .set(handle)
        .expect("set_media_runtime called more than once");
}

/// Return the configured media runtime handle, used by rustrtc
/// `RtcConfiguration` so that all internal spawns land on the media runtime.
pub fn media_runtime_handle() -> Option<Handle> {
    MEDIA_RUNTIME
        .get()
        .cloned()
        .or_else(|| Handle::try_current().ok())
}

/// Spawn a future onto the dedicated media runtime.  Falls back to the
/// ambient tokio runtime if the media runtime has not been initialised
/// (e.g. during tests).
#[track_caller]
pub fn media_spawn<T>(future: T) -> tokio::task::JoinHandle<T::Output>
where
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    let location = std::panic::Location::caller();
    let loc = format!("{}:{}", location.file(), location.line());
    let _guard = TaskGuard::new(loc);
    if let Some(handle) = MEDIA_RUNTIME.get() {
        handle.spawn(async move {
            let _guard = _guard;
            future.await
        })
    } else {
        tokio::spawn(async move {
            let _guard = _guard;
            future.await
        })
    }
}

/// Enter the media runtime context so that bare `tokio::spawn` calls (e.g.
/// inside third-party crate constructors like `rustrtc::PeerConnection::new`)
/// bind to the media runtime rather than the SIP runtime.
///
/// The returned guard is **thread-local**.  It is safe to hold across
/// synchronous code sections, but **not** across `.await` points on a
/// multi-thread runtime (the guard would be lost after task resumption on
/// another thread).  For spawns after an await use `media_spawn` instead.
pub fn media_enter() -> Option<tokio::runtime::EnterGuard<'static>> {
    // SAFETY: OnceLock::get() returns &Handle with the lifetime of the
    // OnceLock, which is 'static because MEDIA_RUNTIME is a static.
    MEDIA_RUNTIME.get().map(|h| h.enter())
}

/// Collect tokio runtime metrics from the current and media runtimes.
/// Returns a serde_json map with key metrics useful for leak detection.
///
/// Available metrics (stable tokio API):
/// - `num_alive_tasks` — current number of alive tasks on the runtime
/// - `num_workers` — number of worker threads
/// - `global_queue_depth` — tasks pending in the global injection queue
pub fn tokio_runtime_metrics() -> serde_json::Value {
    let mut map = serde_json::Map::new();

    let mut collect_metrics = |name: &str, m: &tokio::runtime::RuntimeMetrics| {
        let mut rt_map = serde_json::Map::new();
        rt_map.insert(
            "num_alive_tasks".into(),
            serde_json::json!(m.num_alive_tasks()),
        );
        rt_map.insert("num_workers".into(), serde_json::json!(m.num_workers()));
        rt_map.insert(
            "global_queue_depth".into(),
            serde_json::json!(m.global_queue_depth()),
        );
        map.insert(name.to_string(), serde_json::Value::Object(rt_map));
    };

    // Current (SIP) runtime
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        collect_metrics("sip", &rt.metrics());
    }

    // Media runtime (if configured separately)
    if let Some(handle) = MEDIA_RUNTIME.get() {
        collect_metrics("media", &handle.metrics());
    }

    // Untracked task counts (third-party spawns not going through utils::spawn)
    let untracked = crate::untracked_tasks::snapshot();
    if !untracked.is_empty() {
        let mut entries = Vec::new();
        for (loc, cnt) in &untracked {
            entries.push(serde_json::json!({"loc": loc, "count": cnt}));
        }
        map.insert("untracked".into(), serde_json::json!(entries));
    }

    serde_json::Value::Object(map)
}

/// Get current active task count
pub fn active_task_count() -> usize {
    GLOBAL_TASK_COUNT.load(Ordering::Relaxed) as usize
}

/// Get detailed task metrics keyed by spawn location ("file:line").
pub fn task_metrics_snapshot() -> std::collections::HashMap<String, usize> {
    TASK_LOCATIONS
        .iter()
        .filter(|e| e.value().load(Ordering::Relaxed) > 0)
        .map(|e| (e.key().clone(), e.value().load(Ordering::Relaxed) as usize))
        .collect()
}

/// Reset all metrics (useful for tests)
pub fn reset_task_metrics() {
    GLOBAL_TASK_COUNT.store(0, Ordering::Relaxed);
}

/// Default maximum size (in bytes) for audio files downloaded over HTTP from
/// the console settings (queue prompts, voicemail prompts, ...). 20 MB.
///
/// Overridable via the top-level `max_audio_download_bytes` config field.
pub const MAX_AUDIO_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

/// Validate that a downloaded audio payload is a real `.wav` / `.mp3` file.
///
/// Enforces all three checks:
/// 1. The HTTP `Content-Type` header must be `audio/wav` or `audio/mpeg`
///    (`audio/mp3` is accepted too).
/// 2. The target filename must end with `.wav` or `.mp3`.
/// 3. The payload magic bytes must match a WAV (`RIFF` + `WAVE`) or MP3
///    (`ID3` tag, or an MPEG frame sync) file.
///
/// Returns `Ok(())` on success, or a human-readable error message.
pub fn validate_audio_payload(
    bytes: &[u8],
    content_type: &str,
    filename: &str,
) -> Result<(), String> {
    let content_type = content_type.to_lowercase();
    if content_type != "audio/wav" && content_type != "audio/mpeg" && content_type != "audio/mp3" {
        return Err(format!(
            "Remote response Content-Type '{}' is not audio/wav or audio/mpeg",
            content_type
        ));
    }

    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext != "wav" && ext != "mp3" {
        return Err(format!(
            "Filename '{}' must end with .wav or .mp3",
            filename
        ));
    }

    if is_wav_bytes(bytes) {
        return Ok(());
    }
    if is_mp3_bytes(bytes) {
        return Ok(());
    }
    Err("Downloaded file is neither a valid WAV nor MP3 audio file".to_string())
}

/// True if `bytes` start with the WAV `RIFF`/`WAVE` signature.
fn is_wav_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

/// True if `bytes` look like MP3: either an `ID3` tag or an MPEG frame sync
/// (`0xFF` followed by a byte whose top 3 bits are set, i.e. `0xE0` mask).
fn is_mp3_bytes(bytes: &[u8]) -> bool {
    if bytes.len() >= 3 && &bytes[0..3] == b"ID3" {
        return true;
    }
    bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0
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

    #[test]
    fn test_validate_audio_payload_wav_ok() {
        let mut wav = vec![0u8; 44];
        wav[0..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        assert!(validate_audio_payload(&wav, "audio/wav", "prompt.wav").is_ok());
        // case-insensitive content type / extension
        assert!(validate_audio_payload(&wav, "Audio/WAV", "PROMPT.WAV").is_ok());
    }

    #[test]
    fn test_validate_audio_payload_mp3_ok() {
        let mut id3 = vec![0u8; 10];
        id3[0..3].copy_from_slice(b"ID3");
        assert!(validate_audio_payload(&id3, "audio/mpeg", "prompt.mp3").is_ok());
        assert!(validate_audio_payload(&id3, "audio/mp3", "prompt.mp3").is_ok());

        // MPEG frame sync (no ID3 tag)
        let frame = vec![0xFF, 0xFB, 0x90, 0x64];
        assert!(validate_audio_payload(&frame, "audio/mpeg", "prompt.mp3").is_ok());
    }

    #[test]
    fn test_validate_audio_payload_rejects_bad_content_type() {
        let mut wav = vec![0u8; 44];
        wav[0..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        let err = validate_audio_payload(&wav, "text/html", "prompt.wav").unwrap_err();
        assert!(err.contains("Content-Type"), "got: {}", err);
    }

    #[test]
    fn test_validate_audio_payload_rejects_bad_extension() {
        let mut wav = vec![0u8; 44];
        wav[0..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        let err = validate_audio_payload(&wav, "audio/wav", "prompt.exe").unwrap_err();
        assert!(err.contains("wav"), "got: {}", err);
    }

    #[test]
    fn test_validate_audio_payload_rejects_non_audio_bytes() {
        let body = b"<html><body>not audio</body></html>";
        let err = validate_audio_payload(body, "audio/wav", "prompt.wav").unwrap_err();
        assert!(err.contains("WAV") && err.contains("MP3"), "got: {}", err);
    }
}
