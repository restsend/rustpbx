use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use bytes::Bytes;
use rsip::{
    SipMessage,
    prelude::{HeadersExt, UntypedHeader},
};
use rsipstack::{transaction::endpoint::MessageInspector, transport::SipAddr};
use std::sync::Arc;

struct SipFlowInner {
    backend: Option<Arc<dyn SipFlowBackend>>,
    inspectors: Vec<Box<dyn MessageInspector>>,
}
#[derive(Clone)]
pub struct SipFlow {
    inner: Arc<SipFlowInner>,
}

impl SipFlow {
    pub fn backend(&self) -> Option<Arc<dyn SipFlowBackend>> {
        self.inner.backend.clone()
    }

    pub fn record_sip(
        &self,
        is_outgoing: bool,
        msg: &SipMessage,
        addr: Option<&SipAddr>,
    ) {
        if let Some(backend) = &self.inner.backend {
            if let Ok(id) = match msg {
                rsip::SipMessage::Request(req) => req.call_id_header(),
                rsip::SipMessage::Response(resp) => resp.call_id_header(),
            } {
                let call_id = id.value().to_string();
                let msg_str = msg.to_string();
                let msg_bytes = Bytes::from(msg_str);

                // Extract addresses
                let (src_addr, dst_addr) = if let Some(addr) = addr {
                    let addr_str = addr.addr.to_string();
                    if is_outgoing {
                        // We're sending to addr
                        (String::new(), addr_str)
                    } else {
                        // We received from addr
                        (addr_str, String::new())
                    }
                } else {
                    (String::new(), String::new())
                };

                let item = SipFlowItem {
                    timestamp: chrono::Utc::now().timestamp_micros() as u64,
                    seq: 0,
                    msg_type: SipFlowMsgType::Sip,
                    src_addr,
                    dst_addr,
                    payload: msg_bytes,
                };
                backend.record(&call_id, item).ok();
            }
        }
    }
}

impl MessageInspector for SipFlow {
    fn before_send(&self, msg: SipMessage, dest: Option<&SipAddr>) -> SipMessage {
        self.record_sip(true, &msg, dest);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.before_send(modified_msg, dest);
        }
        modified_msg
    }

    fn after_received(&self, msg: SipMessage, from: &SipAddr) -> SipMessage {
        self.record_sip(false, &msg, Some(from));
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.after_received(modified_msg, from);
        }
        modified_msg
    }
}

pub struct SipFlowBuilder {
    inspectors: Vec<Box<dyn MessageInspector>>,
    backend: Option<Arc<dyn SipFlowBackend>>,
}

impl SipFlowBuilder {
    pub fn new() -> Self {
        Self {
            inspectors: Vec::new(),
            backend: None,
        }
    }

    pub fn with_backend(mut self, backend: Arc<dyn SipFlowBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn register_inspector(mut self, inspector: Box<dyn MessageInspector>) -> Self {
        self.inspectors.push(inspector);
        self
    }

    pub fn build(self) -> SipFlow {
        let backend = self.backend;
        SipFlow {
            inner: Arc::new(SipFlowInner {
                backend,
                inspectors: self.inspectors,
            }),
        }
    }
}
