use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};

use crate::config::{SipFlowEngine, SipFlowSubdirs};
use crate::sipflow::backend::SipFlowBackend;
use crate::sipflow::backend::local::LocalBackend;
use crate::sipflow::flowdb_backend::FlowDbBackend;
use crate::sipflow::{SipFlowItem, SipFlowMediaStats};

/// Local backend that writes with the configured engine but can query data
/// stored by either engine.
///
/// The on-disk subdirectory layout is shared between the SQLite and FlowDB
/// engines, and each bucket directory acts as an engine hint: buckets with
/// `sipflow.db`/`data.raw` are queried through SQLite, buckets with
/// `WAL`/`SST` through FlowDB. This keeps historical data queryable after an
/// `engine` config change and allows mixed layouts under one root.
pub struct HybridLocalBackend {
    write_engine: SipFlowEngine,
    sqlite: LocalBackend,
    flowdb: FlowDbBackend,
}

impl HybridLocalBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: String,
        subdirs: SipFlowSubdirs,
        write_engine: SipFlowEngine,
        flush_count: usize,
        flush_interval_secs: u64,
        id_cache_size: usize,
        compress: Option<u32>,
        ttl_secs: Option<u64>,
        memtable_size_mb: usize,
        block_cache_capacity_mb: usize,
    ) -> Result<Self> {
        let sqlite = LocalBackend::new(
            root.clone(),
            subdirs.clone(),
            flush_count,
            flush_interval_secs,
            id_cache_size,
            compress,
        )?;
        let flowdb = FlowDbBackend::new(
            root,
            subdirs,
            ttl_secs,
            memtable_size_mb,
            block_cache_capacity_mb,
            flush_count,
            flush_interval_secs,
        )?;
        Ok(Self {
            write_engine,
            sqlite,
            flowdb,
        })
    }

    fn primary(&self) -> &dyn SipFlowBackend {
        match self.write_engine {
            SipFlowEngine::FlowDb => &self.flowdb,
            SipFlowEngine::Sqlite => &self.sqlite,
        }
    }

    fn secondary(&self) -> &dyn SipFlowBackend {
        match self.write_engine {
            SipFlowEngine::FlowDb => &self.sqlite,
            SipFlowEngine::Sqlite => &self.flowdb,
        }
    }
}

#[async_trait]
impl SipFlowBackend for HybridLocalBackend {
    fn record(&self, call_id: &str, item: SipFlowItem) -> Result<()> {
        self.primary().record(call_id, item)
    }

    async fn flush(&self) -> Result<()> {
        self.primary().flush().await
    }

    async fn query_flow(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowItem>> {
        let primary = self
            .primary()
            .query_flow(call_id, start_time, end_time)
            .await;
        let secondary = self
            .secondary()
            .query_flow(call_id, start_time, end_time)
            .await;

        match (primary, secondary) {
            (Ok(mut items), Ok(more)) => {
                items.extend(more);
                items.sort_by_key(|i| i.timestamp);
                Ok(items)
            }
            (Ok(items), Err(e)) => {
                tracing::debug!("hybrid sipflow: secondary query_flow failed: {e}");
                Ok(items)
            }
            (Err(e), Ok(items)) => {
                tracing::warn!("hybrid sipflow: primary query_flow failed: {e}");
                Ok(items)
            }
            (Err(e), Err(_)) => Err(e),
        }
    }

    async fn query_media_stats(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<SipFlowMediaStats>> {
        let primary = self
            .primary()
            .query_media_stats(call_id, start_time, end_time)
            .await;
        let secondary = self
            .secondary()
            .query_media_stats(call_id, start_time, end_time)
            .await;

        match (primary, secondary) {
            (Ok(mut stats), Ok(more)) => {
                stats.extend(more);
                Ok(stats)
            }
            (Ok(stats), Err(e)) => {
                tracing::debug!("hybrid sipflow: secondary query_media_stats failed: {e}");
                Ok(stats)
            }
            (Err(e), Ok(stats)) => {
                tracing::warn!("hybrid sipflow: primary query_media_stats failed: {e}");
                Ok(stats)
            }
            (Err(e), Err(_)) => Err(e),
        }
    }

    async fn query_media(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
    ) -> Result<Vec<u8>> {
        match self
            .primary()
            .query_media(call_id, start_time, end_time)
            .await
        {
            Ok(data) if !data.is_empty() => return Ok(data),
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("hybrid sipflow: primary query_media failed: {e}");
            }
        }
        self.secondary()
            .query_media(call_id, start_time, end_time)
            .await
    }

    async fn query_media_stream(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<Vec<u8>> {
        match self
            .primary()
            .query_media_stream(call_id, start_time, end_time, stream_leg)
            .await
        {
            Ok(data) if !data.is_empty() => return Ok(data),
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("hybrid sipflow: primary query_media_stream failed: {e}");
            }
        }
        self.secondary()
            .query_media_stream(call_id, start_time, end_time, stream_leg)
            .await
    }

    async fn generate_wav_file(
        &self,
        call_id: &str,
        start_time: DateTime<Local>,
        end_time: DateTime<Local>,
        stream_leg: Option<i32>,
    ) -> Result<tempfile::NamedTempFile> {
        match self
            .primary()
            .generate_wav_file(call_id, start_time, end_time, stream_leg)
            .await
        {
            Ok(file) => Ok(file),
            Err(e) => {
                tracing::debug!("hybrid sipflow: primary generate_wav_file failed: {e}");
                self.secondary()
                    .generate_wav_file(call_id, start_time, end_time, stream_leg)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sipflow::SipFlowMsgType;
    use bytes::Bytes;
    use chrono::TimeZone;

    fn local_dt_from_micros(ts: i64) -> DateTime<Local> {
        Local
            .timestamp_micros(ts)
            .single()
            .expect("valid local datetime")
    }

    fn make_sip_item(ts_micros: u64, call_id: &str) -> SipFlowItem {
        let payload = format!(
            "INVITE sip:test@example.com SIP/2.0\r\nCall-ID: {}\r\n",
            call_id
        );
        SipFlowItem {
            timestamp: ts_micros,
            seq: 0,
            leg: None,
            msg_type: SipFlowMsgType::Sip,
            src_addr: "127.0.0.1:5060".to_string(),
            dst_addr: "127.0.0.2:5060".to_string(),
            payload: Bytes::from(payload),
        }
    }

    fn new_hybrid(root: &str, engine: SipFlowEngine) -> HybridLocalBackend {
        HybridLocalBackend::new(
            root.to_string(),
            SipFlowSubdirs::Daily,
            engine,
            1,
            1,
            128,
            Some(crate::sipflow::storage::DEFAULT_COMPRESS_LEVEL),
            None,
            1,
            16,
        )
        .expect("hybrid backend")
    }

    /// Data written by the SQLite engine must remain queryable after the
    /// configured engine is switched to FlowDB (engine hint per bucket).
    #[tokio::test]
    async fn test_hybrid_queries_sqlite_data_with_flowdb_engine() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let call_id = "hybrid-sqlite-data";
        let ts = chrono::Utc::now().timestamp_micros() as u64;

        {
            let backend = new_hybrid(&root, SipFlowEngine::Sqlite);
            backend.record(call_id, make_sip_item(ts, call_id)).unwrap();
            backend.flush().await.unwrap();
        }

        let backend = new_hybrid(&root, SipFlowEngine::FlowDb);
        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(ts as i64 - 1),
                local_dt_from_micros(ts as i64 + 1),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert!(items[0].payload.starts_with(b"INVITE"));
    }

    /// Data written by the FlowDB engine must remain queryable after the
    /// configured engine is switched to SQLite.
    #[tokio::test]
    async fn test_hybrid_queries_flowdb_data_with_sqlite_engine() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let call_id = "hybrid-flowdb-data";
        let ts = chrono::Utc::now().timestamp_micros() as u64;

        {
            let backend = new_hybrid(&root, SipFlowEngine::FlowDb);
            backend.record(call_id, make_sip_item(ts, call_id)).unwrap();
            backend.flush().await.unwrap();
        }

        let backend = new_hybrid(&root, SipFlowEngine::Sqlite);
        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(ts as i64 - 1),
                local_dt_from_micros(ts as i64 + 1),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 1);
        assert!(items[0].payload.starts_with(b"INVITE"));
    }

    /// Data straddling an engine migration must be merged from both engines.
    #[tokio::test]
    async fn test_hybrid_merges_flow_from_both_engines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let call_id = "hybrid-merged";
        let base = chrono::Utc::now().timestamp_micros();
        let t0 = (base + 1_000) as u64;
        let t1 = (base + 2_000) as u64;

        {
            let backend = new_hybrid(&root, SipFlowEngine::Sqlite);
            backend.record(call_id, make_sip_item(t0, call_id)).unwrap();
            backend.flush().await.unwrap();
        }
        {
            let backend = new_hybrid(&root, SipFlowEngine::FlowDb);
            backend.record(call_id, make_sip_item(t1, call_id)).unwrap();
            backend.flush().await.unwrap();
        }

        let backend = new_hybrid(&root, SipFlowEngine::FlowDb);
        let items = backend
            .query_flow(
                call_id,
                local_dt_from_micros(base),
                local_dt_from_micros(base + 10_000),
            )
            .await
            .unwrap();

        assert_eq!(items.len(), 2, "items from both engines must be merged");
        assert_eq!(items[0].timestamp, t0);
        assert_eq!(items[1].timestamp, t1);
    }
}
