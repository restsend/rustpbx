use crate::config::AuthCacheConfig;
use lru::LruCache;
use rsipstack::transport::SipAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, trace};

/// Cache key using call-id and from-tag, which remain stable throughout a dialog.
/// Unlike full DialogId, this does not change when the To tag is added during dialog establishment.
pub type AuthCacheKey = (String, String);

/// Cache entry storing authenticated dialog information with source address
#[derive(Clone, Debug)]
struct AuthCacheEntry {
    source_addr: String,
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
        let cache_size = NonZeroUsize::new(config.cache_size).unwrap_or_else(|| {
            NonZeroUsize::new(10000).unwrap()
        });

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
    pub async fn put(&self, key: AuthCacheKey, source_addr: SipAddr) {
        let entry = AuthCacheEntry {
            source_addr: source_addr.to_string(),
            authenticated_at: Instant::now(),
            ttl: self.ttl,
        };

        let mut cache = self.inner.lock().await;
        cache.put(key.clone(), entry);

        debug!(
            call_id = %key.0,
            from_tag = %key.1,
            source_addr = %source_addr,
            "Cached authenticated dialog"
        );
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
    pub async fn is_authenticated(&self, key: &AuthCacheKey, source_addr: &SipAddr) -> bool {
        let mut cache = self.inner.lock().await;

        if let Some(entry) = cache.get(key) {
            if entry.is_expired() {
                trace!(
                    call_id = %key.0,
                    from_tag = %key.1,
                    "Dialog auth cache entry expired"
                );
                return false;
            }

            let source_str = source_addr.to_string();
            if entry.source_addr == source_str {
                trace!(
                    call_id = %key.0,
                    from_tag = %key.1,
                    source_addr = %source_addr,
                    "Dialog auth cache hit, skipping authentication"
                );
                return true;
            }

            trace!(
                call_id = %key.0,
                from_tag = %key.1,
                cached_addr = %entry.source_addr,
                current_addr = %source_addr,
                "Dialog auth cache source address mismatch"
            );
        }

        false
    }

    /// Remove a dialog from the cache.
    pub async fn remove(&self, key: &AuthCacheKey) {
        let mut cache = self.inner.lock().await;
        cache.pop(key);
    }

    /// Get the current cache size.
    pub async fn len(&self) -> usize {
        let cache = self.inner.lock().await;
        cache.len()
    }

    /// Check if the cache is empty.
    pub async fn is_empty(&self) -> bool {
        let cache = self.inner.lock().await;
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
        ("test-call-id".to_string(), "from-tag".to_string())
    }

    fn create_test_sip_addr(host: &str, port: u16) -> SipAddr {
        SipAddr::new(
            Transport::Udp,
            HostWithPort::try_from(format!("{}:{}", host, port)).unwrap(),
        )
    }

    #[tokio::test]
    async fn test_cache_hit() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        cache.put(key.clone(), source_addr.clone()).await;

        assert!(
            cache.is_authenticated(&key, &source_addr).await,
            "Should authenticate cached dialog with matching source"
        );
    }

    #[tokio::test]
    async fn test_cache_miss_different_source() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr1 = create_test_sip_addr("192.168.1.100", 5060);
        let source_addr2 = create_test_sip_addr("192.168.1.101", 5060);

        cache.put(key.clone(), source_addr1).await;

        assert!(
            !cache.is_authenticated(&key, &source_addr2).await,
            "Should not authenticate with different source address"
        );
    }

    #[tokio::test]
    async fn test_cache_miss_unknown_dialog() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        assert!(
            !cache.is_authenticated(&key, &source_addr).await,
            "Should not authenticate unknown dialog"
        );
    }

    #[tokio::test]
    async fn test_cache_ttl_expiration() {
        let mut config = create_test_config();
        config.ttl_seconds = 1; // 1 second TTL

        let cache = DialogAuthCache::new(&config);
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        cache.put(key.clone(), source_addr.clone()).await;

        assert!(
            cache.is_authenticated(&key, &source_addr).await,
            "Should authenticate before TTL expires"
        );

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(
            !cache.is_authenticated(&key, &source_addr).await,
            "Should not authenticate after TTL expires"
        );
    }

    #[tokio::test]
    async fn test_cache_lru_eviction() {
        let mut config = create_test_config();
        config.cache_size = 2; // Very small cache

        let cache = DialogAuthCache::new(&config);
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        let key1: AuthCacheKey = ("call-1".to_string(), "tag-1".to_string());
        let key2: AuthCacheKey = ("call-2".to_string(), "tag-2".to_string());
        let key3: AuthCacheKey = ("call-3".to_string(), "tag-3".to_string());

        cache.put(key1.clone(), source_addr.clone()).await;
        cache.put(key2.clone(), source_addr.clone()).await;
        cache.put(key3.clone(), source_addr.clone()).await;

        // key1 should be evicted (LRU)
        assert!(
            !cache.is_authenticated(&key1, &source_addr).await,
            "Oldest entry should be evicted"
        );

        // key2 and key3 should still be in cache
        assert!(
            cache.is_authenticated(&key2, &source_addr).await,
            "Recent entry should still be cached"
        );
        assert!(
            cache.is_authenticated(&key3, &source_addr).await,
            "Newest entry should still be cached"
        );
    }

    #[tokio::test]
    async fn test_cache_remove() {
        let cache = DialogAuthCache::new(&create_test_config());
        let key = create_test_key();
        let source_addr = create_test_sip_addr("192.168.1.100", 5060);

        cache.put(key.clone(), source_addr.clone()).await;
        assert!(cache.is_authenticated(&key, &source_addr).await);

        cache.remove(&key).await;
        assert!(!cache.is_authenticated(&key, &source_addr).await);
    }
}
