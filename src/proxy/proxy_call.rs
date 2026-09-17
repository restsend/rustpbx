use crate::{
    call::{Dialplan, TransactionCookie},
    callrecord::CallRecordSender,
    proxy::proxy_call::sip_session::SipSession,
    proxy::proxy_call::state::CallContext,
    proxy::server::SipServerRef,
};
use anyhow::Result;
use rsipstack::sip::prelude::HeadersExt;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) mod call_meta;
#[cfg(test)]
pub(crate) mod dtmf;
pub(crate) mod error_catalog;
pub(crate) mod ivr_exec_hook;
pub(crate) mod leg_registry;
pub(crate) mod media_state;
pub(crate) mod reporter;
pub mod session_hooks;
pub(crate) mod session_timer;
pub mod sip_session;
pub(crate) mod state;

/// Build a [`CallContext`] shared by both the live session path and the
/// early-failure (`report_failure`) path so the derived fields (session id,
/// timestamps) stay consistent.
fn build_call_context(
    dialplan: Arc<Dialplan>,
    cookie: TransactionCookie,
    max_forwards: u32,
    original_caller: String,
    original_callee: String,
    metadata: Option<std::collections::HashMap<String, String>>,
) -> CallContext {
    let session_id = dialplan
        .session_id
        .clone()
        .unwrap_or_else(crate::call::session_id::generate);
    CallContext {
        session_id,
        dialplan,
        cookie,
        start_time: Instant::now(),
        original_caller,
        original_callee,
        max_forwards,
        created_at: chrono::Utc::now().to_rfc3339(),
        metadata,
    }
}

pub struct CallSessionBuilder {
    cookie: TransactionCookie,
    dialplan: Dialplan,
    max_forwards: u32,
    cancel_token: Option<CancellationToken>,
    call_record_sender: Option<CallRecordSender>,
}

impl CallSessionBuilder {
    pub fn new(cookie: TransactionCookie, dialplan: Dialplan, max_forwards: u32) -> Self {
        Self {
            cookie,
            dialplan,
            max_forwards,
            cancel_token: None,
            call_record_sender: None,
        }
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_call_record_sender(mut self, sender: Option<CallRecordSender>) -> Self {
        self.call_record_sender = sender;
        self
    }

    pub async fn build_and_serve(
        self,
        server: SipServerRef,
        tx: &mut rsipstack::transaction::transaction::Transaction,
    ) -> Result<()> {
        let dialplan = self.dialplan;
        let dialplan = Arc::new(dialplan);
        let cancel_token = self.cancel_token.unwrap_or_default();

        let original_caller = dialplan
            .original
            .from_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .map(|u| u.to_string())
            .or_else(|| dialplan.caller.as_ref().map(|c| c.to_string()))
            .unwrap_or_default();
        let original_callee = dialplan
            .original
            .to_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .map(|u| u.to_string())
            .or_else(|| {
                dialplan
                    .first_target()
                    .map(|location| location.aor.to_string())
            })
            .unwrap_or_default();

        let metadata = dialplan
            .first_target()
            .and_then(|loc| loc.headers.as_ref())
            .and_then(|hdrs| {
                let meta: std::collections::HashMap<String, String> = hdrs
                    .iter()
                    .filter_map(|h| {
                        let name = h.name().to_string();
                        if name.starts_with("X-CRM-") || name.starts_with("X-CC-") {
                            Some((name, h.value().to_string()))
                        } else {
                            None
                        }
                    })
                    .collect();
                if meta.is_empty() { None } else { Some(meta) }
            });

        let context = build_call_context(
            dialplan,
            self.cookie,
            self.max_forwards,
            original_caller,
            original_callee,
            metadata,
        );

        SipSession::serve(server, context, tx, cancel_token, self.call_record_sender).await
    }

    pub fn report_failure(
        self,
        server: SipServerRef,
        code: rsipstack::sip::StatusCode,
        reason: Option<String>,
    ) -> Result<()> {
        let CallSessionBuilder {
            cookie,
            mut dialplan,
            call_record_sender,
            ..
        } = self;

        // Addon-owned cookie → dialplan promotion (e.g. wholesale billing on Abort).
        if let Some(reg) = server.addon_registry.as_ref() {
            reg.promote_cookie_extensions(&cookie, &mut dialplan);
        }

        let dialplan = Arc::new(dialplan);

        let original_caller = dialplan
            .original
            .from_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .map(|u| u.to_string())
            .unwrap_or_default();

        let original_callee = dialplan
            .original
            .to_header()
            .ok()
            .and_then(|h| h.uri().ok())
            .map(|u| u.to_string())
            .unwrap_or_default();

        let context = build_call_context(
            dialplan.clone(),
            cookie,
            70,
            original_caller.clone(),
            original_callee.clone(),
            None,
        );

        let rwi_gateway = server.rwi_gateway.clone();
        let reporter = crate::proxy::proxy_call::reporter::CallReporter {
            server,
            context,
            call_record_sender,
        };

        // Early/routing failures never create a SipSession, so record_snapshot
        // cannot run. Build a minimal diagnostic trace so these calls (480
        // offline, wholesale rejects, ACL/HTTP-router rejections, ...) also show
        // a trace entry. Severity is resolved from the standardized error code
        // carried in the routing extensions, defaulting to error.
        let mut metadata = std::collections::HashMap::new();
        let route_exts = dialplan
            .extensions
            .get::<std::collections::HashMap<String, String>>();
        let err_code = route_exts.and_then(|m| m.get("error_code").cloned());
        let err_detail = route_exts.and_then(|m| m.get("error_detail").cloned());
        let info = err_code
            .as_deref()
            .and_then(|c| crate::call_errors::registry().find(c))
            .unwrap_or(&crate::proxy::error_catalog::ROUTE_FAILED);

        // Unified log + `call_error` RWI event for this early failure. The
        // raw `error_code` string rides the trace/event even when it is not in
        // this build's registry (forward-compat records / addon codes).
        let session_id = dialplan.session_id.clone().unwrap_or_default();
        let detail = err_detail.clone().map(serde_json::Value::String);
        crate::call_errors::log_call_error(&session_id, "routing", info, detail.clone());
        if let Some(gw) = rwi_gateway.as_ref() {
            let mut ev =
                crate::rwi::CallError::from_info(&session_id, "routing", info, detail.clone());
            if let Some(c) = &err_code {
                ev.code = c.clone();
            }
            gw.read().broadcast(&ev);
        }

        let mut events: Vec<serde_json::Value> = Vec::new();
        // Unified error entry first (detailed, registry code + severity),
        // followed by the terminal `end` entry for continuity with existing
        // consumers/tests.
        if err_code.is_some() {
            let mut err_ev = crate::call_errors::call_error_trace(info, detail);
            if let Some(c) = &err_code {
                err_ev.code = Some(c.clone());
            }
            if let Ok(v) = serde_json::to_value(err_ev) {
                events.push(v);
            }
        }
        let mut end = crate::call_errors::TraceEvent::new(
            crate::call_errors::TraceKind::End,
            format!(
                "Call rejected: {}",
                reason.clone().unwrap_or_else(|| "unknown".to_string())
            ),
        )
        .severity(info.severity);
        if let Some(c) = &err_code {
            end = end.code(c);
        }
        if let Ok(v) = serde_json::to_value(end) {
            events.push(v);
        }
        metadata.insert("trace".to_string(), serde_json::Value::Array(events));

        let snapshot = crate::proxy::proxy_call::state::CallSessionRecordSnapshot {
            ring_time: None,
            answer_time: None,
            last_error: Some((code.clone(), reason)),
            root_session_id: None,
            invite_final_status: Some(u16::from(code)),
            hangup_reason: Some(crate::callrecord::CallRecordHangupReason::Failed),
            hangup_messages: vec![],
            // callee_hangup_reason: None,
            connected_callee: None,
            original_caller: Some(original_caller),
            original_callee: Some(original_callee),
            routed_caller: None,
            routed_callee: None,
            routed_contact: None,
            routed_destination: None,
            // Early failure: no B leg was ever dialed. The A-leg peer still
            // reaches the reporter via the CallerPeerContext cookie extension.
            callee_peer: None,
            last_queue_name: None,
            transferred: false,
            leg_timeline: crate::callrecord::LegTimeline::default(),
            callee_call_ids: vec![],
            server_dialog_id: rsipstack::dialog::DialogId {
                call_id: "".into(),
                local_tag: "".into(),
                remote_tag: "".into(),
            },
            extensions: dialplan.extensions.clone(),
            metadata,
            media_quality: None,
            recording_segments: Vec::new(),
        };

        reporter.report(snapshot);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::DialDirection;
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    #[tokio::test]
    async fn report_failure_records_trace_with_error_code() {
        // Early/routing failures (no SipSession) must still produce a trace so
        // the detail trace tab shows why the call was rejected. Severity + code
        // come from the routing extensions error_code seam.
        let (server, _) = create_test_server().await;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let mut dialplan = crate::call::Dialplan::new(
            "report-failure-trace".to_string(),
            request,
            DialDirection::Inbound,
        );
        let mut exts = std::collections::HashMap::new();
        exts.insert(
            "error_code".to_string(),
            "wholesale.insufficient_funds".to_string(),
        );
        dialplan = dialplan.with_extension(exts);
        let cookie = crate::call::TransactionCookie::default();

        CallSessionBuilder::new(cookie, dialplan, 70)
            .with_call_record_sender(Some(sender))
            .report_failure(
                server,
                rsipstack::sip::StatusCode::PaymentRequired,
                Some("Insufficient funds".to_string()),
            )
            .expect("report failure");

        let record = receiver.recv().await.expect("call record");
        let meta = record.details.metadata.as_ref().expect("metadata present");
        let trace = meta
            .get("trace")
            .and_then(|v| v.as_array())
            .expect("trace array present");
        // Unified error entry first (registry code + severity), then the
        // terminal `end` entry carrying the SIP reason.
        assert_eq!(trace[0]["kind"], "error");
        assert_eq!(trace[0]["severity"], "error");
        assert_eq!(trace[0]["code"], "wholesale.insufficient_funds");
        assert_eq!(trace[1]["kind"], "end");
        assert_eq!(trace[1]["severity"], "error");
        assert_eq!(trace[1]["code"], "wholesale.insufficient_funds");
        assert!(
            trace[1]["message"]
                .as_str()
                .unwrap()
                .contains("Insufficient funds")
        );
    }
}
