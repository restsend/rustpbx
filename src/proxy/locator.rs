use super::locator_db::DbLocator;
use crate::{
    call::Location,
    config::{LocatorConfig, ProxyConfig},
};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::{
    transaction::endpoint::{TargetLocator, TransportEventInspector},
    transport::{SipAddr, TransportEvent},
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Instant,
};
use tokio::sync::Mutex;
use tracing::{debug, info};

#[derive(Clone, Debug)]
pub enum LocatorEvent {
    Registered(Location),
    Unregistered(Location),
    Offline(Vec<Location>),
}

pub type LocatorEventSender = tokio::sync::broadcast::Sender<LocatorEvent>;
pub type LocatorEventReceiver = tokio::sync::broadcast::Receiver<LocatorEvent>;
pub type LocatorCreationFuture = Pin<Box<dyn Future<Output = Result<Box<dyn Locator>>> + Send>>;
pub type RealmChecker =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[async_trait]
pub trait Locator: Send + Sync {
    async fn get_identifier(&self, user: &str, realm: Option<&str>) -> String {
        let username = user.trim().to_ascii_lowercase();
        let realm = match realm {
            Some(r) if !r.trim().is_empty() => {
                let r = r.trim();
                if self.is_local_realm(r).await {
                    Some("localhost".to_string())
                } else {
                    Some(r.to_ascii_lowercase())
                }
            }
            _ => None,
        };

        match (username.is_empty(), realm) {
            (true, _) => String::new(),
            (false, Some(realm)) => format!("{}@{}", username, realm),
            (false, None) => username,
        }
    }
    async fn is_local_realm(&self, realm: &str) -> bool {
        is_local_realm(realm)
    }
    fn set_realm_checker(&self, _checker: RealmChecker) {}
    async fn register(&self, username: &str, realm: Option<&str>, location: Location)
    -> Result<()>;
    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()>;
    async fn unregister_with_address(&self, addr: &SipAddr) -> Result<Option<Vec<Location>>>;
    async fn lookup(&self, uri: &rsipstack::sip::Uri) -> Result<Vec<Location>>;
}

// ───────────────────────────────────────────────────────────────────────────
// Shared helpers for Locator backends (DbLocator, RedisLocator, …)
//
// Any custom [`Locator`] implementation (e.g. a Redis-backed one) MUST honour
// the semantics encoded here so that cluster routing, WebRTC (.invalid)
// contacts, and rapid-reconnect safety work uniformly across backends.
//
// Contract for a custom Locator backend
// -------------------------------------
// `lookup`:
//   1. Try exact AoR match first (`uri.to_string()`).
//   2. If empty **and** [`is_webrtc_invalid_host`] → use
//      [`invalid_host_fallback`] to obtain the Contact user and host, then:
//        a. AoR-pattern match: query by `user@host` so that bindings whose
//           Contact AoR has the same user and `.invalid` host are found even
//           when params differ (e.g. `;ob` in INVITE but not in REGISTER) or
//           when the Contact user part differs from the authenticated username.
//        b. Username match (fallback): query by the Contact user part against
//           the username column / key.
//   3. Filter out expired bindings via [`is_location_expired`].
//   4. Sort surviving results with [`sort_locations_by_recency`].
//
// `unregister_with_address`:
//   Only delete rows whose `last_modified` is older than the grace window
//   ([`is_within_unregister_grace`]) so a stale transport-close event cannot
//   wipe a freshly-registered binding (rapid reconnect / NAT port reuse).
//
// `register`:
//   Store `home_proxy`, `registered_aor`, `destination`, `expires` and
//   `last_modified` so that [`DialogTargetLocator`] can make correct
//   cluster-routing decisions. Resolve the canonical AoR with
//   [`choose_registered_aor`].
// ───────────────────────────────────────────────────────────────────────────

/// Grace period (seconds) protecting freshly-registered bindings from being
/// deleted by a stale transport-close event. See [`is_within_unregister_grace`].
pub const UNREGISTER_GRACE_SECS: i64 = 5;

/// Current UNIX epoch in seconds — the clock all `last_modified` values use.
pub fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Returns `true` when a binding has exceeded its expiry.
///
/// `expires == 0` means "never expire".
pub fn is_location_expired(expires: i64, last_modified: i64, now_epoch: i64) -> bool {
    expires > 0 && (now_epoch - last_modified) >= expires
}

/// Returns `true` when a binding was registered so recently that a stale
/// transport-close event must not delete it (rapid-reconnect / NAT port reuse).
pub fn is_within_unregister_grace(last_modified: i64, now_epoch: i64) -> bool {
    last_modified >= (now_epoch - UNREGISTER_GRACE_SECS)
}

/// Returns `true` for WebRTC (JsSIP) `.invalid` contact hosts (RFC 7118).
pub fn is_webrtc_invalid_host(host: &str) -> bool {
    host.ends_with(".invalid")
}

// ── WebRTC `.invalid` host fallback helpers ─────────────────────────────────

/// Parameters extracted from a WebRTC `.invalid` lookup URI for fallback
/// queries.
///
/// Produced by [`invalid_host_fallback`]. All Locator backends should consult
/// these parameters when the initial exact-AoR lookup returns empty, so that
/// in-dialog requests (BYE, re-INVITE, …) can be routed to WebRTC clients
/// whose Contact uses a random `.invalid` hostname.
pub struct InvalidHostFallback {
    /// Lowercased Contact user part (e.g. `"qn27nogk"`).
    pub user: String,
    /// Contact host (e.g. `"2kbkhn3beiif.invalid"`).
    pub host: String,
}

impl InvalidHostFallback {
    /// Build a glob pattern suitable for SQL `LIKE` or Redis `SCAN MATCH`.
    ///
    /// Matches any stored AoR that contains `user@host`, ignoring leading
    /// scheme and trailing parameters:
    ///
    /// ```text
    /// %qn27nogk@2kbkhn3beiif.invalid%
    /// ```
    ///
    /// This finds e.g. `sip:qn27nogk@2kbkhn3beiif.invalid;transport=ws` when
    /// the lookup URI is `sip:qn27nogk@2kbkhn3beiif.invalid;transport=ws;ob`.
    pub fn aor_like_pattern(&self) -> String {
        format!("%{}@{}%", self.user, self.host)
    }

    /// Check whether a stored AoR matches this fallback (for in-memory
    /// backends such as [`MemoryLocator`]).
    ///
    /// Returns `true` when the AoR's user part (case-insensitive) and host
    /// match the fallback parameters, regardless of URI parameters.
    pub fn matches_aor(&self, aor: &rsipstack::sip::Uri) -> bool {
        let aor_user = aor
            .user()
            .map(|u| u.trim().to_ascii_lowercase())
            .unwrap_or_default();
        aor_user == self.user && aor.host().to_string().eq_ignore_ascii_case(&self.host)
    }
}

/// Determine whether a lookup URI requires the `.invalid` host fallback, and
/// if so, return the [`InvalidHostFallback`] parameters.
///
/// Returns `None` when:
/// - the host is **not** a `.invalid` domain, or
/// - the URI has no user part or an empty one.
///
/// # Example (DbLocator / SQL)
/// ```ignore
/// if models.is_empty() {
///     if let Some(fb) = invalid_host_fallback(uri) {
///         models = Entity::find()
///             .filter(Column::Aor.like(fb.aor_like_pattern()))
///             .all(&db).await?;
///         if models.is_empty() {
///             models = Entity::find()
///                 .filter(Column::Username.eq(&fb.user))
///                 .all(&db).await?;
///         }
///     }
/// }
/// ```
///
/// # Example (RedisLocator / Redis)
/// ```ignore
/// if locations.is_empty() {
///     if let Some(fb) = invalid_host_fallback(uri) {
///         locations = redis.aor_scan(fb.aor_like_pattern()).await;
///         if locations.is_empty() {
///             locations = redis.username_lookup(&fb.user).await;
///         }
///     }
/// }
/// ```
pub fn invalid_host_fallback(uri: &rsipstack::sip::Uri) -> Option<InvalidHostFallback> {
    if !is_webrtc_invalid_host(&uri.host().to_string()) {
        return None;
    }
    let user = uri.user()?.trim().to_ascii_lowercase();
    if user.is_empty() {
        return None;
    }
    Some(InvalidHostFallback {
        user,
        host: uri.host().to_string(),
    })
}

/// Resolve the canonical registered AoR, repairing legacy records where
/// `registered_aor` was incorrectly stored as the raw contact URI.
///
/// Custom backends should call this when materialising a [`Location`] from
/// storage so that downstream cluster routing sees a consistent AoR.
pub fn choose_registered_aor(
    username: &str,
    realm: &str,
    contact_aor: &rsipstack::sip::Uri,
    decoded_registered_aor: Option<rsipstack::sip::Uri>,
) -> rsipstack::sip::Uri {
    let fallback = fallback_registered_aor(username, realm, contact_aor);
    let strict_equals = |a: &rsipstack::sip::Uri, b: &rsipstack::sip::Uri| {
        a.to_string().eq_ignore_ascii_case(&b.to_string())
    };

    match decoded_registered_aor {
        Some(registered)
            if strict_equals(&registered, contact_aor)
                && !strict_equals(&fallback, contact_aor) =>
        {
            fallback
        }
        Some(registered) => registered,
        None => fallback,
    }
}

fn fallback_registered_aor(
    username: &str,
    realm: &str,
    fallback: &rsipstack::sip::Uri,
) -> rsipstack::sip::Uri {
    let user = username.trim();
    let host = realm.trim();
    if user.is_empty() || host.is_empty() {
        return fallback.clone();
    }

    let candidate = format!("sip:{}@{}", user, host);
    rsipstack::sip::Uri::try_from(candidate.as_str()).unwrap_or_else(|_| fallback.clone())
}

pub struct DialogTargetLocator {
    locator: Arc<Box<dyn Locator>>,
    local_addrs: Vec<SipAddr>,
    cluster_enabled: bool,
}

impl DialogTargetLocator {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        locator: Arc<Box<dyn Locator>>,
        local_addrs: Vec<SipAddr>,
        cluster_enabled: bool,
    ) -> Box<dyn TargetLocator> {
        Box::new(Self {
            locator,
            local_addrs,
            cluster_enabled,
        }) as Box<dyn TargetLocator>
    }

    fn is_local_home_proxy(&self, home_proxy: &SipAddr) -> bool {
        self.local_addrs
            .iter()
            .any(|addr| addr.addr.to_string() == home_proxy.addr.to_string())
    }
}

#[async_trait]
impl TargetLocator for DialogTargetLocator {
    async fn locate(&self, uri: &rsipstack::sip::Uri) -> Result<SipAddr, rsipstack::Error> {
        let locs = self.locator.lookup(uri).await;
        if let Ok(locs) = &locs
            && !locs.is_empty()
        {
            if let Some(loc) = locs.iter().find(|loc| {
                if !self.cluster_enabled {
                    return false;
                }
                let Some(home_proxy) = loc.home_proxy.as_ref() else {
                    return false;
                };
                let Some(registered_aor) = loc.registered_aor.as_ref() else {
                    return false;
                };
                let host_is_home = uri.host_with_port == home_proxy.addr;
                let host_is_invalid = is_webrtc_invalid_host(&uri.host().to_string());
                // Exact canonical AoR match — no host requirement. This is the
                // original cluster-forwarding path: a lookup by canonical AoR
                // must always route via home_proxy.
                if registered_aor == uri {
                    return true;
                }
                // User-level match with a host constraint: either the caller
                // already rewrote the host to home_proxy, or it is a WebRTC
                // .invalid contact (JsSIP).
                registered_aor.user() == uri.user() && (host_is_home || host_is_invalid)
            }) {
                if let Some(home_proxy) = &loc.home_proxy {
                    if self.is_local_home_proxy(home_proxy)
                        && let Some(dest) = &loc.destination
                    {
                        return Ok(dest.clone());
                    }
                    return Ok(home_proxy.clone());
                }
            }

            if let Some(loc) = locs.first() {
                // Cluster safety: never hand out a destination that belongs to a
                // remote home node — route to its home_proxy instead so the
                // request is forwarded to the node that owns the connection.
                if self.cluster_enabled
                    && let Some(home_proxy) = &loc.home_proxy
                    && !self.is_local_home_proxy(home_proxy)
                {
                    return Ok(home_proxy.clone());
                }
                if let Some(dest) = &loc.destination {
                    return Ok(dest.clone());
                }
            }
        }
        debug!(%uri, "DialogTargetLocator: location lookup returned empty, using SipAddr fallback");
        SipAddr::try_from(uri).map_err(|e| {
            rsipstack::Error::Error(format!(
                "failed to convert uri to sip addr: {}, error: {}",
                uri, e
            ))
        })
    }
}

pub struct TransportInspectorLocator {
    locator_events: LocatorEventSender,
    locator: Arc<Box<dyn Locator>>,
}

impl TransportInspectorLocator {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        locator: Arc<Box<dyn Locator>>,
        locator_events: LocatorEventSender,
    ) -> Box<dyn TransportEventInspector> {
        Box::new(Self {
            locator,
            locator_events,
        }) as Box<dyn TransportEventInspector>
    }
}

#[async_trait]
impl TransportEventInspector for TransportInspectorLocator {
    async fn handle(&self, event: TransportEvent) -> Option<TransportEvent> {
        if let TransportEvent::Closed(conn) = &event {
            let addr = conn.get_remote_addr().unwrap_or_else(|| conn.get_addr());
            match self.locator.unregister_with_address(addr).await {
                Ok(Some(removed)) => {
                    if !removed.is_empty() {
                        self.locator_events
                            .send(LocatorEvent::Offline(removed))
                            .ok();
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    info!(error = %e, "Error unregistering location on transport close");
                }
            }
        }
        Some(event)
    }
}

pub struct MemoryLocator {
    locations: Mutex<HashMap<String, HashMap<String, Location>>>,
    realm_checker: Mutex<Option<RealmChecker>>,
}

impl Default for MemoryLocator {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryLocator {
    pub fn new() -> Self {
        Self {
            locations: Mutex::new(HashMap::new()),
            realm_checker: Mutex::new(None),
        }
    }
    pub fn create(_config: Arc<ProxyConfig>) -> LocatorCreationFuture {
        Box::pin(async move { Ok(Box::new(MemoryLocator::new()) as Box<dyn Locator>) })
    }
}

#[async_trait]
impl Locator for MemoryLocator {
    async fn is_local_realm(&self, realm: &str) -> bool {
        let checker = self.realm_checker.lock().await.clone();
        if let Some(checker) = checker {
            checker(realm).await
        } else {
            is_local_realm(realm)
        }
    }

    fn set_realm_checker(&self, checker: RealmChecker) {
        let mut lock = self
            .realm_checker
            .try_lock()
            .expect("failed to lock realm_checker");
        *lock = Some(checker);
    }

    async fn register(
        &self,
        username: &str,
        realm: Option<&str>,
        location: Location,
    ) -> Result<()> {
        let identifier = self.get_identifier(username, realm).await;
        if identifier.is_empty() {
            return Ok(());
        }
        let mut location = location;
        let key = location.binding_key();
        let mut locations = self.locations.lock().await;

        // Opportunistic GC: prune expired bindings for THIS identifier on
        // every register. Without this, clients that crash without sending
        // REGISTER expires=0 leave stale entries forever (lookup() only
        // sweeps the entries it actually visits, so never-looked-up AoRs
        // would accumulate). This stays O(1) amortised because each AoR's
        // binding map is small (typically a single binding).
        if let Some(map) = locations.get_mut(&identifier) {
            let now = Instant::now();
            map.retain(|_, loc| !loc.is_expired_at(now));
        }

        let entry = locations
            .entry(identifier.clone())
            .or_insert_with(HashMap::new);
        if location.expires == 0 {
            entry.remove(&key);
            if entry.is_empty() {
                locations.remove(&identifier);
            }
            info!(identifier, binding = %key, %location, "unregistered location (expires=0)");
        } else {
            if location.last_modified.is_none() {
                location.last_modified = Some(Instant::now());
            }
            info!(identifier, binding = %key, %location, "registered location");
            entry.insert(key, location);
        }
        Ok(())
    }

    async fn unregister(&self, username: &str, realm: Option<&str>) -> Result<()> {
        let identifier = self.get_identifier(username, realm).await;
        let mut locations = self.locations.lock().await;
        locations.remove(&identifier);
        Ok(())
    }

    async fn unregister_with_address(&self, addr: &SipAddr) -> Result<Option<Vec<Location>>> {
        let mut locations = self.locations.lock().await;
        let mut identifiers_to_remove = Vec::new();
        let mut removed_locations = Vec::new();

        for (identifier, map) in locations.iter_mut() {
            let keys_to_remove: Vec<String> = map
                .iter()
                .filter_map(|(key, loc)| {
                    if let Some(dest) = &loc.destination
                        && dest == addr
                    {
                        return Some(key.clone());
                    }
                    None
                })
                .collect();

            for key in keys_to_remove {
                if let Some(loc) = map.remove(&key) {
                    removed_locations.push(loc);
                }
            }

            if map.is_empty() {
                identifiers_to_remove.push(identifier.clone());
            }
        }

        for identifier in identifiers_to_remove {
            if let Some(locs) = locations.remove(&identifier) {
                for loc in locs.values() {
                    removed_locations.push(loc.clone());
                }
            }
        }

        if removed_locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(removed_locations))
        }
    }

    async fn lookup(&self, uri: &rsipstack::sip::Uri) -> Result<Vec<Location>> {
        let mut locations = self.locations.lock().await;
        let now: Instant = Instant::now();
        let uri_string = uri.to_string();
        let mut direct_hits = Vec::new();

        // Prune expired bindings and attempt direct contact/GRUU matches first
        locations.retain(|_, map| {
            map.retain(|_, loc| !loc.is_expired_at(now));
            !map.is_empty()
        });
        for map in locations.values() {
            for loc in map.values() {
                if &loc.aor == uri || uri_matches(&loc.aor, uri) {
                    direct_hits.push(loc.clone());
                    continue;
                }
                if let Some(registered) = &loc.registered_aor
                    && (registered == uri || uri_matches(registered, uri))
                {
                    direct_hits.push(loc.clone());
                    continue;
                }
                if let Some(gruu) = &loc.gruu
                    && (gruu == &uri_string || gruu.eq_ignore_ascii_case(&uri_string))
                {
                    direct_hits.push(loc.clone());
                    continue;
                }
            }
        }

        if !direct_hits.is_empty() {
            return Ok(sort_locations_by_recency(direct_hits));
        }

        // Fall back to classic AoR lookup by username/realm
        let username_raw = uri.user().unwrap_or("");
        let username = username_raw.trim();
        let username_lower = username.to_ascii_lowercase();
        let realm_raw = uri.host().to_string();
        let realm_trimmed = realm_raw.trim();

        let mut identifiers = Vec::new();
        if !username.is_empty() {
            if !realm_trimmed.is_empty() {
                identifiers.push(self.get_identifier(username, Some(realm_trimmed)).await);
            }
            identifiers.push(self.get_identifier(username, Some("localhost")).await);
            identifiers.push(self.get_identifier(username, None).await);
        }

        for id in identifiers {
            if let Some(map) = locations.get(&id)
                && !map.is_empty()
            {
                let results: Vec<_> = map.values().cloned().collect();
                return Ok(sort_locations_by_recency(results));
            }
        }

        if !username.is_empty() {
            let mut fallback_hits = Vec::new();

            for map in locations.values() {
                for loc in map.values() {
                    let mut matched = false;

                    if let Some(registered) = &loc.registered_aor {
                        let user_match = registered
                            .user()
                            .map(|u| u.trim().eq_ignore_ascii_case(&username_lower))
                            .unwrap_or(false);
                        let realm_string = registered.host().to_string();
                        let realm_match = if realm_trimmed.ends_with(".invalid") {
                            true
                        } else {
                            realm_matches(self, realm_trimmed, realm_string.trim()).await
                        };

                        if user_match && realm_match {
                            matched = true;
                        }
                    }

                    if !matched {
                        let user_match = loc
                            .aor
                            .user()
                            .map(|u| u.trim().eq_ignore_ascii_case(&username_lower))
                            .unwrap_or(false);
                        let realm_string = loc.aor.host().to_string();
                        let realm_match = if realm_trimmed.ends_with(".invalid") {
                            true
                        } else {
                            realm_matches(self, realm_trimmed, realm_string.trim()).await
                        };

                        if user_match && realm_match {
                            matched = true;
                        }
                    }

                    if matched {
                        fallback_hits.push(loc.clone());
                    }
                }
            }

            if !fallback_hits.is_empty() {
                return Ok(sort_locations_by_recency(fallback_hits));
            }
        }

        Ok(vec![])
    }
}

fn compare_location_recency(a: &Location, b: &Location) -> Ordering {
    match (a.last_modified, b.last_modified) {
        (Some(a_ts), Some(b_ts)) => b_ts.cmp(&a_ts),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn host_without_port(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.starts_with('[')
        && let Some(end) = trimmed.find(']')
    {
        return &trimmed[1..end];
    }

    // If it contains more than one colon, it's likely an IPv6 address without brackets
    if trimmed.matches(':').count() > 1 {
        return trimmed;
    }

    trimmed.split(':').next().unwrap_or(trimmed)
}

pub(crate) fn is_local_realm(realm: &str) -> bool {
    if realm.trim().is_empty() {
        return false;
    }
    let host = host_without_port(realm);
    let host_lower = host.to_ascii_lowercase();
    if matches!(
        host_lower.as_str(),
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1"
    ) {
        return true;
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() || ip.is_unspecified() {
            return true;
        }
        if let std::net::IpAddr::V4(v4) = ip
            && v4.is_private()
        {
            return true;
        }
    }
    false
}

async fn realm_matches(
    locator: &dyn Locator,
    requested_realm: &str,
    candidate_realm: &str,
) -> bool {
    let requested = requested_realm.trim();
    if requested.is_empty() {
        return true;
    }

    let candidate = candidate_realm.trim();
    if locator.is_local_realm(requested).await && locator.is_local_realm(candidate).await {
        return true;
    }

    let requested_host = host_without_port(requested).to_ascii_lowercase();
    let candidate_host = host_without_port(candidate).to_ascii_lowercase();
    requested_host == candidate_host
}

pub fn uri_matches(a: &rsipstack::sip::Uri, b: &rsipstack::sip::Uri) -> bool {
    if a == b {
        return true;
    }

    if a.user() == b.user() {
        let a_host = a.host().to_string();
        let b_host = b.host().to_string();

        if is_local_realm(&a_host) && is_local_realm(&b_host) {
            return true;
        }

        // Special handling for .invalid domains often used in WebRTC
        if (a_host.ends_with(".invalid") || b_host.ends_with(".invalid"))
            && a_host.eq_ignore_ascii_case(&b_host)
        {
            return true;
        }
    }

    // Fallback to case-insensitive string comparison for the whole URI
    // This handles transport=ws vs transport=WS
    a.to_string().eq_ignore_ascii_case(&b.to_string())
}

pub(crate) fn sort_locations_by_recency(mut locations: Vec<Location>) -> Vec<Location> {
    locations.sort_by(compare_location_recency);
    let mut seen: HashSet<String> = HashSet::new();
    locations.retain(|loc| seen.insert(loc.binding_key()));
    locations
}

pub async fn create_locator(config: &LocatorConfig) -> Result<Box<dyn Locator>> {
    create_locator_with_migrate(config, true).await
}

pub async fn create_locator_with_migrate(
    config: &LocatorConfig,
    migrate: bool,
) -> Result<Box<dyn Locator>> {
    match config {
        LocatorConfig::Memory | LocatorConfig::Http { .. } => {
            Ok(Box::new(MemoryLocator::new()) as Box<dyn Locator>)
        }
        LocatorConfig::Database { url } => {
            let db_locator = DbLocator::new_with_migrate(url.clone(), migrate).await?;
            Ok(Box::new(db_locator) as Box<dyn Locator>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::HostWithPort;
    use rsipstack::sip::transport::Transport;
    use rsipstack::transport::SipAddr;
    use std::time::Duration;

    // ── shared helper unit tests ────────────────────────────────────────

    #[test]
    fn test_is_location_expired() {
        // expires == 0 means never expire
        assert!(!is_location_expired(0, 100, 999_999));
        // not yet expired
        assert!(!is_location_expired(3600, 1000, 2000));
        // exactly at expiry boundary
        assert!(is_location_expired(3600, 1000, 4600));
        // long expired
        assert!(is_location_expired(60, 1000, 999_999));
    }

    #[test]
    fn test_is_within_unregister_grace() {
        // freshly registered → within grace
        assert!(is_within_unregister_grace(1000, 1000));
        assert!(is_within_unregister_grace(1000, 1003));
        // at the boundary (exactly GRACE_SECS ago) → still within
        assert!(is_within_unregister_grace(
            1000,
            1000 + UNREGISTER_GRACE_SECS
        ));
        // older than grace → outside
        assert!(!is_within_unregister_grace(
            1000,
            1000 + UNREGISTER_GRACE_SECS + 1
        ));
    }

    #[test]
    fn test_is_webrtc_invalid_host() {
        assert!(is_webrtc_invalid_host("abc123.invalid"));
        assert!(is_webrtc_invalid_host("x.invalid"));
        assert!(!is_webrtc_invalid_host("rustpbx.com"));
        assert!(!is_webrtc_invalid_host("10.0.0.1"));
        assert!(!is_webrtc_invalid_host(""));
    }

    #[test]
    fn test_invalid_host_fallback_extracts_params() {
        let uri: rsipstack::sip::Uri =
            "sip:qn27nogk@2kbkhn3beiif.invalid;transport=ws;ob".try_into().unwrap();
        let fb = invalid_host_fallback(&uri).expect("should produce fallback");
        assert_eq!(fb.user, "qn27nogk");
        assert_eq!(fb.host, "2kbkhn3beiif.invalid");
    }

    #[test]
    fn test_invalid_host_fallback_returns_none_for_non_invalid() {
        let uri: rsipstack::sip::Uri = "sip:alice@pbx.example.com".try_into().unwrap();
        assert!(invalid_host_fallback(&uri).is_none());
    }

    #[test]
    fn test_invalid_host_fallback_returns_none_for_no_user() {
        let uri: rsipstack::sip::Uri = "sip:@2kbkhn3beiif.invalid".try_into().unwrap();
        assert!(invalid_host_fallback(&uri).is_none());
    }

    #[test]
    fn test_aor_like_pattern_format() {
        let fb = InvalidHostFallback {
            user: "qn27nogk".into(),
            host: "2kbkhn3beiif.invalid".into(),
        };
        assert_eq!(fb.aor_like_pattern(), "%qn27nogk@2kbkhn3beiif.invalid%");
    }

    #[test]
    fn test_invalid_host_fallback_matches_aor_ignores_params() {
        let fb = InvalidHostFallback {
            user: "qn27nogk".into(),
            host: "2kbkhn3beiif.invalid".into(),
        };
        // Same user+host, different params → match
        let aor: rsipstack::sip::Uri =
            "sip:qn27nogk@2kbkhn3beiif.invalid;transport=ws".try_into().unwrap();
        assert!(fb.matches_aor(&aor));

        // Different user → no match
        let aor2: rsipstack::sip::Uri =
            "sip:other@2kbkhn3beiif.invalid;transport=ws".try_into().unwrap();
        assert!(!fb.matches_aor(&aor2));

        // Different host → no match
        let aor3: rsipstack::sip::Uri =
            "sip:qn27nogk@other.invalid;transport=ws".try_into().unwrap();
        assert!(!fb.matches_aor(&aor3));
    }

    #[tokio::test]
    async fn memory_locator_orders_by_last_modified() {
        let locator = MemoryLocator::new();
        let uri: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().unwrap();

        let now = Instant::now();
        let older = now - Duration::from_secs(120);

        let destination_primary = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("127.0.0.1:5060").unwrap(),
        };

        let destination_secondary = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("127.0.0.1:5070").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("rustpbx.com"),
                Location {
                    aor: uri.clone(),
                    expires: 3600,
                    destination: Some(destination_secondary),
                    last_modified: Some(older),
                    instance_id: Some("secondary".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        locator
            .register(
                "alice",
                Some("rustpbx.com"),
                Location {
                    aor: uri.clone(),
                    expires: 3600,
                    destination: Some(destination_primary),
                    last_modified: Some(now),
                    instance_id: Some("primary".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&uri).await.unwrap();
        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].instance_id.as_deref(), Some("primary"));
        assert_eq!(locations[1].instance_id.as_deref(), Some("secondary"));
    }

    #[tokio::test]
    async fn memory_locator_matches_localhost_alias() {
        let locator = MemoryLocator::new();
        let registered_uri: rsipstack::sip::Uri = "sip:alice@192.168.3.181".try_into().unwrap();
        let lookup_uri: rsipstack::sip::Uri = "sip:alice@localhost".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.168.3.181:5060").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("192.168.3.181"),
                Location {
                    aor: registered_uri.clone(),
                    registered_aor: Some(registered_uri.clone()),
                    expires: 3600,
                    destination: Some(destination),
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].aor.to_string(), registered_uri.to_string());
    }

    #[test]
    fn test_is_local_realm_logic() {
        assert!(is_local_realm("localhost"));
        assert!(is_local_realm("127.0.0.1"));
        assert!(is_local_realm("0.0.0.0"));
        assert!(is_local_realm("::1"));
        assert!(is_local_realm("192.168.1.1"));
        assert!(is_local_realm("10.0.0.1"));
        assert!(is_local_realm("172.16.0.1"));
        assert!(is_local_realm("[::1]"));
        assert!(is_local_realm("127.0.0.1:5060"));

        assert!(!is_local_realm("rustpbx.com"));
        assert!(!is_local_realm("8.8.8.8"));
        assert!(!is_local_realm(""));
    }

    #[tokio::test]
    async fn test_realm_matches_logic() {
        let locator = MemoryLocator::new();
        // Both local
        assert!(realm_matches(&locator, "127.0.0.1", "localhost").await);
        assert!(realm_matches(&locator, "192.168.1.1", "10.0.0.1").await);

        // One local, one not
        assert!(!realm_matches(&locator, "127.0.0.1", "rustpbx.com").await);
        assert!(!realm_matches(&locator, "rustpbx.com", "127.0.0.1").await);

        // Both same non-local
        assert!(realm_matches(&locator, "rustpbx.com", "rustpbx.com").await);
        assert!(realm_matches(&locator, "rustpbx.com:5060", "rustpbx.com").await);

        // Different non-local
        assert!(!realm_matches(&locator, "rustpbx.com", "other.com").await);
    }

    #[tokio::test]
    async fn test_custom_realm_checker() {
        let locator = MemoryLocator::new();
        // Custom checker that treats "my-special-realm.com" as local
        locator.set_realm_checker(Arc::new(|realm| {
            let is_special = realm == "my-special-realm.com";
            let realm = realm.to_string();
            Box::pin(async move { is_special || is_local_realm(&realm) })
        }));

        let registered_uri: rsipstack::sip::Uri =
            "sip:alice@my-special-realm.com".try_into().unwrap();
        let lookup_uri: rsipstack::sip::Uri = "sip:alice@localhost".try_into().unwrap();

        locator
            .register(
                "alice",
                Some("my-special-realm.com"),
                Location {
                    aor: registered_uri.clone(),
                    expires: 3600,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(locations.len(), 1);
    }

    #[tokio::test]
    async fn test_uri_matches_relaxed() {
        let locator = MemoryLocator::new();
        let registered_uri: rsipstack::sip::Uri =
            "sip:3sf0hatf@eee3se8lru7o.invalid".try_into().unwrap();
        let lookup_uri: rsipstack::sip::Uri = "sip:3sf0hatf@eee3se8lru7o.invalid;transport=ws"
            .try_into()
            .unwrap();

        locator
            .register(
                "test_user",
                Some("localhost"),
                Location {
                    aor: registered_uri.clone(),
                    expires: 3600,
                    transport: Some(Transport::Ws),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(locations.len(), 1);

        let lookup_uri_no_transport: rsipstack::sip::Uri =
            "sip:3sf0hatf@eee3se8lru7o.invalid".try_into().unwrap();
        let locations = locator.lookup(&lookup_uri_no_transport).await.unwrap();
        assert_eq!(locations.len(), 1);
    }

    #[tokio::test]
    async fn test_lookup_with_invalid_host_but_registered_with_real_domain() {
        // JsSIP registers with Contact using random .invalid hostname
        // but the registration is stored with the real domain.
        // Lookup with .invalid should find the registration by username.
        let locator = MemoryLocator::new();
        let registered_uri: rsipstack::sip::Uri =
            "sip:test_user@real-domain.com".try_into().unwrap();
        let lookup_uri: rsipstack::sip::Uri = "sip:test_user@abcdef123456.invalid;transport=ws"
            .try_into()
            .unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Ws),
            addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
        };

        locator
            .register(
                "test_user",
                Some("real-domain.com"),
                Location {
                    aor: registered_uri.clone(),
                    expires: 3600,
                    destination: Some(destination.clone()),
                    transport: Some(Transport::Ws),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let locations = locator.lookup(&lookup_uri).await.unwrap();
        assert_eq!(
            locations.len(),
            1,
            "should find registration via .invalid lookup"
        );
        assert_eq!(
            locations[0].destination,
            Some(destination),
            "destination should match the WebSocket connection"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_routes_registered_aor_via_home_proxy() {
        let locator = MemoryLocator::new();
        let registered_aor: rsipstack::sip::Uri = "sip:alice@10.0.0.1".try_into().unwrap();
        let target_uri: rsipstack::sip::Uri = "sip:alice@10.0.0.1:5060".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        };

        let home_proxy = SipAddr {
            r#type: Some(Transport::Tcp),
            addr: HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("10.0.0.1"),
                Location {
                    aor: registered_aor.clone(),
                    expires: 3600,
                    destination: Some(destination.clone()),
                    home_proxy: Some(home_proxy.clone()),
                    registered_aor: Some(registered_aor),
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let target_locator = DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![], true);
        let result = target_locator.locate(&target_uri).await.unwrap();
        assert_eq!(
            result, home_proxy,
            "DialogTargetLocator should route registered AOR via home_proxy"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_routes_invalid_contact_via_remote_home_proxy() {
        // Cluster scenario: a WebRTC (JsSIP) client is registered on Node A
        // (remote) with a .invalid contact. Node B (local) must route the BYE
        // — whose Request-URI is the .invalid contact — to the remote
        // home_proxy (Node A), NOT to the WebSocket destination which is a
        // private socket only reachable from Node A.
        let locator = MemoryLocator::new();

        // Canonical AoR + the .invalid WebRTC contact
        let registered_aor: rsipstack::sip::Uri = "sip:bob@rustpbx.com".try_into().unwrap();
        let contact_uri: rsipstack::sip::Uri =
            "sip:bob@7s8f2k.invalid;transport=ws".try_into().unwrap();

        // WebSocket connection private to Node A (not reachable from Node B)
        let ws_destination = SipAddr {
            r#type: Some(Transport::Wss),
            addr: HostWithPort::try_from("198.51.100.10:51234").unwrap(),
        };

        // Node A's advertised SIP address (home_proxy, remote)
        let home_proxy = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.5:5060").unwrap(),
        };

        locator
            .register(
                "bob",
                Some("rustpbx.com"),
                Location {
                    aor: contact_uri.clone(),
                    expires: 3600,
                    destination: Some(ws_destination.clone()),
                    home_proxy: Some(home_proxy.clone()),
                    registered_aor: Some(registered_aor),
                    transport: Some(Transport::Wss),
                    supports_webrtc: true,
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // DialogTargetLocator running on Node B (local_addrs = Node B),
        // cluster enabled. Node A's home_proxy is NOT in local_addrs.
        let node_b = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.6:5060").unwrap(),
        };
        let target_locator =
            DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![node_b], true);

        // The BYE Request-URI is the .invalid contact
        let result = target_locator.locate(&contact_uri).await.unwrap();
        assert_eq!(
            result, home_proxy,
            "DialogTargetLocator must route .invalid contact to remote home_proxy in cluster, \
             not to the remote node's private WS destination"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_routes_canonical_aor_via_home_proxy() {
        // Regression guard: an exact canonical-AoR lookup (registered_aor == uri)
        // must route via home_proxy with NO host requirement. This is the original
        // cluster-forwarding path and must not be broken by the .invalid changes.
        let locator = MemoryLocator::new();
        let canonical_aor: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Wss),
            addr: HostWithPort::try_from("198.51.100.10:51234").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.5:5060").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("rustpbx.com"),
                Location {
                    aor: canonical_aor.clone(),
                    expires: 3600,
                    destination: Some(destination),
                    home_proxy: Some(home_proxy.clone()),
                    registered_aor: Some(canonical_aor.clone()),
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Node B (local); home_proxy 10.0.0.5 is remote.
        let node_b = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.6:5060").unwrap(),
        };
        let target_locator =
            DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![node_b], true);

        // Lookup by canonical AoR — uri.host (rustpbx.com) != home_proxy.addr.
        let result = target_locator.locate(&canonical_aor).await.unwrap();
        assert_eq!(
            result, home_proxy,
            "canonical AoR exact match must route via home_proxy regardless of uri host"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_fallback_never_leaks_remote_destination() {
        // Fix 2 guard: even when the cluster-match predicate (Fix 1) does not
        // match — here because registered_aor is absent — the fallback must
        // still route to a remote home_proxy instead of returning the remote
        // node's private WS destination.
        let locator = MemoryLocator::new();
        let contact_uri: rsipstack::sip::Uri =
            "sip:bob@9x2k4j.invalid;transport=ws".try_into().unwrap();

        let ws_destination = SipAddr {
            r#type: Some(Transport::Wss),
            addr: HostWithPort::try_from("198.51.100.10:51234").unwrap(),
        };
        let home_proxy = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.5:5060").unwrap(),
        };

        locator
            .register(
                "bob",
                Some("rustpbx.com"),
                Location {
                    aor: contact_uri.clone(),
                    expires: 3600,
                    destination: Some(ws_destination),
                    home_proxy: Some(home_proxy.clone()),
                    transport: Some(Transport::Wss),
                    // registered_aor intentionally None so Fix 1 predicate
                    // cannot match, forcing the fallback (Fix 2) path.
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let node_b = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.6:5060").unwrap(),
        };
        let target_locator =
            DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![node_b], true);

        let result = target_locator.locate(&contact_uri).await.unwrap();
        assert_eq!(
            result, home_proxy,
            "fallback must route to remote home_proxy, never leak the remote WS destination"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_routes_local_registered_aor_to_destination() {
        let locator = MemoryLocator::new();
        let uri: rsipstack::sip::Uri = "sip:alice@pbx.rustpbx.com".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Ws),
            addr: HostWithPort::try_from("127.0.0.1:63255").unwrap(),
        };

        let home_proxy = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("pbx.rustpbx.com"),
                Location {
                    aor: "sip:abc@invalid.invalid;transport=ws".try_into().unwrap(),
                    expires: 3600,
                    destination: Some(destination.clone()),
                    home_proxy: Some(home_proxy.clone()),
                    registered_aor: Some(uri.clone()),
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let target_locator =
            DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![home_proxy], false);
        let result = target_locator.locate(&uri).await.unwrap();
        assert_eq!(
            result, destination,
            "local registered AOR should be delivered to the stored device/WS destination"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_keeps_contact_destination_for_non_registered_aor() {
        let locator = MemoryLocator::new();
        let uri: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().unwrap();
        let registered_aor: rsipstack::sip::Uri = "sip:alice@pbx.rustpbx.com".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        };

        let home_proxy = SipAddr {
            r#type: Some(Transport::Tcp),
            addr: HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        };

        locator
            .register(
                "alice",
                Some("rustpbx.com"),
                Location {
                    aor: uri.clone(),
                    expires: 3600,
                    destination: Some(destination.clone()),
                    home_proxy: Some(home_proxy),
                    registered_aor: Some(registered_aor),
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let target_locator = DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![], false);
        let result = target_locator.locate(&uri).await.unwrap();
        assert_eq!(
            result, destination,
            "DialogTargetLocator should keep contact destination when URI is not registered AOR"
        );
    }

    #[test]
    fn sort_locations_prefers_home_proxy_binding() {
        let now = Instant::now();
        let aor: rsipstack::sip::Uri = "sip:alice@rustpbx.com".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.10:5060").unwrap(),
        };

        let home_proxy = SipAddr {
            r#type: Some(Transport::Tcp),
            addr: HostWithPort::try_from("10.0.0.1:5060").unwrap(),
        };

        let with_home_proxy = Location {
            aor: aor.clone(),
            destination: Some(destination.clone()),
            home_proxy: Some(home_proxy),
            last_modified: Some(now),
            ..Default::default()
        };

        let without_home_proxy = Location {
            aor,
            destination: Some(destination),
            last_modified: Some(now - Duration::from_secs(1)),
            ..Default::default()
        };

        let sorted = sort_locations_by_recency(vec![without_home_proxy, with_home_proxy.clone()]);
        assert_eq!(sorted.len(), 1);
        assert!(
            sorted[0].home_proxy.is_some(),
            "location variant with home_proxy should be retained during dedupe"
        );
    }

    #[tokio::test]
    async fn dialog_target_locator_fallback_to_destination() {
        let locator = MemoryLocator::new();
        let uri: rsipstack::sip::Uri = "sip:bob@rustpbx.com".try_into().unwrap();

        let destination = SipAddr {
            r#type: Some(Transport::Udp),
            addr: HostWithPort::try_from("192.168.1.20:5060").unwrap(),
        };

        locator
            .register(
                "bob",
                Some("rustpbx.com"),
                Location {
                    aor: uri.clone(),
                    expires: 3600,
                    destination: Some(destination.clone()),
                    home_proxy: None,
                    last_modified: Some(Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let target_locator = DialogTargetLocator::new(Arc::new(Box::new(locator)), vec![], false);
        let result = target_locator.locate(&uri).await.unwrap();
        assert_eq!(
            result, destination,
            "DialogTargetLocator should fallback to destination when home_proxy is absent"
        );
    }
}
