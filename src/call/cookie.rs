use crate::call::SipUser;
use rsipstack::transaction::key::TransactionKey;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Default)]
pub enum SpamResult {
    #[default]
    Nice,
    Spam,
    IpBlacklist,
    UaBlacklist,
}

impl SpamResult {
    fn is_spam(&self) -> bool {
        !matches!(self, SpamResult::Nice)
    }
}

#[derive(Default)]
struct TransactionCookieInner {
    user: Option<SipUser>,
    spam_result: SpamResult,
    max_duration: Option<Duration>,
    extensions: http::Extensions,
}
#[derive(Clone, Default)]
pub struct TransactionCookie {
    inner: Arc<RwLock<TransactionCookieInner>>,
}

impl From<&TransactionKey> for TransactionCookie {
    fn from(_key: &TransactionKey) -> Self {
        Self {
            inner: Arc::new(RwLock::new(TransactionCookieInner {
                user: None,
                spam_result: SpamResult::Nice,
                max_duration: None,
                extensions: http::Extensions::new(),
            })),
        }
    }
}

impl TransactionCookie {
    pub fn set_user(&self, user: SipUser) {
        if let Ok(mut inner) = self.inner.write() {
            inner.user = Some(user);
        }
    }
    pub fn get_user(&self) -> Option<SipUser> {
        self.inner
            .read()
            .map(|inner| inner.user.clone())
            .ok()
            .flatten()
    }

    pub fn mark_as_spam(&self, r: SpamResult) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.spam_result = r;
            })
            .ok();
    }

    pub fn is_spam(&self) -> bool {
        self.inner
            .try_read()
            .map(|inner| inner.spam_result.is_spam())
            .ok()
            .unwrap_or_default()
    }

    pub fn set_max_duration(&self, duration: Duration) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.max_duration = Some(duration);
            })
            .ok();
    }

    pub fn get_max_duration(&self) -> Option<Duration> {
        self.inner
            .try_read()
            .map(|inner| inner.max_duration)
            .ok()
            .flatten()
    }

    pub fn with_extensions<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&http::Extensions) -> R,
    {
        self.inner.read().ok().map(|inner| f(&inner.extensions))
    }

    pub fn with_extensions_mut<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut http::Extensions) -> R,
    {
        self.inner
            .write()
            .ok()
            .map(|mut inner| f(&mut inner.extensions))
    }

    pub fn insert_extension<T: Clone + Send + Sync + 'static>(&self, val: T) {
        self.with_extensions_mut(|ext| ext.insert(val));
    }

    pub fn get_extension<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.with_extensions(|ext| ext.get::<T>().cloned())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalleeDisplayName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkContext {
    pub id: Option<i64>,
    pub name: String,
    pub tenant_id: Option<i64>,
}
