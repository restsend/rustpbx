/// Integration tests for the SipFlow local backend, including the `flush()` command
/// added to support `SipFlowUploadHook`.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::config::{SipFlowConfig, SipFlowSubdirs};
    use crate::sipflow::backend::create_backend;
    use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};

    fn make_rtp_item(leg: &str) -> SipFlowItem {
        SipFlowItem {
            timestamp: chrono::Utc::now().timestamp_micros() as u64,
            seq: 0,
            msg_type: SipFlowMsgType::Rtp,
            src_addr: format!("{}_127.0.0.1:5000", leg),
            dst_addr: "127.0.0.1:5001".to_string(),
            payload: bytes::Bytes::from(vec![0u8; 20]),
        }
    }

    /// `flush()` on an empty LocalBackend should succeed without error.
    #[tokio::test]
    async fn test_local_backend_flush_no_error() {
        let dir = TempDir::new().unwrap();
        let cfg = SipFlowConfig::Local {
            root: dir.path().to_string_lossy().to_string(),
            subdirs: SipFlowSubdirs::None,
            flush_count: 10000,
            flush_interval_secs: 3600,
            id_cache_size: 64,
            upload: None,
        };
        let backend = create_backend(&cfg).expect("backend creation should succeed");
        backend.flush().await.expect("flush should not error");
    }

    /// After `record()` + `flush()` a SQLite `.db` file must exist on disk,
    /// proving the batch was committed before `flush()` returned.
    #[tokio::test]
    async fn test_local_backend_flush_writes_to_disk() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let cfg = SipFlowConfig::Local {
            root: root.clone(),
            subdirs: SipFlowSubdirs::None,
            flush_count: 10000,
            flush_interval_secs: 3600,
            id_cache_size: 64,
            upload: None,
        };
        let backend: Arc<dyn SipFlowBackend> = Arc::from(
            create_backend(&cfg).expect("backend creation should succeed"),
        );

        for _ in 0..5 {
            backend
                .record("test-call-1", make_rtp_item("LegA"))
                .expect("record should succeed");
        }

        backend.flush().await.expect("flush should not error");

        let db_files: Vec<PathBuf> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "db").unwrap_or(false))
            .collect();

        assert!(
            !db_files.is_empty(),
            "expected at least one .db file after flush; got none in {root}"
        );
    }
}
