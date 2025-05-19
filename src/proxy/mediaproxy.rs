use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use rsipstack::transaction::transaction::Transaction;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct MediaSession {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: Option<String>,
    pub start_time: SystemTime,
    pub sdp_offer: Option<String>,
    pub sdp_answer: Option<String>,
    pub recording_enabled: bool,
    pub recording_path: Option<PathBuf>,
}

impl MediaSession {
    pub fn new(call_id: &str, from_tag: &str) -> Self {
        Self {
            call_id: call_id.to_string(),
            from_tag: from_tag.to_string(),
            to_tag: None,
            start_time: SystemTime::now(),
            sdp_offer: None,
            sdp_answer: None,
            recording_enabled: false,
            recording_path: None,
        }
    }
}

#[derive(Clone)]
pub struct MediaProxyModule {
    config: Arc<ProxyConfig>,
    sessions: Arc<Mutex<HashMap<String, MediaSession>>>,
}

impl MediaProxyModule {
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl ProxyModule for MediaProxyModule {
    fn name(&self) -> &str {
        "mediaproxy"
    }

    async fn on_start(&mut self, _inner: SipServerRef) -> Result<()> {
        info!("MediaProxyModule started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("MediaProxyModule stopped");
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
