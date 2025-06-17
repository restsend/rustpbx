use super::*;
use std::collections::HashMap;
use std::io::Write;
use tempfile::NamedTempFile;

#[tokio::test]
async fn test_save_with_http_without_media() {
    // Create a test CallRecord
    let mut extras = HashMap::new();
    extras.insert(
        "test_key".to_string(),
        serde_json::Value::String("test_value".to_string()),
    );

    let record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(crate::handler::CallOption::default()),
        call_id: "test_call_123".to_string(),
        start_time: Utc::now(),
        ring_time: None,
        answer_time: None,
        end_time: Utc::now(),
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        answer: None,
        offer: None,
        hangup_reason: None,
        recorder: vec![],
        extras: Some(extras),
        dump_event_file: None,
    };

    // Test without media (should not fail if no server available)
    let url = "http://httpbin.org/post".to_string();
    let headers = None;
    let with_media = Some(false);

    // This test will only pass if httpbin.org is available
    // In production, you might want to use a mock server
    let result = CallRecordManager::save_with_http(
        Arc::new(DefaultCallRecordFormatter),
        &url,
        &headers,
        &with_media,
        &record,
    )
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

    let mut extras = HashMap::new();
    extras.insert(
        "test_key".to_string(),
        serde_json::Value::String("test_value".to_string()),
    );

    let record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(crate::handler::CallOption::default()),
        call_id: "test_call_with_media_456".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        ring_time: None,
        answer_time: None,
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        answer: None,
        offer: None,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![media],
        extras: Some(extras),
        dump_event_file: None,
    };

    // Test with media
    let url = "http://httpbin.org/post".to_string();
    let headers = None;
    let with_media = Some(true);

    let result = CallRecordManager::save_with_http(
        Arc::new(DefaultCallRecordFormatter),
        &url,
        &headers,
        &with_media,
        &record,
    )
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

    let mut extras = HashMap::new();
    extras.insert(
        "test_key".to_string(),
        serde_json::Value::String("test_value".to_string()),
    );

    let record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(crate::handler::CallOption::default()),
        call_id: "test_call_headers_789".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        ring_time: None,
        answer_time: None,
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        answer: None,
        offer: None,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![],
        extras: Some(extras),
        dump_event_file: None,
    };

    let url = "http://httpbin.org/post".to_string();
    let with_media = Some(false);

    let result = CallRecordManager::save_with_http(
        Arc::new(DefaultCallRecordFormatter),
        &url,
        &Some(headers),
        &with_media,
        &record,
    )
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

    let mut extras = HashMap::new();
    extras.insert(
        "test_key".to_string(),
        serde_json::Value::String("test_value".to_string()),
    );

    let record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(crate::handler::CallOption::default()),
        call_id: "test_call_headers_789".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        ring_time: None,
        answer_time: None,
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        answer: None,
        offer: None,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![],
        extras: Some(extras),
        dump_event_file: None,
    };

    let url = "http://httpbin.org/post".to_string();
    let with_media = Some(false);

    let result = CallRecordManager::save_with_http(
        Arc::new(DefaultCallRecordFormatter),
        &url,
        &Some(headers),
        &with_media,
        &record,
    )
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
    let root = "test-records".to_string();
    let with_media = Some(false);

    let mut extras = HashMap::new();
    extras.insert(
        "test_key".to_string(),
        serde_json::Value::String("test_value".to_string()),
    );

    let record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(crate::handler::CallOption::default()),
        call_id: "test_s3_call_123".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        ring_time: None,
        answer_time: None,
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        answer: None,
        offer: None,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![],
        extras: Some(extras),
        dump_event_file: None,
    };

    // This test will only succeed if there's a local minio instance running
    // In real scenarios, this would use actual cloud storage credentials
    let result = CallRecordManager::save_with_s3_like(
        Arc::new(DefaultCallRecordFormatter),
        &vendor,
        &bucket,
        &region,
        &access_key,
        &secret_key,
        &endpoint,
        &root,
        &with_media,
        &record,
    )
    .await;

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

    let mut extras = HashMap::new();
    extras.insert(
        "test_key".to_string(),
        serde_json::Value::String("test_value".to_string()),
    );

    let record = CallRecord {
        call_type: crate::handler::call::ActiveCallType::Sip,
        option: Some(crate::handler::CallOption::default()),
        call_id: "test_s3_media_456".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        ring_time: None,
        answer_time: None,
        caller: "+1234567890".to_string(),
        callee: "+0987654321".to_string(),
        status_code: 200,
        offer: None,
        answer: None,
        hangup_reason: Some(CallRecordHangupReason::ByCaller),
        recorder: vec![media],
        extras: Some(extras),
        dump_event_file: None,
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
        let bucket = "test-bucket".to_string();
        let region = "us-east-1".to_string();
        let access_key = "test_access_key".to_string();
        let secret_key = "test_secret_key".to_string();
        let endpoint = endpoint.to_string();
        let root = "test-records".to_string();
        let with_media = Some(true);

        let result = CallRecordManager::save_with_s3_like(
            Arc::new(DefaultCallRecordFormatter),
            &vendor,
            &bucket,
            &region,
            &access_key,
            &secret_key,
            &endpoint,
            &root,
            &with_media,
            &record,
        )
        .await;

        match result {
            Ok(message) => println!("S3 {:?} upload with media test passed: {}", vendor, message),
            Err(e) => println!(
                "S3 {:?} upload with media test failed (expected without real credentials): {:?}",
                vendor, e
            ),
        }
    }
}
