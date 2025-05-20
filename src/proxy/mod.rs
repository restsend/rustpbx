use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transaction::transaction::Transaction;
use server::SipServerRef;
use tokio_util::sync::CancellationToken;

use crate::config::ProxyConfig;
pub mod auth;
pub mod ban;
pub mod call;
pub mod cdr;
pub mod locator;
pub mod mediaproxy;
pub mod presence;
pub mod registrar;
pub mod server;
#[cfg(test)]
pub mod tests;
pub mod user;
pub enum ProxyAction {
    Continue,
    Abort,
}

#[async_trait]
pub trait ProxyModule: Send + Sync {
    fn name(&self) -> &str;
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![]
    }
    async fn on_start(&mut self) -> Result<()>;
    async fn on_stop(&self) -> Result<()>;
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

pub type FnCreateProxyModule =
    fn(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>>;
