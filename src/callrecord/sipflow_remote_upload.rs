use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, TimeZone};
use sea_orm::DatabaseConnection;
use tracing::{info, warn};

use crate::{
    callrecord::sipflow_upload::{SipFlowUploadRequest, SipFlowUploadResponse},
    callrecord::{
        CallRecord, CallRecordHook, format_sipflow_media_key, format_sipflow_signaling_file_name,
        format_sipflow_signaling_key,
    },
    config::{SipFlowClusterNode, SipFlowUploadConfig},
    rwi::RwiGatewayRef,
    sipflow::backend::remote::jump_consistent_hash,
};

/// A [`CallRecordHook`] that delegates SipFlow media/signalling upload to a
/// remote sipflow bin's `POST /upload` endpoint.  Used when
/// [`SipFlowConfig::Remote::delegate_upload`] is `true`.
pub struct SipFlowRemoteUploadHook {
    nodes: Vec<SipFlowClusterNode>,
    upload_config: SipFlowUploadConfig,
    db: Option<DatabaseConnection>,
    rwi_gateway: Option<RwiGatewayRef>,
    client: reqwest::Client,
}

impl SipFlowRemoteUploadHook {
    pub fn new(
        nodes: Vec<SipFlowClusterNode>,
        upload_config: SipFlowUploadConfig,
        db: Option<DatabaseConnection>,
        rwi_gateway: Option<RwiGatewayRef>,
    ) -> Result<Self> {
        Ok(Self {
            nodes,
            upload_config,
            db,
            rwi_gateway,
            client: crate::http_util::build_keepalive_client(
                Some(std::time::Duration::from_secs(120)),
                Some(std::time::Duration::from_secs(10)),
            )?,
        })
    }
}

#[async_trait]
impl CallRecordHook for SipFlowRemoteUploadHook {
    async fn on_record_completed(&self, record: &mut CallRecord) -> Result<()> {
        if record.answer_time.is_none() {
            return Ok(());
        }

        let call_id = record.call_id.as_str();
        let start = Local.from_utc_datetime(&record.start_time.naive_utc());
        let end = Local.from_utc_datetime(&record.end_time.naive_utc());
        let duration_secs = (record.end_time - record.start_time).num_seconds() as i32;

        let skip_media = !record.recorder.is_empty();

        // Pick the same node that owns this call_id via consistent hash.
        let idx = jump_consistent_hash(call_id, self.nodes.len());
        let node_http = self.nodes[idx].http.trim_end_matches('/').to_string();
        let upload_url = format!("{}/upload", node_http);

        // Clone upload config so we can adjust per-call flags.
        let mut upload_config = self.upload_config.clone();
        if skip_media {
            match &mut upload_config {
                SipFlowUploadConfig::S3 { media, .. } => *media = Some(false),
                SipFlowUploadConfig::Http { media, .. } => *media = Some(false),
            }
        }

        // Compute default keys.  Client-specified keys are not sent by the
        // hook (the bin will compute them via the fallback).
        let default_media = format_sipflow_media_key(record);
        let _default_signaling = format_sipflow_signaling_key(record);
        let _default_sig_file = format_sipflow_signaling_file_name(record);

        let req = SipFlowUploadRequest {
            call_id: call_id.to_string(),
            start: start.timestamp(),
            end: end.timestamp(),
            upload: upload_config,
            media_key: None,
            signaling_key: None,
            signaling_file_name: None,
        };

        let resp: SipFlowUploadResponse =
            match self.client.post(&upload_url).json(&req).send().await {
                Ok(r) if r.status().is_success() => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(
                            call_id,
                            "SipFlowRemoteUploadHook: decode response failed: {e}"
                        );
                        return Ok(());
                    }
                },
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    warn!(
                        call_id,
                        "SipFlowRemoteUploadHook: upload failed: {status} – {body}"
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(call_id, "SipFlowRemoteUploadHook: request failed: {e}");
                    return Ok(());
                }
            };

        if let Some(ref url) = resp.media_url {
            if let Some(db) = &self.db {
                if let Err(e) = crate::models::call_record::update_recording_url(
                    db,
                    call_id,
                    url,
                    duration_secs,
                )
                .await
                {
                    warn!(
                        call_id,
                        "SipFlowRemoteUploadHook: failed to update recording_url: {e}"
                    );
                }
            }
        }

        // Emit RecordEnd
        if let Some(ref url) = resp.media_url {
            if let Some(gw) = &self.rwi_gateway {
                let gw_ref = gw.read();
                gw_ref.send_to_owner(&crate::rwi::RecordEnd {
                    call_id: call_id.to_string(),
                    url: Some(url.clone()),
                    duration_secs: duration_secs as u64,
                    file_size: resp.media_size,
                });
                info!(call_id, "SipFlowRemoteUploadHook: RecordEnd event emitted");
            }
        }

        // Emit RecordingMetadataAvailable
        if let Some(ref url) = resp.media_url {
            if let Some(gw) = &self.rwi_gateway {
                use crate::rwi::proto::RecordingMetadata;
                let metadata = RecordingMetadata {
                    filename: default_media,
                    file_size: resp.media_size,
                    download_url: Some(url.clone()),
                    caller_name: None,
                    callee_name: None,
                    call_type: "".to_string(),
                    call_start_time: Some(start.to_rfc3339()),
                    call_end_time: Some(end.to_rfc3339()),
                    upload_time: Some(chrono::Utc::now().to_rfc3339()),
                    extra: None,
                };
                let gw_ref = gw.read();
                gw_ref.send_to_owner(&crate::rwi::RecordingMetadataAvailable {
                    call_id: call_id.to_string(),
                    metadata,
                });
                info!(
                    call_id,
                    "SipFlowRemoteUploadHook: RecordingMetadataAvailable event emitted"
                );
            }
        }

        Ok(())
    }
}
