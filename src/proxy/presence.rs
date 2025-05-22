use super::{server::SipServerRef, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone)]
pub struct PresenceModule {}

impl PresenceModule {
    pub fn create(_server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = PresenceModule::new(config);
        Ok(Box::new(module))
    }
    pub fn new(_config: Arc<ProxyConfig>) -> Self {
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
    async fn on_start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }
}
