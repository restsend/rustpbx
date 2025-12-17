use chrono::Utc;
use lru::LruCache;
use rsip::{
    SipMessage,
    prelude::{HeadersExt, UntypedHeader},
};
use rsipstack::transaction::endpoint::MessageInspector;
use serde::{Deserialize, Serialize};
use std::{
    num::NonZero,
    sync::{Arc, Mutex},
};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SipFlowDirection {
    Incoming,
    Outgoing,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SipMessageItem {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub direction: SipFlowDirection,
    pub content: String,
}

pub(self) struct SipFlowInner {
    inspectors: Vec<Box<dyn MessageInspector>>,
    messages: Mutex<LruCache<String, Vec<SipMessageItem>>>,
}
#[derive(Clone)]
pub struct SipFlow {
    inner: Arc<SipFlowInner>,
}

impl SipFlow {
    pub fn count(&self) -> usize {
        match self.inner.messages.lock() {
            Ok(messages) => messages.len(),
            Err(_) => 0,
        }
    }

    pub fn take(&self, call_id: &str) -> Option<Vec<SipMessageItem>> {
        match self.inner.messages.lock() {
            Ok(mut messages) => messages.pop(call_id),
            Err(_) => None,
        }
    }

    pub fn get(&self, call_id: &str) -> Option<Vec<SipMessageItem>> {
        match self.inner.messages.lock() {
            Ok(mut messages) => messages.get(call_id).cloned(),
            Err(_) => None,
        }
    }

    fn record(&self, direction: SipFlowDirection, msg: &SipMessage) {
        if let Ok(mut messages) = self.inner.messages.lock() {
            let call_id = match msg {
                rsip::SipMessage::Request(req) => req.call_id_header(),
                rsip::SipMessage::Response(resp) => resp.call_id_header(),
            };
            if let Ok(id) = call_id {
                if let Some(items_mut) = messages.get_mut(&id.value().to_string()) {
                    let item = SipMessageItem {
                        direction,
                        timestamp: Utc::now(),
                        content: msg.to_string(),
                    };
                    items_mut.push(item);
                } else {
                    // Insert new entry
                    let method = match msg {
                        rsip::SipMessage::Request(req) => Some(req.method.clone()),
                        rsip::SipMessage::Response(resp) => {
                            resp.cseq_header().ok().map(|c| c.method().ok()).flatten()
                        }
                    };
                    if matches!(method, Some(rsip::Method::Invite)) {
                        let item = SipMessageItem {
                            direction,
                            timestamp: Utc::now(),
                            content: msg.to_string(),
                        };
                        messages.put(id.value().to_string(), vec![item]);
                    }
                }
            }
        }
    }
}

impl MessageInspector for SipFlow {
    fn before_send(&self, msg: SipMessage) -> SipMessage {
        self.record(SipFlowDirection::Outgoing, &msg);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.before_send(modified_msg);
        }
        modified_msg
    }

    fn after_received(&self, msg: SipMessage) -> SipMessage {
        self.record(SipFlowDirection::Incoming, &msg);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.after_received(modified_msg);
        }
        modified_msg
    }
}

pub struct SipFlowBuilder {
    max_items: Option<usize>,
    inspectors: Vec<Box<dyn MessageInspector>>,
}

impl SipFlowBuilder {
    pub fn new() -> Self {
        Self {
            max_items: None,
            inspectors: Vec::new(),
        }
    }

    pub fn with_max_items(mut self, max_items: Option<usize>) -> Self {
        self.max_items = max_items;
        self
    }

    pub fn register_inspector(mut self, inspector: Box<dyn MessageInspector>) -> Self {
        self.inspectors.push(inspector);
        self
    }

    pub fn build(self) -> SipFlow {
        let messages = LruCache::new(NonZero::new(self.max_items.unwrap_or(100 * 1024)).unwrap());
        let inspectors = self.inspectors;
        let inner = SipFlowInner {
            messages: Mutex::new(messages),
            inspectors,
        };
        SipFlow {
            inner: Arc::new(inner),
        }
    }
}
