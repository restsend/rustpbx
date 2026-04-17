use crate::call::domain::{CallCommand, LegId, P2PMode};
use crate::call::runtime::SessionId;
use crate::media::Track;
use crate::proxy::active_call_registry::{
    ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
};
use crate::proxy::proxy_call::sip_session::{SipSession, SipSessionHandle};
use crate::rwi::gateway::RwiGateway;
use crate::rwi::proto::RwiEvent;
use futures::FutureExt;
use rsipstack::dialog::dialog::DialogState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Result of a transfer attempt via REFER
#[derive(Debug, Clone)]
pub enum ReferTransferResult {
    /// REFER was accepted (202)
    Accepted,
    /// REFER was rejected with specific status
    Rejected { status: u16 },
    /// REFER not supported (405/420/501) - should trigger 3PCC fallback
    NotSupported { status: u16 },
    /// REFER timed out
    Timeout,
    /// Internal error
    InternalError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransferMode {
    SipRefer,
    Replaces,
    ThreePccFallback,
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
    ThreePccFailed,
    Timeout,
    Cancelled,
    InvalidTarget,
    InvalidState,
    BridgeFailed,
    InternalError,
}

impl TransferFailureReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferFailureReason::ReferRejected => "refer_rejected",
            TransferFailureReason::ThreePccFailed => "3pcc_failed",
            TransferFailureReason::Timeout => "timeout",
            TransferFailureReason::Cancelled => "cancelled",
            TransferFailureReason::InvalidTarget => "invalid_target",
            TransferFailureReason::InvalidState => "invalid_state",
            TransferFailureReason::BridgeFailed => "bridge_failed",
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

    pub fn set_error(&mut self, message: String) {
        self.error_message = Some(message);
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
    pub three_pcc_fallback_enabled: bool,
    pub refer_timeout_secs: u64,
    pub three_pcc_timeout_secs: u64,
    pub max_concurrent_transfers: usize,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            refer_enabled: true,
            attended_enabled: true,
            three_pcc_fallback_enabled: true,
            refer_timeout_secs: 30,
            three_pcc_timeout_secs: 60,
            max_concurrent_transfers: 1000,
        }
    }
}

pub struct TransferController {
    config: TransferConfig,
    transactions: Arc<RwLock<HashMap<String, TransferTransaction>>>,
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
            transactions: Arc::new(RwLock::new(HashMap::new())),
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

    pub async fn initiate_blind_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        if !self.config.refer_enabled {
            return Err(TransferFailureReason::InternalError);
        }

        // Record metric
        crate::metrics::transfer::attempt_total("refer", "blind");

        self.verify_call_state_for_transfer(&call_id).await?;

        let mut transaction =
            TransferTransaction::new(call_id.clone(), target.clone(), TransferMode::SipRefer);
        transaction.update_status(TransferStatus::Accepted);

        {
            let mut txs = self.transactions.write().await;
            if txs.len() >= self.config.max_concurrent_transfers {
                crate::metrics::transfer::failed_total("refer", "max_concurrent_reached");
                return Err(TransferFailureReason::InternalError);
            }
            txs.insert(transaction.transfer_id.clone(), transaction.clone());
        }

        crate::metrics::transfer::set_active_transfers(self.transactions.read().await.len());

        let _handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        let gw = self.gateway.read().await;
        let event = RwiEvent::CallTransferAccepted {
            call_id: call_id.clone(),
        };
        gw.send_event_to_call_owner(&call_id, &event);

        Ok(transaction)
    }

    pub async fn initiate_replace_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        if !self.config.refer_enabled {
            return Err(TransferFailureReason::InternalError);
        }

        crate::metrics::transfer::attempt_total("refer", "replace");

        self.verify_call_state_for_transfer(&call_id).await?;

        let mut transaction =
            TransferTransaction::new(call_id.clone(), target.clone(), TransferMode::Replaces);
        transaction.update_status(TransferStatus::Accepted);

        {
            let mut txs = self.transactions.write().await;
            if txs.len() >= self.config.max_concurrent_transfers {
                crate::metrics::transfer::failed_total("refer", "max_concurrent_reached");
                return Err(TransferFailureReason::InternalError);
            }
            txs.insert(transaction.transfer_id.clone(), transaction.clone());
        }

        crate::metrics::transfer::set_active_transfers(self.transactions.read().await.len());

        let _handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        let gw = self.gateway.read().await;
        let event = RwiEvent::CallTransferAccepted {
            call_id: call_id.clone(),
        };
        gw.send_event_to_call_owner(&call_id, &event);

        Ok(transaction)
    }

    /// Execute a complete blind transfer with automatic 3PCC fallback.
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
    /// will arrive asynchronously via `RwiGateway::send_event_to_call_owner`.
    pub async fn execute_blind_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        // Step 1: Create transaction and verify state
        let tx = self
            .initiate_blind_transfer(call_id.clone(), target.clone())
            .await?;

        info!(transfer_id = %tx.transfer_id, %call_id, %target, "Dispatching blind transfer command");

        // Step 2: Send the Transfer command – result is async via RWI events
        match self.try_refer_transfer(&tx).await {
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
            Err(_) => {
                // Should not happen with the new fire-and-forget implementation,
                // but handle defensively.
                warn!(transfer_id = %tx.transfer_id, "Unexpected error dispatching transfer");
                self.fail_transfer(&tx.transfer_id, TransferFailureReason::InternalError, None)
                    .await;
                Err(TransferFailureReason::InternalError)
            }
        }
    }

    pub async fn execute_replace_transfer(
        &self,
        call_id: String,
        target: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let tx = self
            .initiate_replace_transfer(call_id.clone(), target.clone())
            .await?;

        match self.try_refer_transfer_with_replaces(&tx).await {
            Ok(_) => Ok(tx),
            Err(ReferTransferResult::InternalError(_)) => {
                self.fail_transfer(&tx.transfer_id, TransferFailureReason::InternalError, None)
                    .await;
                Err(TransferFailureReason::InternalError)
            }
            Err(_) => {
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
    ) -> Result<(), ReferTransferResult> {
        let handle = self
            .get_handle(&tx.call_id)
            .await
            .ok_or_else(|| ReferTransferResult::InternalError("Call not found".to_string()))?;

        let leg_id = LegId::new(&tx.call_id);
        handle
            .send_command(CallCommand::Transfer {
                leg_id,
                target: tx.target.clone(),
                attended: false,
            })
            .map_err(|e| {
                ReferTransferResult::InternalError(format!(
                    "Failed to send transfer command: {}",
                    e
                ))
            })
    }

    async fn try_refer_transfer_with_replaces(
        &self,
        tx: &TransferTransaction,
    ) -> Result<(), ReferTransferResult> {
        let handle = self
            .get_handle(&tx.call_id)
            .await
            .ok_or_else(|| ReferTransferResult::InternalError("Call not found".to_string()))?;

        let leg_id = LegId::new(&tx.call_id);
        handle
            .send_command(CallCommand::Transfer {
                leg_id,
                target: tx.target.clone(),
                attended: true,
            })
            .map_err(|e| {
                ReferTransferResult::InternalError(format!(
                    "Failed to send replace transfer command: {}",
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
            let mut txs = self.transactions.write().await;
            if let Some(tx) = txs.get_mut(transfer_id) {
                tx.update_status(TransferStatus::Failed(reason.clone()));
                tx.sip_status = sip_status;
                Some(tx.clone())
            } else {
                None
            }
        };
        if let Some(failed_tx) = failed_tx_opt {
            let gw = self.gateway.read().await;
            let event = RwiEvent::CallTransferFailed {
                call_id: failed_tx.call_id.clone(),
                sip_status,
                reason: Some(reason.as_str().to_string()),
            };
            gw.send_event_to_call_owner(&failed_tx.call_id, &event);
        }
    }

    pub async fn initiate_attended_transfer(
        &self,
        call_id: String,
        target: String,
        timeout_secs: Option<u32>,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        if !self.config.attended_enabled {
            return Err(TransferFailureReason::InternalError);
        }

        self.verify_call_state_for_transfer(&call_id).await?;

        let _timeout = timeout_secs
            .map(|s| s as u64)
            .unwrap_or(self.config.three_pcc_timeout_secs);

        let mut transaction =
            TransferTransaction::new(call_id.clone(), target.clone(), TransferMode::SipRefer);
        transaction.consultation_call_id = Some(Uuid::new_v4().to_string());
        transaction.original_leg = Some(call_id.clone());

        {
            let mut txs = self.transactions.write().await;
            if txs.len() >= self.config.max_concurrent_transfers {
                return Err(TransferFailureReason::InternalError);
            }
            txs.insert(transaction.transfer_id.clone(), transaction.clone());
        }

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

        let gw = self.gateway.read().await;
        let event = RwiEvent::CallTransferAccepted {
            call_id: call_id.clone(),
        };
        gw.send_event_to_call_owner(&call_id, &event);

        Ok(transaction)
    }

    pub async fn complete_attended_transfer(
        &self,
        call_id: String,
        consultation_call_id: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let txs = self.transactions.read().await;
        let transaction = txs
            .values()
            .find(|tx| {
                tx.call_id == call_id
                    && tx.consultation_call_id.as_ref() == Some(&consultation_call_id)
            })
            .cloned();
        drop(txs);

        let mut transaction = transaction.ok_or(TransferFailureReason::InvalidState)?;

        let handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        let leg_a = LegId::new(&call_id);
        let leg_b = LegId::new(&consultation_call_id);
        let _ = handle.send_command(CallCommand::Bridge {
            leg_a,
            leg_b,
            mode: crate::call::domain::P2PMode::Audio,
        });

        transaction.update_status(TransferStatus::Completed);

        let gw = self.gateway.read().await;
        let event = RwiEvent::CallTransferred {
            call_id: call_id.clone(),
        };
        gw.send_event_to_call_owner(&call_id, &event);

        Ok(transaction)
    }

    pub async fn cancel_attended_transfer(
        &self,
        consultation_call_id: String,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let txs = self.transactions.read().await;
        let transaction = txs
            .values()
            .find(|tx| tx.consultation_call_id.as_ref() == Some(&consultation_call_id))
            .cloned();
        drop(txs);

        let mut transaction = transaction.ok_or(TransferFailureReason::InvalidState)?;

        let handle = self.get_handle(&consultation_call_id).await;
        if let Some(handle) = handle {
            let _leg_id = LegId::new(&consultation_call_id);
            let _ = handle.send_command(CallCommand::Hangup(
                crate::call::domain::HangupCommand::local(
                    "transfer",
                    Some(crate::callrecord::CallRecordHangupReason::BySystem),
                    Some(487),
                ),
            ));
        }

        if let Some(ref original_call_id) = transaction.original_leg {
            let original_handle = self.get_handle(original_call_id).await;
            if let Some(original_handle) = original_handle {
                let leg_id = LegId::new(original_call_id);
                let _ = original_handle.send_command(CallCommand::Unhold { leg_id });
            }

            let gw = self.gateway.read().await;
            let event = RwiEvent::CallTransferFailed {
                call_id: original_call_id.clone(),
                sip_status: Some(487),
                reason: Some("cancelled".to_string()),
            };
            gw.send_event_to_call_owner(original_call_id, &event);
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
            let mut txs = self.transactions.write().await;
            let tx = txs.get_mut(&transfer_id)?;

            tx.set_sip_status(sip_status);

            let gw_event = if sip_status >= 200 && sip_status < 300 {
                if tx.status == TransferStatus::Accepted {
                    GatewayEvent::None
                } else {
                    tx.update_status(TransferStatus::Accepted);
                    GatewayEvent::Accepted(tx.call_id.clone())
                }
            } else if sip_status >= 400 {
                let allow_3pcc_fallback =
                    self.config.three_pcc_fallback_enabled && tx.mode == TransferMode::SipRefer;
                let reason = if allow_3pcc_fallback {
                    tx.update_status(TransferStatus::NotifyTrying);
                    TransferFailureReason::ReferRejected
                } else {
                    tx.update_status(TransferStatus::Failed(TransferFailureReason::ReferRejected));
                    TransferFailureReason::ReferRejected
                };

                if !allow_3pcc_fallback {
                    GatewayEvent::Failed {
                        call_id: tx.call_id.clone(),
                        sip_status,
                        reason,
                    }
                } else {
                    GatewayEvent::None
                }
            } else {
                GatewayEvent::None
            };

            (tx.clone(), gw_event)
        };

        match gw_event {
            GatewayEvent::Accepted(call_id) => {
                let gw = self.gateway.read().await;
                let event = RwiEvent::CallTransferAccepted {
                    call_id: call_id.clone(),
                };
                gw.send_event_to_call_owner(&call_id, &event);
            }
            GatewayEvent::Failed {
                call_id,
                sip_status,
                reason,
            } => {
                let gw = self.gateway.read().await;
                let event = RwiEvent::CallTransferFailed {
                    call_id: call_id.clone(),
                    sip_status: Some(sip_status),
                    reason: Some(reason.as_str().to_string()),
                };
                gw.send_event_to_call_owner(&call_id, &event);
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
            TransferFailed(TransferTransaction, TransferFailureReason),
            None,
        }

        let (result_tx, post_action) = {
            let mut txs = self.transactions.write().await;
            let tx = txs.get_mut(&transfer_id)?;

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
                    let active_count = txs
                        .values()
                        .filter(|t| {
                            !matches!(
                                t.status,
                                TransferStatus::Completed | TransferStatus::Failed(_)
                            )
                        })
                        .count();
                    crate::metrics::transfer::set_active_transfers(active_count);
                    let completed_tx = txs.get(&transfer_id)?.clone();
                    return {
                        drop(txs);
                        let gw = self.gateway.read().await;
                        let event = RwiEvent::CallTransferred {
                            call_id: completed_tx.call_id.clone(),
                        };
                        gw.send_event_to_call_owner(&completed_tx.call_id, &event);
                        Some(completed_tx)
                    };
                }
                _ if notify_status >= 400 => {
                    crate::metrics::transfer::failed_total(
                        "refer",
                        &format!("sip_{}", notify_status),
                    );
                    if self.config.three_pcc_fallback_enabled && tx.mode == TransferMode::SipRefer {
                        crate::metrics::transfer::three_pcc_fallback_triggered();
                        tx.mode = TransferMode::ThreePccFallback;
                        tx.update_status(TransferStatus::NotifyTrying);
                        PostAction::None
                    } else {
                        let reason = TransferFailureReason::ReferRejected;
                        tx.update_status(TransferStatus::Failed(reason.clone()));
                        PostAction::TransferFailed(tx.clone(), reason)
                    }
                }
                _ => PostAction::None,
            };

            (tx.clone(), post_action)
        };

        match post_action {
            PostAction::TransferFailed(failed_tx, reason) => {
                let gw = self.gateway.read().await;
                let event = RwiEvent::CallTransferFailed {
                    call_id: failed_tx.call_id.clone(),
                    sip_status: Some(notify_status),
                    reason: Some(reason.as_str().to_string()),
                };
                gw.send_event_to_call_owner(&failed_tx.call_id, &event);
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
        let transfer_id = {
            let txs = self.transactions.read().await;
            txs.values()
                .find(|tx| tx.call_id == call_id && !tx.is_terminal())
                .map(|tx| tx.transfer_id.clone())?
        };
        self.handle_refer_response(transfer_id, sip_status).await
    }

    /// Handle a REFER NOTIFY matched by `call_id`.
    pub async fn handle_notify_by_call_id(
        &self,
        call_id: &str,
        notify_status: u16,
    ) -> Option<TransferTransaction> {
        let transfer_id = {
            let txs = self.transactions.read().await;
            txs.values()
                .find(|tx| tx.call_id == call_id && !tx.is_terminal())
                .map(|tx| tx.transfer_id.clone())?
        };
        self.handle_notify(transfer_id, notify_status).await
    }

    pub async fn fallback_to_3pcc(&self, transfer_id: String) -> Option<TransferTransaction> {
        let mut txs = self.transactions.write().await;
        let tx = txs.get_mut(&transfer_id)?;

        if !self.config.three_pcc_fallback_enabled {
            let reason = TransferFailureReason::ReferRejected;
            tx.update_status(TransferStatus::Failed(reason.clone()));
            let failed_tx = tx.clone();
            drop(txs);
            let gw = self.gateway.read().await;
            let event = RwiEvent::CallTransferFailed {
                call_id: failed_tx.call_id.clone(),
                sip_status: failed_tx.sip_status,
                reason: Some(reason.as_str().to_string()),
            };
            gw.send_event_to_call_owner(&failed_tx.call_id, &event);
            return Some(failed_tx);
        }

        tx.mode = TransferMode::ThreePccFallback;
        tx.update_status(TransferStatus::NotifyTrying);

        Some(tx.clone())
    }

    /// Execute 3PCC (Third Party Call Control) transfer
    ///
    /// This is the fallback mechanism when REFER is not supported.
    /// The 3PCC flow:
    /// 1. PBX originates a new call to the transfer target
    /// 2. When target answers, PBX bridges the original call with the new call
    /// 3. PBX then hangs up the original call leg to the transferor
    ///
    /// Returns the transfer transaction or None if not found
    pub async fn execute_3pcc_transfer(
        &self,
        transfer_id: &str,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        // Get transaction
        let tx = {
            let txs = self.transactions.read().await;
            txs.get(transfer_id).cloned()
        }
        .ok_or(TransferFailureReason::InvalidState)?;

        // Verify we're in 3PCC mode
        if tx.mode != TransferMode::ThreePccFallback {
            return Err(TransferFailureReason::InvalidState);
        }

        // Check if sip_server is available
        let sip_server = self
            .sip_server
            .as_ref()
            .ok_or(TransferFailureReason::InternalError)?;

        let call_id = tx.call_id.clone();
        let target = tx.target.clone();
        let original_call_id = call_id.clone(); // Capture for spawned task

        info!(%transfer_id, %call_id, %target, "Executing 3PCC transfer with originate");

        // Record 3PCC attempt metric
        crate::metrics::transfer::attempt_total("3pcc", "blind");

        // Get handle for original call and hold it
        let original_handle = self
            .get_handle(&call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        // Step 1: Hold the original call
        let leg_id = LegId::new(&call_id);
        let _ = original_handle.send_command(CallCommand::Hold {
            leg_id: leg_id.clone(),
            music: Some(crate::call::domain::MediaSource::File {
                path: "hold.wav".to_string(),
            }),
        });

        // Step 2: Originate new call to transfer target
        let new_call_id = Uuid::new_v4().to_string();
        info!(%new_call_id, %target, "Originating new call for 3PCC transfer");

        // Perform originate
        match self
            .originate_for_3pcc(
                &new_call_id,
                &target,
                sip_server,
                transfer_id,
                &original_call_id,
            )
            .await
        {
            Ok(_) => {
                info!(%new_call_id, "3PCC originate initiated successfully");

                // Update transaction with new call leg
                {
                    let mut txs = self.transactions.write().await;
                    if let Some(tx) = txs.get_mut(transfer_id) {
                        tx.consultation_call_id = Some(new_call_id.clone());
                        tx.update_status(TransferStatus::NotifyProgress);
                    }
                }

                // Emit 3PCC started event
                let gw = self.gateway.read().await;
                let event = RwiEvent::CallTransferred {
                    call_id: call_id.clone(),
                };
                gw.send_event_to_call_owner(&call_id, &event);

                info!(%transfer_id, "3PCC transfer initiated, waiting for answer");
                Ok(tx)
            }
            Err(e) => {
                error!(%new_call_id, error = %e, "Failed to originate 3PCC call");

                // Record 3PCC failure metric
                crate::metrics::transfer::three_pcc_failed("originate_failed");
                crate::metrics::transfer::failed_total("3pcc", "originate_failed");

                // Rollback: unhold original call
                let _ = original_handle.send_command(CallCommand::Unhold {
                    leg_id: LegId::new(&call_id),
                });

                // Update transaction as failed
                {
                    let mut txs = self.transactions.write().await;
                    if let Some(tx) = txs.get_mut(transfer_id) {
                        tx.update_status(TransferStatus::Failed(
                            TransferFailureReason::ThreePccFailed,
                        ));
                        tx.error_message = Some(format!("Originate failed: {}", e));
                    }
                }

                // Emit failure event
                let gw = self.gateway.read().await;
                let event = RwiEvent::CallTransferFailed {
                    call_id: call_id.clone(),
                    sip_status: None,
                    reason: Some(format!("3pcc_originate_failed: {}", e)),
                };
                gw.send_event_to_call_owner(&call_id, &event);

                Err(TransferFailureReason::ThreePccFailed)
            }
        }
    }

    /// Originate a call for 3PCC transfer
    ///
    /// This is an internal helper that performs the actual SIP originate
    async fn originate_for_3pcc(
        &self,
        call_id: &str,
        target: &str,
        sip_server: &crate::proxy::server::SipServerRef,
        transfer_id: &str,
        original_call_id: &str,
    ) -> Result<(), String> {
        // Parse destination URI
        let destination_uri: rsipstack::sip::Uri = rsipstack::sip::Uri::try_from(target)
            .map_err(|e| format!("Invalid target URI: {:?}", e))?;

        // Build caller URI (use server realm)
        let realm = sip_server
            .proxy_config
            .realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| sip_server.proxy_config.addr.clone());
        let caller_uri_str = format!("sip:transfer@{}", realm);
        let caller_uri: rsipstack::sip::Uri =
            rsipstack::sip::Uri::try_from(caller_uri_str.as_str())
                .map_err(|e| format!("Invalid caller URI: {:?}", e))?;

        // Build headers
        let headers = vec![
            rsipstack::sip::Header::Other("Max-Forwards".into(), "70".into()),
            rsipstack::sip::Header::Other("X-Transfer-Id".into(), transfer_id.into()),
        ];

        // Get external IP for SDP
        let external_ip = sip_server
            .rtp_config
            .external_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());

        // Create media track and SDP offer
        let media_track = crate::media::RtpTrackBuilder::new(format!("3pcc-{}", call_id))
            .with_cancel_token(tokio_util::sync::CancellationToken::new())
            .with_external_ip(external_ip)
            .build();

        let sdp_offer = media_track
            .local_description()
            .await
            .map_err(|e| format!("Failed to generate SDP: {}", e))?;

        // Build invite options
        let invite_option = rsipstack::dialog::invitation::InviteOption {
            callee: destination_uri,
            caller: caller_uri.clone(),
            contact: caller_uri,
            content_type: Some("application/sdp".to_string()),
            offer: Some(sdp_offer.into_bytes()),
            destination: None,
            credential: None,
            headers: Some(headers),
            call_id: Some(call_id.to_string()),
            ..Default::default()
        };

        let dialog_layer = sip_server.dialog_layer.clone();
        let gateway = self.gateway.clone();
        let registry = self.call_registry.clone();
        let call_id_for_spawn = call_id.to_string();
        let transfer_id_for_spawn = transfer_id.to_string();
        let target_for_log = target.to_string();
        let orig_call_id_spawn = original_call_id.to_string(); // Clone for spawned task

        // Spawn originate task
        tokio::spawn(async move {
            let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut invitation = dialog_layer.do_invite(invite_option, state_tx).boxed();

            // Create session and register
            let id = SessionId::from(call_id_for_spawn.clone());
            let (handle, mut _cmd_rx) = SipSession::with_handle(id);

            let entry = ActiveProxyCallEntry {
                session_id: call_id_for_spawn.clone(),
                caller: Some("transfer".to_string()),
                callee: Some(target_for_log),
                direction: "outbound".to_string(),
                started_at: chrono::Utc::now(),
                answered_at: None,
                status: ActiveProxyCallStatus::Ringing,
            };
            registry.upsert(entry, handle.clone());

            // Wait for invitation result
            let timeout_secs = 60;
            let result = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                async {
                    loop {
                        tokio::select! {
                            res = &mut invitation => break res,
                            state = state_rx.recv() => {
                                if let Some(state) = state {
                                    match state {
                                        DialogState::Calling(_) => {
                                            let gw = gateway.read().await;
                                            gw.send_event_to_call_owner(
                                                &call_id_for_spawn,
                                                &RwiEvent::CallRinging { call_id: call_id_for_spawn.clone() },
                                            );
                                        }
                                        DialogState::Early(_, _) => {
                                            let gw = gateway.read().await;
                                            gw.send_event_to_call_owner(
                                                &call_id_for_spawn,
                                                &RwiEvent::CallEarlyMedia { call_id: call_id_for_spawn.clone() },
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            ).await;

            match result {
                Ok(Ok((_, Some(resp))))
                    if resp.status_code().kind()
                        == rsipstack::sip::status_code::StatusCodeKind::Successful =>
                {
                    info!(%call_id_for_spawn, "3PCC originate answered, completing transfer");

                    // Record 3PCC success metrics
                    crate::metrics::transfer::three_pcc_success();
                    crate::metrics::transfer::success_total("3pcc");

                    // Update registry
                    registry.update(&call_id_for_spawn, |entry| {
                        entry.answered_at = Some(chrono::Utc::now());
                        entry.status = ActiveProxyCallStatus::Talking;
                    });

                    // Send answered event
                    let gw = gateway.read().await;
                    gw.send_event_to_call_owner(
                        &call_id_for_spawn,
                        &RwiEvent::CallAnswered {
                            call_id: call_id_for_spawn.clone(),
                        },
                    );

                    // Bridge the calls - find original call and bridge with new call
                    // The transfer_id_for_spawn contains the transfer ID which maps to original call
                    // We need to get the original call_id from the transfer transaction
                    // For now, we rely on the caller to complete the bridge
                    info!(%call_id_for_spawn, %transfer_id_for_spawn, "3PCC call answered, scheduling bridge");

                    // Schedule bridge completion after short delay to allow media setup
                    let registry_clone = registry.clone();
                    let gateway_clone = gateway.clone();
                    let new_call_id = call_id_for_spawn.clone();
                    let orig_call_id = orig_call_id_spawn.clone();

                    tokio::spawn(async move {
                        // Wait for media to stabilize
                        tokio::time::sleep(Duration::from_millis(500)).await;

                        // Bridge original call with new 3PCC call
                        if let Some(orig_handle) = registry_clone.get_handle(&orig_call_id) {
                            let leg_a = LegId::new(&orig_call_id);
                            let leg_b = LegId::new(&new_call_id);

                            match orig_handle.send_command(CallCommand::Bridge {
                                leg_a,
                                leg_b,
                                mode: P2PMode::Audio,
                            }) {
                                Ok(_) => {
                                    info!(%orig_call_id, %new_call_id, "3PCC bridge command sent successfully");

                                    // Emit transfer completion event
                                    let gw = gateway_clone.read().await;
                                    gw.send_event_to_call_owner(
                                        &orig_call_id,
                                        &RwiEvent::CallTransferred {
                                            call_id: orig_call_id.clone(),
                                        },
                                    );
                                }
                                Err(e) => {
                                    error!(%orig_call_id, %new_call_id, error = %e, "Failed to bridge 3PCC calls");

                                    // Emit failure event
                                    let gw = gateway_clone.read().await;
                                    gw.send_event_to_call_owner(
                                        &orig_call_id,
                                        &RwiEvent::CallTransferFailed {
                                            call_id: orig_call_id.clone(),
                                            sip_status: None,
                                            reason: Some(format!("bridge_failed: {}", e)),
                                        },
                                    );
                                }
                            }
                        } else {
                            warn!(%orig_call_id, "Original call not found for 3PCC bridging");
                        }
                    });
                }
                Ok(Ok((_, Some(resp)))) => {
                    warn!(%call_id_for_spawn, status = %resp.status_code(), "3PCC originate failed");
                    crate::metrics::transfer::three_pcc_failed(&format!(
                        "sip_{}",
                        resp.status_code()
                    ));
                    registry.remove(&call_id_for_spawn);
                }
                Ok(Err(e)) => {
                    warn!(%call_id_for_spawn, error = %e, "3PCC originate error");
                    crate::metrics::transfer::three_pcc_failed("error");
                    registry.remove(&call_id_for_spawn);
                }
                Err(_) => {
                    warn!(%call_id_for_spawn, "3PCC originate timeout");
                    crate::metrics::transfer::three_pcc_failed("timeout");
                    registry.remove(&call_id_for_spawn);
                }
                _ => {
                    registry.remove(&call_id_for_spawn);
                }
            }
        });

        Ok(())
    }

    /// Complete 3PCC transfer by bridging calls
    ///
    /// This should be called when the new call leg is answered
    pub async fn complete_3pcc_transfer(
        &self,
        transfer_id: &str,
    ) -> Result<TransferTransaction, TransferFailureReason> {
        let tx = {
            let txs = self.transactions.read().await;
            txs.get(transfer_id).cloned()
        }
        .ok_or(TransferFailureReason::InvalidState)?;

        if tx.mode != TransferMode::ThreePccFallback {
            return Err(TransferFailureReason::InvalidState);
        }

        let call_id = &tx.call_id;
        let consultation_call_id = tx
            .consultation_call_id
            .as_ref()
            .ok_or(TransferFailureReason::InvalidState)?;

        info!(%transfer_id, %call_id, %consultation_call_id, "Completing 3PCC transfer");

        // Get handles for both calls
        let original_handle = self
            .get_handle(call_id)
            .await
            .ok_or(TransferFailureReason::InvalidState)?;

        // Bridge the calls
        let leg_a = LegId::new(call_id);
        let leg_b = LegId::new(consultation_call_id);

        let _ = original_handle.send_command(CallCommand::Bridge {
            leg_a,
            leg_b,
            mode: crate::call::domain::P2PMode::Audio,
        });

        // Update transaction status
        {
            let mut txs = self.transactions.write().await;
            if let Some(tx) = txs.get_mut(transfer_id) {
                tx.update_status(TransferStatus::Completed);
            }
        }

        // Emit completion event
        let gw = self.gateway.read().await;
        let event = RwiEvent::CallTransferred {
            call_id: call_id.clone(),
        };
        gw.send_event_to_call_owner(call_id, &event);

        info!(%transfer_id, "3PCC transfer completed");

        Ok(tx)
    }

    /// Handle 3PCC failure and rollback
    ///
    /// This should be called if the new call leg fails
    pub async fn handle_3pcc_failure(
        &self,
        transfer_id: &str,
        reason: &str,
    ) -> Result<(), TransferFailureReason> {
        let tx = {
            let txs = self.transactions.read().await;
            txs.get(transfer_id).cloned()
        }
        .ok_or(TransferFailureReason::InvalidState)?;

        let call_id = &tx.call_id;

        warn!(%transfer_id, %call_id, %reason, "3PCC transfer failed");

        // Get handle for original call
        if let Some(original_handle) = self.get_handle(call_id).await {
            // Unhold the original call (rollback)
            let leg_id = LegId::new(call_id);
            let _ = original_handle.send_command(CallCommand::Unhold {
                leg_id: leg_id.clone(),
            });

            // Also send unhold to the call
            let _ = original_handle.send_command(CallCommand::Unhold { leg_id });
        }

        // Update transaction status
        {
            let mut txs = self.transactions.write().await;
            if let Some(tx) = txs.get_mut(transfer_id) {
                tx.update_status(TransferStatus::Failed(
                    TransferFailureReason::ThreePccFailed,
                ));
                tx.error_message = Some(reason.to_string());
            }
        }

        // Emit failure event
        let gw = self.gateway.read().await;
        let event = RwiEvent::CallTransferFailed {
            call_id: call_id.clone(),
            sip_status: None,
            reason: Some(format!("3pcc_failed: {}", reason)),
        };
        gw.send_event_to_call_owner(call_id, &event);

        Ok(())
    }

    pub async fn get_transaction(&self, transfer_id: &str) -> Option<TransferTransaction> {
        let txs = self.transactions.read().await;
        txs.get(transfer_id).cloned()
    }

    pub async fn get_transaction_by_call_id(&self, call_id: &str) -> Option<TransferTransaction> {
        let txs = self.transactions.read().await;
        txs.values().find(|tx| tx.call_id == call_id).cloned()
    }

    pub async fn cleanup_terminal_transactions(&self) -> usize {
        let mut txs = self.transactions.write().await;
        let before = txs.len();
        txs.retain(|_, tx| !tx.is_terminal());
        before - txs.len()
    }

    pub async fn get_active_transfer_count(&self) -> usize {
        let txs = self.transactions.read().await;
        txs.values().filter(|tx| !tx.is_terminal()).count()
    }

    pub async fn cancel_all_transfers_for_call(&self, call_id: &str) -> usize {
        let mut txs = self.transactions.write().await;
        let mut count = 0;
        for tx in txs.values_mut() {
            if tx.call_id == call_id && !tx.is_terminal() {
                tx.update_status(TransferStatus::Canceled);
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;

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
    ) -> tokio::sync::mpsc::UnboundedReceiver<CallCommand> {
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
                assert_eq!(leg_id.as_str(), call_id);
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
                assert_eq!(leg_id.as_str(), call_id);
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
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string(), None)
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
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string(), None)
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

    /// A 4xx REFER response with 3PCC disabled emits a failure event and marks Failed.
    #[tokio::test]
    async fn test_handle_refer_response_rejected_no_3pcc() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let mut config = TransferConfig::default();
        config.three_pcc_fallback_enabled = false;
        let ctrl = TransferController::new(config, Arc::clone(&registry), gateway);

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

    /// NOTIFY 4xx with 3PCC disabled marks the transaction Failed.
    #[tokio::test]
    async fn test_handle_notify_4xx_no_3pcc_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let mut config = TransferConfig::default();
        config.three_pcc_fallback_enabled = false;
        let ctrl = TransferController::new(config, Arc::clone(&registry), gateway);

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
            "should be Failed with no 3PCC fallback, got {:?}",
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

    // ────────────────────────────────────────────────────────────────────────────
    // cancel_all_transfers_for_call tests
    // ────────────────────────────────────────────────────────────────────────────

    /// cancel_all_transfers_for_call returns 0 when there are no transfers.
    #[tokio::test]
    async fn test_cancel_all_transfers_for_call_empty() {
        let ctrl = make_controller();
        let count = ctrl.cancel_all_transfers_for_call("call-x").await;
        assert_eq!(count, 0);
    }

    /// cancel_all_transfers_for_call cancels all non-terminal transfers for a call.
    #[tokio::test]
    async fn test_cancel_all_transfers_for_call_cancels_pending() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-cancel-all";
        register_talking_call(&registry, call_id);

        // Create two transfers for the same call
        let _tx1 = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t1@local".to_string())
            .await
            .expect("first initiate should succeed");
        // Registry handle is consumed by first call, re-register for second
        register_talking_call(&registry, call_id);
        let _tx2 = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t2@local".to_string())
            .await
            .ok(); // may fail if handle already moved

        let count = ctrl.cancel_all_transfers_for_call(call_id).await;
        assert!(count >= 1, "at least one transfer should be cancelled");
    }

    // ────────────────────────────────────────────────────────────────────────────
    // cleanup_terminal_transactions tests
    // ────────────────────────────────────────────────────────────────────────────

    /// cleanup_terminal_transactions removes only completed/failed/cancelled entries.
    #[tokio::test]
    async fn test_cleanup_terminal_transactions() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-cleanup";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate_blind_transfer should succeed");

        // Mark the transfer as completed via NOTIFY 200
        ctrl.handle_notify(tx.transfer_id.clone(), 200).await;

        // Count total transactions before cleanup (includes terminal ones)
        let before_total = {
            let txs = ctrl.transactions.read().await;
            txs.len()
        };
        assert!(
            before_total >= 1,
            "should have at least one transaction before cleanup"
        );

        // Cleanup terminal transactions
        let removed = ctrl.cleanup_terminal_transactions().await;
        assert!(
            removed >= 1,
            "at least one terminal transaction should be removed"
        );

        // After cleanup, total count should be lower
        let after_total = {
            let txs = ctrl.transactions.read().await;
            txs.len()
        };
        assert!(
            after_total < before_total,
            "total count should decrease after cleanup"
        );
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
        assert_eq!(
            TransferFailureReason::ThreePccFailed.as_str(),
            "3pcc_failed"
        );
        assert_eq!(TransferFailureReason::Timeout.as_str(), "timeout");
        assert_eq!(TransferFailureReason::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn test_transfer_config_default() {
        let config = TransferConfig::default();
        assert!(config.refer_enabled);
        assert!(config.attended_enabled);
        assert!(config.three_pcc_fallback_enabled);
        assert_eq!(config.refer_timeout_secs, 30);
        assert_eq!(config.three_pcc_timeout_secs, 60);
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

        tx.set_error("Test error".to_string());
        assert_eq!(tx.error_message, Some("Test error".to_string()));
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

    /// REFER 5xx (e.g. 500) with 3PCC disabled marks the transaction Failed.
    #[tokio::test]
    async fn test_handle_refer_response_5xx_fails() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let mut config = TransferConfig::default();
        config.three_pcc_fallback_enabled = false;
        let ctrl = TransferController::new(config, Arc::clone(&registry), gateway);

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

    /// REFER 4xx with 3PCC **enabled** moves to NotifyTrying (fallback pending), not Failed.
    #[tokio::test]
    async fn test_handle_refer_response_4xx_with_3pcc_stays_trying() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-refer-4xx-3pcc";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl
            .handle_refer_response(tx.transfer_id.clone(), 405)
            .await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(
            updated.status,
            TransferStatus::NotifyTrying,
            "4xx + 3PCC enabled should transition to NotifyTrying, got {:?}",
            updated.status
        );
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

    /// NOTIFY 4xx with 3PCC **enabled** triggers the 3PCC fallback path (mode →
    /// ThreePccFallback, status → NotifyTrying) instead of marking Failed.
    #[tokio::test]
    async fn test_handle_notify_4xx_with_3pcc_triggers_fallback() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-notify-4xx-3pcc";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.handle_notify(tx.transfer_id.clone(), 486).await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(
            updated.mode,
            TransferMode::ThreePccFallback,
            "mode should be ThreePccFallback, got {:?}",
            updated.mode
        );
        assert_eq!(
            updated.status,
            TransferStatus::NotifyTrying,
            "status should be NotifyTrying, got {:?}",
            updated.status
        );
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
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string(), None)
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
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string(), None)
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
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string(), None)
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

    // ────────────────────────────────────────────────────────────────────────────
    // fallback_to_3pcc
    // ────────────────────────────────────────────────────────────────────────────

    /// fallback_to_3pcc switches the transaction mode to ThreePccFallback and
    /// sets status to NotifyTrying.
    #[tokio::test]
    async fn test_fallback_to_3pcc_mode_transition() {
        let (ctrl, registry) = make_controller_with_registry();
        let call_id = "call-3pcc-fallback-001";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.fallback_to_3pcc(tx.transfer_id.clone()).await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert_eq!(updated.mode, TransferMode::ThreePccFallback);
        assert_eq!(updated.status, TransferStatus::NotifyTrying);
    }

    /// fallback_to_3pcc returns None for unknown transfer_id.
    #[tokio::test]
    async fn test_fallback_to_3pcc_unknown_id() {
        let ctrl = make_controller();
        let result = ctrl.fallback_to_3pcc("no-such-id".to_string()).await;
        assert!(result.is_none());
    }

    /// fallback_to_3pcc with 3PCC disabled emits a failure event and marks Failed.
    #[tokio::test]
    async fn test_fallback_to_3pcc_disabled_marks_failed() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let gateway = Arc::new(RwLock::new(crate::rwi::gateway::RwiGateway::new()));
        let mut config = TransferConfig::default();
        config.three_pcc_fallback_enabled = false;
        let ctrl = TransferController::new(config, Arc::clone(&registry), gateway);

        let call_id = "call-3pcc-disabled";
        register_talking_call(&registry, call_id);

        let tx = ctrl
            .initiate_blind_transfer(call_id.to_string(), "sip:t@local".to_string())
            .await
            .expect("initiate should succeed");

        let result = ctrl.fallback_to_3pcc(tx.transfer_id.clone()).await;
        assert!(result.is_some());
        let updated = result.unwrap();
        assert!(
            matches!(updated.status, TransferStatus::Failed(_)),
            "3PCC disabled fallback should mark Failed, got {:?}",
            updated.status
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
            .initiate_attended_transfer(call_id.to_string(), "sip:consult@local".to_string(), None)
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
