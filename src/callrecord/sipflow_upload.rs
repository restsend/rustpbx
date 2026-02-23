//! `SipFlowUploadHook` – uploads the WAV recording produced by SipFlow to S3 or
//! an HTTP endpoint after each call completes.
//!
//! The hook is inserted into the `CallRecordManager` hook chain when
//! `[sipflow.upload]` is present in the configuration.  It:
//!
//! 1. Calls `backend.flush()` so the local SipFlow SQLite database is up-to-date.
//! 2. Calls `backend.query_media()` to reconstruct a WAV from captured RTP.
//! 3. Uploads the WAV bytes to the configured S3 bucket or HTTP endpoint.
//! 4. Sets `record.details.recording_url` and `recording_duration_secs` in the
//!    mutable `CallRecord` so that `DatabaseHook` (which runs next) persists them.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{Local, TimeZone};
use tracing::{info, warn};

use crate::{
    callrecord::{CallRecord, CallRecordHook},
    config::SipFlowUploadConfig,
    sipflow::SipFlowBackend,
};

pub struct SipFlowUploadHook {
    pub backend: Arc<dyn SipFlowBackend>,
    pub upload_config: SipFlowUploadConfig,
}

#[async_trait]
impl CallRecordHook for SipFlowUploadHook {
    async fn on_record_completed(&self, record: &mut CallRecord) -> anyhow::Result<()> {
        // Flush the SipFlow batch so all RTP packets are in the SQLite file.
        if let Err(e) = self.backend.flush().await {
            warn!(call_id = %record.call_id, "SipFlowUploadHook: flush failed: {e}");
        }

        let start = Local.from_utc_datetime(&record.start_time.naive_utc());
        let end = Local.from_utc_datetime(&record.end_time.naive_utc());

        let wav_bytes = match self
            .backend
            .query_media(&record.call_id, start, end)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                warn!(call_id = %record.call_id, "SipFlowUploadHook: query_media failed: {e}");
                return Ok(());
            }
        };

        if wav_bytes.is_empty() {
            return Ok(());
        }

        // Path inside the storage target: YYYYMMDD/<call_id>.wav
        let date_prefix = record.start_time.format("%Y%m%d").to_string();
        let key = format!("{}/{}.wav", date_prefix, &record.call_id);

        let wav_len = wav_bytes.len();
        let url_result = match &self.upload_config {
            SipFlowUploadConfig::S3 {
                vendor,
                bucket,
                region,
                access_key,
                secret_key,
                endpoint,
                root,
            } => {
                let full_key = if root.is_empty() {
                    key.clone()
                } else {
                    format!("{}/{}", root.trim_end_matches('/'), key)
                };
                upload_s3(
                    vendor,
                    bucket,
                    region,
                    access_key,
                    secret_key,
                    endpoint,
                    &full_key,
                    wav_bytes,
                )
                .await
                .map(|_| {
                    format!(
                        "{}/{}",
                        endpoint.trim_end_matches('/'),
                        full_key.trim_start_matches('/')
                    )
                })
            }
            SipFlowUploadConfig::Http {
                url,
                headers,
            } => upload_http(url, headers.as_ref(), &record.call_id, wav_bytes).await,
        };

        match url_result {
            Ok(url) => {
                info!(
                    call_id = %record.call_id,
                    url,
                    bytes = wav_len,
                    "SipFlowUploadHook: recording uploaded"
                );
                let duration_secs = (record.end_time - record.start_time).num_seconds() as i32;
                record.details.recording_url = Some(url);
                record.details.recording_duration_secs = Some(duration_secs);
            }
            Err(e) => {
                warn!(call_id = %record.call_id, "SipFlowUploadHook: upload failed: {e}");
            }
        }

        Ok(())
    }
}

// ── Internal upload helpers ───────────────────────────────────────────────────

async fn upload_s3(
    vendor: &crate::config::S3Vendor,
    bucket: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    endpoint: &str,
    key: &str,
    data: Vec<u8>,
) -> Result<()> {
    use crate::storage::{Storage, StorageConfig};
    let storage = Storage::new(&StorageConfig::S3 {
        vendor: vendor.clone(),
        bucket: bucket.to_string(),
        region: region.to_string(),
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        endpoint: Some(endpoint.to_string()),
        prefix: None,
    })?;
    storage.write(key, Bytes::from(data)).await?;
    Ok(())
}

async fn upload_http(
    url: &str,
    headers: Option<&HashMap<String, String>>,
    call_id: &str,
    data: Vec<u8>,
) -> Result<String> {
    let client = reqwest::Client::new();
    let file_name = format!("{}.wav", call_id);
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name)
        .mime_str("audio/wav")?;
    let form = reqwest::multipart::Form::new().part("recording", part);

    let mut req = client.post(url).multipart(form);
    if let Some(h) = headers {
        for (k, v) in h {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    let response = req.send().await?;
    if response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        // Return a URL: use the posted URL or parse from response if it looks like one.
        let recording_url = if body.starts_with("http") {
            body.trim().to_string()
        } else {
            url.to_string()
        };
        Ok(recording_url)
    } else {
        Err(anyhow::anyhow!(
            "HTTP upload failed: {} – {}",
            response.status(),
            response.text().await.unwrap_or_default()
        ))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sipflow::{SipFlowBackend, SipFlowItem};
    use chrono::{DateTime, Local};

    struct MockBackend {
        /// WAV bytes returned by query_media
        media: Vec<u8>,
        /// Track flush call count
        flush_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl SipFlowBackend for MockBackend {
        fn record(&self, _call_id: &str, _item: SipFlowItem) -> anyhow::Result<()> {
            Ok(())
        }
        async fn flush(&self) -> anyhow::Result<()> {
            self.flush_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        async fn query_flow(
            &self,
            _call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<SipFlowItem>> {
            Ok(vec![])
        }
        async fn query_media_stats(
            &self,
            _call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<(i32, String, usize)>> {
            Ok(vec![])
        }
        async fn query_media(
            &self,
            _call_id: &str,
            _start: DateTime<Local>,
            _end: DateTime<Local>,
        ) -> anyhow::Result<Vec<u8>> {
            Ok(self.media.clone())
        }
    }

    fn make_record() -> CallRecord {
        use crate::callrecord::CallDetails;
        let now = chrono::Utc::now();
        let mut record = CallRecord::default();
        record.call_id = "test-call-id".to_string();
        record.start_time = now - chrono::Duration::seconds(30);
        record.end_time = now;
        record.caller = "alice".to_string();
        record.callee = "bob".to_string();
        record.details = CallDetails {
            direction: "inbound".to_string(),
            status: "completed".to_string(),
            ..Default::default()
        };
        record
    }

    #[tokio::test]
    async fn test_hook_skips_empty_media() {
        let flush_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook = SipFlowUploadHook {
            backend: Arc::new(MockBackend {
                media: vec![],
                flush_count: flush_count.clone(),
            }),
            upload_config: SipFlowUploadConfig::Http {
                url: "http://localhost:9999/upload".to_string(),
                headers: None,
            },
        };
        let mut record = make_record();
        hook.on_record_completed(&mut record).await.unwrap();
        // Flush was called even though media is empty
        assert_eq!(flush_count.load(std::sync::atomic::Ordering::Relaxed), 1);
        // URL was not set because media was empty
        assert!(record.details.recording_url.is_none());
    }

    #[tokio::test]
    async fn test_hook_calls_flush_before_query() {
        let flush_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Minimal valid WAV bytes (header only, 44 bytes)
        let fake_wav = b"RIFF$\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x80\x3e\x00\x00\x00\x7d\x00\x00\x02\x00\x10\x00data\x00\x00\x00\x00".to_vec();
        let hook = SipFlowUploadHook {
            backend: Arc::new(MockBackend {
                media: fake_wav,
                flush_count: flush_count.clone(),
            }),
            upload_config: SipFlowUploadConfig::Http {
                // Use a non-existent URL; upload will fail but flush must have run first.
                url: "http://127.0.0.1:1".to_string(),
                headers: None,
            },
        };
        let mut record = make_record();
        // Hook should not return an error even if upload fails (it only warns)
        hook.on_record_completed(&mut record).await.unwrap();
        // flush() must have been called exactly once
        assert_eq!(flush_count.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn test_upload_config_parse_s3() {
        // Parse SipFlowConfig directly (avoids needing the full Config with [proxy])
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "s3"
vendor = "aws"
bucket = "my-recordings"
region = "us-east-1"
access_key = "AKID"
secret_key = "SECRET"
endpoint = "https://s3.amazonaws.com"
root = "recordings"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse s3 upload config");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::S3 {
                        bucket,
                        region,
                        ..
                    } => {
                        assert_eq!(bucket, "my-recordings");
                        assert_eq!(region, "us-east-1");
                    }
                    _ => panic!("expected S3 variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_upload_config_parse_http() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"

[upload]
type = "http"
url = "https://example.com/recordings"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse http upload config");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                let upload = upload.expect("upload should be set");
                match upload {
                    SipFlowUploadConfig::Http {
                        url, ..
                    } => {
                        assert_eq!(url, "https://example.com/recordings");
                    }
                    _ => panic!("expected Http variant"),
                }
            }
            _ => panic!("expected Local sipflow config"),
        }
    }

    #[test]
    fn test_sipflow_config_default_no_upload() {
        let toml_str = r#"
type = "local"
root = "/var/sipflow"
"#;
        let cfg: crate::config::SipFlowConfig =
            toml::from_str(toml_str).expect("should parse sipflow without upload");
        match cfg {
            crate::config::SipFlowConfig::Local { upload, .. } => {
                assert!(upload.is_none(), "upload should default to None");
            }
            _ => panic!("expected Local sipflow config"),
        }
    }
}
