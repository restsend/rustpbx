use sea_orm::sea_query::{Func, IntoCondition, SimpleExpr};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::runtime::Handle;
use tracing::warn;

use crate::config::Config;
use dashmap::DashMap;

// ─── Node identity ──────────────────────────────────────────────────────────

/// Pinned node identity — fixed by [`init_instance_identity`] at startup, or
/// frozen lazily by [`instance_id`].
static INSTANCE_ID: Mutex<Option<String>> = Mutex::new(None);

/// Fix this node's identity for the process lifetime. Called once at startup,
/// before any cluster/CC code runs: the identity keys CC presence ownership,
/// cluster_sync dedup, the monitor merge, and the log_search/callrecord
/// "node" fields.
///
/// Resolution order (first match wins):
/// 1. `RUSTPBX_INSTANCE_ID` env — explicit operator override;
/// 2. the `[cluster].peers` entry that IS this node, matched against the
///    host's interface IPs — yields `"addr:sip_port"`, the same string other
///    peers see in `cluster_self_addr` and `"node"` fields;
/// 3. `HOSTNAME`/`HOST` env, then the system hostname;
/// 4. `"unknown"`.
pub fn init_instance_identity(config: &Config) {
    let mut guard = INSTANCE_ID.lock().unwrap();
    if guard.is_some() {
        return;
    }
    let env_override = std::env::var("RUSTPBX_INSTANCE_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let hostname_env = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .ok()
        .filter(|h| !h.is_empty());
    *guard = Some(resolve_identity_from_parts(
        env_override,
        config.cluster.as_ref(),
        hostname_env,
        config.proxy.udp_port,
        &local_ips(),
    ));
}

/// This node's stable identity string. Freezes lazily on first use when
/// [`init_instance_identity`] was not called (single-node processes, tests) —
/// which skips the `[cluster].peers` self-match, since there is no config at
/// hand. Cluster deployments must init early to get the peer-derived identity.
pub fn instance_id() -> String {
    let mut guard = INSTANCE_ID.lock().unwrap();
    guard
        .get_or_insert_with(|| {
            let env_override = std::env::var("RUSTPBX_INSTANCE_ID")
                .ok()
                .filter(|s| !s.is_empty());
            let hostname_env = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("HOST"))
                .ok()
                .filter(|h| !h.is_empty());
            resolve_identity_from_parts(env_override, None, hostname_env, None, &local_ips())
        })
        .clone()
}

/// Test hook: drop the pinned identity so a later call re-resolves.
pub fn reset_instance_identity_for_test() {
    *INSTANCE_ID.lock().unwrap() = None;
}

/// Pure identity resolution — env reads happen at the call sites so tests can
/// exercise every branch without touching (and racing over) process env.
///
/// `sip_port` is this node's configured SIP listen port: on a shared host
/// several peers may share one interface IP (two containers both declaring
/// `addr = host.ip` with different ports), and the port is what disambiguates
/// which peer entry IS this node.
fn resolve_identity_from_parts(
    env_override: Option<String>,
    cluster: Option<&crate::config::ClusterConfig>,
    hostname_env: Option<String>,
    sip_port: Option<u16>,
    interface_ips: &HashSet<String>,
) -> String {
    if let Some(id) = env_override {
        return id;
    }
    if let Some(cluster) = cluster
        && !cluster.peers.is_empty()
    {
        match cluster.peers.iter().find(|p| {
            interface_ips.contains(&p.addr) && sip_port.map_or(true, |port| port == p.sip_port)
        }) {
            Some(peer) => return format!("{}:{}", peer.addr, peer.sip_port),
            None => warn!(
                "cluster peers configured but none matched a local interface IP \
                 (with the sip_port filter); falling back to hostname for the node identity"
            ),
        }
    }
    if let Some(h) = hostname_env {
        return h;
    }
    // `HOSTNAME`/`HOST` are shell conventions — a systemd service often has
    // neither set, so consult the kernel before giving up.
    if let Ok(os) = hostname::get()
        && let Some(s) = os.into_string().ok()
        && !s.is_empty()
    {
        return s;
    }
    "unknown".to_string()
}

/// IPs of the local network interfaces — used to skip fan-out targets that
/// are actually this node (cluster configs commonly list every member,
/// including self).
pub fn local_ips() -> std::collections::HashSet<String> {
    let mut ips = std::collections::HashSet::new();
    if let Ok(addrs) = local_ip_address::list_afinet_netifas() {
        for (_name, ip) in addrs {
            ips.insert(ip.to_string());
        }
    }
    ips
}

/// Canonical timestamp shape for every outbound webhook/event payload:
/// RFC 3339 in the **server's local timezone**.
///
/// An Asia/Shanghai host yields `2026-09-10T16:54:18.312004+08:00`; a host
/// with `TZ` unset (e.g. most containers) is UTC and yields `+00:00`. This is
/// intentionally environment-dependent: the payload carries the wall-clock
/// time the operator sees. Consumers must normalize to UTC before comparing
/// values across nodes with different timezones.
///
/// `DateTime<Utc>` can never render a non-UTC offset, so the value is
/// converted with `with_timezone(&Local)` first.
pub fn rfc3339_local(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.with_timezone(&chrono::Local).to_rfc3339()
}

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

/// Check whether a URL points at a public host. Blocks non-HTTP(S) schemes,
/// private / loopback / link-local IP literals (including decimal / hex IPv4
/// encodings — the `url` crate normalizes those), and internal-looking
/// hostnames. Synchronous and DNS-free; use [`is_url_ssrf_safe_async`] when
/// hostname resolution should be verified too.
pub fn is_url_ssrf_safe(url: &str) -> bool {
    let parsed = match url::Url::parse(url.trim()) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host_lower = host.to_lowercase();
    if host_lower == "localhost"
        || host_lower == "localhost6"
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".internal")
        || host_lower.ends_with(".lan")
        || host_lower.ends_with(".localdomain")
    {
        return false;
    }

    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => !is_private_ip(&IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !is_private_ip(&IpAddr::V6(ip)),
        Some(url::Host::Domain(_)) => !host_lower.is_empty(),
        None => false,
    }
}

/// Like [`is_url_ssrf_safe`], but additionally resolves hostnames and rejects
/// URLs whose DNS answers point at private / loopback ranges. This closes the
/// DNS-rebinding and hostname→private-IP bypasses of the sync check.
pub async fn is_url_ssrf_safe_async(url: &str) -> bool {
    if !is_url_ssrf_safe(url) {
        return false;
    }
    let parsed = match url::Url::parse(url.trim()) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    if let Some(url::Host::Domain(domain)) = parsed.host() {
        let port = parsed.port_or_known_default().unwrap_or(80);
        match tokio::net::lookup_host((domain, port)).await {
            Ok(addrs) => {
                let addrs: Vec<_> = addrs.collect();
                !addrs.is_empty() && addrs.iter().all(|a| !is_private_ip(&a.ip()))
            }
            Err(_) => false,
        }
    } else {
        true
    }
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
        // entry() holds the shard write-lock across insert-or-get +
        // increment, pairing with the lock-protected conditional removal in
        // `drop` (a plain get-then-add could race a concurrent removal and
        // orphan the increment).
        TASK_LOCATIONS
            .entry(loc.clone())
            .or_insert_with(|| AtomicI64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        Self { loc }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        GLOBAL_TASK_COUNT.fetch_sub(1, Ordering::Relaxed);
        let reached_zero = TASK_LOCATIONS
            .get_mut(&self.loc)
            .is_some_and(|entry| entry.fetch_sub(1, Ordering::Relaxed) == 1);
        if reached_zero {
            // Remove-at-zero keeps the diagnostics map from accumulating one
            // entry per spawn site forever. The count is re-checked under the
            // map lock: a concurrent spawn that re-incremented between the
            // decrement and this removal keeps its entry.
            TASK_LOCATIONS.remove_if(&self.loc, |_k, v| v.load(Ordering::Relaxed) == 0);
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

// ---------------------------------------------------------------------------
// RWI webhook runtime isolation: a dedicated tokio runtime for the RWI HTTP
// push consumer.  The webhook handler performs sequential blocking-ish HTTP
// POSTs to the router; running it on its own pool keeps its egress (and any
// backpressure from a slow router) off the SIP runtime shared by signalling,
// the HTTP route path, and the CDR saver.
// ---------------------------------------------------------------------------
static RWI_WEBHOOK_RUNTIME: OnceLock<Handle> = OnceLock::new();

/// Atomically set the global RWI webhook runtime handle.  Must be called
/// exactly once at startup, before the webhook handler is spawned.
pub fn set_rwi_webhook_runtime(handle: Handle) {
    RWI_WEBHOOK_RUNTIME
        .set(handle)
        .expect("set_rwi_webhook_runtime called more than once");
}

/// Spawn a future onto the dedicated RWI webhook runtime.  Falls back to the
/// ambient tokio runtime if the runtime has not been initialised (e.g. during
/// tests).
#[track_caller]
pub fn rwi_webhook_spawn<T>(future: T) -> tokio::task::JoinHandle<T::Output>
where
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    let location = std::panic::Location::caller();
    let loc = format!("{}:{}", location.file(), location.line());
    let _guard = TaskGuard::new(loc);
    if let Some(handle) = RWI_WEBHOOK_RUNTIME.get() {
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
    TASK_LOCATIONS.clear();
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
    use crate::config::{ClusterConfig, ClusterPeer};

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
    fn test_instance_id_non_empty() {
        assert!(!instance_id().is_empty());
    }

    fn identity_peer(addr: &str, sip_port: u16) -> ClusterPeer {
        ClusterPeer {
            addr: addr.to_string(),
            sip_port,
            ami_port: 8088,
        }
    }

    fn identity_config(peers: Vec<ClusterPeer>) -> ClusterConfig {
        ClusterConfig {
            peers,
            ..Default::default()
        }
    }

    fn ips(entries: &[&str]) -> HashSet<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    /// Cluster peers configured and one matches a local interface IP → the
    /// identity is that peer entry as `addr:sip_port` (what other nodes see).
    #[test]
    fn test_instance_identity_prefers_cluster_self_peer() {
        let cluster = identity_config(vec![
            identity_peer("192.168.1.10", 5060),
            identity_peer("10.0.0.6", 5060),
        ]);
        let id = resolve_identity_from_parts(None, Some(&cluster), None, None, &ips(&["10.0.0.6"]));
        assert_eq!(id, "10.0.0.6:5060");
    }

    /// Shared host: BOTH peer entries use the same interface IP and only the
    /// SIP port differs (two containers on one machine). The configured
    /// listen port must disambiguate — without it every node resolves to the
    /// first entry and the identities collide (prd 2026-10-14).
    #[test]
    fn test_instance_identity_shared_host_disambiguates_by_sip_port() {
        let cluster = identity_config(vec![
            identity_peer("192.168.3.9", 35060),
            identity_peer("192.168.3.9", 35061),
        ]);
        let host_ips = ips(&["192.168.3.9"]);
        let id_a = resolve_identity_from_parts(
            None,
            Some(&cluster),
            None,
            Some(35060),
            &host_ips,
        );
        let id_b = resolve_identity_from_parts(
            None,
            Some(&cluster),
            None,
            Some(35061),
            &host_ips,
        );
        assert_eq!(id_a, "192.168.3.9:35060");
        assert_eq!(id_b, "192.168.3.9:35061");
        // No configured port (WS-only node) keeps the historic IP-only match.
        let id_any = resolve_identity_from_parts(None, Some(&cluster), None, None, &host_ips);
        assert_eq!(id_any, "192.168.3.9:35060");
        // A port that matches NO peer falls back to the hostname chain.
        let id_miss = resolve_identity_from_parts(
            None,
            Some(&cluster),
            Some("fallback-node".to_string()),
            Some(40000),
            &host_ips,
        );
        assert_eq!(id_miss, "fallback-node");
    }

    /// `RUSTPBX_INSTANCE_ID` is an explicit operator override — it beats the
    /// cluster self-match.
    #[test]
    fn test_instance_identity_env_overrides_cluster() {
        let cluster = identity_config(vec![identity_peer("10.0.0.6", 5060)]);
        let id = resolve_identity_from_parts(
            Some("explicit-node".to_string()),
            Some(&cluster),
            None,
            None,
            &ips(&["10.0.0.6"]),
        );
        assert_eq!(id, "explicit-node");
    }

    /// No peer matches any interface → fall back through the hostname chain
    /// instead of inventing a non-local identity.
    #[test]
    fn test_instance_identity_cluster_miss_falls_back_to_hostname() {
        let cluster = identity_config(vec![identity_peer("10.9.9.9", 5060)]);
        let id = resolve_identity_from_parts(
            None,
            Some(&cluster),
            Some("sip-node-7".to_string()),
            None,
            &ips(&["10.0.0.6"]),
        );
        assert_eq!(id, "sip-node-7");
    }

    /// No env override, no cluster, no `HOSTNAME`/`HOST` (systemd services
    /// often have neither) → the kernel hostname, never an empty identity.
    #[test]
    fn test_instance_identity_last_resort_non_empty() {
        let id = resolve_identity_from_parts(None, None, None, None, &HashSet::new());
        assert!(!id.is_empty());
    }

    /// The canonical outbound timestamp is always a fully-qualified RFC 3339
    /// string (offset present, never naive/local-ambiguous) that round-trips
    /// back to the same instant, regardless of the host timezone.
    #[test]
    fn test_rfc3339_local_roundtrip() {
        let now = chrono::Utc::now();
        let s = rfc3339_local(now);
        let parsed = chrono::DateTime::parse_from_rfc3339(&s)
            .unwrap_or_else(|e| panic!("rfc3339_local produced {s:?}: {e}"));
        assert_eq!(parsed.with_timezone(&chrono::Utc), now);
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
