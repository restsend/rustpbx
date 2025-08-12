use crate::call::SipUser;
use rsipstack::transaction::key::TransactionKey;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

#[derive(Clone, Default)]
pub struct TransactionCookie {
    user: Arc<RwLock<Option<SipUser>>>,
    values: Arc<RwLock<HashMap<String, String>>>,
}

impl From<&TransactionKey> for TransactionCookie {
    fn from(_key: &TransactionKey) -> Self {
        Self {
            user: Arc::new(RwLock::new(None)),
            values: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl TransactionCookie {
    pub fn set_user(&self, user: SipUser) {
        self.user
            .try_write()
            .map(|mut u| {
                *u = Some(user);
            })
            .ok();
    }
    pub fn get_user(&self) -> Option<SipUser> {
        self.user.try_read().map(|user| user.clone()).ok().flatten()
    }
    pub fn set(&self, key: &str, value: &str) {
        self.values
            .try_write()
            .map(|mut values| {
                values.insert(key.to_string(), value.to_string());
            })
            .ok();
    }
    pub fn get(&self, key: &str) -> Option<String> {
        self.values
            .try_read()
            .map(|values| values.get(key).cloned())
            .ok()
            .flatten()
    }
    pub fn remove(&self, key: &str) {
        self.values
            .try_write()
            .map(|mut values| {
                values.remove(key);
            })
            .ok();
    }
}
