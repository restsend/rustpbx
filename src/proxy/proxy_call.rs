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
pub(crate) mod dtmf;
pub(crate) mod leg_registry;
pub(crate) mod media_peer;
pub(crate) mod media_state;
pub(crate) mod reporter;
pub(crate) mod session_hooks;
pub(crate) mod session_timer;
pub(crate) mod sip_session;
pub(crate) mod state;

#[cfg(test)]
pub(crate) mod test_util;

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
        #[cfg(feature = "addon-wholesale")]
        let mut dialplan = self.dialplan;
        #[cfg(not(feature = "addon-wholesale"))]
        let dialplan = self.dialplan;
        #[cfg(feature = "addon-wholesale")]
        let wholesale_tenant_concurrency_hold =
            dialplan.wholesale_tenant_concurrency_hold.take();
        let dialplan = Arc::new(dialplan);
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
            original_caller,
            original_callee,
            max_forwards: self.max_forwards,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: dialplan
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
                }),
        };

        let result =
            SipSession::serve(server, context, tx, cancel_token, self.call_record_sender).await;
        #[cfg(feature = "addon-wholesale")]
        drop(wholesale_tenant_concurrency_hold);
        result
    }

    pub fn report_failure(
        self,
        server: SipServerRef,
        code: rsipstack::sip::StatusCode,
        reason: Option<String>,
    ) -> Result<()> {
        let CallSessionBuilder {
            cookie,
            #[cfg(feature = "addon-wholesale")]
            mut dialplan,
            #[cfg(not(feature = "addon-wholesale"))]
            dialplan,
            call_record_sender,
            ..
        } = self;

        #[cfg(feature = "addon-wholesale")]
        {
            if let Some(ctx) = cookie
                .get_extension::<crate::addons::wholesale::route::WholesaleBillingContext>()
            {
                dialplan.extensions.insert(ctx);
            }
        }

        let dialplan = Arc::new(dialplan);
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
            cookie,
            start_time: Instant::now(),
            original_caller: original_caller.clone(),
            original_callee: original_callee.clone(),
            max_forwards: 70,
            created_at: chrono::Utc::now().to_rfc3339(),
            metadata: None,
        };

        let reporter = crate::proxy::proxy_call::reporter::CallReporter {
            server,
            context,
            call_record_sender,
        };

        let snapshot = crate::proxy::proxy_call::state::CallSessionRecordSnapshot {
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
            callee_call_ids: vec![],
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

#[cfg(all(test, feature = "addon-wholesale"))]
mod tests {
    use super::*;
    use crate::call::DialDirection;
    use crate::proxy::tests::common::{create_test_request, create_test_server};

    #[tokio::test]
    async fn report_failure_copies_wholesale_context_to_record_extensions() {
        use crate::addons::wholesale::route::WholesaleBillingContext;

        let (server, _) = create_test_server().await;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let request = create_test_request(
            rsipstack::sip::Method::Invite,
            "alice",
            None,
            "rustpbx.com",
            None,
        );
        let dialplan = crate::call::Dialplan::new(
            "report-failure-tenant".to_string(),
            request,
            DialDirection::Inbound,
        );
        let cookie = crate::call::TransactionCookie::default();
        cookie.insert_extension(WholesaleBillingContext {
            tenant_id: 42,
            ..Default::default()
        });

        CallSessionBuilder::new(cookie, dialplan, 70)
            .with_call_record_sender(Some(sender))
            .report_failure(
                server,
                rsipstack::sip::StatusCode::ServiceUnavailable,
                Some("route failed".to_string()),
            )
            .expect("report failure");

        let record = receiver.recv().await.expect("call record");
        let ctx = record
            .extensions
            .get::<WholesaleBillingContext>()
            .expect("wholesale context");
        assert_eq!(ctx.tenant_id, 42);
        assert_eq!(ctx.carrier_id, None);
    }
}
