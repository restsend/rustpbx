use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::{
    callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender},
    config::ProxyConfig,
    handler::call::ActiveCallType,
    handler::CallOption,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rsip::prelude::{HeadersExt, ToTypedHeader, UntypedHeader};
use rsipstack::{dialog::DialogId, transaction::transaction::Transaction};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub(crate) struct CallSession {
    pub dialog_id: DialogId,
    pub call_id: String,
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub caller: String,
    pub callee: String,
    pub status_code: u16,
    pub call_option: CallOption,
}

impl CallSession {
    pub fn new(
        dialog_id: DialogId,
        call_id: String,
        caller: String,
        callee: String,
        call_option: CallOption,
    ) -> Self {
        Self {
            dialog_id,
            call_id,
            start_time: Utc::now(),
            ring_time: None,
            answer_time: None,
            caller,
            callee,
            status_code: 180, // Ringing as default
            call_option,
        }
    }

    pub fn to_call_record(&self, hangup_reason: CallRecordHangupReason) -> CallRecord {
        let end_time = Utc::now();

        CallRecord {
            call_type: ActiveCallType::Sip,
            option: Some(self.call_option.clone()),
            call_id: self.call_id.clone(),
            start_time: self.start_time,
            ring_time: self.ring_time,
            answer_time: self.answer_time,
            end_time,
            caller: self.caller.clone(),
            callee: self.callee.clone(),
            status_code: self.status_code,
            hangup_reason: Some(hangup_reason),
            recorder: vec![], // CDR module doesn't handle media recording
            extras: None,
            dump_event_file: None,
        }
    }
}

pub(crate) struct CdrModuleInner {
    pub callrecord_sender: Option<CallRecordSender>,
    pub active_sessions: Mutex<HashMap<String, CallSession>>, // key: call_id
    pub transaction_counter: Mutex<u64>,                      // For cleanup logic
}

#[derive(Clone)]
pub struct CdrModule {
    pub(crate) inner: Arc<CdrModuleInner>,
}

impl CdrModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CdrModule::new(server, config);
        Ok(Box::new(module))
    }

    pub fn new(server: SipServerRef, _config: Arc<ProxyConfig>) -> Self {
        let callrecord_sender = server.callrecord_sender.clone();
        let inner = Arc::new(CdrModuleInner {
            callrecord_sender,
            active_sessions: Mutex::new(HashMap::new()),
            transaction_counter: Mutex::new(0),
        });
        Self { inner }
    }

    fn extract_caller_callee_from_invite(&self, tx: &Transaction) -> Result<(String, String)> {
        let from = tx.original.from_header()?.typed()?;
        let to = tx.original.to_header()?.typed()?;

        let caller = if let Some(display_name) = from.display_name {
            display_name
        } else {
            from.uri.user().unwrap_or("unknown").to_string()
        };

        let callee = if let Some(display_name) = to.display_name {
            display_name
        } else {
            to.uri.user().unwrap_or("unknown").to_string()
        };

        Ok((caller, callee))
    }

    fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        let call_id = tx.original.call_id_header()?.value().to_string();

        // Try to create DialogId, but don't fail if we can't
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                debug!("Could not create DialogId from INVITE: {}", e);
                return Ok(()); // Continue processing, this might be a retransmission
            }
        };

        let (caller, callee) = match self.extract_caller_callee_from_invite(tx) {
            Ok((caller, callee)) => (caller, callee),
            Err(e) => {
                warn!("Failed to extract caller/callee from INVITE: {}", e);
                ("unknown".to_string(), "unknown".to_string())
            }
        };

        // Create a basic CallOption for SIP calls
        let call_option = CallOption {
            denoise: None,
            offer: None,
            callee: Some(callee.clone()),
            caller: Some(caller.clone()),
            recorder: None,
            vad: None,
            asr: None,
            tts: None,
            handshake_timeout: None,
            enable_ipv6: None,
            sip: None,
            extra: None,
            codec: None,
            eou: None,
        };

        let session = CallSession::new(dialog_id, call_id.clone(), caller, callee, call_option);

        let mut active_sessions = self.inner.active_sessions.lock().unwrap();
        active_sessions.insert(call_id.clone(), session);

        info!(
            "CDR: New call session started - Call-ID: {}, Caller: {}, Callee: {}",
            call_id,
            active_sessions.get(&call_id).unwrap().caller,
            active_sessions.get(&call_id).unwrap().callee
        );

        Ok(())
    }

    fn handle_response(&self, tx: &mut Transaction) -> Result<()> {
        let call_id = tx.original.call_id_header()?.value().to_string();

        if let Some(last_response) = &tx.last_response {
            let status_code = match last_response.status_code {
                rsip::StatusCode::Trying => 100,
                rsip::StatusCode::Ringing => 180,
                rsip::StatusCode::SessionProgress => 183,
                rsip::StatusCode::OK => 200,
                rsip::StatusCode::NotFound => 404,
                rsip::StatusCode::ServerInternalError => 500,
                rsip::StatusCode::BusyHere => 486,
                rsip::StatusCode::TemporarilyUnavailable => 480,
                _ => {
                    let code_str = format!("{}", last_response.status_code);
                    code_str.parse::<u16>().unwrap_or(500)
                }
            };

            let mut active_sessions = self.inner.active_sessions.lock().unwrap();
            if let Some(session) = active_sessions.get_mut(&call_id) {
                session.status_code = status_code;
                debug!(
                    "CDR: Updated call status - Call-ID: {}, Status: {}",
                    call_id, status_code
                );
            }
        }

        Ok(())
    }

    fn handle_bye(&self, tx: &mut Transaction, sender: &CallRecordSender) -> Result<()> {
        let call_id = tx.original.call_id_header()?.value().to_string();

        let mut active_sessions = self.inner.active_sessions.lock().unwrap();
        if let Some(session) = active_sessions.remove(&call_id) {
            let hangup_reason = CallRecordHangupReason::ByCaller; // Assume caller initiated BYE
            let call_record = session.to_call_record(hangup_reason);

            match sender.send(call_record.clone()) {
                Ok(_) => {
                    info!(
                        "CDR: Call record generated - Call-ID: {}, Duration: {}s, Caller: {}, Callee: {}",
                        call_record.call_id,
                        call_record.end_time.signed_duration_since(call_record.start_time).as_seconds_f32(),
                        call_record.caller,
                        call_record.callee
                    );
                }
                Err(e) => {
                    error!("CDR: Failed to send call record: {}", e);
                }
            }
        } else {
            debug!("CDR: BYE received for unknown call: {}", call_id);
        }

        Ok(())
    }

    fn handle_cancel(&self, tx: &mut Transaction, sender: &CallRecordSender) -> Result<()> {
        let call_id = tx.original.call_id_header()?.value().to_string();

        let mut active_sessions = self.inner.active_sessions.lock().unwrap();
        if let Some(session) = active_sessions.remove(&call_id) {
            let hangup_reason = CallRecordHangupReason::Canceled;
            let call_record = session.to_call_record(hangup_reason);

            match sender.send(call_record.clone()) {
                Ok(_) => {
                    info!(
                        "CDR: Call canceled - Call-ID: {}, Caller: {}, Callee: {}",
                        call_record.call_id, call_record.caller, call_record.callee
                    );
                }
                Err(e) => {
                    error!("CDR: Failed to send call record for canceled call: {}", e);
                }
            }
        }

        Ok(())
    }

    // Clean up sessions that have been active too long without proper termination
    pub(crate) fn cleanup_stale_sessions(&self, sender: &CallRecordSender, max_duration_secs: u64) {
        let now = Utc::now();
        let mut active_sessions = self.inner.active_sessions.lock().unwrap();
        let mut to_remove = Vec::new();

        for (call_id, session) in active_sessions.iter() {
            if now.signed_duration_since(session.start_time).num_seconds() as u64
                >= max_duration_secs
            {
                to_remove.push(call_id.clone());
            }
        }

        for call_id in to_remove {
            if let Some(session) = active_sessions.remove(&call_id) {
                let hangup_reason = CallRecordHangupReason::Autohangup;
                let call_record = session.to_call_record(hangup_reason);

                match sender.send(call_record.clone()) {
                    Ok(_) => {
                        warn!(
                            "CDR: Stale call session cleaned up - Call-ID: {}, Duration: {}s",
                            call_record.call_id,
                            call_record
                                .end_time
                                .signed_duration_since(call_record.start_time)
                                .as_seconds_f32()
                        );
                    }
                    Err(e) => {
                        error!("CDR: Failed to send call record for stale session: {}", e);
                    }
                }
            }
        }
    }

    // Get current active session count for monitoring
    pub fn get_active_session_count(&self) -> usize {
        self.inner.active_sessions.lock().unwrap().len()
    }
}

#[async_trait]
impl ProxyModule for CdrModule {
    fn name(&self) -> &str {
        "cdr"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Bye,
            rsip::Method::Cancel,
            rsip::Method::Ack,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        info!("CDR module started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("CDR module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        let sender = match self.inner.callrecord_sender.as_ref() {
            Some(sender) => sender,
            None => {
                return Ok(ProxyAction::Continue);
            }
        };

        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx) {
                    error!("CDR: Error handling INVITE: {}", e);
                }
            }
            rsip::Method::Bye => {
                if let Err(e) = self.handle_bye(tx, sender) {
                    error!("CDR: Error handling BYE: {}", e);
                }
            }
            rsip::Method::Cancel => {
                if let Err(e) = self.handle_cancel(tx, sender) {
                    error!("CDR: Error handling CANCEL: {}", e);
                }
            }
            _ => {}
        }

        Ok(ProxyAction::Continue)
    }

    async fn on_transaction_end(&self, tx: &mut Transaction) -> Result<()> {
        // Update status code if we have a response
        if let Err(e) = self.handle_response(tx) {
            debug!("CDR: Error handling response: {}", e);
        }

        // Periodically cleanup stale sessions (every 100th transaction)
        let mut counter = self.inner.transaction_counter.lock().unwrap();
        *counter += 1;
        if *counter % 100 == 0 {
            if let Some(sender) = self.inner.callrecord_sender.as_ref() {
                self.cleanup_stale_sessions(&sender, 3600); // 1 hour timeout
            }
        }

        Ok(())
    }
}

impl CdrModule {}
