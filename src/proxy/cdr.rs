use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::{
    callrecord::{CallRecord, CallRecordHangupReason, CallRecordSender},
    config::ProxyConfig,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use rsip::prelude::{HeadersExt, ToTypedHeader};
use rsipstack::{
    dialog::DialogId,
    transaction::{key::TransactionKey, transaction::Transaction},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub(crate) struct CdrModuleInner {
    pub callrecord_sender: Option<CallRecordSender>,
    pub pending_sessions: Mutex<HashMap<TransactionKey, CallRecord>>, // key: call_id
    pub confrim_sessions: Mutex<HashMap<DialogId, CallRecord>>,
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
            pending_sessions: Mutex::new(HashMap::new()),
            confrim_sessions: Mutex::new(HashMap::new()),
        });
        Self { inner }
    }

    fn extract_caller_callee(&self, tx: &Transaction) -> Result<(String, String)> {
        let from = tx.original.from_header()?.typed()?;
        let to = tx.original.to_header()?.typed()?;
        Ok((from.uri.to_string(), to.uri.to_string()))
    }

    fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        let (caller, callee) = match self.extract_caller_callee(tx) {
            Ok((caller, callee)) => (caller, callee),
            Err(e) => {
                warn!("Failed to extract caller/callee from INVITE: {}", e);
                ("unknown".to_string(), "unknown".to_string())
            }
        };
        let call_record = CallRecord {
            call_type: crate::handler::call::ActiveCallType::Sip,
            option: None,
            call_id: tx.key.to_string(),
            start_time: Utc::now(),
            ring_time: None,
            answer_time: None,
            end_time: Utc::now(),
            caller,
            callee,
            status_code: 0,
            offer: Some(String::from_utf8_lossy(&tx.original.body).to_string()),
            answer: None,
            hangup_reason: None,
            recorder: vec![],
            extras: None,
            dump_event_file: None,
        };
        let mut pending_sessions = self.inner.pending_sessions.lock().unwrap();
        pending_sessions.insert(tx.key.clone(), call_record);
        Ok(())
    }

    fn handle_response(&self, tx: &mut Transaction) -> Result<()> {
        if let Some(last_response) = &tx.last_response {
            let status_code = last_response.status_code.code();

            let mut pending_sessions = self.inner.pending_sessions.lock().unwrap();
            if let Some(mut record) = pending_sessions.remove(&tx.key) {
                let dialog_id = match DialogId::try_from(last_response) {
                    Ok(id) => id,
                    Err(_) => return Ok(()), // ignore
                };
                record.call_id = dialog_id.to_string();
                record.status_code = status_code;

                self.inner
                    .confrim_sessions
                    .lock()
                    .unwrap()
                    .insert(dialog_id, record);
            }
        }
        Ok(())
    }

    fn handle_bye(&self, tx: &mut Transaction, sender: &CallRecordSender) -> Result<()> {
        let call_id = DialogId::try_from(&tx.original).map_err(|e| anyhow::anyhow!("{}", e))?;

        let mut confrim_sessions = self.inner.confrim_sessions.lock().unwrap();
        if let Some(mut record) = confrim_sessions.remove(&call_id) {
            let (caller, _) = match self.extract_caller_callee(tx) {
                Ok((caller, callee)) => (caller, callee),
                Err(e) => {
                    warn!("Failed to extract caller/callee from BYE: {}", e);
                    ("unknown".to_string(), "unknown".to_string())
                }
            };
            let reason = if record.caller == caller {
                CallRecordHangupReason::ByCaller
            } else if record.callee == caller {
                CallRecordHangupReason::ByCallee
            } else {
                CallRecordHangupReason::BySystem
            };
            record.end_time = Utc::now();
            record.hangup_reason.replace(reason);
            sender.send(record).ok();
        } else {
            info!(
                dialog_id = format!("{}", call_id),
                "CDR: BYE received for unknown call"
            );
        }

        Ok(())
    }

    fn handle_cancel(&self, tx: &mut Transaction, sender: &CallRecordSender) -> Result<()> {
        let mut pending_sessions = self.inner.pending_sessions.lock().unwrap();
        if let Some(mut record) = pending_sessions.remove(&tx.key) {
            record.end_time = Utc::now();
            record
                .hangup_reason
                .replace(CallRecordHangupReason::Canceled);
            sender.send(record).ok();
        }
        Ok(())
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
        if let Err(e) = self.handle_response(tx) {
            debug!("CDR: Error handling response: {}", e);
        }
        Ok(())
    }
}

impl CdrModule {}
