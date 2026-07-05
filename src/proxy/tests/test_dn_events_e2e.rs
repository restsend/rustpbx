use super::test_ua::TestUaEvent;
use crate::config::MediaProxyMode;
use crate::rwi::gateway::{EventCacheEntry, RwiGateway};
use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::sleep;
use tracing::info;

struct DnEventCapture {
    events: Vec<String>,
    _rx: broadcast::Receiver<EventCacheEntry>,
}

impl DnEventCapture {
    fn collect(&mut self) {
        while let Ok(entry) = self._rx.try_recv() {
            let flat = &entry.event;
            if flat.event_type.contains("dn") || flat.event_type.contains("state") {
                self.events.push(flat.event_type.to_string());
            }
        }
    }

    fn has_event(&self, name: &str) -> bool {
        self.events.iter().any(|n| n == name)
    }
}

fn setup_gateway_with_capture() -> (
    Arc<RwLock<RwiGateway>>,
    broadcast::Receiver<EventCacheEntry>,
) {
    let (tx, rx) = broadcast::channel::<EventCacheEntry>(1000);
    let mut gw = RwiGateway::new();
    gw.set_webhook_tx(tx);
    (Arc::new(RwLock::new(gw)), rx)
}

fn pcmu_sdp(port: u16) -> String {
    format!(
        "v=0\r\n\
         o=- 12345 12345 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=sendrecv\r\n"
    )
}


