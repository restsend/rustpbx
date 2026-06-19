use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use lru::LruCache;
use rsipstack::sip::{Header, prelude::HeadersExt};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::call::{TransactionCookie, user::SipUser};
use crate::proxy::auth::{AuthBackend, AuthError};
use crate::proxy::user_http::HttpUserBackend;

struct CacheEntry {
    user: SipUser,
    inserted_at: Instant,
}

struct TokenCache {
    inner: Mutex<LruCache<String, CacheEntry>>,
    ttl: Duration,
}

impl TokenCache {
    fn new(max_size: NonZeroUsize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(max_size)),
            ttl,
        }
    }

    async fn get(&self, key: &str) -> Option<SipUser> {
        let mut cache = self.inner.lock().await;
        if let Some(entry) = cache.get(key) {
            if entry.inserted_at.elapsed() < self.ttl {
                debug!(key_prefix = &key[..8.min(key.len())], "token cache hit");
                return Some(entry.user.clone());
            } else {
                cache.pop(key);
                debug!(
                    key_prefix = &key[..8.min(key.len())],
                    "token cache entry expired"
                );
            }
        }
        None
    }

    async fn put(&self, key: String, user: SipUser) {
        let mut cache = self.inner.lock().await;
        cache.put(
            key.clone(),
            CacheEntry {
                user,
                inserted_at: Instant::now(),
            },
        );
    }
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(result)
}

pub struct HttpTokenAuthBackend {
    backend: HttpUserBackend,
    token_header: String,
    cache: Option<TokenCache>,
}

impl HttpTokenAuthBackend {
    pub fn new(
        backend: HttpUserBackend,
        token_header: String,
        cache_ttl: Duration,
        cache_size: usize,
    ) -> Self {
        let cache = if cache_ttl.is_zero() || cache_size == 0 {
            None
        } else {
            let max =
                NonZeroUsize::new(cache_size).unwrap_or_else(|| NonZeroUsize::new(10000).unwrap());
            Some(TokenCache::new(max, cache_ttl))
        };

        Self {
            backend,
            token_header,
            cache,
        }
    }

    fn extract_token(&self, request: &rsipstack::sip::Request) -> Option<String> {
        request.headers.iter().find_map(|h| {
            if let Header::Other(name, val) = h {
                if name.eq_ignore_ascii_case(&self.token_header) {
                    return Some(val.clone());
                }
            }
            None
        })
    }
}

#[async_trait]
impl AuthBackend for HttpTokenAuthBackend {
    async fn authenticate(
        &self,
        original: &rsipstack::sip::Request,
        _cookie: &TransactionCookie,
    ) -> Result<Option<SipUser>, AuthError> {
        let token = match self.extract_token(original) {
            Some(t) => t.trim().to_string(),
            None => return Ok(None),
        };

        let cache_key = hash_token(&token);

        if let Some(ref cache) = self.cache {
            if let Some(user) = cache.get(&cache_key).await {
                return Ok(Some(user));
            }
        }

        let username = original
            .from_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .and_then(|uri| uri.user().map(|u| u.to_string()))
            .unwrap_or_default();
        let realm = original.uri().host().to_string();

        match self
            .backend
            .fetch_user(&username, Some(&realm), Some(original))
            .await
        {
            Ok(Some(user)) => {
                if !user.enabled {
                    info!(username = %username, "Token-authenticated user is disabled");
                    return Ok(None);
                }
                if let Some(ref cache) = self.cache {
                    cache.put(cache_key, user.clone()).await;
                }
                Ok(Some(user))
            }
            Ok(None) => {
                info!(username = %username, "HTTP token auth returned no user, falling back");
                Ok(None)
            }
            Err(e) => {
                warn!(error = %e, username = %username, "HTTP token auth error, falling back to next backend");
                Ok(None)
            }
        }
    }
}
