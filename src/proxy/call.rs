use crate::config::ProxyConfig;

use super::{server::SipServerRef, ProxyAction, ProxyModule};
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transaction::transaction::Transaction;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
}
#[derive(Clone)]
pub struct CallModule {
    inner: Arc<CallModuleInner>,
}

impl CallModule {
    pub fn create(_server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CallModule::new(config);
        Ok(Box::new(module))
    }
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        let inner = Arc::new(CallModuleInner { config });
        Self { inner }
    }
}
#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Ack,
            rsip::Method::Bye,
            rsip::Method::Cancel,
            rsip::Method::Options,
            rsip::Method::Info,
            rsip::Method::Refer,
        ]
    }
    async fn on_start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }
    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        _tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        Ok(ProxyAction::Continue)
    }
    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}
