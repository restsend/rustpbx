use std::sync::Arc;
use tokio::sync::RwLock;

use rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry;
use rustpbx::rwi::gateway::RwiGateway;
use rustpbx::rwi::proto::RwiEvent;
use rustpbx::rwi::transfer::{TransferConfig, TransferController, TransferFailureReason, TransferMode, TransferStatus};

#[tokio::test]
async fn test_transfer_controller_new() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let controller = TransferController::with_default_config(registry, gateway);
    
    let active_count = controller.get_active_transfer_count().await;
    assert_eq!(active_count, 0);
}

#[tokio::test]
async fn test_transfer_transaction_lifecycle() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let controller = TransferController::with_default_config(registry, gateway);
    
    let call_id = "test-call-001".to_string();
    let target = "sip:target@local".to_string();
    
    let tx = controller.initiate_blind_transfer(call_id.clone(), target.clone()).await;
    
    assert!(tx.is_err());
}

#[tokio::test]
async fn test_transfer_config_default() {
    let config = TransferConfig::default();
    assert!(config.refer_enabled);
    assert!(config.attended_enabled);
    assert!(config.three_pcc_fallback_enabled);
    assert_eq!(config.refer_timeout_secs, 30);
    assert_eq!(config.three_pcc_timeout_secs, 60);
    assert_eq!(config.max_concurrent_transfers, 1000);
}

#[tokio::test]
async fn test_transfer_mode_variants() {
    let sip_refer = TransferMode::SipRefer;
    let three_pcc = TransferMode::ThreePccFallback;
    
    assert_ne!(sip_refer, three_pcc);
    assert_eq!(sip_refer, TransferMode::SipRefer);
    assert_eq!(three_pcc, TransferMode::ThreePccFallback);
}

#[tokio::test]
async fn test_transfer_status_transitions() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let config = TransferConfig::default();
    let controller = TransferController::new(config, registry, gateway);
    
    let transfer_id = uuid::Uuid::new_v4().to_string();
    
    let result = controller.handle_refer_response(transfer_id.clone(), 100).await;
    assert!(result.is_none());
    
    let result = controller.handle_notify(transfer_id.clone(), 100).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_transfer_failure_reason_as_str() {
    assert_eq!(TransferFailureReason::ReferRejected.as_str(), "refer_rejected");
    assert_eq!(TransferFailureReason::ThreePccFailed.as_str(), "3pcc_failed");
    assert_eq!(TransferFailureReason::Timeout.as_str(), "timeout");
    assert_eq!(TransferFailureReason::Cancelled.as_str(), "cancelled");
    assert_eq!(TransferFailureReason::InvalidTarget.as_str(), "invalid_target");
    assert_eq!(TransferFailureReason::InvalidState.as_str(), "invalid_state");
    assert_eq!(TransferFailureReason::BridgeFailed.as_str(), "bridge_failed");
    assert_eq!(TransferFailureReason::InternalError.as_str(), "internal_error");
}

#[tokio::test]
async fn test_transfer_status_is_terminal() {
    use TransferStatus::*;
    
    assert!(!is_terminal(&Init));
    assert!(!is_terminal(&ReferSent));
    assert!(!is_terminal(&NotifyTrying));
    assert!(!is_terminal(&NotifyProgress));
    assert!(!is_terminal(&Accepted));
    assert!(is_terminal(&Completed));
    assert!(is_terminal(&Failed(TransferFailureReason::ReferRejected)));
    assert!(is_terminal(&Canceled));
    assert!(is_terminal(&TimedOut));
}

fn is_terminal(status: &TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Completed
            | TransferStatus::Failed(_)
            | TransferStatus::Canceled
            | TransferStatus::TimedOut
    )
}

#[tokio::test]
async fn test_rwi_event_transfer_failed_with_reason() {
    let event = RwiEvent::CallTransferFailed {
        call_id: "test-call".to_string(),
        sip_status: Some(486),
        reason: Some("refer_rejected".to_string()),
    };
    
    match event {
        RwiEvent::CallTransferFailed { call_id, sip_status, reason } => {
            assert_eq!(call_id, "test-call");
            assert_eq!(sip_status, Some(486));
            assert_eq!(reason, Some("refer_rejected".to_string()));
        }
        _ => panic!("Expected CallTransferFailed event"),
    }
}

#[tokio::test]
async fn test_rwi_event_conference_consult_dialing() {
    let event = RwiEvent::ConferenceConsultDialing {
        call_id: "call-001".to_string(),
        target: "sip:target@local".to_string(),
    };
    
    match event {
        RwiEvent::ConferenceConsultDialing { call_id, target } => {
            assert_eq!(call_id, "call-001");
            assert_eq!(target, "sip:target@local");
        }
        _ => panic!("Expected ConferenceConsultDialing event"),
    }
}

#[tokio::test]
async fn test_rwi_event_conference_consult_connected() {
    let event = RwiEvent::ConferenceConsultConnected {
        call_id: "call-001".to_string(),
        target: "sip:target@local".to_string(),
    };
    
    match event {
        RwiEvent::ConferenceConsultConnected { call_id, target } => {
            assert_eq!(call_id, "call-001");
            assert_eq!(target, "sip:target@local");
        }
        _ => panic!("Expected ConferenceConsultConnected event"),
    }
}

#[tokio::test]
async fn test_rwi_event_conference_merge_requested() {
    let event = RwiEvent::ConferenceMergeRequested {
        call_id: "call-001".to_string(),
        consultation_call_id: "call-002".to_string(),
    };
    
    match event {
        RwiEvent::ConferenceMergeRequested { call_id, consultation_call_id } => {
            assert_eq!(call_id, "call-001");
            assert_eq!(consultation_call_id, "call-002");
        }
        _ => panic!("Expected ConferenceMergeRequested event"),
    }
}

#[tokio::test]
async fn test_rwi_event_conference_merged() {
    let event = RwiEvent::ConferenceMerged {
        conf_id: "conf-001".to_string(),
        call_id: "call-001".to_string(),
    };
    
    match event {
        RwiEvent::ConferenceMerged { conf_id, call_id } => {
            assert_eq!(conf_id, "conf-001");
            assert_eq!(call_id, "call-001");
        }
        _ => panic!("Expected ConferenceMerged event"),
    }
}

#[tokio::test]
async fn test_rwi_event_conference_merge_failed() {
    let event = RwiEvent::ConferenceMergeFailed {
        conf_id: "conf-001".to_string(),
        call_id: "call-001".to_string(),
        reason: "bridge_failed".to_string(),
    };
    
    match event {
        RwiEvent::ConferenceMergeFailed { conf_id, call_id, reason } => {
            assert_eq!(conf_id, "conf-001");
            assert_eq!(call_id, "call-001");
            assert_eq!(reason, "bridge_failed");
        }
        _ => panic!("Expected ConferenceMergeFailed event"),
    }
}

#[tokio::test]
async fn test_rwi_event_call_ownership_changed() {
    let event = RwiEvent::CallOwnershipChanged {
        call_id: "call-001".to_string(),
        session_id: "session-001".to_string(),
        mode: "control".to_string(),
    };
    
    match event {
        RwiEvent::CallOwnershipChanged { call_id, session_id, mode } => {
            assert_eq!(call_id, "call-001");
            assert_eq!(session_id, "session-001");
            assert_eq!(mode, "control");
        }
        _ => panic!("Expected CallOwnershipChanged event"),
    }
}

#[tokio::test]
async fn test_rwi_event_session_resumed() {
    let event = RwiEvent::SessionResumed {
        session_id: "session-001".to_string(),
        last_sequence: 12345,
    };
    
    match event {
        RwiEvent::SessionResumed { session_id, last_sequence } => {
            assert_eq!(session_id, "session-001");
            assert_eq!(last_sequence, 12345);
        }
        _ => panic!("Expected SessionResumed event"),
    }
}

#[tokio::test]
async fn test_transfer_disabled_refer() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let mut config = TransferConfig::default();
    config.refer_enabled = false;
    let controller = TransferController::new(config, registry, gateway);
    
    let result = controller.initiate_blind_transfer("call-001".to_string(), "sip:target@local".to_string()).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_transfer_disabled_attended() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let mut config = TransferConfig::default();
    config.attended_enabled = false;
    let controller = TransferController::new(config, registry, gateway);
    
    let result = controller.initiate_attended_transfer("call-001".to_string(), "sip:target@local".to_string(), None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_transfer_cleanup_terminal_transactions() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let controller = TransferController::with_default_config(registry, gateway);
    
    let cleaned = controller.cleanup_terminal_transactions().await;
    assert_eq!(cleaned, 0);
    
    let active_count = controller.get_active_transfer_count().await;
    assert_eq!(active_count, 0);
}

#[tokio::test]
async fn test_transfer_cancel_all_transfers_for_call() {
    let registry = Arc::new(ActiveProxyCallRegistry::new());
    let gateway = Arc::new(RwLock::new(RwiGateway::new()));
    let controller = TransferController::with_default_config(registry, gateway);
    
    let canceled = controller.cancel_all_transfers_for_call("call-001").await;
    assert_eq!(canceled, 0);
}
