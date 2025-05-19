use super::{server::SipServerRef, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone)]
pub struct PresenceModule {}

impl PresenceModule {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {}
    }
}
#[async_trait]
impl ProxyModule for PresenceModule {
    fn name(&self) -> &str {
        "presence"
    }
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Subscribe,
            rsip::Method::Publish,
            rsip::Method::Notify,
        ]
    }
    async fn on_start(&mut self, _inner: SipServerRef) -> Result<()> {
        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }
}
