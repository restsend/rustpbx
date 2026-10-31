use super::prelude::*;
use super::session::{SipSession, RECORDING_STOP_ATTEMPTS, RECORDING_STOP_RETRY_DELAY, RECORDING_STOP_TIMEOUT};
use super::RUSTPBX_COMMAND_CT;
use crate::proxy::proxy_call::sip_session::util::format_duration_ms;


// ===================================================================
// Recording for SipSession (auto-recording lifecycle, segment
// finalize/publish, recording routes, trace/play/leg event recording,
// snapshot assembly).
// Moved from sip_session/session.rs.
// ===================================================================

impl SipSession {
    /// Create the capture queue and task before constructing caller leg A.
    /// The sender becomes immutable state on the leg's plaintext RTP tap.
    /// Recording capture is attached to the caller peer so a call can
    /// start recording on demand at any later stage (IVR node, agent
    /// connect, API command) even when no recording policy enabled it.
    /// Without an installed recorder the task simply discards the tapped
    /// packets, so the idle cost is a single lightweight task per session.
    pub(crate) fn setup_recording_capture(
        &mut self,
    ) -> Result<Option<crate::media::media_recorder::RecorderSender>> {
        let _media_runtime_guard = crate::utils::media_enter();
        let sender = self.media.recording.setup_recorder_task()?;
        Ok(Some(sender))
    }

    /// Fill missing file options from per-call policy, then server policy.
    /// Explicit RecorderOption values win, including stereo_swap = false.
    pub(crate) fn recorder_option(&self, path: String) -> crate::media::recorder::RecorderOption {
        let policy_guard = self.server.recording_policy.load();
        let global = policy_guard.as_ref().as_ref();
        let local = self.context.dialplan.recording_policy.as_ref();
        let mut option = self
            .context
            .dialplan
            .recording
            .option
            .clone()
            .unwrap_or_default();
        option.recorder_file = path;
        option.samplerate = option
            .samplerate
            .or_else(|| local.and_then(|p| p.samplerate))
            .or_else(|| global.and_then(|p| p.samplerate));
        option.ptime = option
            .ptime
            .or_else(|| local.and_then(|p| p.ptime))
            .or_else(|| global.and_then(|p| p.ptime));
        self.resolve_stereo_assignment(&mut option, local, global);
        option
    }

    /// Clear the `discard` marker on the active recording segment. The
    /// originate failure path uses this to KEEP the pre-answer ringback slice
    /// (an unanswered call's artifact) when it stops the recorder — answered
    /// calls instead stop while the marker is armed, deleting the file.
    pub(crate) fn clear_active_recording_discard(&mut self) {
        if let Some(active) = self.recording.active.as_mut() {
            active.discard = false;
        }
    }

    /// Close the active recording segment when the session hands control to a
    /// successor application (transfer / queue / IVR jump). App-scoped sources
    /// ([`crate::callrecord::RecordingSource::Ivr`]) end with their owning
    /// app — the IVR stage's file is finalized at the hand-off ("截断") so the
    /// agent stage records as its own segment. Policy full-call recordings
    /// (`full`) and external segments keep rolling across the hand-off.
    pub(crate) async fn close_app_scoped_recording(&mut self) {
        let is_app_scoped = self
            .recording.active
            .as_ref()
            .is_some_and(|a| a.source == crate::callrecord::RecordingSource::Ivr);
        if !is_app_scoped {
            return;
        }
        match self.stop_and_publish_recording().await {
            Ok(Some(path)) => {
                info!(
                    session_id = %self.id,
                    path = %path,
                    "IVR recording segment closed at app hand-off"
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    session_id = %self.id,
                    %error,
                    "Failed to close IVR recording segment at app hand-off"
                );
            }
        }
    }

    /// Install the recorder implementation selected for this call. Signaling
    /// call sites decide when automatic installation is allowed.
    pub(crate) async fn set_auto_recorder(&mut self) -> Result<()> {
        let recording = self.context.dialplan.recording.clone();
        if self.media.recording.has_recorder().await {
            return Ok(());
        }

        // Source-scoped auto start ([recording].auto_start_except): a call
        // routed to an exempt application (e.g. IVR) installs no auto
        // recorder — the IVR flow records only via its smart-node
        // `record start` actions (segmented IVR/agent recordings).
        let routed_app = match &self.context.dialplan.flow {
            crate::call::DialplanFlow::Application { app_name, .. } => Some(app_name.as_str()),
            _ => None,
        };
        {
            let policy_guard = self.server.recording_policy.load();
            let policy = policy_guard.as_ref().as_ref().or(self
                .context
                .dialplan
                .recording_policy
                .as_ref());
            if let Some(policy) = policy
                && !policy.auto_start_covers_app(routed_app)
            {
                debug!(
                    session_id = %self.id,
                    routed_app = routed_app.unwrap_or("(none)"),
                    "auto recorder skipped: routed app exempted by [recording].auto_start_except"
                );
                self.record_trace(
                    crate::call_errors::TraceEvent::new(
                        crate::call_errors::TraceKind::Recording,
                        format!(
                            "Auto recording skipped (app '{}' exempted by [recording].auto_start_except)",
                            routed_app.unwrap_or("(none)")
                        ),
                    )
                    .severity(crate::call_errors::ErrSeverity::Info)
                    .code(crate::callrecord::recording_upload::recording_trace_codes::AUTOSTART_SKIPPED)
                    .detail(serde_json::json!({ "routed_app": routed_app })),
                );
                return Ok(());
            }
        }

        if recording.uses_file_media() || recording.option.is_some() {
            let path = recording
                .option
                .as_ref()
                .map(|option| option.recorder_file.clone())
                .ok_or_else(|| anyhow!("file recording strategy has no output path"))?;
            if path.trim().is_empty() {
                return Err(anyhow!("file recording strategy has no output path"));
            }
            let profile = self
                .media_leg(&LegId::from("caller"))
                .and_then(|peer| peer.negotiated())
                .ok_or_else(|| anyhow!("No caller media profile for recording"))?;
            let option = self.recorder_option(path.clone());
            self.media
                .recording
                .start_recording(profile, option, 2, false, None)
                .await?;
            self.recording.active = Some(crate::callrecord::ActiveRecording {
                path,
                segment_type: "full".to_string(),
                source: crate::callrecord::RecordingSource::Full,
                segment_id: "full".to_string(),
                seq: 1,
                label: "full".to_string(),
                started_at: chrono::Utc::now(),
                notify_app: false,
                unique_id: uuid::Uuid::new_v4().to_string(),
                discard: false,
            });
            debug!(session_id = %self.id, backend = "file", "auto recorder installed");
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Recording,
                    "Recording started (auto, full call)",
                )
                .severity(crate::call_errors::ErrSeverity::Info)
                .code(crate::callrecord::recording_upload::recording_trace_codes::STARTED)
                .detail(serde_json::json!({
                    "backend": "file",
                    "path": self.recording.active.as_ref().map(|r| r.path.clone()),
                    "segment_type": "full",
                    "recording_type": format!("{:?}", recording.recording_type),
                })),
            );
            return Ok(());
        }

        let sipflow_backend = self
            .server
            .sip_flow
            .as_ref()
            .and_then(|flow| flow.backend())
            .ok_or_else(|| anyhow!("SipFlow recording strategy has no backend"))?;
        // RTP capture stores leg ids per packet; resolving the stereo mode
        // here lets the capture swap them so the external party lands on
        // leg 0 — the left channel of every exported WAV.
        let policy_guard = self.server.recording_policy.load();
        let mut channel_option = crate::media::recorder::RecorderOption::default();
        self.resolve_stereo_assignment(
            &mut channel_option,
            self.context.dialplan.recording_policy.as_ref(),
            policy_guard.as_ref().as_ref(),
        );
        let recorder = crate::media::media_recorder::SipflowRecorder::new(
            sipflow_backend,
            self.context.session_id.clone(),
        )
        .with_swapped_legs(channel_option.stereo_swap.unwrap_or(false));
        self.media
            .recording
            .set_recorder(Box::new(recorder), None)
            .await?;
        debug!(session_id = %self.id, backend = "sipflow", "auto recorder installed");
        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Recording,
                "Recording started (auto, sipflow capture)",
            )
            .severity(crate::call_errors::ErrSeverity::Info)
            .code(crate::callrecord::recording_upload::recording_trace_codes::STARTED)
            .detail(serde_json::json!({
                "backend": "sipflow",
                "recording_type": format!("{:?}", recording.recording_type),
            })),
        );
        Ok(())
    }

    /// Install the auto recorder when the dialplan asks for `stage`, tracing
    /// the outcome into the call trace: `recording.start_failed` (kind error,
    /// severity warn) when installation fails — previously this path only
    /// logged and the CDR could not explain a call without its recording.
    pub(crate) async fn set_auto_recorder_traced(
        &mut self,
        stage: Option<crate::config::RecordingAutoStartAt>,
    ) {
        if !self.should_auto_start_recording(stage) {
            return;
        }
        if let Err(error) = self.set_auto_recorder().await {
            warn!(session_id = %self.id, %error, "Auto recorder installation failed");
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Error,
                    format!("Recording failed to start: {error}"),
                )
                .severity(crate::call_errors::ErrSeverity::Warn)
                .code(crate::callrecord::recording_upload::recording_trace_codes::START_FAILED)
                .detail(serde_json::json!({ "error": error.to_string() })),
            );
        }
    }

    pub(crate) fn recording_root_dir(&self) -> String {
        self.server
            .recording_policy
            .load()
            .as_ref()
            .as_ref()
            .map(|p| p.recorder_path())
            .unwrap_or_else(|| "recordings".to_string())
    }

    /// Resolve the file-name label for a recording segment: explicit
    /// `config.label` → resolved/known agent id → ivr name (session
    /// extensions) → `segment_type`. Extensions are populated by the routing
    /// layer (`ivr`) and the CC addon (`resolved_agent_id` / `agent_id`) as
    /// the call progresses, so the same session yields `main-ivr` during the
    /// IVR stage and `1001` once an agent answers.
    pub(crate) fn recording_label(&self, config_label: Option<&str>, segment_type: &str) -> String {
        config_label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.pinned_agent_id())
            .or_else(|| self.session_ext_get("agent_id"))
            .or_else(|| self.session_ext_get("ivr"))
            .unwrap_or_else(|| segment_type.to_string())
    }

    /// Close out the active recording segment: move
    /// [`crate::callrecord::ActiveRecording`] into a completed
    /// [`crate::callrecord::RecordingSegment`]. Returns `None` for a
    /// `discard`-flagged segment (file deleted, CDR entry suppressed);
    /// otherwise `(notify_app, unique_id)` — the id carried on RWI
    /// `record_stopped` — plus whether the running CallApp is notified.
    pub(crate) fn finalize_active_recording_segment(
        &mut self,
        result: &crate::media::media_recorder::RecordingResult,
    ) -> Option<(bool, Option<String>)> {
        let ended_at = chrono::Utc::now();
        let active = self.recording.active.take();
        if active.as_ref().is_some_and(|a| a.discard) {
            let path = std::path::Path::new(&result.path);
            if let Err(error) = std::fs::remove_file(path) {
                warn!(
                    session_id = %self.id,
                    path = %result.path,
                    %error,
                    "Failed to discard recording segment file"
                );
            }
            let _ = std::fs::remove_file(crate::callrecord::upload_failed_marker_path(path));
            return None;
        }
        let (segment_type, segment_id, seq, label, started_at, unique_id, notify_app) =
            if let Some(active) = active {
                (
                    active.segment_type,
                    active.segment_id,
                    active.seq,
                    active.label,
                    Some(active.started_at.to_rfc3339()),
                    Some(active.unique_id),
                    active.notify_app,
                )
            } else {
                (
                    "full".to_string(),
                    "full".to_string(),
                    0,
                    "full".to_string(),
                    None,
                    Some(uuid::Uuid::new_v4().to_string()),
                    false,
                )
            };
        self.recording.completed_segments
            .push(crate::callrecord::RecordingSegment {
                path: result.path.clone(),
                size: result.file_size,
                segment_type,
                segment_id,
                seq,
                label,
                started_at,
                ended_at: Some(ended_at.to_rfc3339()),
                duration_secs: result.duration_secs,
                unique_id: unique_id.clone(),
            });
        Some((notify_app, unique_id))
    }

    pub(crate) async fn handle_recording_segment_stop_info(
        &mut self,
        params: Option<&serde_json::Value>,
        dialog_leg: Option<LegId>,
        transaction: &TransactionHandle,
    ) -> Result<()> {
        let text = |key| {
            params
                .and_then(|p| p.get(key))
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
        };
        let Some((label, unique_id)) = text("label").zip(text("unique_id")) else {
            transaction
                .respond(StatusCode::BadRequest, None, None)
                .await?;
            return Ok(());
        };
        let ours = self.recording.active.as_ref().is_some_and(|active| {
            active.segment_type == "agent" && active.label == label && active.unique_id == unique_id
        });
        let completed = self.recording.completed_segments.iter().any(|segment| {
            segment.segment_type == "agent"
                && segment.label == label
                && segment.unique_id.as_deref() == Some(unique_id)
        });
        let ack = serde_json::json!({"unique_id": unique_id, "stopped": true})
            .to_string()
            .into_bytes();
        if ours {
            // Execute in the receiving loop: queue acceptance is not recorder completion.
            let result = self.stop_and_publish_recording().await;
            // A failed finalize consumes the backend; a later Ok(None) is not success.
            let finalized = matches!(&result, Ok(Some(_)))
                && self.recording.completed_segments.iter().any(|segment| {
                    segment.segment_type == "agent"
                        && segment.label == label
                        && segment.unique_id.as_deref() == Some(unique_id)
                });
            if !finalized {
                let error = result.err().unwrap_or_else(|| {
                    anyhow!("recording finalization did not publish the requested segment")
                });
                warn!(session_id = %self.id, %unique_id, %error, "Recording segment stop failed");
                self.record_trace(
                    crate::call_errors::TraceEvent::new(
                        crate::call_errors::TraceKind::Recording,
                        "Recording segment stop failed to finalize the recorder",
                    )
                    .severity(crate::call_errors::ErrSeverity::Warn)
                    .code(crate::callrecord::recording_upload::recording_trace_codes::STOP_FAILED)
                    .detail(serde_json::json!({
                        "label": label,
                        "unique_id": unique_id,
                        "error": error.to_string(),
                    })),
                );
                transaction
                    .respond(StatusCode::ServerInternalError, None, None)
                    .await?;
                return Ok(());
            }
        } else if !completed {
            let upstream_peer = self
                .context
                .cookie
                .get_extension::<crate::call::CallerPeerContext>()
                .and_then(|peer| peer.0.parse::<std::net::SocketAddr>().ok())
                .is_some_and(|peer| self.server.cluster_peer_ips.contains(&peer.ip()));
            if upstream_peer
                && dialog_leg
                    .as_ref()
                    .is_some_and(|leg| leg.as_str() != "caller")
                && let Some(dialog) = self.caller_dialog.clone()
            {
                let transaction = transaction.clone();
                let label = label.to_string();
                let unique_id = unique_id.to_string();
                let cancel = self.cancel_token.clone();
                // Only downstream-to-upstream requests may traverse a peer chain.
                // The origin owns retries; intermediate nodes forward one attempt.
                tokio::spawn(async move {
                    let result = tokio::select! {
                        _ = cancel.cancelled() => Err(anyhow!("session ended during recording stop")),
                        result = Self::send_recording_segment_stop_once(&dialog, &label, &unique_id) => result,
                    };
                    let (status, body) = match result {
                        Ok(()) => (StatusCode::OK, Some(ack)),
                        Err(_) => (StatusCode::ServiceUnavailable, None),
                    };
                    let _ = transaction.respond(status, None, body).await;
                });
                return Ok(());
            }
            // A missing recorder is not proof that this identity was finalized.
            // This 404 is the smoking gun for "the promised agent recording
            // never started upstream" — log it and surface it in the CDR
            // timeline (it used to be completely silent).
            warn!(session_id = %self.id,
                session_id = %self.context.session_id,
                label = %label,
                unique_id = %unique_id,
                "record.stop_segment: no active or completed agent segment matches this identity"
            );
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Recording,
                    "Peer record.stop_segment matched no segment on this node",
                )
                .severity(crate::call_errors::ErrSeverity::Warn)
                .code(crate::callrecord::recording_upload::recording_trace_codes::PEER_SEGMENT_STOP_NOT_FOUND)
                .detail(serde_json::json!({
                    "label": label,
                    "unique_id": unique_id,
                })),
            );
            transaction
                .respond(StatusCode::NotFound, None, None)
                .await?;
            return Ok(());
        }
        transaction.respond(StatusCode::OK, None, Some(ack)).await?;
        Ok(())
    }

    pub(crate) async fn send_recording_segment_stop_once(
        dialog: &InviteDialog,
        label: &str,
        unique_id: &str,
    ) -> Result<()> {
        let body = serde_json::json!({
            "action": "record.stop_segment",
            "params": {"label": label, "unique_id": unique_id},
        })
        .to_string()
        .into_bytes();
        let headers = vec![rsipstack::sip::Header::ContentType(
            RUSTPBX_COMMAND_CT.into(),
        )];
        let response = tokio::time::timeout(
            RECORDING_STOP_TIMEOUT,
            dialog.info(Some(headers), Some(body)),
        )
        .await??
        .ok_or_else(|| anyhow!("upstream dialog is not confirmed"))?;
        if response.status_code != StatusCode::OK {
            return Err(anyhow!("recording stop rejected: {}", response.status_code));
        }
        let ack: serde_json::Value = serde_json::from_slice(response.body())?;
        if ack.get("unique_id").and_then(|v| v.as_str()) != Some(unique_id)
            || ack.get("stopped").and_then(|v| v.as_bool()) != Some(true)
        {
            return Err(anyhow!("peer did not confirm recording finalization"));
        }
        Ok(())
    }

    pub(crate) async fn request_recording_segment_stop(
        dialog: &InviteDialog,
        label: &str,
        unique_id: &str,
    ) -> Result<()> {
        for attempt in 0..RECORDING_STOP_ATTEMPTS {
            match Self::send_recording_segment_stop_once(dialog, label, unique_id).await {
                Ok(()) => return Ok(()),
                Err(error) if attempt + 1 == RECORDING_STOP_ATTEMPTS => return Err(error),
                Err(_) => tokio::time::sleep(RECORDING_STOP_RETRY_DELAY).await,
            }
        }
        unreachable!("recording stop attempt budget is positive")
    }

    /// Address advertised for a finished local recording in RWI events. The
    /// CDR pipeline archives pipeline-generated WAVs into
    /// `{root}/{YYYYMMDD}[/{HH}]/` right after teardown (before the CDR row
    /// is persisted), so events must carry the final archived location.
    /// Mirrors `RecordingUploadHook::archive_local_artifacts`: same root and
    /// daily/hourly layout, date from the call start time; custom paths
    /// outside the root stay untouched. Falls back to the original path when
    /// no policy is active, the media type never archives (`http`/`sipflow`),
    /// or the start time is unknown — the renamer also leaves the file in
    /// place in every fallback, so the address stays valid.
    pub(crate) fn predicted_recording_event_path(&self, path: &str) -> String {
        let policy_guard = self.server.recording_policy.load();
        let Some(policy) = policy_guard.as_ref() else {
            return path.to_string();
        };
        match policy.effective_recording_type() {
            crate::config::RecordingType::Local | crate::config::RecordingType::S3 => {}
            crate::config::RecordingType::Http | crate::config::RecordingType::Sipflow => {
                return path.to_string();
            }
        }
        let Ok(start) = chrono::DateTime::parse_from_rfc3339(&self.context.created_at)
            .map(|t| t.with_timezone(&chrono::Utc))
        else {
            return path.to_string();
        };
        let subdir = crate::callrecord::RecordingSubdir::parse(policy.subdir.as_deref());
        crate::callrecord::predicted_archive_path(
            &policy.recorder_path(),
            Path::new(path),
            subdir,
            start,
        )
    }

    /// Stop the active recorder and publish the completed segment (CDR +
    /// RWI events). Returns the finalized segment path, or `Ok(None)` when
    /// no recorder was armed; each caller keeps its own log/command-result
    /// policy for `Err`.
    pub(crate) async fn stop_and_publish_recording(&mut self) -> Result<Option<String>> {
        match self.media.recording.stop_recording().await {
            Ok(Some(result)) => {
                let path = result.path.clone();
                self.publish_recording_complete(result);
                Ok(Some(path))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                // The CDR timeline must explain a missing/incomplete
                // recording — a failed stop often means the segment was not
                // finalized (no file, no upload, no CDR entry).
                self.record_trace(
                    crate::call_errors::TraceEvent::new(
                        crate::call_errors::TraceKind::Error,
                        format!("Recording failed to stop: {e}"),
                    )
                    .severity(crate::call_errors::ErrSeverity::Warn)
                    .code(crate::callrecord::recording_upload::recording_trace_codes::STOP_FAILED)
                    .detail(serde_json::json!({ "error": e.to_string() })),
                );
                Err(e)
            }
        }
    }

    pub(crate) fn publish_recording_complete(
        &mut self,
        result: crate::media::media_recorder::RecordingResult,
    ) {
        let (notify_app, unique_id) = match self.finalize_active_recording_segment(&result) {
            Some(pair) => pair,
            None => {
                // Discarded segment (e.g. outbound ringback slice on an
                // answered call): the file is already gone and no CDR entry
                // was registered — suppress every recording event.
                info!(
                    session_id = %self.id,
                    path = %result.path,
                    "Recording segment discarded; no events emitted"
                );
                return;
            }
        };
        let path = result.path;
        let duration = Duration::from_secs_f64(result.duration_secs);
        let file_size = result.file_size;
        info!(session_id = %self.id, path = %path, unique_id = ?unique_id, duration = ?duration, file_size, "Recording stopped");
        // Trace the finalized segment so the CDR timeline shows captured
        // media even when the upload pipeline fails later (missing
        // `recording.uploaded` next to this entry = upload-side problem).
        if let Some(segment) = self.recording.completed_segments.last() {
            self.record_trace(
                crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::Recording,
                    format!("Recording stopped ({})", segment.segment_type),
                )
                .severity(crate::call_errors::ErrSeverity::Info)
                .code(crate::callrecord::recording_upload::recording_trace_codes::STOPPED)
                .duration((result.duration_secs * 1000.0) as i64)
                .detail(serde_json::json!({
                    "path": path,
                    "duration_secs": result.duration_secs,
                    "size_bytes": file_size,
                    "segment_type": segment.segment_type,
                    "unique_id": unique_id,
                })),
            );
        }
        let info = crate::call::app::RecordingInfo {
            path,
            duration,
            size_bytes: file_size,
        };
        if notify_app {
            let _ = self.app_event_bridge.send_app_event(
                crate::call::app::ControllerEvent::RecordingComplete(info.clone()),
            );
        }
        // UAC sessions (RWI originates) produce no session-path CDR — the
        // originate task builds the call record from the gateway state. Mirror
        // the completed segment into the call's user_data so that CDR carries
        // `recorder[]` + `recording_segments` exactly like every session-path
        // call (docs contract: every completed segment is written to the CDR).
        // Session-path calls are excluded: their reporter already consumes
        // `completed_recording_segments`.
        if self.reporter.is_none()
            && let Some(segment) = self.recording.completed_segments.last()
            && let Some(gateway) = self.server.rwi_gateway.as_ref()
        {
            let call_id = self.context.session_id.clone();
            match serde_json::to_value(segment) {
                Ok(entry) if !entry.is_null() => {
                    let mut data = gateway.read().get_user_data(&call_id);
                    let slots = data
                        .entry("completed_recording_segments".to_string())
                        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                    if let serde_json::Value::Array(slots) = slots {
                        slots.push(entry);
                    }
                    if let Err(error) = gateway.write().set_user_data(&call_id, data) {
                        warn!(
                            session_id = %self.id,
                            path = %info.path,
                            %error,
                            "Failed to publish recording segment for the originate CDR"
                        );
                    }
                }
                _ => {
                    warn!(
                        session_id = %self.id,
                        path = %info.path,
                        "Failed to serialize recording segment for the originate CDR"
                    );
                }
            }
        }
        if let Some(gateway) = self.server.rwi_gateway.as_ref() {
            let call_id = self.context.session_id.clone();
            let meta = gateway.read().meta_store.get_sync(&call_id);
            let (caller_name, callee_name) = match meta {
                Some(ref meta) => (meta.caller_name.clone(), meta.callee_name.clone()),
                None => (None, None),
            };
            let reported_path = self.predicted_recording_event_path(&info.path);
            gateway.read().send_to_owner(&crate::rwi::RecordStopped {
                call_id: call_id.clone(),
                duration_secs: Some(info.duration.as_secs()),
                filename: Some(reported_path.clone()),
                unique_id,
                file_size: Some(info.size_bytes),
                download_url: Some(reported_path),
                caller_name,
                callee_name,
                called_phone: None,
                call_type: None,
                agent_id: None,
                agent_name: None,
                call_start_time: None,
                call_end_time: None,
                upload_time: None,
                switch_flag: None,
            });
        }
    }

    /// Called from the caller-hangup path: if an app is recording when the
    /// caller hangs up, finalize the recording and emit `RecordingComplete` so
    /// the running app's `on_record_complete` (e.g. voicemail message
    /// persistence) runs before the event loop is cancelled. Without this, a
    /// caller leaving a voicemail and hanging up would lose the message
    /// entirely — the primary voicemail use case.
    ///
    /// `on_record_complete` spawns its persistence work, so we only need to
    /// give the app event loop a brief, bounded window to execute it once.
    pub(crate) async fn finalize_recording_for_app_shutdown(&mut self) {
        if !self.media.recording.has_recorder_task() {
            return;
        }
        let completed = match self.stop_and_publish_recording().await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                warn!(
                    session_id = %self.id,
                    %error,
                    "Failed to finalize recording on caller hangup"
                );
                return;
            }
        };
        // Only grant the event loop a grace when an app is actually running to
        // receive the RecordingComplete we just emitted. Call-level auto
        // recording has no app, so we must not delay session teardown (which
        // would also delay CDR generation) in that case.
        if completed && self.app_runtime.is_running() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Append an event to the session's diagnostic trace timeline. `ts` is
    /// computed as milliseconds since the session started.
    pub fn record_trace(&mut self, event: crate::call_errors::TraceEvent) {
        let mut ev = event;
        ev.ts = self.context.start_time.elapsed().as_millis() as i64;
        self.meta.trace.push(ev);
    }

    /// Record the start of a media playback for the trace. The `track_id` is
    /// later passed to [`Self::record_play_end`] to emit a `Play` event with
    /// duration and interruption status.
    pub fn record_play_start(&mut self, track_id: impl Into<String>, source: impl Into<String>) {
        self.active_plays.insert(
            track_id.into(),
            crate::proxy::proxy_call::state::ActivePlay {
                source: source.into(),
                started_at: std::time::Instant::now(),
            },
        );
    }

    /// Record the end of a media playback: emits a `Play` trace event carrying
    /// the played duration and whether it was interrupted. Idempotent — a
    /// second call for the same `track_id` is a no-op.
    pub fn record_play_end(&mut self, track_id: &str, interrupted: bool) {
        let Some(play) = self.active_plays.remove(track_id) else {
            return;
        };
        let duration_ms = play.started_at.elapsed().as_millis() as i64;
        let message = format!(
            "Played {} · {} · {}",
            play.source,
            format_duration_ms(duration_ms),
            if interrupted {
                "interrupted"
            } else {
                "completed"
            }
        );
        self.record_trace(
            crate::call_errors::TraceEvent::new(crate::call_errors::TraceKind::Play, message)
                .duration(duration_ms)
                .interrupted(interrupted)
                .detail(serde_json::json!({ "source": play.source })),
        );
    }

    /// Record a leg lifecycle event into the CDR leg timeline (bounded —
    /// see [`LegTimeline::MAX_EVENTS`]).
    pub(crate) fn record_leg_event(
        &mut self,
        leg_id: &str,
        event_type: crate::callrecord::LegTimelineEventType,
        peer_leg_id: Option<String>,
        details: Option<serde_json::Value>,
    ) {
        self.meta
            .leg_timeline
            .add_event(leg_id.to_string(), event_type, peer_leg_id, details);
    }

    pub fn record_snapshot(&self) -> CallSessionRecordSnapshot {
        // Merge agent + routing data into a JSON map so the reporter picks it up.
        // Agent info was written into session extensions by CcCallSessionHook
        // (as a HashMap<String, String>). Routing metadata comes from the
        // dialplan extensions (also HashMap<String, String>). Values are JSON
        // so structured entries (e.g. the `trace` array) persist cleanly.
        let extensions = self.context.dialplan.extensions.clone();
        // Per-leg media counters, captured once: they feed both the CDR
        // `media_quality` metadata and the answered-but-silent-leg detection
        // below. Empty when no bridge legs exist.
        let legs: Vec<_> = [
            ("A", LegId::from("caller")),
            ("B", self.resolve_transfer_leg()),
        ]
        .into_iter()
        .filter_map(|(side, id)| {
            let peer = self.media_leg(&id)?;
            Some(peer.quality_report(side))
        })
        .collect();
        // Relay reconciliation: packets received on one leg but never emitted
        // on the peer leg. Only meaningful on same-codec relays (mirrors the
        // bridge monitor's `relay_mode` guard — transcode paths run at
        // different packet rates). `relay_drop == 0` on both legs is the
        // per-call "nothing dropped inside the bridge" receipt.
        let mut legs = legs;
        if legs.len() == 2 {
            let same_codec = match (legs[0].codec.as_ref(), legs[1].codec.as_ref()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if same_codec {
                let pick = |side: &str| {
                    legs.iter()
                        .find(|l| l.side == side)
                        .map(|l| (l.ingress_packets, l.egress_packets))
                        .unwrap_or((0, 0))
                };
                let (a_ing, a_eg) = pick("A");
                let (b_ing, b_eg) = pick("B");
                for leg in legs.iter_mut() {
                    leg.relay_drop = Some(if leg.side == "A" {
                        a_ing.saturating_sub(b_eg)
                    } else {
                        b_ing.saturating_sub(a_eg)
                    });
                }
            }
        }
        // Answered but a leg never delivered a single media packet — the
        // "silent leg" (browser ICE/DTLS never completed, one-way NAT/UDP
        // filtering, muted softphone, carrier answering without media).
        // Diagnostic only: never changes the hangup outcome; logged once and
        // annotated into the CDR (error chip + trace) so recordings without
        // audio are explainable. Skipped when a more specific error already
        // claimed the call (e.g. the RTP watchdog's `proxy.rtp_timeout`).
        // The callee side is only checked when a real remote callee leg
        // answered (`ever_connected_callee`): IVR/app playback legs never
        // receive remote RTP, so flagging them would be noise.
        let leg_silent = |side: &str| {
            legs.iter().any(|leg| {
                leg.side == side && leg.transport_rx_packets == 0 && leg.ingress_packets == 0
            })
        };
        let caller_silent = leg_silent("A");
        let callee_silent = self.meta.ever_connected_callee && leg_silent("B");
        let media_issue_legs = match (caller_silent, callee_silent) {
            (true, true) => Some("caller+callee"),
            (true, false) => Some("caller"),
            (false, true) => Some("callee"),
            (false, false) => None,
        };
        let media_issue = self.meta.answer_time.is_some()
            && self.meta.error_code.is_none()
            && media_issue_legs.is_some();
        if media_issue {
            let legs_str = media_issue_legs.unwrap_or_default();
            let callee_rx = legs
                .iter()
                .find(|leg| leg.side == "B")
                .map(|leg| leg.transport_rx_packets);
            warn!(
                session_id = %self.context.session_id,
                call_id = %self.context.session_id,
                legs = legs_str,
                caller_rx_packets = 0u64,
                callee_rx_packets = callee_rx.unwrap_or(0),
                "answered call ended with no media on leg(s); recording will have no audio from the affected side(s) (code=proxy.leg_media_incomplete)"
            );
        }
        // The effective error code: the detected leg-media issue applies only
        // when no more specific error already claimed the call.
        let effective_error_code = if media_issue {
            Some(&crate::proxy::proxy_call::error_catalog::LEG_MEDIA_INCOMPLETE)
        } else {
            self.meta.error_code
        };
        let metadata = {
            // Start with session extensions (CC agent info)
            let mut meta: std::collections::HashMap<String, serde_json::Value> = self
                .extensions
                .read()
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            // Merge in dialplan extensions (routing metadata)
            if let Some(route_meta) = extensions
                .get::<std::collections::HashMap<String, String>>()
                .cloned()
            {
                for (k, v) in route_meta {
                    meta.entry(k)
                        .or_insert_with(|| serde_json::Value::String(v));
                }
            }
            if let Some(ref qn) = self.meta.queue_name {
                meta.insert(
                    "queue_name".to_string(),
                    serde_json::Value::String(qn.clone()),
                );
            }
            if let Some(info) = effective_error_code {
                meta.insert(
                    "error_code".to_string(),
                    serde_json::Value::String(info.code.to_string()),
                );
                if let Some(app) = info.code.split('.').next() {
                    meta.insert(
                        "error_app".to_string(),
                        serde_json::Value::String(app.to_string()),
                    );
                }
            }
            // Which leg(s) lacked media + a human-readable detail line. The
            // reporter's error enrichment prefers `error_detail` over the
            // catalog's generic message, so the UI chip names the side too;
            // `mediaIssueLegs` stays a flat string for queryability.
            if media_issue {
                if let Some(legs_str) = media_issue_legs {
                    meta.insert(
                        "mediaIssueLegs".to_string(),
                        serde_json::Value::String(legs_str.to_string()),
                    );
                }
                let detail = match media_issue_legs {
                    Some("caller+callee") => "no media from the caller and callee legs",
                    Some("callee") => "no media from the callee leg",
                    _ => "no media from the caller leg",
                };
                meta.insert(
                    "error_detail".to_string(),
                    serde_json::Value::String(detail.to_string()),
                );
            }
            if let Some(ref app) = self.meta.app_name {
                meta.insert(
                    "app_name".to_string(),
                    serde_json::Value::String(app.clone()),
                );
            }
            if let Some(ref ql) = self.meta.queue_label {
                meta.insert(
                    "queue_label".to_string(),
                    serde_json::Value::String(ql.clone()),
                );
            }
            if let Some(sg) = self.meta.skill_group_id.as_ref().filter(|s| !s.is_empty()) {
                meta.insert(
                    "skill_group_id".to_string(),
                    serde_json::Value::String(sg.clone()),
                );
            }
            if let Some(ref callee) = self.meta.connected_callee {
                meta.insert(
                    "connected_callee".to_string(),
                    serde_json::Value::String(callee.clone()),
                );
            }
            if let Some(side) = self.meta.rtp_timeout_side {
                let side_str = match side {
                    crate::call::domain::RtpTimeoutSide::Caller => "caller",
                    crate::call::domain::RtpTimeoutSide::Callee => "callee",
                };
                meta.insert(
                    "rtpTimeoutSide".to_string(),
                    serde_json::Value::String(side_str.to_string()),
                );
            }
            // AMD verdict (answering machine / AI assistant detection), when
            // the resolved `amd` config enabled detection. Mirrored into
            // metadata so the DB JSON column stays queryable without a schema
            // migration; the same verdict also rides `details.amd`.
            if let Some(ref verdict) = self.amd_verdict {
                if let Ok(value) = serde_json::to_value(verdict) {
                    meta.insert("amd".to_string(), value);
                }
            }
            if let Some(leg) = self.meta.rtp_timeout_leg.clone() {
                meta.insert("rtpTimeoutLeg".to_string(), serde_json::Value::String(leg));
            }
            // Call trace: ordered timeline of transitions + media plays +
            // terminal outcome. Stored as a real JSON array under `trace`.
            let mut trace: Vec<crate::call_errors::TraceEvent> = self.meta.trace.clone();
            // One-shot media-issue event so the timeline explains the missing
            // audio next to the terminal outcome.
            if media_issue {
                let detail = serde_json::json!({ "legs": media_issue_legs });
                let mut ev = crate::call_errors::TraceEvent::new(
                    crate::call_errors::TraceKind::MediaIssue,
                    "No media received on leg(s) of an answered call (0 RTP packets)",
                )
                .severity(crate::call_errors::ErrSeverity::Warn)
                .code(crate::proxy::proxy_call::error_catalog::LEG_MEDIA_INCOMPLETE.code)
                .detail(detail);
                ev.ts = self.context.start_time.elapsed().as_millis() as i64;
                trace.push(ev);
            }
            // Terminal End event — carries the hangup initiator and any
            // standardized error code so "why did the call end" is visible.
            {
                let queue_ctx =
                    crate::proxy::proxy_call::call_meta::effective_queue_name(&self.meta)
                        .map(|q| format!(" (queue '{}')", q));
                let (severity, code, msg) = if let Some(info) = effective_error_code {
                    let base = format!("Call ended: {}", info.message);
                    (
                        info.severity,
                        Some(info.code.to_string()),
                        match &queue_ctx {
                            Some(ctx) => format!("{base}{ctx}"),
                            None => base,
                        },
                    )
                } else {
                    let reason = self
                        .meta
                        .hangup_reason
                        .as_ref()
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let base = format!("Call ended: {}", reason);
                    (
                        crate::call_errors::ErrSeverity::Info,
                        None,
                        match &queue_ctx {
                            Some(ctx) => format!("{base}{ctx}"),
                            None => base,
                        },
                    )
                };
                let end =
                    crate::call_errors::TraceEvent::new(crate::call_errors::TraceKind::End, msg)
                        .severity(severity);
                let end = match code {
                    Some(c) => end.code(&c),
                    None => end,
                };
                // The End event is pushed directly (not via record_trace), so
                // stamp its timestamp explicitly or it would render as "+0ms".
                let mut end = end;
                end.ts = self.context.start_time.elapsed().as_millis() as i64;
                trace.push(end);
            }
            if !trace.is_empty() {
                if let Ok(arr) = serde_json::to_value(&trace) {
                    meta.insert("trace".to_string(), arr);
                }
            }
            // Effective dialplan parameters (media/RTP/WebRTC/recording
            // config) so the CDR explains which setup governed the call.
            // Complements the `dialplan` timeline entry seeded at session
            // construction.
            meta.insert(
                "dialplan".to_string(),
                self.context.dialplan.snapshot_json(),
            );
            // Transfer marker: lets consumers (CSAT suppression, reports)
            // spot transferred calls without parsing the leg timeline.
            if self.meta.transferred {
                meta.insert("transferred".to_string(), serde_json::Value::Bool(true));
            }
            // Who hung up + the normalized hangup reason, so the CC call-history
            // UI can display and filter by them.
            match self.meta.hangup_reason.as_ref() {
                Some(reason) => {
                    meta.insert(
                        "hangup_reason".to_string(),
                        serde_json::Value::String(reason.to_string()),
                    );
                    meta.insert(
                        "hangup_by".to_string(),
                        serde_json::Value::String(reason.initiator().to_string()),
                    );
                }
                None => {
                    meta.insert(
                        "hangup_reason".to_string(),
                        serde_json::Value::String("unknown".to_string()),
                    );
                    meta.insert(
                        "hangup_by".to_string(),
                        serde_json::Value::String("unknown".to_string()),
                    );
                }
            }
            // Session user data (set via REST `PUT .../userdata` or the RWI
            // `call.set_userdata` command) rides into the CDR under a
            // namespaced `user_data` key so it cannot collide with the flat
            // system keys above. Safe to read here: the RwiCallRecordGuard
            // keeps gateway state alive until the record has been built.
            if let Some(ref gw) = self.server.rwi_gateway {
                let data = gw.read().get_user_data(&self.context.session_id);
                if !data.is_empty() {
                    meta.insert("user_data".to_string(), serde_json::Value::Object(data));
                }
            }
            meta
        };

        let media_quality = if legs.is_empty() {
            None
        } else {
            serde_json::to_value(&legs).ok()
        };

        CallSessionRecordSnapshot {
            ring_time: self.meta.ring_time,
            answer_time: self.meta.answer_time,
            last_error: self.meta.last_error.clone(),
            root_session_id: self.meta.root_session_id.clone(),
            invite_final_status: self.meta.invite_final_status,
            hangup_reason: self.meta.hangup_reason.clone(),
            hangup_messages: self.recorded_hangup_messages(),
            original_caller: Some(self.context.original_caller.clone()),
            original_callee: Some(self.context.original_callee.clone()),
            routed_caller: self.meta.routed_caller.clone(),
            routed_callee: self.meta.routed_callee.clone(),
            connected_callee: self.meta.connected_callee.clone(),
            routed_contact: self.meta.routed_contact.clone(),
            routed_destination: self.meta.routed_destination.clone(),
            callee_peer: self.meta.callee_peer.clone(),
            last_queue_name: self.meta.queue_name.clone(),
            callee_call_ids: self.meta.callee_call_ids.iter().cloned().collect(),
            server_dialog_id: self.caller_dialog_id(),
            transferred: self.meta.transferred,
            leg_timeline: self.meta.leg_timeline.clone(),
            metadata,
            amd: self.amd_verdict.clone(),
            media_quality,
            recording_segments: self.recording.completed_segments.clone(),
            extensions,
        }
    }

    pub(crate) fn recorded_hangup_messages(&self) -> Vec<CallRecordHangupMessage> {
        self.meta
            .hangup_messages
            .iter()
            .map(CallRecordHangupMessage::from)
            .collect()
    }

    /// Bridged audio is already in the caller's negotiated format. Disable
    /// every other tap, including the previously selected transfer peer.
    pub(crate) fn restore_recording_routes(&self) {
        for (id, _) in self.legs.iter() {
            if id.as_str() != "caller"
                && let Some(leg) = self.media_leg(id)
            {
                leg.ingress_tap().set_capture_route(None, None);
            }
        }
        if let Some(caller) = self.media_leg(&LegId::from("caller")) {
            caller.ingress_tap().set_capture_route(
                Some(crate::media::media_recorder::CaptureChannel::Caller),
                Some(crate::media::media_recorder::CaptureChannel::Callee),
            );
        }
    }
}
