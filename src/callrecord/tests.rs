use super::*;
use crate::callrecord::CallRecordRow;
use crate::callrecord::{
    CallRecordHangupReason, create_call_record_table, derive_daily_url, today_string,
};
use crate::config::{CallRecordStorageConfig, RotationMode};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tempfile::{NamedTempFile, TempDir};

fn make_record() -> CallRecord {
    let now = Utc::now();
    CallRecord {
        call_id: "test-call-id".to_string(),
        start_time: now,
        end_time: now + chrono::Duration::seconds(30),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        details: crate::callrecord::CallDetails {
            direction: "inbound".to_string(),
            status: "completed".to_string(),
            from_number: Some("+1234567890".to_string()),
            to_number: Some("+0987654321".to_string()),
            caller_name: Some("Alice".to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn in_memory_db() -> DatabaseConnection {
    crate::models::connect_db("sqlite::memory:").await.unwrap()
}

async fn count_rows(db: &DatabaseConnection, table: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    let backend = db.get_database_backend();
    let result = db
        .query_all(Statement::from_sql_and_values(
            backend,
            &format!("SELECT COUNT(*) as c FROM {}", table),
            Vec::new(),
        ))
        .await;
    match result {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.try_get::<i64>("", "c").ok())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

// ── CallRecordRow extraction ───────────────────────────────────────────────────

#[test]
fn test_call_record_row_from_record() {
    let record = make_record();
    let row = CallRecordRow::from_record(&record);
    assert_eq!(row.call_id, "test-call-id");
    assert_eq!(row.direction, "inbound");
    assert_eq!(row.status, "completed");
    assert_eq!(row.from_number.as_deref(), Some("+1234567890"));
    assert_eq!(row.caller_name.as_deref(), Some("Alice"));
    assert!(row.ended_at.is_some());
    assert_eq!(row.duration_secs, 30);
    assert!(row.has_transcript == false);
    assert!(row.display_id.is_none());
    assert!(row.archived_at.is_none());
}

// ── today_string / derive_daily_url ─────────────────────────────────────────────

#[test]
fn test_today_string_format() {
    let s = today_string();
    assert_eq!(s.len(), 8);
    assert!(s.chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn test_derive_daily_url_sqlite() {
    let url = derive_daily_url("sqlite:///config/cdr/cdr.db", "20260722");
    assert_eq!(url, "sqlite:///config/cdr/cdr-20260722.db");
}

#[test]
fn test_derive_daily_url_sqlite_no_ext() {
    let url = derive_daily_url("sqlite:///config/cdr/data", "20260722");
    assert_eq!(url, "sqlite:///config/cdr/data-20260722");
}

#[test]
fn test_derive_daily_url_single_slash() {
    let url = derive_daily_url("sqlite:./config/cdr/cdr.db", "20260722");
    assert_eq!(url, "sqlite:./config/cdr/cdr-20260722.db");
}

#[test]
fn test_derive_daily_url_non_sqlite() {
    let url = derive_daily_url("postgres://localhost/cdrs", "20260722");
    assert_eq!(url, "postgres://localhost/cdrs");
}

// ── create_call_record_table ───────────────────────────────────────────────────

#[tokio::test]
async fn test_create_call_record_table() {
    let db = in_memory_db().await;
    create_call_record_table(&db, "test_cdrs").await.unwrap();
    // Verify table exists by querying it
    let rows = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "SELECT COUNT(*) AS cnt FROM test_cdrs",
        ))
        .await;
    assert!(rows.is_ok(), "table should exist");
}

// ── BuiltinDatabaseSaver ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_builtin_saver_writes_to_rustpbx_call_records() {
    let db = in_memory_db().await;
    create_call_record_table(&db, "rustpbx_call_records")
        .await
        .unwrap();
    let saver = crate::callrecord::BuiltinDatabaseSaver { db: db.clone() };
    let record = make_record();
    let result = saver.save(&record).await;
    assert!(
        result.is_ok(),
        "builtin saver should succeed: {:?}",
        result.err()
    );
}

// ── CustomDatabaseSaver ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_custom_saver_writes_full_schema() {
    let db = in_memory_db().await;
    let table = "my_cdrs";
    create_call_record_table(&db, table).await.unwrap();
    let saver = crate::callrecord::CustomDatabaseSaver {
        db: db.clone(),
        table_name: table.to_string(),
    };
    let record = make_record();
    let result = saver.save(&record).await;
    assert!(
        result.is_ok(),
        "custom saver should succeed: {:?}",
        result.err()
    );
}

// ── RotatingSqliteSaver ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_rotating_sqlite_saver_creates_daily_file() {
    let dir = TempDir::new().unwrap();
    let base = format!("sqlite://{}/cdr.db", dir.path().display());
    let today = today_string();
    let daily_url = derive_daily_url(&base, &today);
    let db = crate::models::connect_db(&daily_url).await.unwrap();
    create_call_record_table(&db, "rustpbx_call_records").await.unwrap();
    let saver = crate::callrecord::RotatingSqliteSaver {
        base_url: base.clone(),
        table_name: "rustpbx_call_records".to_string(),
        skip_create_table: false,
        state: Arc::new(tokio::sync::Mutex::new(crate::callrecord::RotateState {
            current_date: today.clone(),
            db,
        })),
    };
    let record = make_record();
    let result = saver.save(&record).await;
    assert!(
        result.is_ok(),
        "rotating saver should succeed: {:?}",
        result.err()
    );

    let expected_path = dir.path().join(format!("cdr-{}.db", today));
    assert!(
        expected_path.exists(),
        "daily file should exist: {:?}",
        expected_path
    );
}

// ── Builder without config (needs main_db) ─────────────────────────────────────

#[tokio::test]
async fn test_builder_without_callrecord_config_uses_builtin_saver() {
    let db = in_memory_db().await;
    create_call_record_table(&db, "rustpbx_call_records")
        .await
        .unwrap();
    let manager = CallRecordManagerBuilder::new()
        .with_main_db(db)
        .build()
        .await
        .unwrap();
    let record = make_record();
    let result = manager.saver.save(&record).await;
    assert!(
        result.is_ok(),
        "default saver should succeed: {:?}",
        result.err()
    );
    assert!(result.unwrap().starts_with("rustpbx_call_records/"));
}

// ── Builder with custom database requires main_db or database_url ─────────────

#[tokio::test]
async fn test_database_saver_without_url_needs_main_db() {
    let err = CallRecordManagerBuilder::new()
        .with_config(CallRecordConfig {
            max_concurrent: 64,
            storage: CallRecordStorageConfig::Database {
                database_url: None,
                table_name: "custom_table".to_string(),
                skip_create_table: false,
                rotate: RotationMode::None,
            },
        })
        .build()
        .await
        .err()
        .unwrap();

    assert!(
        err.to_string().contains("database_url") || err.to_string().contains("main_db"),
        "error should mention database_url or main_db: {}",
        err
    );
}

// ── No config + no main_db → error ────────────────────────────────────────────

#[tokio::test]
async fn test_none_config_without_main_db_errors() {
    let err = CallRecordManagerBuilder::new()
        .build()
        .await
        .err()
        .expect("should error when no config and no main_db");

    assert!(
        err.to_string().contains("main_db"),
        "error should mention main_db: {}",
        err
    );
}

// ── Local config + no main_db → OK, writes to local file only ──────────────────

#[tokio::test]
async fn test_local_config_without_main_db_ok() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_string_lossy().to_string();

    let manager = CallRecordManagerBuilder::new()
        .with_config(CallRecordConfig {
            max_concurrent: 4,
            storage: CallRecordStorageConfig::Local {
                root: root.clone(),
            },
        })
        .build()
        .await
        .expect("Local config should not require main_db");

    let record = make_record();
    let result = manager.saver.save(&record).await;
    assert!(result.is_ok(), "save should succeed: {:?}", result.err());

    // File should exist on disk
    let saved_path = result.unwrap();
    assert!(
        std::path::Path::new(&saved_path).exists(),
        "local file should exist: {}",
        saved_path
    );
}

// ── HTTP config + no main_db → OK ──────────────────────────────────────────────

#[tokio::test]
async fn test_http_config_without_main_db_ok() {
    let manager = CallRecordManagerBuilder::new()
        .with_config(CallRecordConfig {
            max_concurrent: 4,
            storage: CallRecordStorageConfig::Http {
                url: "http://127.0.0.1:1/cdr".to_string(),
                headers: None,
                with_media: None,
                keep_media_copy: None,
            },
        })
        .build()
        .await
        .expect("HTTP config should not require main_db");

    // Saver is built; actual POST will fail (no server) but build() itself succeeds.
    let record = make_record();
    let _ = manager.saver.save(&record).await; // expected to fail (connection refused)
}

// ── S3 config + no main_db → OK (Storage::new doesn't actually connect) ────────

#[tokio::test]
async fn test_s3_config_without_main_db_ok() {
    let manager = CallRecordManagerBuilder::new()
        .with_config(CallRecordConfig {
            max_concurrent: 4,
            storage: CallRecordStorageConfig::S3 {
                vendor: crate::config::S3Vendor::Minio,
                bucket: "test-bucket".to_string(),
                region: "us-east-1".to_string(),
                access_key: "minioadmin".to_string(),
                secret_key: "minioadmin".to_string(),
                endpoint: Some("http://127.0.0.1:1".to_string()),
                root: "cdr".to_string(),
                with_media: None,
                keep_media_copy: None,
            },
        })
        .build()
        .await
        .expect("S3 config should not require main_db");

    // Saver is built; actual upload will fail (no server) but build() succeeds.
    let record = make_record();
    let _ = manager.saver.save(&record).await; // expected to fail (connection refused)
}

// ── Database config + database_url + no main_db → OK ───────────────────────────

#[tokio::test]
async fn test_database_with_url_without_main_db_ok() {
    let manager = CallRecordManagerBuilder::new()
        .with_config(CallRecordConfig {
            max_concurrent: 4,
            storage: CallRecordStorageConfig::Database {
                database_url: Some("sqlite::memory:".to_string()),
                table_name: "custom_cdr".to_string(),
                skip_create_table: false,
                rotate: RotationMode::None,
            },
        })
        .build()
        .await
        .expect("Database config with database_url should not require main_db");

    let record = make_record();
    let result = manager.saver.save(&record).await;
    assert!(result.is_ok(), "save should succeed: {:?}", result.err());
}

// ── S3/HTTP/Local savers do NOT touch the database ────────────────────────────

#[tokio::test]
async fn test_local_saver_does_not_write_to_db() {
    let db = in_memory_db().await;
    create_call_record_table(&db, "rustpbx_call_records")
        .await
        .unwrap();

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_string_lossy().to_string();

    let manager = CallRecordManagerBuilder::new()
        .with_main_db(db.clone())
        .with_config(CallRecordConfig {
            max_concurrent: 4,
            storage: CallRecordStorageConfig::Local {
                root: root.clone(),
            },
        })
        .build()
        .await
        .unwrap();

    let record = make_record();
    manager.saver.save(&record).await.unwrap();

    // Verify the DB table is empty — Local saver must not have written to it.
    let count = count_rows(&db, "rustpbx_call_records").await;
    assert_eq!(
        count, 0,
        "Local saver must not write any rows to the database"
    );
}

#[tokio::test]
async fn test_default_db_saver_writes_to_db() {
    // Sanity check: when no [callrecord] is configured, the default DB saver
    // DOES write to the database. This complements the above test.
    let db = in_memory_db().await;
    create_call_record_table(&db, "rustpbx_call_records")
        .await
        .unwrap();

    let manager = CallRecordManagerBuilder::new()
        .with_main_db(db.clone())
        .build()
        .await
        .unwrap();

    let record = make_record();
    manager.saver.save(&record).await.unwrap();

    let count = count_rows(&db, "rustpbx_call_records").await;
    assert_eq!(count, 1, "default DB saver should write exactly one row");
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
                endpoint: Some(endpoint),
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
                    endpoint: Some(endpoint),
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
    assert_eq!(CallRecordHangupReason::Abandoned.to_string(), "abandoned");
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
    let db = in_memory_db().await;
    create_call_record_table(&db, "rustpbx_call_records")
        .await
        .unwrap();

    let manager = CallRecordManagerBuilder::new()
        .with_main_db(db)
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
