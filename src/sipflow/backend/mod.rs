pub mod local;
pub mod remote;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};

use crate::config::{SipFlowClusterNode, SipFlowConfig, SipFlowEngine};
use crate::sipflow::flowdb_backend::FlowDbBackend;
use crate::sipflow::{SipFlowItem, SipFlowMediaStats};

#[async_trait]
pub trait SipFlowBackend: Send + Sync {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()>;
    /// Flush any in-memory batch to durable storage.
    /// This is a best-effort operation; implementations that have no in-memory
    /// buffer (e.g. the Remote backend) may ignore it.
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
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
    ) -> Result<Vec<SipFlowMediaStats>>;
    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>>;

    /// Query media with an optional leg filter.
    ///
    /// `stream_leg` maps logical stream selectors to legacy stored leg ids:
    /// - `Some(0)`: caller/A-leg only
    /// - `Some(1)`: callee/B-leg only
    /// - `None`: mixed/all available legs
    ///
    /// Default implementation keeps backward compatibility by delegating to
    /// `query_media` (mixed behavior) when backend-specific filtering is not implemented.
    async fn query_media_stream(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<Vec<u8>> {
        let _ = stream_leg;
        self.query_media(call_id, start_time, end_time).await
    }

    /// Generate WAV audio and write it to a temporary file on disk.
    ///
    /// This is the preferred way to export large recordings because it avoids
    /// holding the full WAV buffer in memory.  The returned `NamedTempFile`
    /// keeps the underlying file alive and automatically deletes it on drop.
    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        let data = self
            .query_media_stream(call_id, start_time, end_time, stream_leg)
            .await?;
        let mut file = tempfile::NamedTempFile::new()?;
        std::io::Write::write_all(&mut file, &data)?;
        std::io::Write::flush(&mut file)?;
        Ok(file)
    }
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
            engine,
            ttl_secs,
            memtable_size_mb,
            block_cache_capacity_mb,
            ..
        } => {
            if *engine == SipFlowEngine::FlowDb {
                FlowDbBackend::new(
                    root.clone(),
                    *ttl_secs,
                    *memtable_size_mb,
                    *block_cache_capacity_mb,
                    *flush_count,
                    *flush_interval_secs,
                )
                .map(|b| Box::new(b) as Box<dyn SipFlowBackend>)
            } else {
                local::LocalBackend::new(
                    root.clone(),
                    subdirs.clone(),
                    *flush_count,
                    *flush_interval_secs,
                    *id_cache_size,
                )
                .map(|b| Box::new(b) as Box<dyn SipFlowBackend>)
            }
        }
        SipFlowConfig::Remote {
            nodes,
            udp_addr,
            http_addr,
            timeout_secs,
            ..
        } => {
            let resolved = if !nodes.is_empty() {
                nodes.clone()
            } else if let (Some(udp), Some(http)) = (udp_addr, http_addr) {
                vec![SipFlowClusterNode {
                    udp: udp.clone(),
                    http: http.clone(),
                }]
            } else {
                anyhow::bail!(
                    "Remote backend requires either `nodes` or both `udp_addr` and `http_addr`"
                )
            };
            remote::RemoteBackend::new(resolved, *timeout_secs)
                .map(|b| Box::new(b) as Box<dyn SipFlowBackend>)
        }
    }
}
