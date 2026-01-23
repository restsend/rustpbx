use crate::call::{RouteInvite, RoutingState, TransactionCookie};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transaction::transaction::Transaction;
use server::SipServerRef;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub mod acl;
pub mod active_call_registry;
pub mod auth;
pub mod call;
pub mod data;
pub mod locator;
pub mod locator_db;
pub mod locator_webhook;
pub mod nat;
pub mod presence;
pub mod proxy_call;
pub mod registrar;
pub mod routing;
pub mod server;
pub mod tests;
pub mod user;
pub mod user_db;
pub mod user_extension;
pub mod user_http;
pub mod user_plain;
pub mod ws;

#[derive(Debug)]
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
        _cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        Ok(ProxyAction::Continue)
    }
    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}

pub type FnCreateProxyModule =
    fn(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>>;

pub type FnCreateRouteInvite = Arc<
    dyn Fn(SipServerRef, Arc<ProxyConfig>, Arc<RoutingState>) -> Result<Box<dyn RouteInvite>>
        + Send
        + Sync,
>;
