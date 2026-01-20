pub mod local;
pub mod remote;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};

use crate::config::SipFlowConfig;
use crate::sipflow::SipFlowItem;

#[async_trait]
pub trait SipFlowBackend: Send + Sync {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()>;
    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>>;
    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<(i32, String, usize)>>;
    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>>;
}

/// Create backend from configuration
pub fn create_backend(config: &SipFlowConfig) -> Result<Box<dyn SipFlowBackend>> {
    match config {
        SipFlowConfig::Local {
            root,
            subdirs,
            flush_count,
            flush_interval_secs,
            id_cache_size,
        } => local::LocalBackend::new(
            root.clone(),
            subdirs.clone(),
            *flush_count,
            *flush_interval_secs,
            *id_cache_size,
        )
        .map(|b| Box::new(b) as Box<dyn SipFlowBackend>),
        SipFlowConfig::Remote {
            udp_addr,
            http_addr,
            timeout_secs,
        } => remote::RemoteBackend::new(udp_addr.clone(), http_addr.clone(), *timeout_secs)
            .map(|b| Box::new(b) as Box<dyn SipFlowBackend>),
    }
}
