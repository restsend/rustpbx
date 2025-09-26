use crate::call::SipUser;
use rsipstack::transaction::key::TransactionKey;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
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
    values: HashMap<String, String>,
    spam_result: SpamResult,
    source_trunk: Option<String>,
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
                values: HashMap::new(),
                spam_result: SpamResult::Nice,
                source_trunk: None,
            })),
        }
    }
}

impl TransactionCookie {
    pub fn set_user(&self, user: SipUser) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.user = Some(user);
            })
            .ok();
    }
    pub fn get_user(&self) -> Option<SipUser> {
        self.inner
            .try_read()
            .map(|inner| inner.user.clone())
            .ok()
            .flatten()
    }
    pub fn set(&self, key: &str, value: &str) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.values.insert(key.to_string(), value.to_string());
            })
            .ok();
    }
    pub fn get(&self, key: &str) -> Option<String> {
        self.inner
            .try_read()
            .ok()
            .and_then(|inner| inner.values.get(key).cloned())
    }

    pub fn remove(&self, key: &str) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.values.remove(key);
            })
            .ok();
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

    pub fn set_source_trunk(&self, trunk: &str) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.source_trunk = Some(trunk.to_string());
            })
            .ok();
    }

    pub fn is_from_trunk(&self) -> bool {
        self.inner
            .try_read()
            .map(|inner| inner.source_trunk.is_some())
            .ok()
            .unwrap_or_default()
    }

    pub fn get_source_trunk(&self) -> Option<String> {
        self.inner
            .try_read()
            .map(|inner| inner.source_trunk.clone())
            .ok()
            .flatten()
    }
}
