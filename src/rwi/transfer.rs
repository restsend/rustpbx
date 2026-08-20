use crate::call::domain::{CallCommand, LegId};
use crate::proxy::active_call_registry::{ActiveProxyCallRegistry, ActiveProxyCallStatus};
use crate::proxy::proxy_call::sip_session::SipSessionHandle;
use crate::rwi::gateway::RwiGateway;
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info};
use uuid::Uuid;

/// Result of a transfer attempt via REFER
#[derive(Debug, Clone)]
pub enum ReferTransferResult {
    /// Internal error
    InternalError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferMode {
    SipRefer,
    Replaces,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferStatus {
    Init,
    ReferSent,
    NotifyTrying,
    NotifyProgress,
    Accepted,
    Completed,
    Failed(TransferFailureReason),
    Canceled,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferFailureReason {
    ReferRejected,
    Timeout,
    Cancelled,
    InvalidTarget,
    InvalidState,
    InternalError,
}

impl TransferFailureReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferFailureReason::ReferRejected => "refer_rejected",
            TransferFailureReason::Timeout => "timeout",
            TransferFailureReason::Cancelled => "cancelled",
            TransferFailureReason::InvalidTarget => "invalid_target",
            TransferFailureReason::InvalidState => "invalid_state",
            TransferFailureReason::InternalError => "internal_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferTransaction {
    pub transfer_id: String,
    pub call_id: String,
    pub dialog_id: Option<String>,
    pub target: String,
    pub status: TransferStatus,
    pub mode: TransferMode,
    pub created_at: Instant,
    pub updated_at: Instant,
    pub sip_status: Option<u16>,
    pub error_message: Option<String>,
    pub consultation_call_id: Option<String>,
    pub original_leg: Option<String>,
}

impl TransferTransaction {
    pub fn new(call_id: String, target: String, mode: TransferMode) -> Self {
        let now = Instant::now();
        Self {
            transfer_id: Uuid::new_v4().to_string(),
            call_id,
            dialog_id: None,
            target,
            status: TransferStatus::Init,
            mode,
            created_at: now,
            updated_at: now,
            sip_status: None,
            error_message: None,
            consultation_call_id: None,
            original_leg: None,
        }
    }

    pub fn update_status(&mut self, status: TransferStatus) {
        self.status = status;
        self.updated_at = Instant::now();
    }

    pub fn set_sip_status(&mut self, status: u16) {
        self.sip_status = Some(status);
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            TransferStatus::Completed
                | TransferStatus::Failed(_)
                | TransferStatus::Canceled
                | TransferStatus::TimedOut
        )
    }

    pub fn duration_ms(&self) -> u64 {
        self.created_at.elapsed().as_millis() as u64
    }
}

#[derive(Debug, Clone)]
pub struct TransferConfig {
    pub refer_enabled: bool,
    pub attended_enabled: bool,
    pub max_concurrent_transfers: usize,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            refer_enabled: true,
            attended_enabled: true,
            max_concurrent_transfers: 1000,
        }
    }
}

pub struct TransferController {
    config: TransferConfig,
    transactions: Arc<DashMap<String, TransferTransaction>>,
    call_registry: Arc<ActiveProxyCallRegistry>,
    gateway: Arc<RwLock<RwiGateway>>,
    sip_server: Option<crate::proxy::server::SipServerRef>,
}

impl TransferController {
    pub fn new(
        config: TransferConfig,
        call_registry: Arc<ActiveProxyCallRegistry>,
        gateway: Arc<RwLock<RwiGateway>>,
    ) -> Self {
        Self {
            config,
            transactions: Arc::new(DashMap::new()),
            call_registry,
            gateway,
            sip_server: None,
        }
    }

    pub fn with_sip_server(mut self, sip_server: crate::proxy::server::SipServerRef) -> Self {
        self.sip_server = Some(sip_server);
        self
    }

    pub fn with_default_config(
        call_registry: Arc<ActiveProxyCallRegistry>,
        gateway: Arc<RwLock<RwiGateway>>,
    ) -> Self {
        Self::new(TransferConfig::default(), call_registry, gateway)
    }

    async fn get_handle(&self, call_id: &str) -> Option<SipSessionHandle> {
        self.call_registry.get_handle(call_id)
    }

    async fn verify_call_state_for_transfer(
        &self,
        call_id: &str,
    ) -> Result<(), TransferFailureReason> {
        let entry = self.call_registry.get(call_id);
        if entry.is_none() {
            return Err(TransferFailureReason::InvalidState);
        }
        let entry = entry.unwrap();
        if !matches!(entry.status, ActiveProxyCallStatus::Talking) {
            return Err(TransferFailureReason::InvalidState);
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn initiate_blind_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        self.initiate_transfer(call_id, target, TransferMode::SipRefer, "blind")
            .await
    }

    async fn initiate_transfer(
        &self,
        call_id: String,
        target: String,
        mode: TransferMode,
        direction: &'static str,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        if !self.config.refer_enabled {
            return Err(TransferFailureReason::InternalError);
        }

        crate::metrics::transfer::attempt_total("refer", direction);

        self.verify_call_state_for_transfer(&call_id).await?;

        let mut transaction = TransferTransaction::new(call_id.clone(), target.clone(), mode);
        transaction.update_status(TransferStatus::Accepted);

        if self.transactions.len() >= self.config.max_concurrent_transfers {
            crate::metrics::transfer::failed_total("refer", "max_concurrent_reached");
            return Err(TransferFailureReason::InternalError);
        }
        self.transactions
            .insert(transaction.transfer_id.clone(), transaction.clone());

        crate::metrics::transfer::set_active_transfers(self.transactions.len());

        let _handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::CallTransferAccepted {
            call_id: call_id.clone(),
            transfer_target: Some(target.clone()),
        });

        Ok(transaction)
    }

    /// Execute a complete blind transfer.
    ///
    /// Architecture note: `SipSessionHandle::send_command` is fire-and-forget (mpsc).
    /// The actual REFER outcome (202 / 4xx / timeout) is handled inside `SipSession`
    /// and surfaced back to callers via RWI events through the gateway. This method
    /// is therefore responsible only for:
    ///   1. Creating and registering a transfer transaction.
    ///   2. Sending the `CallCommand::Transfer` to the session.
    ///   3. Returning `Ok(transaction)` once the command is dispatched.
    ///
    /// The true success/failure events (`CallTransferAccepted` / `CallTransferFailed`)
    /// will arrive asynchronously via `RwiGateway::send_to_owner`.
    pub async fn execute_blind_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        self.execute_transfer(call_id, target, TransferMode::SipRefer, false)
            .await
    }

    pub async fn execute_replace_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        self.execute_transfer(call_id, target, TransferMode::Replaces, true)
            .await
    }

    async fn execute_transfer(
        &self,
        call_id: String,
        target: String,
        mode: TransferMode,
        attended: bool,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let direction: &'static str = if attended { "replace" } else { "blind" };
        let tx = self
            .initiate_transfer(call_id.clone(), target.clone(), mode, direction)
            .await?;

        info!(transfer_id = %tx.transfer_id, %call_id, %target, attended, "Dispatching transfer command");

        match self.try_refer_transfer(&tx, attended).await {
            Ok(_) => {
                info!(transfer_id = %tx.transfer_id, "Transfer command dispatched successfully");
                Ok(tx)
            }
            Err(ReferTransferResult::InternalError(e)) => {
                error!(transfer_id = %tx.transfer_id, error = %e, "Failed to dispatch transfer command");
                self.fail_transfer(&tx.transfer_id, TransferFailureReason::InternalError, None)
                    .await;
                Err(TransferFailureReason::InternalError)
            }
        }
    }

    /// Send a `CallCommand::Transfer` to the SipSession for this transaction.
    ///
    /// **Design**: `SipSessionHandle::send_command` is an mpsc fire-and-forget channel.
    /// The SipSession processes the command asynchronously and emits `emit_transfer_event`
    /// which surfaces the outcome via the RWI gateway.  There is no back-channel from
    /// SipSession to TransferController, so this method returns `Ok(())` as soon as the
    /// command is enqueued, or `Err(InternalError)` if the channel is closed.
    async fn try_refer_transfer(
        &self,
        tx: &TransferTransaction,
        attended: bool,
    ) -> Result<(), ReferTransferResult> {
        let handle = self
            .get_handle(&tx.call_id)
            .await
            .ok_or_else(|| ReferTransferResult::InternalError("Call not found".to_string()))?;

        // The transfer command targets the session's caller leg. RWI identifies
        // calls by their session id (`tx.call_id`), but SipSession legs are
        // named "caller"/"callee" — passing the call_id as the leg_id would make
        // `handle_transfer` fail at `require_leg`. The transferee is the caller leg.
        let leg_id = LegId::new("caller");
        handle
            .send_command(CallCommand::Transfer {
                leg_id,
                target: tx.target.clone(),
                attended,
            })
            .map_err(|e| {
                ReferTransferResult::InternalError(format!(
                    "Failed to send transfer command: {}",
                    e
                ))
            })
    }

    /// Mark a transfer as failed and emit events
    async fn fail_transfer(
        &self,
        transfer_id: &str,
        reason: TransferFailureReason,
        sip_status: Option<u16>,
    ) {
        let failed_tx_opt = {
            if let Some(mut tx) = self.transactions.get_mut(transfer_id) {
                tx.update_status(TransferStatus::Failed(reason.clone()));
                tx.sip_status = sip_status;
                Some(tx.clone())
            } else {
                None
            }
        };
        if let Some(failed_tx) = failed_tx_opt {
            let gw = self.gateway.read();
            gw.send_to_owner(&crate::rwi::CallTransferFailed {
                call_id: failed_tx.call_id.clone(),
                sip_status,
                reason: Some(reason.as_str().to_string()),
                transfer_target: Some(failed_tx.target.clone()),
            });
        }
    }

    pub async fn initiate_attended_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        if !self.config.attended_enabled {
            return Err(TransferFailureReason::InternalError);
        }

        self.verify_call_state_for_transfer(&call_id).await?;

        let mut transaction =
            TransferTransaction::new(call_id.clone(), target.clone(), TransferMode::SipRefer);
        transaction.consultation_call_id = Some(Uuid::new_v4().to_string());
        transaction.original_leg = Some(call_id.clone());

        if self.transactions.len() >= self.config.max_concurrent_transfers {
            return Err(TransferFailureReason::InternalError);
        }
        self.transactions
            .insert(transaction.transfer_id.clone(), transaction.clone());

        let handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        let leg_id = LegId::new(&call_id);
        let _ = handle.send_command(CallCommand::Hold {
            leg_id,
            music: Some(crate::call::domain::MediaSource::File {
                path: "hold.wav".to_string(),
            }),
        });

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::CallTransferAccepted {
            call_id: call_id.clone(),
            transfer_target: Some(target.clone()),
        });

        Ok(transaction)
    }

    pub async fn complete_attended_transfer(
        &self,
        call_id: String,
        consultation_call_id: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let transaction = self
            .transactions
            .iter()
            .find(|r| {
                r.value().call_id == call_id
                    && r.value().consultation_call_id.as_ref() == Some(&consultation_call_id)
            })
            .map(|r| r.value().clone());

        let mut transaction = transaction.ok_or(TransferFailureReason::InvalidState)?;

        let handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        // Inherit the root session id onto the consultation leg so its
        // events/CDR stay correlated with the logical call (root = the
        // original call's root, resolved via the RWI CallMetaStore).
        {
            let gw = self.gateway.read();
            let root = gw
                .meta_store
                .get_sync(&call_id)
                .and_then(|m| m.session_id)
                .unwrap_or_else(|| call_id.clone());
            let mut meta = gw
                .meta_store
                .get_sync(&consultation_call_id)
                .unwrap_or_default();
            meta.session_id = Some(root);
            gw.meta_store.insert(consultation_call_id.clone(), meta);
        }

        let leg_a = LegId::new(&call_id);
        let leg_b = LegId::new(&consultation_call_id);
        let _ = handle.send_command(CallCommand::Bridge {
            leg_a,
            leg_b,
            mode: crate::call::domain::P2PMode::Audio,
        });

        transaction.update_status(TransferStatus::Completed);

        let gw = self.gateway.read();
        gw.send_to_owner(&crate::rwi::CallTransferred {
            call_id: call_id.clone(),
            transfer_target: Some(transaction.target.clone()),
        });

        Ok(transaction)
    }

    pub async fn cancel_attended_transfer(
        &self,
        consultation_call_id: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let transaction = self
            .transactions
            .iter()
            .find(|r| r.value().consultation_call_id.as_ref() == Some(&consultation_call_id))
            .map(|r| r.value().clone());

        let mut transaction = transaction.ok_or(TransferFailureReason::InvalidState)?;

        let handle = self.get_handle(&consultation_call_id).await;
        if let Some(handle) = handle {
            let _leg_id = LegId::new(&consultation_call_id);
            let _ = handle
                .send_command_async(CallCommand::Hangup(
                    crate::call::domain::HangupCommand::local(
                        "transfer",
                        Some(crate::callrecord::CallRecordHangupReason::BySystem),
                        Some(487),
                    ),
                ))
                .await;
        }

        if let Some(ref original_call_id) = transaction.original_leg {
            let original_handle = self.get_handle(original_call_id).await;
            if let Some(original_handle) = original_handle {
                let leg_id = LegId::new(original_call_id);
                let _ = original_handle.send_command(CallCommand::Unhold { leg_id });
            }

            let gw = self.gateway.read();
            gw.send_to_owner(&crate::rwi::CallTransferFailed {
                call_id: original_call_id.clone(),
                sip_status: Some(487),
                reason: Some("cancelled".to_string()),
                transfer_target: Some(transaction.target.clone()),
            });
        }

        transaction.update_status(TransferStatus::Canceled);

        Ok(transaction)
    }

    pub async fn handle_refer_response(
        &self,
        transfer_id: String,
        sip_status: u16,
    ) -> Option<TransferTransaction> {
        enum GatewayEvent {
            Accepted(String),
            Failed {
                call_id: String,
                sip_status: u16,
                reason: TransferFailureReason,
            },
            None,
        }

        let (tx_clone, gw_event) = {
            let mut tx = self.transactions.get_mut(&transfer_id)?;

            tx.set_sip_status(sip_status);

            let gw_event = if rsipstack::sip::StatusCode::from(sip_status).kind()
                == rsipstack::sip::StatusCodeKind::Successful
            {
                if tx.status == TransferStatus::Accepted {
                    GatewayEvent::None
                } else {
                    tx.update_status(TransferStatus::Accepted);
                    GatewayEvent::Accepted(tx.call_id.clone())
                }
            } else if sip_status >= 400 {
                let reason = TransferFailureReason::ReferRejected;
                tx.update_status(TransferStatus::Failed(reason.clone()));
                GatewayEvent::Failed {
                    call_id: tx.call_id.clone(),
                    sip_status,
                    reason,
                }
            } else {
                GatewayEvent::None
            };

            (tx.clone(), gw_event)
        };

        match gw_event {
            GatewayEvent::Accepted(call_id) => {
                let gw = self.gateway.read();
                gw.send_to_owner(&crate::rwi::CallTransferAccepted {
                    call_id: call_id.clone(),
                    transfer_target: Some(tx_clone.target.clone()),
                });
            }
            GatewayEvent::Failed {
                call_id,
                sip_status,
                reason,
            } => {
                let gw = self.gateway.read();
                gw.send_to_owner(&crate::rwi::CallTransferFailed {
                    call_id: call_id.clone(),
                    sip_status: Some(sip_status),
                    reason: Some(reason.as_str().to_string()),
                    transfer_target: Some(tx_clone.target.clone()),
                });
            }
            GatewayEvent::None => {}
        }

        Some(tx_clone)
    }

    pub async fn handle_notify(
        &self,
        transfer_id: String,
        notify_status: u16,
    ) -> Option<TransferTransaction> {
        // We need to handle the case where we must drop the write lock before
        // acquiring the gateway read lock to avoid deadlock.
        enum PostAction {
            TransferFailed(Box<TransferTransaction>, TransferFailureReason),
            None,
        }

        let (result_tx, post_action) = {
            let mut tx = self.transactions.get_mut(&transfer_id)?;

            tx.set_sip_status(notify_status);

            let post_action = match notify_status {
                100 => {
                    tx.update_status(TransferStatus::NotifyTrying);
                    PostAction::None
                }
                180 | 183 => {
                    tx.update_status(TransferStatus::NotifyProgress);
                    PostAction::None
                }
                200 => {
                    tx.update_status(TransferStatus::Completed);
                    crate::metrics::transfer::success_total("refer");
                    let completed_tx = tx.clone();
                    drop(tx);
                    let active_count = self
                        .transactions
                        .iter()
                        .filter(|r| {
                            !matches!(
                                r.value().status,
                                TransferStatus::Completed | TransferStatus::Failed(_)
                            )
                        })
                        .count();
                    crate::metrics::transfer::set_active_transfers(active_count);
                    let gw = self.gateway.read();
                    gw.send_to_owner(&crate::rwi::CallTransferred {
                        call_id: completed_tx.call_id.clone(),
                        transfer_target: Some(completed_tx.target.clone()),
                    });
                    return Some(completed_tx);
                }
                _ if notify_status >= 400 => {
                    crate::metrics::transfer::failed_total(
                        "refer",
                        &format!("sip_{}", notify_status),
                    );
                    let reason = TransferFailureReason::ReferRejected;
                    tx.update_status(TransferStatus::Failed(reason.clone()));
                    PostAction::TransferFailed(Box::new(tx.clone()), reason)
                }
                _ => PostAction::None,
            };

            (tx.clone(), post_action)
        };

        match post_action {
            PostAction::TransferFailed(failed_tx, reason) => {
                let failed_tx = *failed_tx;
                let gw = self.gateway.read();
                gw.send_to_owner(&crate::rwi::CallTransferFailed {
                    call_id: failed_tx.call_id.clone(),
                    sip_status: Some(notify_status),
                    reason: Some(reason.as_str().to_string()),
                    transfer_target: Some(failed_tx.target.clone()),
                });
                Some(failed_tx)
            }
            PostAction::None => Some(result_tx),
        }
    }

    /// Handle a synchronous REFER response matched by `call_id`.
    pub async fn handle_refer_response_by_call_id(
        &self,
        call_id: &str,
        sip_status: u16,
    ) -> Option<TransferTransaction> {
        let transfer_id = self
            .transactions
            .iter()
            .find(|r| r.value().call_id == call_id && !r.value().is_terminal())
            .map(|r| r.value().transfer_id.clone())?;
        self.handle_refer_response(transfer_id, sip_status).await
    }

    /// Handle a REFER NOTIFY matched by `call_id`.
    pub async fn handle_notify_by_call_id(
        &self,
        call_id: &str,
        notify_status: u16,
    ) -> Option<TransferTransaction> {
        let transfer_id = self
            .transactions
            .iter()
            .find(|r| r.value().call_id == call_id && !r.value().is_terminal())
            .map(|r| r.value().transfer_id.clone())?;
        self.handle_notify(transfer_id, notify_status).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::active_call_registry::ActiveProxyCallEntry;
    use crate::proxy::proxy_call::sip_session::SipSession;
    use std::sync::Arc;

    // ────────────────────────────────────────────────────────────────────────────
    // Test helpers
    // ────────────────────────────────────────────────────────────────────────────

    fn make_controller() -> TransferController {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        TransferController::with_default_config(registry, gateway)
    }

    fn make_controller_with_registry() -> (TransferController, Arc<ActiveProxyCallRegistry>) {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let ctrl = TransferController::with_default_config(Arc::clone(&registry), gateway);
        (ctrl, registry)
    }

    /// Register a call in Talking state with a live SipSession handle.
    /// Returns the command receiver so tests can inspect dispatched commands.
    fn register_talking_call(
        registry: &ActiveProxyCallRegistry,
        call_id: &str,
    ) -> crate::call::domain::CallCommandRx {
        let id = crate::call::runtime::SessionId(call_id.to_string());
        let (handle, cmd_rx) = SipSession::with_handle(id);
        let entry = ActiveProxyCallEntry {
            session_id: call_id.to_string(),
            caller: Some("sip:caller@local".to_string()),
            callee: Some("sip:callee@local".to_string()),
            direction: "inbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: Some(chrono::Utc::now()),
            status: ActiveProxyCallStatus::Talking,
        };
        registry.upsert(entry, handle);
        cmd_rx
    }

    /// Register a call in Ringing state (invalid for transfer).
    fn register_ringing_call(registry: &ActiveProxyCallRegistry, call_id: &str) {
        let id = crate::call::runtime::SessionId(call_id.to_string());
        let (handle, _rx) = SipSession::with_handle(id);
        let entry = ActiveProxyCallEntry {
            session_id: call_id.to_string(),
            caller: Some("sip:caller@local".to_string()),
            callee: Some("sip:callee@local".to_string()),
            direction: "inbound".to_string(),
            started_at: chrono::Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        };
        registry.upsert(entry, handle);
    }

    // ────────────────────────────────────────────────────────────────────────────
    // execute_blind_transfer tests
    // ────────────────────────────────────────────────────────────────────────────

    /// execute_blind_transfer dispatches a CallCommand::Transfer when the call
    /// is in Talking state and returns Ok(transaction).
    #[tokio::test]
    async fn test_execute_blind_transfer_dispatches_command() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-exec-001";
        let target = "sip:target@local";

        let mut cmd_rx = register_talking_call(&registry, call_id);

        let result = ctrl
            .execute_blind_transfer(call_id.to_string(), target.to_string())
            .await;
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        let tx = result.unwrap();
        assert_eq!(tx.call_id, call_id);
        assert_eq!(tx.target, target);

        // The Transfer command must have been sent to the SipSession channel
        let cmd = cmd_rx
            .try_recv()
            .expect("expected a CallCommand to be dispatched");
        match cmd {
            CallCommand::Transfer {
                leg_id,
                target: t,
                attended,
            } => {
                // The transfer command targets the session's caller leg (the
                // transferee); RWI identifies calls by session id but legs are
                // named "caller"/"callee".
                assert_eq!(leg_id.as_str(), "caller");
                assert_eq!(t, target);
                assert!(!attended, "blind transfer must have attended=false");
            }
            other => panic!("unexpected command: {:?}", other),
        }
    }

    /// execute_blind_transfer fails with InvalidState when the call_id does not
    /// exist in the registry.
    #[tokio::test]
    async fn test_execute_blind_transfer_call_not_found() {
        let ctrl = make_controller();
        let result = ctrl
            .execute_blind_transfer(
                "nonexistent-call".to_string(),
                "sip:target@local".to_string(),
            )
            .await;
        assert!(result.is_err());
    }

    /// execute_blind_transfer fails when the call is in Ringing state (not Talking).
    #[tokio::test]
    async fn test_execute_blind_transfer_wrong_state() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-ring-001";
        register_ringing_call(&registry, call_id);

        let result = ctrl
            .execute_blind_transfer(call_id.to_string(), "sip:target@local".to_string())
            .await;
        assert!(result.is_err(), "Ringing call must not be transferable");
    }

    #[tokio::test]
    async fn test_execute_replace_transfer_dispatches_attended_transfer_command() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-repl-001";
        let target = "sip:target@local";

        let mut cmd_rx = register_talking_call(&registry, call_id);

        let result = ctrl
            .execute_replace_transfer(call_id.to_string(), target.to_string())
            .await;
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let tx = result.unwrap();
        assert_eq!(tx.mode, TransferMode::Replaces);

        let cmd = cmd_rx
            .try_recv()
            .expect("expected a CallCommand to be dispatched");
        match cmd {
            CallCommand::Transfer {
                leg_id,
                target: t,
                attended,
            } => {
                // The transfer targets the session's caller leg (legs are
                // named "caller"/"callee", never the call id).
                assert_eq!(leg_id.as_str(), "caller");
                assert_eq!(t, target);
                assert!(attended, "replace transfer must have attended=true");
            }
            other => panic!("unexpected command: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_handle_refer_response_replace_rejected_marks_failed() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-repl-reject-001";
        let _cmd_rx = register_talking_call(&registry, call_id);

        let tx = ctrl
            .execute_replace_transfer(call_id.to_string(), "sip:target@local".to_string())
            .await
            .expect("replace transfer should start");

        let updated = ctrl
            .handle_refer_response(tx.transfer_id.clone(), 486)
            .await
            .expect("transaction should exist");

        assert_eq!(updated.mode, TransferMode::Replaces);
        assert_eq!(
            updated.status,
            TransferStatus::Failed(TransferFailureReason::ReferRejected)
        );
    }

    // ────────────────────────────────────────────────────────────────────────────
    // initiate_attended_transfer tests
    // ────────────────────────────────────────────────────────────────────────────

    /// initiate_attended_transfer must return quickly (no blocking sleep loop).
    /// Verify it completes in well under 1 second.
    #[tokio::test]
    async fn test_initiate_attended_transfer_no_block() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-att-001";
        register_talking_call(&registry, call_id);

        let start = std::time::Instant::now();
        let result = ctrl
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string())
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "initiate_attended_transfer should succeed");
        assert!(
            elapsed.as_millis() < 500,
            "initiate_attended_transfer must not block; took {}ms",
            elapsed.as_millis()
        );
    }

    /// initiate_attended_transfer is rejected when attended transfers are disabled.
    #[tokio::test]
    async fn test_initiate_attended_transfer_disabled() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let mut config = TransferConfig::default();
        config.attended_enabled = false;
        let ctrl = TransferController::new(config, Arc::clone(&registry), gateway);

        let call_id = "call-att-dis-001";
        register_talking_call(&registry, call_id);

        let result = ctrl
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string())
            .await;
        assert!(result.is_err(), "should fail when attended is disabled");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // handle_refer_response tests
    // ────────────────────────────────────────────────────────────────────────────

    /// A 202 REFER response moves the transaction to Accepted and returns Some(tx).
    #[tokio::test]
    async fn test_handle_refer_response_accepted() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-refer-202";
        register_talking_call(&registry, call_id);

        // Create a transaction first
        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate_blind_transfer should succeed");

        let result = ctrl
            .handle_refer_response(tx.transfer_id.clone(), 202)
            .await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TransferStatus::Accepted);
        assert_eq!(updated.sip_status, Some(202));
    }

    /// A 4xx REFER response emits a failure event and marks Failed.
    #[tokio::test]
    async fn test_handle_refer_response_rejected() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let ctrl =
            TransferController::new(TransferConfig::default(), Arc::clone(&registry), gateway);

        let call_id = "call-refer-4xx";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate_blind_transfer should succeed");

        let result = ctrl
            .handle_refer_response(tx.transfer_id.clone(), 486)
            .await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert!(
            matches!(updated.status, TransferStatus::Failed(_)),
            "should be Failed, got {:?}",
            updated.status
        );
        assert_eq!(updated.sip_status, Some(486));
    }

    /// handle_refer_response returns None for an unknown transfer_id.
    #[tokio::test]
    async fn test_handle_refer_response_unknown_id() {
        let ctrl = make_controller();
        let result = ctrl
            .handle_refer_response("no-such-id".to_string(), 200)
            .await;
        assert!(result.is_none());
    }

    // ────────────────────────────────────────────────────────────────────────────
    // handle_notify tests
    // ────────────────────────────────────────────────────────────────────────────

    /// NOTIFY 200 marks the transaction Completed.
    #[tokio::test]
    async fn test_handle_notify_200_completes() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-notify-200";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate_blind_transfer should succeed");

        let result = ctrl.handle_notify(tx.transfer_id.clone(), 200).await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TransferStatus::Completed);
    }

    /// NOTIFY 100 moves the transaction to NotifyTrying.
    #[tokio::test]
    async fn test_handle_notify_100_trying() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-notify-100";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate_blind_transfer should succeed");

        let result = ctrl.handle_notify(tx.transfer_id.clone(), 100).await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.status, TransferStatus::NotifyTrying);
    }

    /// NOTIFY 4xx marks the transaction Failed.
    #[tokio::test]
    async fn test_handle_notify_4xx_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let ctrl =
            TransferController::new(TransferConfig::default(), Arc::clone(&registry), gateway);

        let call_id = "call-notify-4xx";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate_blind_transfer should succeed");

        let result = ctrl.handle_notify(tx.transfer_id.clone(), 486).await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert!(
            matches!(updated.status, TransferStatus::Failed(_)),
            "should be Failed, got {:?}",
            updated.status
        );
    }

    /// handle_notify returns None for an unknown transfer_id.
    #[tokio::test]
    async fn test_handle_notify_unknown_id() {
        let ctrl = make_controller();
        let result = ctrl.handle_notify("no-such-id".to_string(), 200).await;
        assert!(result.is_none());
    }

    #[test]
    fn test_transfer_transaction_new() {
        let tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        assert_eq!(tx.call_id, "call-001");
        assert_eq!(tx.target, "sip:target@local");
        assert_eq!(tx.status, TransferStatus::Init);
        assert_eq!(tx.mode, TransferMode::SipRefer);
        assert!(!tx.is_terminal());
    }

    #[test]
    fn test_transfer_transaction_update_status() {
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        assert_eq!(tx.status, TransferStatus::Init);

        tx.update_status(TransferStatus::ReferSent);
        assert_eq!(tx.status, TransferStatus::ReferSent);
        assert!(!tx.is_terminal());

        tx.update_status(TransferStatus::Completed);
        assert!(tx.is_terminal());
    }

    #[test]
    fn test_transfer_failure_reason_as_str() {
        assert_eq!(
            TransferFailureReason::ReferRejected.as_str(),
            "refer_rejected"
        );
        assert_eq!(TransferFailureReason::Timeout.as_str(), "timeout");
        assert_eq!(TransferFailureReason::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn test_transfer_config_default() {
        let config = TransferConfig::default();
        assert!(config.refer_enabled);
        assert!(config.attended_enabled);
        assert_eq!(config.max_concurrent_transfers, 1000);
    }

    #[test]
    fn test_transfer_transaction_terminal_states() {
        // Test Completed is terminal
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        tx.update_status(TransferStatus::Completed);
        assert!(tx.is_terminal());

        // Test Failed is terminal
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        tx.update_status(TransferStatus::Failed(TransferFailureReason::ReferRejected));
        assert!(tx.is_terminal());

        // Test Canceled is terminal
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        tx.update_status(TransferStatus::Canceled);
        assert!(tx.is_terminal());

        // Test TimedOut is terminal
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        tx.update_status(TransferStatus::TimedOut);
        assert!(tx.is_terminal());

        // Test non-terminal states
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );
        tx.update_status(TransferStatus::ReferSent);
        assert!(!tx.is_terminal());

        tx.update_status(TransferStatus::NotifyTrying);
        assert!(!tx.is_terminal());
    }

    #[test]
    fn test_transfer_transaction_sip_status() {
        let mut tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );

        tx.set_sip_status(202);
        assert_eq!(tx.sip_status, Some(202));
    }

    #[test]
    fn test_transfer_transaction_duration() {
        let tx = TransferTransaction::new(
            "call-001".to_string(),
            "sip:target@local".to_string(),
            TransferMode::SipRefer,
        );

        // Duration should be non-zero after some time
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(tx.duration_ms() >= 10);
    }

    // ────────────────────────────────────────────────────────────────────────────
    // REFER 1xx / 5xx transitions
    // ────────────────────────────────────────────────────────────────────────────

    /// REFER 1xx (e.g. 100 Trying) is a no-op: status stays Accepted.
    #[tokio::test]
    async fn test_handle_refer_response_1xx_is_noop() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-refer-1xx";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl
            .handle_refer_response(tx.transfer_id.clone(), 100)
            .await;
        assert!(result.is_some());
        let updated = result.unwrap();
        // 1xx must not change a pre-Accepted transaction to Failed/Completed
        assert_eq!(updated.status, TransferStatus::Accepted);
        assert_eq!(updated.sip_status, Some(100));
    }

    /// REFER 5xx (e.g. 500) marks the transaction Failed.
    #[tokio::test]
    async fn test_handle_refer_response_5xx_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let ctrl =
            TransferController::new(TransferConfig::default(), Arc::clone(&registry), gateway);

        let call_id = "call-refer-5xx";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl
            .handle_refer_response(tx.transfer_id.clone(), 500)
            .await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert!(
            matches!(updated.status, TransferStatus::Failed(_)),
            "5xx should mark transaction Failed, got {:?}",
            updated.status
        );
        assert_eq!(updated.sip_status, Some(500));
    }

    // ────────────────────────────────────────────────────────────────────────────
    // NOTIFY 180/183 transitions
    // ────────────────────────────────────────────────────────────────────────────

    /// NOTIFY 180 moves the transaction to NotifyProgress.
    #[tokio::test]
    async fn test_handle_notify_180_progress() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-notify-180";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.handle_notify(tx.transfer_id.clone(), 180).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().status, TransferStatus::NotifyProgress);
    }

    /// NOTIFY 183 moves the transaction to NotifyProgress.
    #[tokio::test]
    async fn test_handle_notify_183_progress() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-notify-183";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.handle_notify(tx.transfer_id.clone(), 183).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().status, TransferStatus::NotifyProgress);
    }

    // ────────────────────────────────────────────────────────────────────────────
    // by_call_id routing (consultation_call_id canonical mapping)
    // ────────────────────────────────────────────────────────────────────────────

    /// handle_refer_response_by_call_id finds the active transaction by call_id.
    #[tokio::test]
    async fn test_handle_refer_response_by_call_id_routes_correctly() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-refer-byid-001";
        register_talking_call(&registry, call_id);

        let _tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.handle_refer_response_by_call_id(call_id, 202).await;
        assert!(result.is_some(), "should find transaction by call_id");
        let updated = result.unwrap();
        assert_eq!(updated.call_id, call_id);
        assert_eq!(updated.sip_status, Some(202));
    }

    /// handle_refer_response_by_call_id returns None for unknown call_id.
    #[tokio::test]
    async fn test_handle_refer_response_by_call_id_not_found() {
        let ctrl = make_controller();
        let result = ctrl
            .handle_refer_response_by_call_id("no-such-call", 202)
            .await;
        assert!(result.is_none());
    }

    /// handle_notify_by_call_id finds the active transaction by call_id.
    #[tokio::test]
    async fn test_handle_notify_by_call_id_routes_correctly() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-notify-byid-001";
        register_talking_call(&registry, call_id);

        let _tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.handle_notify_by_call_id(call_id, 100).await;
        assert!(result.is_some(), "should find transaction by call_id");
        let updated = result.unwrap();
        assert_eq!(updated.status, TransferStatus::NotifyTrying);
    }

    // ────────────────────────────────────────────────────────────────────────────
    // complete_attended_transfer / cancel_attended_transfer
    // ────────────────────────────────────────────────────────────────────────────

    /// complete_attended_transfer with valid consultation_call_id returns Ok and
    /// dispatches a Bridge command to the original call's SipSession.
    #[tokio::test]
    async fn test_complete_attended_transfer_valid() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-att-complete-001";
        let mut cmd_rx = register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string())
            .await
            .expect("initiate should succeed");

        let consult_id = tx
            .consultation_call_id
            .clone()
            .expect("consultation_call_id must be set");

        register_talking_call(&registry, &consult_id);

        let result = ctrl
            .complete_attended_transfer(call_id.to_string(), consult_id.clone())
            .await;
        assert!(
            result.is_ok(),
            "complete_attended_transfer should succeed, got {:?}",
            result
        );
        let completed = result.unwrap();
        assert_eq!(completed.status, TransferStatus::Completed);

        // Drain commands: the first is Hold (from initiate), the second is Bridge.
        let mut bridge_received = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let CallCommand::Bridge { leg_a, leg_b, .. } = &cmd {
                assert_eq!(leg_a.as_str(), call_id);
                assert_eq!(leg_b.as_str(), consult_id);
                bridge_received = true;
            }
        }
        assert!(bridge_received, "Bridge command must be dispatched");
    }

    /// complete_attended_transfer with unknown consultation_call_id returns InvalidState.
    #[tokio::test]
    async fn test_complete_attended_transfer_invalid_leg() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-att-complete-inv";
        register_talking_call(&registry, call_id);

        let _tx = ctrl
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl
            .complete_attended_transfer(call_id.to_string(), "totally-wrong-consult-id".to_string())
            .await;
        assert!(
            matches!(result, Err(TransferFailureReason::InvalidState)),
            "should be InvalidState, got {:?}",
            result
        );
    }

    /// cancel_attended_transfer with valid consultation_call_id returns Ok and
    /// marks the transaction Canceled.
    #[tokio::test]
    async fn test_cancel_attended_transfer_valid() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-att-cancel-001";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string())
            .await
            .expect("initiate should succeed");

        let consult_id = tx
            .consultation_call_id
            .clone()
            .expect("consultation_call_id must be set");

        let result = ctrl.cancel_attended_transfer(consult_id.clone()).await;
        assert!(
            result.is_ok(),
            "cancel_attended_transfer should succeed, got {:?}",
            result
        );
        assert_eq!(result.unwrap().status, TransferStatus::Canceled);
    }

    /// cancel_attended_transfer with unknown consultation_call_id returns InvalidState.
    #[tokio::test]
    async fn test_cancel_attended_transfer_invalid_leg() {
        let ctrl = make_controller();
        let result = ctrl
            .cancel_attended_transfer("no-such-consult".to_string())
            .await;
        assert!(
            matches!(result, Err(TransferFailureReason::InvalidState)),
            "should be InvalidState, got {:?}",
            result
        );
    }

    /// Race condition: call is hung up (removed from registry) after transfer is initiated
    /// but before complete_attended_transfer is called. Should return InvalidState.
    #[tokio::test]
    async fn test_complete_attended_transfer_after_hangup_returns_invalid_state() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-race-001";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string())
            .await
            .expect("initiate should succeed");

        let consult_id = tx
            .consultation_call_id
            .clone()
            .expect("consultation_call_id must be set");

        // Simulate hangup by removing the original call from registry
        registry.remove(call_id);

        // complete_attended_transfer should fail because original call is gone
        let result = ctrl
            .complete_attended_transfer(call_id.to_string(), consult_id.clone())
            .await;
        assert!(
            matches!(result, Err(TransferFailureReason::InvalidState)),
            "should be InvalidState after hangup, got {:?}",
            result
        );
    }

    /// Race condition: call is hung up (removed from registry) before execute_blind_transfer.
    #[tokio::test]
    async fn test_execute_blind_transfer_after_hangup_returns_invalid_state() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-race-blind-001";
        let target = "sip:target@local";
        register_talking_call(&registry, call_id);

        // Simulate hangup by removing the original call from registry
        registry.remove(call_id);

        // execute_blind_transfer should fail because original call is gone
        let result = ctrl
            .execute_blind_transfer(call_id.to_string(), target.to_string())
            .await;
        assert!(
            matches!(result, Err(TransferFailureReason::InvalidState)),
            "should be InvalidState after hangup, got {:?}",
            result
        );
    }
}
