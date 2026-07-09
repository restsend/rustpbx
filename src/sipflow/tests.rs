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
            batch_size: 256,
            batch_flush_ms: 20,
            channel_capacity: 8192,
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
            batch_size: 256,
            batch_flush_ms: 20,
            channel_capacity: 8192,
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
            batch_size: 256,
            batch_flush_ms: 20,
            channel_capacity: 8192,
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
            batch_size: 256,
            batch_flush_ms: 20,
            channel_capacity: 8192,
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

    /// Remote batch defaults: TOML with no `batch_size` / `batch_flush_ms`
    /// / `channel_capacity` must yield the documented defaults
    /// (256 / 20 / 8192).
    #[test]
    fn test_remote_config_batch_defaults() {
        let toml_str = r#"
type = "remote"
nodes = [{ udp = "127.0.0.1:3000", http = "http://127.0.0.1:3001" }]
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse remote config");
        match cfg {
            crate::config::SipFlowConfig::Remote {
                batch_size,
                batch_flush_ms,
                channel_capacity,
                ..
            } => {
                assert_eq!(batch_size, 256);
                assert_eq!(batch_flush_ms, 20);
                assert_eq!(channel_capacity, 8192);
            }
            _ => panic!("expected Remote config"),
        }
    }

    /// End-to-end batch validation: many `record()` calls must arrive at a
    /// UDP receiver as **fewer** datagrams than records (i.e. the worker
    /// coalesced them), and every received datagram must parse as a batch
    /// containing the original packets in order.
    ///
    /// This guards the whole pipeline: `try_send` on the bounded channel →
    /// `recv_many` → per-node buffers → `encode_batch_into` → `send_to` →
    /// `parse_datagram`.
    #[tokio::test]
    async fn test_remote_backend_batches_on_the_wire() {
        use std::net::SocketAddr;
        use tokio::net::UdpSocket;

        // Bind the fake sipflow server first so the backend can resolve it.
        let rx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr: SocketAddr = rx_sock.local_addr().unwrap();
        let tx_port = rx_addr.port();

        let cfg = SipFlowConfig::Remote {
            nodes: vec![SipFlowClusterNode {
                udp: format!("127.0.0.1:{tx_port}"),
                http: format!("http://127.0.0.1:{tx_port}"),
            }],
            udp_addr: None,
            http_addr: None,
            timeout_secs: 5,
            // Small batch + short flush so the test resolves quickly while
            // still exercising the coalescing path.
            batch_size: 16,
            batch_flush_ms: 20,
            channel_capacity: 8192,
            upload: None,
        };
        let backend = create_backend(&cfg).expect("backend creation");

        // Send N records (N > batch_size) with the same call_id so they all
        // hash to the same single node.
        const N: u32 = 50;
        for i in 0..N {
            backend
                .record("call-batch-e2e", make_rtp_item(i as i32))
                .expect("record must succeed");
        }

        // Collect datagrams until we've gathered all N packets worth.
        let mut got_packets = 0u32;
        let mut datagram_count = 0u32;
        let mut buf = vec![0u8; 65535];
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(1500);
        while got_packets < N && tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, rx_sock.recv(&mut buf)).await {
                Ok(Ok(size)) => {
                    datagram_count += 1;
                    let packets = crate::sipflow::protocol::parse_datagram(&buf[..size])
                        .expect("received datagram must parse");
                    got_packets += packets.len() as u32;
                    for p in &packets {
                        assert_eq!(p.msg_type, crate::sipflow::protocol::MsgType::Rtp);
                        assert_eq!(p.call_id.as_deref(), Some("call-batch-e2e"));
                    }
                }
                _ => break,
            }
        }

        assert_eq!(
            got_packets, N,
            "all {N} records should reach the receiver"
        );
        // The whole point: 50 records must arrive in fewer than 50 datagrams.
        assert!(
            datagram_count < N,
            "expected batching to coalesce records into fewer datagrams, \
             got {datagram_count} datagrams for {N} records"
        );
        tracing::debug!(
            "batch e2e: {N} records -> {datagram_count} datagrams"
        );
    }

    /// `batch_size = 0` disables batching: each record must arrive as its
    /// own legacy single-packet datagram (magic `0x5346`, not `0x5347`),
    /// and the number of datagrams equals the number of records.
    #[tokio::test]
    async fn test_remote_backend_batch_size_zero_disables_batching() {
        use std::net::SocketAddr;
        use tokio::net::UdpSocket;

        let rx_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_addr: SocketAddr = rx_sock.local_addr().unwrap();
        let tx_port = rx_addr.port();

        let cfg = SipFlowConfig::Remote {
            nodes: vec![SipFlowClusterNode {
                udp: format!("127.0.0.1:{tx_port}"),
                http: format!("http://127.0.0.1:{tx_port}"),
            }],
            udp_addr: None,
            http_addr: None,
            timeout_secs: 5,
            // batch_size=0 → opt out of batching entirely.
            batch_size: 0,
            // Sub-MIN value; must be clamped to 20 internally. Doesn't
            // affect this test since flushes are immediate when disabled.
            batch_flush_ms: 1,
            channel_capacity: 8192,
            upload: None,
        };
        let backend = create_backend(&cfg).expect("backend creation");

        const N: u32 = 10;
        for i in 0..N {
            backend
                .record("call-disabled", make_rtp_item(i as i32))
                .expect("record must succeed");
        }

        let mut got_packets = 0u32;
        let mut datagram_count = 0u32;
        let mut buf = vec![0u8; 65535];
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(1500);
        while got_packets < N && tokio::time::Instant::now() < deadline {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, rx_sock.recv(&mut buf)).await {
                Ok(Ok(size)) => {
                    datagram_count += 1;
                    // Each datagram must be the LEGACY single-packet format
                    // (PACKET_MAGIC 0x5346), not the batch format (0x5347).
                    assert_eq!(
                        &buf[..2],
                        &[0x53, 0x46],
                        "batch_size=0 must produce legacy single-packet datagrams"
                    );
                    let packets = crate::sipflow::protocol::parse_datagram(&buf[..size])
                        .expect("received datagram must parse");
                    assert_eq!(
                        packets.len(),
                        1,
                        "disabled backend must put exactly one packet per datagram"
                    );
                    got_packets += 1;
                }
                _ => break,
            }
        }

        assert_eq!(got_packets, N, "all {N} records should reach the receiver");
        // With batching disabled: one datagram per record, no exceptions.
        assert_eq!(
            datagram_count, N,
            "disabled backend must send one datagram per record"
        );
    }
}
