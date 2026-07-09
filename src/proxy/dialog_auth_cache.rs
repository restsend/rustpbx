use crate::config::AuthCacheConfig;
use lru::LruCache;
use parking_lot::Mutex;
use rsipstack::transport::SipAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cache key using call-id and from-tag, which remain stable throughout a dialog.
/// Unlike full DialogId, this does not change when the To tag is added during dialog establishment.
pub type AuthCacheKey = (String, String);

/// Cache entry storing authenticated dialog information with source address
#[derive(Clone, Debug)]
struct AuthCacheEntry {
    source_addr: SipAddr,
    authenticated_at: Instant,
    ttl: Duration,
}

impl AuthCacheEntry {
    fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.authenticated_at) > self.ttl
    }
}

/// Dialog authentication cache with LRU eviction and TTL support.
///
/// When a dialog is successfully authenticated, its call-id + from-tag and source address
/// are cached. Subsequent in-dialog requests from the same source address can
/// skip authentication if the cache entry is still valid.
#[derive(Clone)]
pub struct DialogAuthCache {
    inner: Arc<Mutex<LruCache<AuthCacheKey, AuthCacheEntry>>>,
    ttl: Duration,
}

impl DialogAuthCache {
    pub fn new(config: &AuthCacheConfig) -> Self {
        let cache_size = NonZeroUsize::new(config.cache_size)
            .unwrap_or_else(|| NonZeroUsize::new(10000).unwrap());

        Self {
            inner: Arc::new(Mutex::new(LruCache::new(cache_size))),
            ttl: Duration::from_secs(config.ttl_seconds),
        }
    }

    /// Store an authenticated dialog in the cache.
    ///
    /// # Arguments
    /// * `key` - The cache key (call_id, from_tag) which is stable throughout the dialog
    /// * `source_addr` - The source address (IP:port) that authenticated successfully
    pub fn put(&self, key: AuthCacheKey, source_addr: SipAddr) {
        let entry = AuthCacheEntry {
            source_addr,
            authenticated_at: Instant::now(),
            ttl: self.ttl,
        };

        self.inner.lock().put(key.clone(), entry);
    }

    /// Check if an in-dialog request can skip authentication.
    ///
    /// Returns true if:
    /// - The cache key exists in the cache
    /// - The source address matches the cached address
    /// - The cache entry has not expired (TTL)
    ///
    /// # Arguments
    /// * `key` - The cache key (call_id, from_tag)
    /// * `source_addr` - The current source address of the request
    pub fn is_authenticated(&self, key: &AuthCacheKey, source_addr: &SipAddr) -> bool {
        let mut cache = self.inner.lock();

        if let Some(entry) = cache.get(key) {
            if entry.is_expired() {
                return false;
            }

            if entry.source_addr == *source_addr {
                return true;
            }
        }
        false
    }

    /// Remove a dialog from the cache.
    pub fn remove(&self, key: &AuthCacheKey) {
        let mut cache = self.inner.lock();
        cache.pop(key);
    }

    /// Get the current cache size.
    pub fn len(&self) -> usize {
        let cache = self.inner.lock();
        cache.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        let cache = self.inner.lock();
        cache.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::{HostWithPort, Transport};
    use std::time::Duration;

    fn create_test_config() -> AuthCacheConfig {
        AuthCacheConfig {
            enabled: true,
            cache_size: 100,
            ttl_seconds: 3600,
        }
    }

    fn create_test_key() -> AuthCacheKey {
        ("test-call-id".to_string(), "test-from-tag".to_string())
    }

    fn create_test_sip_addr(host: &str, port: u16) -> SipAddr {
        SipAddr::new(
            Transport::Udp,
            HostWithPort::try_from(format!("{}:{}", host, port)).unwrap(),
        )
    }

    #[test]
    fn test_cache_hit() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        cache.put(key.clone(), source_addr.clone());

        assert!(
            cache.is_authenticated(&key, &source_addr),
            "Should authenticate cached dialog with matching source"
        );
    }

    #[test]
    fn test_cache_miss_different_source() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr1 = create_test_sip_addr("192.168.1.100", 5060);
        let source_addr2 = create_test_sip_addr("192.168.1.101", 5060);

        cache.put(key.clone(), source_addr1);

        assert!(
            !cache.is_authenticated(&key, &source_addr2),
            "Should not authenticate with different source address"
        );
    }

    #[test]
    fn test_cache_miss_unknown_dialog() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        assert!(
            !cache.is_authenticated(&key, &source_addr),
            "Should not authenticate unknown dialog"
        );
    }

    #[test]
    fn test_cache_ttl_expiration() {
        let mut config = create_test_config();
        config.ttl_seconds = 1; // 1 second TTL

        let cache = DialogAuthCache::new(&config);
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        cache.put(key.clone(), source_addr.clone());

        assert!(
            cache.is_authenticated(&key, &source_addr),
            "Should authenticate before TTL expires"
        );

        // Wait for TTL to expire
        std::thread::sleep(Duration::from_secs(2));

        assert!(
            !cache.is_authenticated(&key, &source_addr),
            "Should not authenticate after TTL expires"
        );
    }

    #[test]
    fn test_cache_lru_eviction() {
        let mut config = create_test_config();
        config.cache_size = 2; // Very small cache

        let cache = DialogAuthCache::new(&config);
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        let key1: AuthCacheKey = ("call-1".to_string(), "from-tag-1".to_string());
        let key2: AuthCacheKey = ("call-2".to_string(), "from-tag-2".to_string());
        let key3: AuthCacheKey = ("call-3".to_string(), "from-tag-3".to_string());

        cache.put(key1.clone(), source_addr.clone());
        cache.put(key2.clone(), source_addr.clone());
        cache.put(key3.clone(), source_addr.clone());

        // key1 should be evicted (LRU)
        assert!(
            !cache.is_authenticated(&key1, &source_addr),
            "Oldest entry should be evicted"
        );

        // key2 and key3 should still be in cache
        assert!(
            cache.is_authenticated(&key2, &source_addr),
            "Recent entry should still be cached"
        );
        assert!(
            cache.is_authenticated(&key3, &source_addr),
            "Newest entry should still be cached"
        );
    }

    #[test]
    fn test_cache_remove() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        cache.put(key.clone(), source_addr.clone());
        assert!(cache.is_authenticated(&key, &source_addr));

        cache.remove(&key);
        assert!(!cache.is_authenticated(&key, &source_addr));
    }
}
