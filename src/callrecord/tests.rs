use super::*;
use crate::callrecord::CallRecordHangupReason;
use crate::config::CallRecordStorageConfig;
use chrono::Utc;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_builder_without_callrecord_config_uses_noop_saver() {
    let manager = CallRecordManagerBuilder::new().build().await.unwrap();
    let record = CallRecord {
        call_id: "noop_without_config".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        ..Default::default()
    };

    let result = manager.saver.save(&record).await.unwrap();
    assert_eq!(result, "");
}

#[tokio::test]
async fn test_database_saver_requires_database_url() {
    let err = CallRecordManagerBuilder::new()
        .with_config(CallRecordConfig {
            max_concurrent: 64,
            storage: CallRecordStorageConfig::Database {
                database_url: None,
                table_name: "call_records".to_string(),
            },
        })
        .build()
        .await
        .err()
        .unwrap();

    assert_eq!(
        err.to_string(),
        "database call record saver requires database_url"
    );
}

#[tokio::test]
async fn test_save_with_http_without_media() {
    // Create a test CallRecord
    let record = CallRecord {
        call_id: "test_call_123".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_messages: Vec::new(),
        ..Default::default()
    };

    // Test without media (should not fail if no server available)
    let url = "http://httpbin.org/post".to_string();
    let headers = None;

    // This test will only pass if httpbin.org is available
    // In production, you might want to use a mock server
    let result = HttpCallRecordSaver {
        url,
        headers,
        client: reqwest::Client::new(),
    }
    .save(&record)
    .await;

    // We expect this to succeed for the JSON upload
    if result.is_ok() {
        println!("HTTP upload test passed: {}", result.unwrap());
    } else {
        println!(
            "HTTP upload test failed (expected if no internet): {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn test_save_with_http_with_media() {
    // Create a temporary media file
    let mut temp_file = NamedTempFile::new().unwrap();
    let test_content = b"fake audio content";
    temp_file.write_all(test_content).unwrap();
    temp_file.flush().unwrap();

    let media = CallRecordMedia {
        track_id: "track_001".to_string(),
        path: temp_file.path().to_string_lossy().to_string(),
        size: test_content.len() as u64,
        extra: None,
    };

    let record = CallRecord {
        call_id: "test_call_with_media_456".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![media],
        ..Default::default()
    };

    // Media stays local; `[recording]` controls recording upload.
    let url = "http://httpbin.org/post".to_string();
    let headers = None;

    let result = HttpCallRecordSaver {
        url,
        headers,
        client: reqwest::Client::new(),
    }
    .save(&record)
    .await;

    if result.is_ok() {
        println!("HTTP upload with media test passed: {}", result.unwrap());
    } else {
        println!(
            "HTTP upload with media test failed (expected if no internet): {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn test_save_with_http_with_custom_headers() {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer test_token".to_string());
    headers.insert("X-Custom-Header".to_string(), "test_value".to_string());

    let record = CallRecord {
        call_id: "test_call_headers_789".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        ..Default::default()
    };

    let url = "http://httpbin.org/post".to_string();

    let result = HttpCallRecordSaver {
        url,
        headers: Some(headers),
        client: reqwest::Client::new(),
    }
    .save(&record)
    .await;

    if result.is_ok() {
        println!("HTTP upload with headers test passed: {}", result.unwrap());
    } else {
        println!(
            "HTTP upload with headers test failed (expected if no internet): {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn test_save_with_s3_like_with_custom_headers() {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer test_token".to_string());
    headers.insert("X-Custom-Header".to_string(), "test_value".to_string());

    let record = CallRecord {
        call_id: "test_call_headers_789".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        ..Default::default()
    };

    let url = "http://httpbin.org/post".to_string();

    let result = HttpCallRecordSaver {
        url,
        headers: Some(headers),
        client: reqwest::Client::new(),
    }
    .save(&record)
    .await;

    if result.is_ok() {
        println!("HTTP upload with headers test passed: {}", result.unwrap());
    } else {
        println!(
            "HTTP upload with headers test failed (expected if no internet): {:?}",
            result.err()
        );
    }
}

#[tokio::test]
async fn test_save_with_s3_like_memory_store() {
    // Test using memory store for S3-like functionality without real cloud storage
    let vendor = crate::config::S3Vendor::Minio;
    let bucket = "test-bucket".to_string();
    let region = "us-east-1".to_string();
    let access_key = "minioadmin".to_string();
    let secret_key = "minioadmin".to_string();
    let endpoint = "http://localhost:9000".to_string(); // Local minio endpoint

    let record = CallRecord {
        call_id: "test_s3_call_123".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        ..Default::default()
    };

    // This test will only succeed if there's a local minio instance running
    // In real scenarios, this would use actual cloud storage credentials
    let result = match crate::storage::Storage::new(&crate::storage::StorageConfig::S3 {
        vendor,
        bucket: bucket.clone(),
        region,
        access_key,
        secret_key,
        endpoint: Some(endpoint.clone()),
        prefix: None,
    }) {
        Ok(storage) => {
            S3CallRecordSaver {
                root: "./config/cdr".to_string(),
                bucket,
                endpoint,
                storage,
            }
            .save(&record)
            .await
        }
        Err(e) => Err(e),
    };

    // We expect this might fail in test environment without real S3 storage
    match result {
        Ok(message) => println!("S3 upload test passed: {}", message),
        Err(e) => println!(
            "S3 upload test failed (expected without real S3 setup): {:?}",
            e
        ),
    }
}

#[tokio::test]
async fn test_save_with_s3_like_with_media() {
    // Create a temporary media file
    let mut temp_file = NamedTempFile::new().unwrap();
    let test_content = b"fake audio content for S3 test";
    temp_file.write_all(test_content).unwrap();
    temp_file.flush().unwrap();

    let media = CallRecordMedia {
        track_id: "s3_track_001".to_string(),
        path: temp_file.path().to_string_lossy().to_string(),
        size: test_content.len() as u64,
        extra: None,
    };

    let record = CallRecord {
        call_id: "test_s3_media_456".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![media],
        ..Default::default()
    };

    // Test with different S3 vendors
    let test_cases = vec![
        (crate::config::S3Vendor::AWS, "https://s3.amazonaws.com"),
        (crate::config::S3Vendor::Minio, "http://localhost:9000"),
        (
            crate::config::S3Vendor::Aliyun,
            "https://oss-cn-hangzhou.aliyuncs.com",
        ),
    ];

    for (vendor, endpoint) in test_cases {
        let record = record.clone();
        let bucket = "test-bucket".to_string();
        let region = "us-east-1".to_string();
        let access_key = "test_access_key".to_string();
        let secret_key = "test_secret_key".to_string();
        let endpoint = endpoint.to_string();

        let result = match crate::storage::Storage::new(&crate::storage::StorageConfig::S3 {
            vendor: vendor.clone(),
            bucket: bucket.clone(),
            region,
            access_key,
            secret_key,
            endpoint: Some(endpoint.clone()),
            prefix: None,
        }) {
            Ok(storage) => {
                S3CallRecordSaver {
                    root: "./config/cdr".to_string(),
                    bucket,
                    endpoint,
                    storage,
                }
                .save(&record)
                .await
            }
            Err(e) => Err(e),
        };

        match result {
            Ok(message) => println!("S3 {:?} upload with media test passed: {}", vendor, message),
            Err(e) => println!(
                "S3 {:?} upload with media test failed (expected without real credentials): {:?}",
                vendor, e
            ),
        }
    }
}

#[test]
fn test_call_record_filename_sanitization() {
    let record = CallRecord {
        call_id: "session~id/with..dots|and|pipes".to_string(),
        start_time: Utc::now(),
        ..Default::default()
    };

    let filename = default_cdr_file_name(&record);
    // session_id_with__dots_and_pipes
    assert!(filename.contains("session_id_with__dots_and_pipes"));
    assert!(!filename.contains("~"));
    assert!(!filename.contains("/"));
    assert!(!filename.contains("|"));
}

// ── Abandoned variant tests ──────────────────────────────────────────────

#[test]
fn test_hangup_reason_abandoned_display() {
    assert_eq!(
        CallRecordHangupReason::Abandoned.to_string(),
        "abandoned"
    );
}

#[test]
fn test_hangup_reason_abandoned_from_str() {
    use std::str::FromStr;
    let reason = CallRecordHangupReason::from_str("abandoned").unwrap();
    assert_eq!(reason, CallRecordHangupReason::Abandoned);
}

#[test]
fn test_hangup_reason_abandoned_distinct_from_canceled() {
    // The two must NOT be equal — they represent different scenarios.
    assert_ne!(
        CallRecordHangupReason::Abandoned,
        CallRecordHangupReason::Canceled
    );
    assert_ne!(
        CallRecordHangupReason::Abandoned.to_string(),
        CallRecordHangupReason::Canceled.to_string()
    );
}

#[test]
fn test_hangup_reason_abandoned_roundtrip() {
    use std::str::FromStr;
    let original = CallRecordHangupReason::Abandoned;
    let s = original.to_string();
    let parsed = CallRecordHangupReason::from_str(&s).unwrap();
    assert_eq!(original, parsed);
}

// ── CallRecordHangupReason::initiator() (module C) ───────────────────────

/// `initiator()` is the single source of truth for the normalized hangup
/// initiator used by both `call_hangup` and `cc_hangup`. Exhaustive mapping.
#[test]
fn test_initiator_mapping() {
    use CallRecordHangupReason::*;
    assert_eq!(ByCaller.initiator(), "caller");
    assert_eq!(Abandoned.initiator(), "caller");
    assert_eq!(ByCallee.initiator(), "agent");
    assert_eq!(BySystem.initiator(), "system");
    assert_eq!(Autohangup.initiator(), "system");
    assert_eq!(ByRefer.initiator(), "transfer");
    // Everything else is "unknown".
    assert_eq!(NoAnswer.initiator(), "unknown");
    assert_eq!(Canceled.initiator(), "unknown");
    assert_eq!(Rejected.initiator(), "unknown");
    assert_eq!(Failed.initiator(), "unknown");
    assert_eq!(RtpTimeout.initiator(), "unknown");
    assert_eq!(AnswerMachine.initiator(), "unknown");
    assert_eq!(NoBalance.initiator(), "unknown");
    assert_eq!(ServerUnavailable.initiator(), "unknown");
    assert_eq!(Other("x".into()).initiator(), "unknown");
}

// ── Two-phase CallRecordHook ordering (module B) ─────────────────────────
//
// The enrichment phase must run (in registration order) BEFORE the record is
// saved and before any side-effect (`on_record_completed`) hook, so that an
// enrich hook can populate fields a completed hook relies on.

struct CompletedProbe {
    log: Arc<Mutex<Vec<&'static str>>>,
    tag: &'static str,
    expect_queue: &'static str,
}

#[async_trait::async_trait]
impl CallRecordHook for CompletedProbe {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        // completed runs after enrich → the field set by the enrich probe
        // must already be visible here.
        assert_eq!(
            record.details.queue.as_deref(),
            Some(self.expect_queue),
            "enrich must run before completed"
        );
        self.log.lock().unwrap().push(self.tag);
        Ok(())
    }
}

struct EnrichSetsQueue;
#[async_trait::async_trait]
impl CallRecordHook for EnrichSetsQueue {
    async fn on_record_enrich(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        record.details.queue = Some("from-enrich".to_string());
        Ok(())
    }
}

#[tokio::test]
async fn test_enrich_phase_runs_before_completed_and_can_mutate() {
    // Drive the manager end-to-end: enrich → save → completed.
    let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    let manager = CallRecordManagerBuilder::new()
        .with_hook(Box::new(EnrichSetsQueue))
        .with_hook(Box::new(CompletedProbe {
            log: log.clone(),
            tag: "completed",
            expect_queue: "from-enrich",
        }))
        .build()
        .await
        .unwrap();

    let sender = manager.sender.clone();
    let cancel = manager.cancel_token.clone();
    let serve_handle = tokio::spawn(async move {
        let mut mgr = manager;
        mgr.serve().await;
    });

    let mut record = CallRecord {
        call_id: "order-test-1".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "a".to_string(),
        callee: "b".to_string(),
        status_code: 200,
        ..Default::default()
    };
    record.details.queue = None;
    sender.send(record).await.unwrap();

    // Wait for the completed probe to record its tag.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if log.lock().unwrap().contains(&"completed") {
            break;
        }
        if std::time::Instant::now() > deadline {
            cancel.cancel();
            serve_handle.await.ok();
            panic!("completed hook never ran; log={:?}", log.lock().unwrap());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    cancel.cancel();
    serve_handle.await.ok();
}
