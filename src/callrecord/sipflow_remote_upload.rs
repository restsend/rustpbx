use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, TimeZone};
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use tracing::{info, warn};

use crate::{
    callrecord::sipflow_upload::{
        SipFlowUploadRequest, SipFlowUploadResponse, SipFlowUploadRuntime,
    },
    callrecord::{
        CallRecord, CallRecordHook, format_sipflow_media_key, format_sipflow_signaling_file_name,
        format_sipflow_signaling_key, sipflow::SipFlowSlot,
    },
    config::{SipFlowClusterNode, SipFlowUploadConfig},
    sipflow::backend::remote::jump_consistent_hash,
};

/// A [`CallRecordHook`] that delegates SipFlow media/signalling upload to a
/// remote sipflow bin's `POST /upload` endpoint.  Used when
/// [`SipFlowConfig::Remote::delegate_upload`] is `true`.
pub struct SipFlowRemoteUploadHook {
    nodes: Vec<SipFlowClusterNode>,
    /// Late-bound `[sipflow.upload]` config; re-resolved per batch so config
    /// hot-reloads take effect without a restart.
    runtime: Arc<SipFlowUploadRuntime>,
    db: Option<DatabaseConnection>,
    client: reqwest::Client,
    /// Late-bound handle to the SipFlow wrapper. Flushed before delegating so
    /// the tail messages (BYE / 200 OK) leave the client-side pipeline (async
    /// writer batch → UDP sender) and are on the collector before it flushes
    /// + queries on /upload.
    sipflow: SipFlowSlot,
}

impl SipFlowRemoteUploadHook {
    pub fn new(
        nodes: Vec<SipFlowClusterNode>,
        runtime: Arc<SipFlowUploadRuntime>,
        db: Option<DatabaseConnection>,
        sipflow: SipFlowSlot,
    ) -> Result<Self> {
        Ok(Self {
            nodes,
            runtime,
            db,
            client: crate::http_util::build_keepalive_client(
                Some(std::time::Duration::from_secs(120)),
                Some(std::time::Duration::from_secs(10)),
            )?,
            sipflow,
        })
    }
}

#[async_trait]
impl CallRecordHook for SipFlowRemoteUploadHook {
    async fn on_record_enrich(&self, records: &mut [CallRecord]) -> Result<()> {
        let Some(upload_config) = self.runtime.config() else {
            return Ok(());
        };
        for record in records {
            crate::callrecord::sipflow_upload::preconstruct_signaling_url(
                record,
                &upload_config,
                false,
            );
        }
        Ok(())
    }

    async fn on_record_completed(&self, records: &mut [CallRecord]) -> Result<()> {
        let Some(upload_config) = self.runtime.config() else {
            return Ok(());
        };
        // Flush the client-side pipeline (writer thread → UDP sender batch)
        // once for the whole batch, so the collector has everything before it
        // flushes + queries on /upload. Bounded: proceed on timeout.
        if let Some(sipflow) = self.sipflow.get() {
            crate::callrecord::sipflow::flush_with_deadline(sipflow).await;
        } else {
            warn!("SipFlowRemoteUploadHook: no SipFlow handle, skipping pre-upload flush");
        }

        for record in records {
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
            let mut upload_config = upload_config.clone();
            if skip_media {
                match &mut upload_config {
                    SipFlowUploadConfig::S3 { media, .. } => *media = Some(false),
                    SipFlowUploadConfig::Http { media, .. } => *media = Some(false),
                }
            }

            // Compute default keys.  Client-specified keys are not sent by the
            // hook (the bin will compute them via the fallback).
            let _default_media = format_sipflow_media_key(record);
            let _default_signaling = format_sipflow_signaling_key(record);
            let _default_sig_file = format_sipflow_signaling_file_name(record);

            let req = SipFlowUploadRequest {
                call_id: call_id.to_string(),
                signaling_call_ids: record.sip_leg_roles.keys().cloned().collect(),
                start: start.timestamp(),
                end: end.timestamp(),
                upload: upload_config,
                media_key: None,
                signaling_key: None,
                signaling_file_name: None,
            };

            info!(
                call_id,
                upload_url, "SipFlowRemoteUploadHook: delegating upload"
            );

            let resp: SipFlowUploadResponse =
                match self.client.post(&upload_url).json(&req).send().await {
                    Ok(r) if r.status().is_success() => match r.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(
                                call_id,
                                upload_url, "SipFlowRemoteUploadHook: decode response failed: {e}"
                            );
                            continue;
                        }
                    },
                    Ok(r) => {
                        let status = r.status();
                        let body = r.text().await.unwrap_or_default();
                        warn!(
                            call_id,
                            upload_url, "SipFlowRemoteUploadHook: upload failed: {status} – {body}"
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            call_id,
                            upload_url, "SipFlowRemoteUploadHook: request failed: {e}"
                        );
                        continue;
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
                record.details.recording_url = Some(url.clone());
                record.details.recording_duration_secs = Some(duration_secs.max(0));
                record
                    .extensions
                    .insert(crate::callrecord::RecordingFileSize(resp.media_size));
            }

            if let Some(ref jsonl_url) = resp.signaling_url {
                crate::callrecord::sipflow_upload::stash_sipflow_jsonl(
                    record,
                    jsonl_url,
                    self.db.as_ref(),
                )
                .await;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn delegates_upload_for_unanswered_early_media_call() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/upload",
            post(move |Json(_request): Json<SipFlowUploadRequest>| {
                let request_count = request_count.clone();
                async move {
                    request_count.fetch_add(1, Ordering::Relaxed);
                Json(SipFlowUploadResponse {
                    media_url: None,
                    media_size: 0,
                    signaling_uploaded: false,
                    signaling_url: None,
                })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upload server");
        let address = listener.local_addr().expect("upload server address");
        crate::utils::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let runtime = Arc::new(SipFlowUploadRuntime::for_config(Some(
            crate::config::SipFlowConfig::Remote {
                nodes: vec![],
                udp_addr: None,
                http_addr: None,
                timeout_secs: 10,
                channel_capacity: 40000,
                dns_ttl_secs: 5,
                mtu: 1500,
                report_interval_secs: 10,
                upload: Some(SipFlowUploadConfig::Http {
                    url: "http://recording-upload.invalid".to_string(),
                    signaling_url: None,
                    headers: None,
                    method: None,
                    file_field: None,
                    body_field: None,
                    file_name: None,
                    content_type: None,
                    fields: None,
                    response_url_path: None,
                    response_success: None,
                    connect_timeout_ms: None,
                    request_timeout_ms: None,
                    signaling: Some(true),
                    media: Some(true),
                    force_pcm: None,
                    pcm_sample_rate: None,
                }),
                delegate_upload: true,
            },
        )));
        let hook = SipFlowRemoteUploadHook::new(
            vec![SipFlowClusterNode {
                udp: "127.0.0.1:0".to_string(),
                http: format!("http://{address}"),
            }],
            runtime,
            None,
            Arc::new(std::sync::OnceLock::new()),
        )
        .expect("remote upload hook");
        let now = chrono::Utc::now();
        let mut record = CallRecord {
            call_id: "remote-early-media".to_string(),
            start_time: now - chrono::Duration::seconds(5),
            answer_time: None,
            end_time: now,
            ..Default::default()
        };

        hook.on_record_completed(std::slice::from_mut(&mut record))
            .await
            .expect("delegate early media upload");

        assert_eq!(requests.load(Ordering::Relaxed), 1);
    }
}
