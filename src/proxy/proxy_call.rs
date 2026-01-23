use crate::{
    call::MediaConfig,
    call::{Dialplan, TransactionCookie},
    callrecord::CallRecordSender,
    proxy::proxy_call::session::CallSession,
    proxy::proxy_call::state::CallContext,
    proxy::server::SipServerRef,
};
use anyhow::Result;
use rsip::prelude::{HeadersExt, UntypedHeader};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub(crate) mod media_bridge;
pub(crate) mod media_peer;
pub(crate) mod reporter;
pub(crate) mod session;
pub(crate) mod session_timer;
pub(crate) mod state;

#[cfg(test)]
pub(crate) mod test_util;

#[cfg(test)]
mod callsession_tests;

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
        let dialplan = Arc::new(self.dialplan);
        let cancel_token = self.cancel_token.unwrap_or_default();
        let session_id = dialplan
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

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

        let context = CallContext {
            session_id,
            dialplan: dialplan.clone(),
            cookie: self.cookie,
            start_time: Instant::now(),
            media_config: MediaConfig {
                proxy_mode: server.proxy_config.media_proxy,
                external_ip: server.rtp_config.external_ip.clone(),
                rtp_start_port: server.rtp_config.start_port,
                rtp_end_port: server.rtp_config.end_port,
                ice_servers: server.rtp_config.ice_servers.clone(),
                enable_latching: server.proxy_config.enable_latching,
            },
            original_caller,
            original_callee,
            max_forwards: self.max_forwards,
        };

        CallSession::serve(server, context, tx, cancel_token, self.call_record_sender).await
    }

    pub fn report_failure(
        self,
        server: SipServerRef,
        code: rsip::StatusCode,
        reason: Option<String>,
    ) -> Result<()> {
        let dialplan = Arc::new(self.dialplan);
        let session_id = dialplan
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let original_caller = dialplan
            .original
            .from_header()
            .ok()
            .map(|t| t.value().to_string())
            .unwrap_or_default();

        let original_callee = dialplan
            .original
            .to_header()
            .ok()
            .map(|t| t.value().to_string())
            .unwrap_or_default();

        let context = CallContext {
            session_id,
            dialplan: dialplan.clone(),
            cookie: self.cookie,
            start_time: Instant::now(),
            media_config: MediaConfig {
                proxy_mode: server.proxy_config.media_proxy,
                external_ip: server.rtp_config.external_ip.clone(),
                rtp_start_port: server.rtp_config.start_port,
                rtp_end_port: server.rtp_config.end_port,
                ice_servers: server.rtp_config.ice_servers.clone(),
                enable_latching: server.proxy_config.enable_latching,
            },
            original_caller: original_caller.clone(),
            original_callee: original_callee.clone(),
            max_forwards: self.max_forwards,
        };

        let reporter = crate::proxy::proxy_call::reporter::CallReporter {
            server,
            context,
            call_record_sender: self.call_record_sender,
        };

        let snapshot = crate::proxy::proxy_call::session::CallSessionRecordSnapshot {
            ring_time: None,
            answer_time: None,
            last_error: Some((code, reason)),
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
            last_queue_name: None,
            callee_dialogs: vec![],
            server_dialog_id: rsipstack::dialog::DialogId {
                call_id: "".into(),
                local_tag: "".into(),
                remote_tag: "".into(),
            },
            extensions: dialplan.extensions.clone(),
        };

        reporter.report(snapshot);
        Ok(())
    }
}
