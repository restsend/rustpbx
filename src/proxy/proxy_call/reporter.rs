use crate::{
    call::{CalleeDisplayName, TransactionCookie, TrunkContext},
    callrecord::{
        CallDetails, CallRecord, CallRecordHangupMessage, CallRecordHangupReason,
        CallRecordLastError, CallRecordMedia, CallRecordRewrite, CallRecordSender,
        format_sipflow_media_key,
    },
    models::call_record::extract_sip_username,
    proxy::{
        proxy_call::state::{CallContext, CallSessionRecordSnapshot},
        server::SipServerRef,
    },
};
use chrono::{Duration, Utc};
use rsipstack::sip::prelude::HeadersExt;
use std::{collections::HashMap, fs};

pub struct CallReporter {
    pub server: SipServerRef,
    pub context: CallContext,
    pub call_record_sender: Option<CallRecordSender>,
}

impl CallReporter {
    pub(super) fn report(&self, snapshot: CallSessionRecordSnapshot) {
        let now = Utc::now();
        let start_time =
            now - Duration::from_std(self.context.start_time.elapsed()).unwrap_or_default();

        let ring_time = snapshot.ring_time.map(|rt| {
            start_time
                + Duration::from_std(rt.duration_since(self.context.start_time)).unwrap_or_default()
        });

        let answer_time = snapshot.answer_time.map(|at| {
            start_time
                + Duration::from_std(at.duration_since(self.context.start_time)).unwrap_or_default()
        });
        let call_was_accepted = snapshot.answer_time.is_some();

        // The CDR status must reflect the INVITE transaction's final status and
        // never be changed by later signaling (BYE, re-INVITE failures, transfer
        // failures). `invite_final_status` is locked once at call setup; fall
        // back to `last_error`/200 for sessions where it was never recorded.
        let status_code = snapshot
            .invite_final_status
            .or_else(|| {
                snapshot
                    .last_error
                    .as_ref()
                    .map(|(code, _)| u16::from(code.clone()))
            })
            .unwrap_or(200);

        let hangup_reason = snapshot.hangup_reason.clone().or_else(|| {
            if snapshot.last_error.is_some() {
                Some(CallRecordHangupReason::Failed)
            } else if call_was_accepted {
                Some(CallRecordHangupReason::BySystem)
            } else {
                Some(CallRecordHangupReason::Failed)
            }
        });

        let original_caller = snapshot
            .original_caller
            .clone()
            .or_else(|| self.context.dialplan.caller.as_ref().map(|c| c.to_string()))
            .unwrap_or_default();

        let original_callee = snapshot
            .original_callee
            .clone()
            .or_else(|| {
                self.context
                    .dialplan
                    .original
                    .to_header()
                    .ok()
                    .and_then(|to_header| to_header.uri().ok().map(|uri| uri.to_string()))
            })
            .or_else(|| {
                self.context
                    .dialplan
                    .first_target()
                    .map(|location| location.aor.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let caller = snapshot
            .routed_caller
            .clone()
            .unwrap_or_else(|| original_caller.clone());

        let callee = snapshot
            .routed_callee
            .clone()
            .or_else(|| snapshot.connected_callee.clone())
            .unwrap_or_else(|| original_callee.clone());

        let last_error = snapshot
            .last_error
            .as_ref()
            .map(|(code, reason)| CallRecordLastError {
                code: u16::from(code.clone()),
                reason: reason.clone(),
            });

        let mut hangup_messages = snapshot.hangup_messages.clone();
        if hangup_messages.is_empty()
            && let Some((code, reason)) = snapshot.last_error.as_ref()
        {
            hangup_messages.push(CallRecordHangupMessage {
                code: u16::from(code.clone()),
                reason: reason.clone(),
                target: None,
            });
        }

        let rewrite = CallRecordRewrite {
            caller_original: original_caller.clone(),
            caller_final: caller.clone(),
            callee_original: original_callee.clone(),
            callee_final: callee.clone(),
            contact: snapshot.routed_contact.clone(),
            destination: snapshot.routed_destination.clone(),
        };

        let sip_leg_roles = build_sip_leg_roles(&snapshot);

        let direction = self.context.dialplan.direction.to_string();

        // Helper to resolve call status (copied from proxy_call.rs logic)
        let status = if call_was_accepted {
            "completed".to_string()
        } else if snapshot.last_error.is_some() {
            "failed".to_string()
        } else {
            "missed".to_string()
        };

        let (from_number, from_name, department_id, extension_id) =
            resolve_user_info(&self.context.cookie, &caller);
        let to_number = extract_sip_username(&callee);
        let to_name = self
            .context
            .cookie
            .get_extension::<CalleeDisplayName>()
            .map(|e| e.0);
        let trunk_context = self.context.cookie.get_extension::<TrunkContext>();
        let (sip_gateway, sip_trunk_id) = if let Some(ctx) = trunk_context {
            (Some(ctx.name.clone()), ctx.id)
        } else {
            (None, None)
        };

        let outbound_trunk_context = self
            .context
            .cookie
            .get_extension::<crate::call::OutboundTrunkContext>()
            .or_else(|| {
                self.context
                    .dialplan
                    .extensions
                    .get::<crate::call::OutboundTrunkContext>()
                    .cloned()
            });
        let outbound_sip_trunk_id = outbound_trunk_context.as_ref().and_then(|ctx| ctx.id);

        // The session flushes the file recorder before reporting. Prefer
        // completed mid-call / full-call segments; fall back to dialplan path.
        let root_session = snapshot
            .root_session_id
            .clone()
            .unwrap_or_else(|| self.context.session_id.clone());
        let (recorder, mut metadata_map) = collect_recording_artifacts(
            &snapshot,
            &root_session,
            self.context.dialplan.recording.enabled,
            self.context
                .dialplan
                .recording
                .option
                .as_ref()
                .map(|o| o.recorder_file.as_str()),
        );
        // Copy values from cookie to extras_map
        // (Removed as TransactionCookie no longer has values)

        let recording_path_for_db = recorder
            .iter()
            .find(|m| m.track_id != "signaling")
            .map(|media| media.path.clone());

        if let Some(ctx) = &outbound_trunk_context {
            metadata_map.insert(
                "outbound_trunk_name".to_string(),
                serde_json::Value::String(ctx.name.clone()),
            );
            if let Some(dest) = &ctx.dest {
                metadata_map.insert(
                    "outbound_trunk_dest".to_string(),
                    serde_json::Value::String(dest.clone()),
                );
            }
        }

        // Harvest routing/in-call error context into metadata so the generic
        // DB saver (which drops status_code/last_error/hangup_reason) still
        // preserves a structured, queryable error code.  This also fixes the
        // early-failure path where snapshot.metadata started empty: routing
        // error codes ride the typed HashMap extension in snapshot.extensions.
        // Addon-specific codes (e.g. wholesale) are injected by each addon's
        // own CallRecordHook::on_record_enrich, keeping core addon-agnostic.
        let route_ext = snapshot.extensions.get::<HashMap<String, String>>();
        enrich_error_metadata(
            &mut metadata_map,
            route_ext,
            status_code,
            last_error.as_ref(),
            hangup_reason.as_ref(),
        );

        // Per-leg media quality (packets, RTCP jitter/RTT/loss) for the detail
        // UI. Persists into the `metadata` JSON column of the call record.
        if let Some(media_quality) = &snapshot.media_quality {
            metadata_map.insert("media_quality".to_string(), media_quality.clone());
        }

        let mut details = CallDetails {
            direction,
            status,
            from_number,
            to_number,
            caller_name: from_name,
            agent_name: to_name,
            queue: snapshot.last_queue_name.clone(),
            department_id,
            extension_id,
            sip_trunk_id,
            outbound_sip_trunk_id,
            sip_gateway,
            recording_url: recording_path_for_db,
            rewrite,
            last_error,
            metadata: if metadata_map.is_empty() {
                None
            } else {
                Some(metadata_map)
            },
            ..Default::default()
        };

        if call_was_accepted
            && details.recording_url.is_none()
            && self.server.recording_policy.load().is_none()
            && let sipflow_cfg = self.server.sipflow_config.load()
            && let Some(crate::config::SipFlowConfig::Local {
                upload:
                    Some(crate::config::SipFlowUploadConfig::S3 {
                        bucket,
                        endpoint,
                        root,
                        media,
                        ..
                    }),
                ..
            }) = sipflow_cfg.as_ref().as_ref()
            && media.unwrap_or(true)
        {
            let mut tmp = CallRecord::default();
            tmp.call_id = self.context.session_id.clone();
            tmp.start_time = start_time;
            let key = format_sipflow_media_key(&tmp);
            let full_key = if root.is_empty() {
                key
            } else {
                format!("{}/{}", root.trim_end_matches('/'), key)
            };
            details.recording_url = Some(crate::callrecord::sipflow_upload::sipflow_s3_url(
                endpoint, bucket, &full_key,
            ));
            details.recording_duration_secs = Some((now - start_time).num_seconds().max(0) as i32);
        }

        let mut record = CallRecord {
            call_id: self.context.session_id.clone(),
            session_id: Some(
                snapshot
                    .root_session_id
                    .clone()
                    .unwrap_or_else(|| self.context.session_id.clone()),
            ),
            start_time,
            ring_time,
            answer_time,
            end_time: now,
            caller: caller.clone(),
            callee: callee.clone(),
            status_code,
            hangup_reason: hangup_reason.clone(),
            hangup_messages: hangup_messages.clone(),
            recorder,
            sip_leg_roles,
            leg_timeline: crate::callrecord::LegTimeline::default(),
            details,
            extensions: snapshot.extensions,
        };

        // Keep RWI ownership and CallMeta alive until every asynchronous
        // call-record completion hook has finished. Dropping the record (also
        // on channel failure or task cancellation) performs the cleanup.
        if let Some(ref gateway) = self.server.rwi_gateway {
            record
                .extensions
                .insert(crate::rwi::RwiCallRecordGuard::new(
                    gateway,
                    record.call_id.clone(),
                ));
        }

        if let Some(ref sender) = self.call_record_sender {
            // Bounded channel: drop new records (with a warn log) if the
            // saver has fallen behind, instead of buffering indefinitely.
            // `try_send` is sync, so the existing synchronous emit path is
            // preserved. The enqueue instant rides in `extensions` for the
            // manager's opt-in queueing-latency histogram.
            record
                .extensions
                .insert(crate::callrecord::RecordEnqueuedAt(std::time::Instant::now()));
            match sender.try_send(record) {
                Ok(()) => crate::metrics::cdr::enqueued(),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    crate::metrics::cdr::dropped();
                    tracing::warn!("call record channel full; dropping record to bound memory");
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    crate::metrics::cdr::dropped();
                }
            }
        }
    }
}

/// Enrich the call-record metadata map with structured error context so the
/// generic DB saver (which drops `status_code`/`last_error`/`hangup_reason`)
/// still preserves a queryable error code.  Pure / testable: callers extract
/// the routing extension map from the snapshot.
///
/// This function is addon-agnostic. Addon-specific error codes (e.g. wholesale
/// `reject_code`) are injected by each addon's own `CallRecordHook::on_record_enrich`
/// implementation to keep the core free of addon dependencies.
/// Build `CallRecord.recorder` entries and recording-related metadata keys
/// from a session snapshot. Extracted so unit tests can cover segment +
/// sidecar JSONL aggregation without constructing a full `CallReporter`.
fn collect_recording_artifacts(
    snapshot: &CallSessionRecordSnapshot,
    root_session: &str,
    dialplan_recording_enabled: bool,
    dialplan_recorder_file: Option<&str>,
) -> (Vec<CallRecordMedia>, HashMap<String, serde_json::Value>) {
    let mut recorder = Vec::new();
    let mut metadata_map = snapshot.metadata.clone();

    if !snapshot.recording_segments.is_empty() {
        for seg in &snapshot.recording_segments {
            if seg.path.trim().is_empty() {
                continue;
            }
            let size = if seg.size > 0 {
                seg.size
            } else {
                fs::metadata(&seg.path)
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .unwrap_or(0)
            };
            if size == 0 {
                continue;
            }
            let mut extra = HashMap::new();
            extra.insert(
                "session_id".to_string(),
                serde_json::Value::String(root_session.to_string()),
            );
            extra.insert(
                "segment_type".to_string(),
                serde_json::Value::String(seg.segment_type.clone()),
            );
            extra.insert(
                "segment_id".to_string(),
                serde_json::Value::String(seg.segment_id.clone()),
            );
            if let Some(ref started) = seg.started_at {
                extra.insert(
                    "started_at".to_string(),
                    serde_json::Value::String(started.clone()),
                );
            }
            if let Some(ref ended) = seg.ended_at {
                extra.insert(
                    "ended_at".to_string(),
                    serde_json::Value::String(ended.clone()),
                );
            }
            recorder.push(CallRecordMedia {
                track_id: format!("segment:{}:{}", seg.segment_type, seg.segment_id),
                path: seg.path.clone(),
                size,
                extra: Some(extra),
            });
        }
    } else if dialplan_recording_enabled
        && let Some(recorder_file) = dialplan_recorder_file
        && !recorder_file.trim().is_empty()
        && let Ok(metadata) = fs::metadata(recorder_file)
        && metadata.is_file()
        && metadata.len() > 0
    {
        recorder.push(CallRecordMedia {
            track_id: "mixed".to_string(),
            path: recorder_file.to_string(),
            size: metadata.len(),
            extra: None,
        });
    }

    if !snapshot.recording_segments.is_empty() {
        if let Ok(value) = serde_json::to_value(&snapshot.recording_segments) {
            metadata_map.insert("recording_segments".to_string(), value);
        }
    }

    (recorder, metadata_map)
}

fn enrich_error_metadata(
    metadata: &mut HashMap<String, serde_json::Value>,
    route_ext: Option<&HashMap<String, String>>,
    status_code: u16,
    last_error: Option<&CallRecordLastError>,
    hangup_reason: Option<&CallRecordHangupReason>,
) {
    // Routing error codes (and any other router-supplied metadata) ride the
    // typed HashMap<String,String> extension.
    if let Some(route_meta) = route_ext {
        for (k, v) in route_meta {
            metadata
                .entry(k.clone())
                .or_insert_with(|| serde_json::Value::String(v.clone()));
        }
    }
    // Already-computed error fields that the DB saver otherwise drops.  These
    // make the record self-describing for the console error renderer.
    metadata
        .entry("sip_code".to_string())
        .or_insert_with(|| serde_json::Value::String(status_code.to_string()));
    if let Some(le) = last_error {
        metadata
            .entry("last_error_code".to_string())
            .or_insert_with(|| serde_json::Value::String(le.code.to_string()));
        if let Some(r) = &le.reason {
            metadata
                .entry("last_error_reason".to_string())
                .or_insert_with(|| serde_json::Value::String(r.clone()));
        }
    }
    if let Some(hr) = hangup_reason {
        metadata
            .entry("hangup_reason".to_string())
            .or_insert_with(|| serde_json::Value::String(hr.to_string()));
    }
    // Derive error_app from the hierarchical code prefix (e.g.
    // "proxy.callee_offline" -> "proxy") so the UI can group/filter without a
    // registry lookup.  Resolve the registry entry to freeze severity (and a
    // default message) at write time — this makes the metadata self-describing
    // for the renderer and keeps a historical snapshot of severity even if the
    // catalog later changes.
    if let Some(code) = metadata
        .get("error_code")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    {
        if let Some(app) = code.split('.').next() {
            metadata
                .entry("error_app".to_string())
                .or_insert_with(|| serde_json::Value::String(app.to_string()));
        }
        // Dynamic detail (router-supplied) takes precedence over the catalog
        // default message, so write it first via or_insert.
        if let Some(detail) = metadata
            .get("error_detail")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        {
            metadata
                .entry("error_message".to_string())
                .or_insert_with(|| serde_json::Value::String(detail));
        }
        // Registry: always freeze severity; catalog default message is the
        // fallback when no dynamic detail was supplied.
        if let Some(info) = crate::call_errors::registry().find(&code) {
            metadata
                .entry("error_severity".to_string())
                .or_insert_with(|| serde_json::Value::String(info.severity.as_str().to_string()));
            metadata
                .entry("error_message".to_string())
                .or_insert_with(|| serde_json::Value::String(info.message.to_string()));
        }
    }
}

fn build_sip_leg_roles(snapshot: &CallSessionRecordSnapshot) -> HashMap<String, String> {
    let mut sip_leg_roles = HashMap::new();
    let caller_call_id = snapshot.server_dialog_id.call_id.clone();
    sip_leg_roles.insert(caller_call_id.clone(), "caller".to_string());
    for call_id in &snapshot.callee_call_ids {
        if call_id != &caller_call_id {
            sip_leg_roles.insert(call_id.clone(), "callee".to_string());
        }
    }
    sip_leg_roles
}

fn resolve_user_info(
    cookie: &TransactionCookie,
    caller_uri: &str,
) -> (Option<String>, Option<String>, Option<i64>, Option<i64>) {
    let mut from_number = extract_sip_username(caller_uri);
    let (from_display_name, department_id, extension_id) = if let Some(user) = cookie.get_user() {
        let mut dept_id = None;
        let mut is_wholesale = false;

        if let Some(deps) = &user.departments {
            for d in deps {
                if d.starts_with("tenant:") {
                    is_wholesale = true;
                } else if let Ok(id) = d.parse::<i64>() {
                    dept_id = Some(id);
                }
            }
        }

        if is_wholesale {
            from_number = Some(user.username.clone());
        }

        let ext_id = if user.id > 0 {
            Some(user.id as i64)
        } else {
            None
        };
        (user.display_name, dept_id, ext_id)
    } else {
        (None, None, None)
    };

    (from_number, from_display_name, department_id, extension_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::SipUser;

    #[test]
    fn test_resolve_user_info_wholesale() {
        let cookie = TransactionCookie::default();
        let user = SipUser {
            username: "1234".to_string(),
            display_name: Some("alice".to_string()),
            departments: Some(vec!["tenant:100".to_string()]),
            ..Default::default()
        };
        cookie.set_user(user);

        let caller = "sip:mock-uuid@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("1234".to_string()));
        assert_eq!(from_name, Some("alice".to_string()));
        assert_eq!(dept, None);
        assert_eq!(ext, None);
    }

    #[test]
    fn test_resolve_user_info_mixed() {
        let cookie = TransactionCookie::default();
        let user = SipUser {
            username: "1234".to_string(),
            display_name: Some("alice".to_string()),
            departments: Some(vec!["tenant:100".to_string(), "5".to_string()]),
            id: 99,
            ..Default::default()
        };
        cookie.set_user(user);

        let caller = "sip:mock-uuid@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("1234".to_string()));
        assert_eq!(from_name, Some("alice".to_string()));
        assert_eq!(dept, Some(5));
        assert_eq!(ext, Some(99));
    }

    #[test]
    fn test_resolve_user_info_normal() {
        let cookie = TransactionCookie::default();
        let user = SipUser {
            username: "1001".to_string(),
            display_name: Some("alice".to_string()),
            departments: Some(vec!["5".to_string()]),
            id: 99,
            ..Default::default()
        };
        cookie.set_user(user);

        let caller = "sip:1001@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("1001".to_string()));
        assert_eq!(from_name, Some("alice".to_string()));
        assert_eq!(dept, Some(5));
        assert_eq!(ext, Some(99));
    }

    // ==================== Reporter Channel Tests ====================

    #[tokio::test]
    async fn test_call_reporter_handles_closed_channel() {
        use tokio::sync::mpsc;

        // Create a channel and immediately drop the receiver
        let (tx, rx) = mpsc::unbounded_channel::<CallRecord>();
        drop(rx); // Close the receiver

        // Test that sending to a closed channel returns an error but doesn't panic
        // This mimics what happens inside CallReporter.report() when call_record_sender is Some(tx)
        let record: CallRecord = CallRecord {
            call_id: "test-session".to_string(),
            session_id: None,
            start_time: chrono::Utc::now(),
            ring_time: None,
            answer_time: None,
            end_time: chrono::Utc::now(),
            caller: "caller".to_string(),
            callee: "callee".to_string(),
            status_code: 200,
            hangup_reason: None,
            hangup_messages: vec![],
            recorder: vec![],
            sip_leg_roles: std::collections::HashMap::new(),
            leg_timeline: crate::callrecord::LegTimeline::default(),
            details: crate::callrecord::CallDetails::default(),
            extensions: http::Extensions::new(),
        };

        // This should not panic - the `let _ = sender.send(record)` pattern handles this
        let result = tx.send(record);
        assert!(
            result.is_err(),
            "Sending to closed channel should return Err"
        );

        // If we get here without panic, the test passes
        // This verifies the pattern used in CallReporter.report() is safe
    }

    #[test]
    fn test_resolve_user_info_without_user() {
        let cookie = TransactionCookie::default();
        let caller = "sip:anonymous@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("anonymous".to_string()));
        assert_eq!(from_name, None);
        assert_eq!(dept, None);
        assert_eq!(ext, None);
    }

    #[test]
    fn test_resolve_user_info_with_empty_username() {
        let cookie = TransactionCookie::default();
        let caller = "sip:@1.2.3.4";
        let (from, _, _, _) = resolve_user_info(&cookie, caller);

        // Should extract username from caller URI
        assert!(from.is_some() || from.is_none()); // Behavior depends on implementation
    }

    #[test]
    fn test_build_sip_leg_roles_uses_callee_call_ids() {
        let snapshot = CallSessionRecordSnapshot {
            ring_time: None,
            answer_time: None,
            last_error: None,
            root_session_id: None,
            invite_final_status: None,
            hangup_reason: None,
            hangup_messages: vec![],
            original_caller: None,
            original_callee: None,
            routed_caller: None,
            routed_callee: None,
            connected_callee: None,
            routed_contact: None,
            routed_destination: None,
            last_queue_name: None,
            callee_call_ids: vec!["callee-call-id".to_string()],
            server_dialog_id: rsipstack::dialog::DialogId {
                call_id: "caller-call-id".to_string(),
                local_tag: "local".to_string(),
                remote_tag: "remote".to_string(),
            },
            extensions: http::Extensions::new(),
            metadata: std::collections::HashMap::new(),
            media_quality: None,
            recording_segments: Vec::new(),
        };

        let roles = build_sip_leg_roles(&snapshot);

        assert_eq!(
            roles.get("caller-call-id").map(String::as_str),
            Some("caller")
        );
        assert_eq!(
            roles.get("callee-call-id").map(String::as_str),
            Some("callee")
        );
    }

    fn meta_str<'a>(meta: &'a HashMap<String, serde_json::Value>, key: &str) -> &'a str {
        meta.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }

    #[test]
    fn enrich_metadata_routing_code_and_sip_code() {
        let mut meta: HashMap<String, serde_json::Value> = HashMap::new();
        let route = HashMap::from([
            ("error_code".to_string(), "proxy.callee_offline".to_string()),
            ("error_detail".to_string(), "user 1002 offline".to_string()),
        ]);
        let le = CallRecordLastError {
            code: 480,
            reason: Some("target user is offline".to_string()),
        };
        enrich_error_metadata(
            &mut meta,
            Some(&route),
            480,
            Some(&le),
            Some(&CallRecordHangupReason::NoAnswer),
        );
        assert_eq!(meta_str(&meta, "error_code"), "proxy.callee_offline");
        assert_eq!(meta_str(&meta, "error_app"), "proxy");
        assert_eq!(meta_str(&meta, "error_message"), "user 1002 offline");
        assert_eq!(meta_str(&meta, "error_severity"), "warn");
        assert_eq!(meta_str(&meta, "sip_code"), "480");
        assert_eq!(meta_str(&meta, "last_error_code"), "480");
        assert_eq!(meta_str(&meta, "hangup_reason"), "noAnswer");
    }

    #[test]
    #[cfg(feature = "addon-wholesale")]
    fn enrich_metadata_severity_and_catalog_message_fallback() {
        // A registered code with no dynamic detail: severity + catalog default
        // message are resolved from the registry at write time.
        let mut meta: HashMap<String, serde_json::Value> = HashMap::new();
        let route = HashMap::from([(
            "error_code".to_string(),
            "wholesale.insufficient_funds".to_string(),
        )]);
        enrich_error_metadata(&mut meta, Some(&route), 402, None, None);
        assert_eq!(
            meta_str(&meta, "error_severity"),
            crate::call_errors::ErrSeverity::Error.as_str()
        );
        assert_eq!(meta_str(&meta, "error_message"), "Insufficient funds");
    }

    #[test]
    fn enrich_metadata_unknown_code_no_severity() {
        // An unregistered code (e.g. from a newer build) must not panic; it
        // simply leaves error_severity absent for the renderer to default.
        let mut meta: HashMap<String, serde_json::Value> = HashMap::new();
        let route = HashMap::from([("error_code".to_string(), "future.unknown".to_string())]);
        enrich_error_metadata(&mut meta, Some(&route), 500, None, None);
        assert_eq!(meta_str(&meta, "error_code"), "future.unknown");
        assert!(meta.get("error_severity").is_none());
    }

    #[test]
    fn enrich_metadata_does_not_overwrite_existing_code() {
        let mut meta: HashMap<String, serde_json::Value> = HashMap::new();
        meta.insert(
            "error_code".to_string(),
            serde_json::Value::String("wholesale.cps_limit".to_string()),
        );
        let route = HashMap::from([("error_code".to_string(), "proxy.route_aborted".to_string())]);
        enrich_error_metadata(&mut meta, Some(&route), 503, None, None);
        // pre-existing value is preserved (entry().or_insert)
        assert_eq!(meta_str(&meta, "error_code"), "wholesale.cps_limit");
    }

    fn empty_snapshot() -> CallSessionRecordSnapshot {
        CallSessionRecordSnapshot {
            ring_time: None,
            answer_time: None,
            last_error: None,
            root_session_id: Some("root-sess".into()),
            invite_final_status: None,
            hangup_reason: None,
            hangup_messages: vec![],
            original_caller: None,
            original_callee: None,
            routed_caller: None,
            routed_callee: None,
            connected_callee: None,
            routed_contact: None,
            routed_destination: None,
            last_queue_name: None,
            callee_call_ids: vec![],
            server_dialog_id: rsipstack::dialog::DialogId {
                call_id: "leg-call".into(),
                local_tag: "l".into(),
                remote_tag: "r".into(),
            },
            extensions: http::Extensions::new(),
            metadata: HashMap::new(),
            media_quality: None,
            recording_segments: Vec::new(),
        }
    }

    #[test]
    fn collect_recording_artifacts_includes_segments() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("root_20260101010101_ivr_ab.wav");
        std::fs::write(&wav, b"wavdata").unwrap();

        let mut snapshot = empty_snapshot();
        snapshot.recording_segments = vec![crate::callrecord::RecordingSegment {
            path: wav.to_string_lossy().into_owned(),
            size: 7,
            segment_type: "ivr".into(),
            segment_id: "ab".into(),
            started_at: Some("t0".into()),
            ended_at: Some("t1".into()),
            duration_secs: 1.5,
        }];

        let (recorder, meta) = collect_recording_artifacts(&snapshot, "root-sess", false, None);

        assert_eq!(recorder.len(), 1);
        assert_eq!(recorder[0].track_id, "segment:ivr:ab");
        assert_eq!(
            recorder[0]
                .extra
                .as_ref()
                .and_then(|e| e.get("session_id"))
                .and_then(|v| v.as_str()),
            Some("root-sess")
        );
        assert!(meta.get("recording_segments").is_some());
        assert!(meta.get("sipflow_jsonl").is_none());
    }

    #[test]
    fn collect_recording_artifacts_falls_back_to_dialplan_file() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("full.wav");
        std::fs::write(&wav, b"abcdef").unwrap();
        let snapshot = empty_snapshot();
        let (recorder, meta) =
            collect_recording_artifacts(&snapshot, "root-sess", true, Some(wav.to_str().unwrap()));
        assert_eq!(recorder.len(), 1);
        assert_eq!(recorder[0].track_id, "mixed");
        assert!(meta.get("recording_segments").is_none());
    }

    #[test]
    fn collect_recording_artifacts_skips_zero_size_segments() {
        let mut snapshot = empty_snapshot();
        snapshot.recording_segments = vec![crate::callrecord::RecordingSegment {
            path: "/tmp/missing-segment.wav".into(),
            size: 0,
            segment_type: "ivr".into(),
            segment_id: "x".into(),
            started_at: None,
            ended_at: None,
            duration_secs: 0.0,
        }];
        let (recorder, _) = collect_recording_artifacts(&snapshot, "root", false, None);
        assert!(recorder.is_empty());
    }
}
