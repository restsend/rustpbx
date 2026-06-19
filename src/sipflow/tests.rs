/// Integration tests for the SipFlow backends.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::config::{SipFlowClusterNode, SipFlowConfig, SipFlowEngine, SipFlowSubdirs};
    use crate::sipflow::backend::create_backend;
    use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
    use chrono::{Local, TimeZone};

    fn make_rtp_item(leg: i32) -> SipFlowItem {
        SipFlowItem {
            timestamp: chrono::Utc::now().timestamp_micros() as u64,
            seq: 0,
            leg: Some(leg),
            msg_type: SipFlowMsgType::Rtp,
            src_addr: "127.0.0.1:5000".to_string(),
            dst_addr: "127.0.0.1:5001".to_string(),
            payload: bytes::Bytes::from(vec![0u8; 20]),
        }
    }

    /// `flush()` on an empty Local backend should succeed without error.
    #[tokio::test]
    async fn test_local_backend_flush_no_error() {
        let dir = TempDir::new().unwrap();
        let cfg = SipFlowConfig::Local {
            root: dir.path().to_string_lossy().to_string(),
            subdirs: SipFlowSubdirs::None,
            flush_count: 10000,
            flush_interval_secs: 3600,
            id_cache_size: 64,
            engine: SipFlowEngine::Sqlite,
            ttl_secs: None,
            memtable_size_mb: 64,
            block_cache_capacity_mb: 128,
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
            engine: SipFlowEngine::Sqlite,
            ttl_secs: None,
            memtable_size_mb: 64,
            block_cache_capacity_mb: 128,
            upload: None,
        };
        let backend: Arc<dyn SipFlowBackend> =
            Arc::from(create_backend(&cfg).expect("backend creation should succeed"));

        for _ in 0..5 {
            backend
                .record("test-call-1", make_rtp_item(0))
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

    /// RemoteBackend with legacy single-node format (udp_addr + http_addr)
    #[tokio::test]
    async fn test_remote_backend_legacy_format() {
        let cfg = SipFlowConfig::Remote {
            nodes: vec![],
            udp_addr: Some("127.0.0.1:3000".to_string()),
            http_addr: Some("http://127.0.0.1:3001".to_string()),
            timeout_secs: 10,
            upload: None,
        };
        let backend = create_backend(&cfg);
        assert!(backend.is_ok(), "legacy format should create backend");
    }

    /// RemoteBackend should accept hostnames for Kubernetes service DNS names.
    #[tokio::test]
    async fn test_remote_backend_domain_udp_addr() {
        let cfg = SipFlowConfig::Remote {
            nodes: vec![],
            udp_addr: Some("localhost:3000".to_string()),
            http_addr: Some("http://localhost:3001".to_string()),
            timeout_secs: 10,
            upload: None,
        };
        let backend = create_backend(&cfg);
        assert!(backend.is_ok(), "domain UDP address should create backend");
    }

    /// RemoteBackend with new multi-node format
    #[tokio::test]
    async fn test_remote_backend_multi_node_format() {
        let cfg = SipFlowConfig::Remote {
            nodes: vec![
                SipFlowClusterNode {
                    udp: "192.168.1.1:3000".to_string(),
                    http: "http://192.168.1.1:3001".to_string(),
                },
                SipFlowClusterNode {
                    udp: "192.168.1.2:3000".to_string(),
                    http: "http://192.168.1.2:3001".to_string(),
                },
            ],
            udp_addr: None,
            http_addr: None,
            timeout_secs: 10,
            upload: None,
        };
        let backend = create_backend(&cfg);
        assert!(backend.is_ok(), "multi-node format should create backend");
    }

    /// RemoteBackend with neither nodes nor legacy fields must fail
    #[test]
    fn test_remote_backend_missing_config() {
        let cfg = SipFlowConfig::Remote {
            nodes: vec![],
            udp_addr: None,
            http_addr: None,
            timeout_secs: 10,
            upload: None,
        };
        let result = create_backend(&cfg);
        assert!(
            result.is_err(),
            "expected error when neither nodes nor udp_addr/http_addr provided"
        );
    }

    /// Local backend with FlowDB engine (default) should create successfully.
    #[tokio::test]
    async fn test_flowdb_backend_default_engine() {
        let dir = TempDir::new().unwrap();
        let cfg = SipFlowConfig::Local {
            root: dir.path().to_string_lossy().to_string(),
            subdirs: SipFlowSubdirs::None,
            flush_count: 1000,
            flush_interval_secs: 5,
            id_cache_size: 1024,
            engine: SipFlowEngine::FlowDb,
            ttl_secs: None,
            memtable_size_mb: 1,
            block_cache_capacity_mb: 16,
            upload: None,
        };
        let backend = create_backend(&cfg).expect("flowdb backend creation should succeed");
        backend.flush().await.expect("flush should not error");
    }

    /// FlowDB backend via create_backend should support record + query.
    #[tokio::test]
    async fn test_flowdb_backend_factory_record_and_query() {
        use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
        use bytes::Bytes;

        let dir = TempDir::new().unwrap();
        let cfg = SipFlowConfig::Local {
            root: dir.path().to_string_lossy().to_string(),
            subdirs: SipFlowSubdirs::None,
            flush_count: 1000,
            flush_interval_secs: 5,
            id_cache_size: 1024,
            engine: SipFlowEngine::FlowDb,
            ttl_secs: None,
            memtable_size_mb: 1,
            block_cache_capacity_mb: 16,
            upload: None,
        };
        let backend: Arc<dyn SipFlowBackend> =
            Arc::from(create_backend(&cfg).expect("backend creation should succeed"));

        let base = chrono::Utc::now().timestamp_micros();
        let item = SipFlowItem {
            timestamp: base as u64,
            seq: 0,
            leg: None,
            msg_type: SipFlowMsgType::Sip,
            src_addr: "127.0.0.1:5060".to_string(),
            dst_addr: "127.0.0.2:5060".to_string(),
            payload: Bytes::from_static(b"INVITE sip:test SIP/2.0\r\nCall-ID: factory-test\r\n"),
        };

        backend
            .record("factory-test", item)
            .expect("record should succeed");
        backend.flush().await.expect("flush should succeed");

        let items = backend
            .query_flow(
                "factory-test",
                Local.timestamp_micros(base - 1).single().expect("valid dt"),
                Local.timestamp_micros(base + 1).single().expect("valid dt"),
            )
            .await
            .expect("query should succeed");

        assert_eq!(items.len(), 1);
        assert!(items[0].payload.starts_with(b"INVITE"));
    }

    /// Config TOML with engine = "flowdb" should deserialize correctly.
    #[test]
    fn test_config_parse_flowdb_engine() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"
engine = "flowdb"
ttl_secs = 3600
memtable_size_mb = 32
block_cache_capacity_mb = 64
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse flowdb config");
        match cfg {
            crate::config::SipFlowConfig::Local {
                engine,
                ttl_secs,
                memtable_size_mb,
                block_cache_capacity_mb,
                ..
            } => {
                assert_eq!(engine, SipFlowEngine::FlowDb);
                assert_eq!(ttl_secs, Some(3600));
                assert_eq!(memtable_size_mb, 32);
                assert_eq!(block_cache_capacity_mb, 64);
            }
            _ => panic!("expected Local config"),
        }
    }

    /// Config TOML with engine = "sqlite" should deserialize correctly.
    #[test]
    fn test_config_parse_sqlite_engine() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"
engine = "sqlite"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse sqlite config");
        match cfg {
            crate::config::SipFlowConfig::Local { engine, .. } => {
                assert_eq!(engine, SipFlowEngine::Sqlite);
            }
            _ => panic!("expected Local config"),
        }
    }

    /// Default engine (omitted in config) should be Sqlite.
    #[test]
    fn test_config_default_engine_is_sqlite() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse default config");
        match cfg {
            crate::config::SipFlowConfig::Local { engine, .. } => {
                assert_eq!(engine, SipFlowEngine::Sqlite);
            }
            _ => panic!("expected Local config"),
        }
    }
}
