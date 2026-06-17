use std::sync::Arc;

use rsipstack::transport::SipAddr;
use tokio::sync::Mutex;

pub struct PreAuthRegistry {
    map: Mutex<Vec<(SipAddr, String)>>,
}

impl PreAuthRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::new(Vec::new()),
        })
    }

    pub async fn register(&self, addr: SipAddr, agent_id: String) {
        let mut map = self.map.lock().await;
        map.retain(|(a, _)| !addrs_equal(a, &addr));
        map.push((addr, agent_id));
    }

    pub async fn lookup(&self, addr: &SipAddr) -> Option<String> {
        let map = self.map.lock().await;
        for (a, agent) in map.iter() {
            if addrs_equal(a, addr) {
                return Some(agent.clone());
            }
        }
        None
    }

    pub async fn remove(&self, addr: &SipAddr) {
        let mut map = self.map.lock().await;
        map.retain(|(a, _)| !addrs_equal(a, addr));
    }
}

fn addrs_equal(a: &SipAddr, b: &SipAddr) -> bool {
    a.r#type == b.r#type && a.addr.host == b.addr.host && a.addr.port == b.addr.port
}
