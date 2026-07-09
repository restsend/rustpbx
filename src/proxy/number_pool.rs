use crate::call::{Dialplan, TransactionCookie, TrunkContext};
use crate::proxy::call::{DialplanInspector, DialplanVerdict};
use lru::LruCache;
use parking_lot::Mutex;
use rsipstack::sip::Request as SipRequest;
use std::num::NonZeroUsize;
use std::sync::OnceLock;

/// Upper bound on the number of DIDs we track per-process for the
/// "least-used" assignment strategy. Real deployments use far fewer DIDs, so
/// this cap only evicts genuinely stale entries (whose count would otherwise
/// stay artificially low forever, biasing assignment toward the same DID).
/// Eviction is safe: a forgotten DID simply competes as if freshly seen.
const NUMBER_POOL_USAGE_CAP: usize = 10_000;

static USAGE: OnceLock<Mutex<LruCache<String, u64>>> = OnceLock::new();

fn usage_counter() -> &'static Mutex<LruCache<String, u64>> {
    USAGE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(NUMBER_POOL_USAGE_CAP).expect("non-zero cap"),
        ))
    })
}

pub struct NumberPoolInspector;

impl NumberPoolInspector {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NumberPoolInspector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl DialplanInspector for NumberPoolInspector {
    async fn inspect_dialplan(
        &self,
        mut dialplan: Dialplan,
        cookie: &TransactionCookie,
        _original: &SipRequest,
    ) -> DialplanVerdict {
        let Some(ctx) = cookie.get_extension::<TrunkContext>() else {
            return DialplanVerdict::Continue(dialplan);
        };

        if ctx.did_numbers.is_empty() {
            return DialplanVerdict::Continue(dialplan);
        }

        let counter = usage_counter();
        // Use `peek` so the min-scan does not perturb LRU ordering — only the
        // actually-selected DID should be promoted/marked-used. Pass `&str`
        // so the `String: Borrow<str>` lookup bound is satisfied.
        let did = {
            let usage = counter.lock();
            ctx.did_numbers
                .iter()
                .min_by_key(|d| usage.peek(d.as_str()).copied().unwrap_or(0))
                .cloned()
                .unwrap_or_default()
        };

        if let Ok(uri) = format!("sip:{}", did).parse() {
            tracing::info!(
                trunk = %ctx.name,
                did = %did,
                "number pool: assigned least-used DID as caller"
            );
            dialplan.caller = Some(uri);
        }

        // Bump usage counter for the chosen DID, evicting the least-recently
        // used entry when at capacity. The cap keeps the table bounded; an
        // evicted DID simply rejoins the pool as "freshly seen" (count 0).
        let mut usage = counter.lock();
        if let Some(count) = usage.get_mut(&did) {
            *count += 1;
        } else {
            usage.put(did.clone(), 1);
        }

        DialplanVerdict::Continue(dialplan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_cache_is_bounded_and_increments() {
        let counter = usage_counter();
        // Sanity: cap is positive and matches the documented constant.
        let cap = NonZeroUsize::new(NUMBER_POOL_USAGE_CAP).unwrap();
        assert_eq!(counter.lock().cap(), cap);

        // Helper to bump and read.
        let bump = |did: &str| {
            let mut usage = counter.lock();
            if let Some(count) = usage.get_mut(did) {
                *count += 1;
            } else {
                usage.put(did.to_string(), 1);
            }
        };
        let read = |did: &str| -> Option<u64> { counter.lock().peek(did).copied() };

        bump("1001");
        bump("1001");
        bump("1002");
        assert_eq!(read("1001"), Some(2));
        assert_eq!(read("1002"), Some(1));

        // Eviction: pushing `cap` distinct DIDs must evict "1001"/"1002".
        {
            let mut usage = counter.lock();
            for i in 0..NUMBER_POOL_USAGE_CAP {
                usage.put(format!("did-{i}"), 1);
            }
            assert_eq!(usage.len(), NUMBER_POOL_USAGE_CAP);
        }
        assert!(read("1001").is_none(), "1001 should have been evicted");
        assert!(read("1002").is_none(), "1002 should have been evicted");

        // After eviction, the same DID re-inserts as a fresh entry (count starts from 1).
        bump("1001");
        assert_eq!(read("1001"), Some(1));
    }
}
