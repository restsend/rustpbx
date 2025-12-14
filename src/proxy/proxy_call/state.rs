use crate::call::{DialDirection, QueueHoldConfig};
use crate::callrecord::CallRecordHangupReason;
use crate::proxy::active_call_registry::{
    ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rsip::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionAction {
    AcceptCall {
        callee: Option<String>,
        sdp: Option<String>,
    },
    EnterQueue {
        name: String,
    },
    ExitQueue,
    EnterIvr {
        reference: String,
    },
    TransferTarget(String),
    ProvideEarlyMedia(String),
    StartRinging {
        ringback: Option<String>,
        passthrough: bool,
    },
    PlayPrompt {
        audio_file: String,
        send_progress: bool,
        await_completion: bool,
    },
    SetQueueName(Option<String>),
    SetQueueRingbackPassthrough(bool),
    StartQueueHold(QueueHoldConfig),
    StopQueueHold,
    Hangup {
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
        initiator: Option<String>,
    },
    HandleReInvite(String), // Stores the request method or body if needed, but here we just signal
}

impl SessionAction {
    pub fn enter_queue(name: &str) -> Self {
        Self::EnterQueue {
            name: name.trim().to_string(),
        }
    }

    pub fn enter_ivr(reference: &str) -> Self {
        Self::EnterIvr {
            reference: reference.trim().to_string(),
        }
    }

    pub fn from_transfer_target(target: &str) -> Self {
        let trimmed = target.trim();
        if let Some(queue) = trimmed.strip_prefix("queue:") {
            return Self::enter_queue(queue);
        }
        if let Some(ivr) = trimmed.strip_prefix("ivr:") {
            return Self::enter_ivr(ivr);
        }
        Self::TransferTarget(trimmed.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyCallPhase {
    Initializing,
    Ringing,
    EarlyMedia,
    Bridged,
    Terminating,
    Failed,
    Ended,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallSessionSnapshot {
    session_id: String,
    phase: ProxyCallPhase,
    started_at: DateTime<Utc>,
    ring_time: Option<DateTime<Utc>>,
    answer_time: Option<DateTime<Utc>>,
    hangup_reason: Option<CallRecordHangupReason>,
    last_error_code: Option<u16>,
    last_error_reason: Option<String>,
    caller: Option<String>,
    callee: Option<String>,
    current_target: Option<String>,
    queue_name: Option<String>,
    direction: String,
    tenant_id: Option<i64>,
}

#[derive(Clone)]
pub struct CallSessionShared {
    inner: Arc<RwLock<CallSessionSnapshot>>,
    registry: Option<Arc<ActiveProxyCallRegistry>>,
    events: Arc<RwLock<Option<ProxyCallEventSender>>>,
}

impl CallSessionShared {
    pub fn new(
        session_id: String,
        direction: DialDirection,
        caller: Option<String>,
        callee: Option<String>,
        registry: Option<Arc<ActiveProxyCallRegistry>>,
        tenant_id: Option<i64>,
    ) -> Self {
        let started_at = Utc::now();
        let inner = CallSessionSnapshot {
            session_id: session_id.clone(),
            phase: ProxyCallPhase::Initializing,
            started_at,
            ring_time: None,
            answer_time: None,
            hangup_reason: None,
            last_error_code: None,
            last_error_reason: None,
            caller,
            callee,
            current_target: None,
            queue_name: None,
            direction: direction.to_string(),
            tenant_id,
        };
        Self {
            inner: Arc::new(RwLock::new(inner)),
            registry,
            events: Arc::new(RwLock::new(None)),
        }
    }

    pub fn snapshot(&self) -> CallSessionSnapshot {
        let inner = self.inner.read().unwrap();
        inner.clone()
    }

    pub fn register_active_call(&self, handle: CallSessionHandle) {
        if let Some(registry) = &self.registry {
            let inner = self.inner.read().unwrap();
            let entry = ActiveProxyCallEntry {
                session_id: inner.session_id.clone(),
                caller: inner.caller.clone(),
                callee: inner.callee.clone(),
                direction: inner.direction.clone(),
                started_at: inner.started_at,
                answered_at: inner.answer_time,
                status: ActiveProxyCallStatus::Ringing,
                tenant_id: inner.tenant_id,
            };
            registry.upsert(entry, handle);
        }
    }

    pub fn unregister(&self) {
        if let Some(registry) = &self.registry {
            let inner = self.inner.read().unwrap();
            registry.remove(&inner.session_id);
        }
    }

    pub fn set_event_sender(&self, sender: ProxyCallEventSender) {
        if let Ok(mut slot) = self.events.write() {
            *slot = Some(sender);
        }
    }

    pub fn emit_custom_event(&self, event: ProxyCallEvent) {
        if let Some(sender) = self.event_sender() {
            let _ = sender.send(event);
        }
    }

    pub fn session_id(&self) -> String {
        self.inner
            .read()
            .map(|inner| inner.session_id.clone())
            .unwrap_or_default()
    }

    pub fn update_routed_parties(&self, caller: Option<String>, callee: Option<String>) {
        self.update(|inner| {
            let mut changed = false;
            if let Some(ref caller) = caller {
                if inner.caller != Some(caller.clone()) {
                    inner.caller = Some(caller.clone());
                    changed = true;
                }
            }
            if let Some(ref callee) = callee {
                if inner.callee != Some(callee.clone()) {
                    inner.callee = Some(callee.clone());
                    changed = true;
                }
            }
            changed
        });
    }

    pub fn transition_to_ringing(&self, has_early_media: bool) -> bool {
        let changed = self.update(|inner| match inner.phase {
            ProxyCallPhase::Initializing | ProxyCallPhase::Ringing | ProxyCallPhase::EarlyMedia => {
                if inner.ring_time.is_none() {
                    inner.ring_time = Some(Utc::now());
                }
                inner.phase = if has_early_media {
                    ProxyCallPhase::EarlyMedia
                } else {
                    ProxyCallPhase::Ringing
                };
                true
            }
            ProxyCallPhase::Bridged
            | ProxyCallPhase::Terminating
            | ProxyCallPhase::Failed
            | ProxyCallPhase::Ended => false,
        });
        changed
    }

    pub fn transition_to_answered(&self) {
        self.update(|inner| {
            if inner.answer_time.is_none() {
                inner.answer_time = Some(Utc::now());
            }
            inner.phase = ProxyCallPhase::Bridged;
            true
        });
    }

    pub fn note_failure(&self, code: StatusCode, reason: Option<String>) {
        self.update(|inner| {
            inner.last_error_code = Some(u16::from(code.clone()));
            inner.last_error_reason = reason.clone();
            if inner.phase != ProxyCallPhase::Bridged {
                inner.phase = ProxyCallPhase::Failed;
            }
            true
        });
    }

    pub fn mark_hangup(&self, reason: CallRecordHangupReason) {
        self.update(|inner| {
            inner.hangup_reason = Some(reason);
            inner.phase = ProxyCallPhase::Ended;
            true
        });
    }

    pub fn set_current_target(&self, target: Option<String>) {
        let target_clone = target.clone();
        let changed = self.update(|inner| {
            if inner.current_target == target_clone {
                return false;
            }
            inner.current_target = target_clone.clone();
            true
        });
        if changed {
            self.emit_custom_event(ProxyCallEvent::TargetRinging {
                session_id: self.session_id(),
                target,
            });
        }
    }

    pub fn set_queue_name(&self, queue: Option<String>) {
        self.update(|inner| {
            if inner.queue_name == queue {
                return false;
            }
            inner.queue_name = queue;
            true
        });
    }

    fn update<F>(&self, mutate: F) -> bool
    where
        F: FnOnce(&mut CallSessionSnapshot) -> bool,
    {
        let mut inner = self.inner.write().unwrap();
        let prev_phase = inner.phase;
        let prev_queue = inner.queue_name.clone();
        let prev_hangup = inner.hangup_reason.clone();
        let changed = mutate(&mut inner);
        if !changed {
            return false;
        }
        if let Some(registry) = &self.registry {
            registry.update(&inner.session_id, |entry| {
                entry.caller = inner.caller.clone();
                entry.callee = inner.callee.clone();
                entry.answered_at = inner.answer_time;
                entry.status = match inner.phase {
                    ProxyCallPhase::Bridged => ActiveProxyCallStatus::Talking,
                    _ => ActiveProxyCallStatus::Ringing,
                };
            });
        }

        let phase_event = if inner.phase != prev_phase {
            Some((inner.session_id.clone(), inner.phase))
        } else {
            None
        };

        let queue_event = if inner.queue_name != prev_queue {
            Some((
                inner.session_id.clone(),
                prev_queue,
                inner.queue_name.clone(),
            ))
        } else {
            None
        };

        let hangup_event = if inner.hangup_reason != prev_hangup {
            Some((inner.session_id.clone(), inner.hangup_reason.clone()))
        } else {
            None
        };

        drop(inner);

        if let Some((session_id, phase)) = phase_event {
            self.emit_custom_event(ProxyCallEvent::PhaseChanged { session_id, phase });
        }

        if let Some((session_id, previous, current)) = queue_event {
            match (previous, current) {
                (Some(prev), Some(curr)) => {
                    self.emit_custom_event(ProxyCallEvent::QueueLeft {
                        session_id: session_id.clone(),
                        name: Some(prev),
                    });
                    self.emit_custom_event(ProxyCallEvent::QueueEntered {
                        session_id,
                        name: curr,
                    });
                }
                (Some(prev), None) => {
                    self.emit_custom_event(ProxyCallEvent::QueueLeft {
                        session_id,
                        name: Some(prev),
                    });
                }
                (None, Some(curr)) => {
                    self.emit_custom_event(ProxyCallEvent::QueueEntered {
                        session_id,
                        name: curr,
                    });
                }
                (None, None) => {}
            }
        }

        if let Some((session_id, reason)) = hangup_event {
            self.emit_custom_event(ProxyCallEvent::Hangup { session_id, reason });
        }

        true
    }

    fn event_sender(&self) -> Option<ProxyCallEventSender> {
        self.events.read().ok().and_then(|opt| opt.clone())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProxyCallEvent {
    PhaseChanged {
        session_id: String,
        phase: ProxyCallPhase,
    },
    TargetRinging {
        session_id: String,
        target: Option<String>,
    },
    TargetAnswered {
        session_id: String,
        callee: Option<String>,
    },
    TargetFailed {
        session_id: String,
        target: Option<String>,
        code: Option<u16>,
        reason: Option<String>,
    },
    QueueEntered {
        session_id: String,
        name: String,
    },
    QueueLeft {
        session_id: String,
        name: Option<String>,
    },
    Hangup {
        session_id: String,
        reason: Option<CallRecordHangupReason>,
    },
}

pub type SessionActionSender = mpsc::UnboundedSender<SessionAction>;
pub type SessionActionReceiver = mpsc::UnboundedReceiver<SessionAction>;
pub type ProxyCallEventSender = mpsc::UnboundedSender<ProxyCallEvent>;

#[derive(Clone)]
pub struct CallSessionHandle {
    session_id: String,
    shared: CallSessionShared,
    cmd_tx: SessionActionSender,
}

impl CallSessionHandle {
    pub fn with_shared(shared: CallSessionShared) -> (Self, SessionActionReceiver) {
        let session_id = shared.session_id();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        shared.set_event_sender(event_tx);
        let handle = Self {
            session_id,
            shared,
            cmd_tx,
        };
        (handle, cmd_rx)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn snapshot(&self) -> CallSessionSnapshot {
        self.shared.snapshot()
    }

    pub fn send_command(&self, action: SessionAction) -> Result<()> {
        self.cmd_tx.send(action).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::active_call_registry::{ActiveProxyCallRegistry, ActiveProxyCallStatus};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[test]
    fn transfer_target_detects_queue() {
        let action = SessionAction::from_transfer_target("queue: support ");
        assert_eq!(
            action,
            SessionAction::EnterQueue {
                name: "support".to_string()
            }
        );
    }

    #[test]
    fn transfer_target_detects_ivr() {
        let action = SessionAction::from_transfer_target("ivr: main_menu");
        assert_eq!(
            action,
            SessionAction::EnterIvr {
                reference: "main_menu".to_string()
            }
        );
    }

    #[test]
    fn transfer_target_keeps_uri() {
        let action = SessionAction::from_transfer_target("sip:1001@example.com");
        assert_eq!(
            action,
            SessionAction::TransferTarget("sip:1001@example.com".to_string())
        );
    }

    #[tokio::test]
    async fn shared_emits_queue_events() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let shared = CallSessionShared::new(
            "test-session".to_string(),
            DialDirection::Inbound,
            Some("sip:alice@example.com".to_string()),
            Some("sip:bob@example.com".to_string()),
            Some(registry.clone()),
            None,
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        shared.set_event_sender(tx);

        shared.set_queue_name(Some("support".to_string()));
        let entered = rx.recv().await.expect("queue entered event");
        match entered {
            ProxyCallEvent::QueueEntered { name, .. } => assert_eq!(name, "support"),
            other => panic!("unexpected event: {:?}", other),
        }

        shared.set_queue_name(None);
        let left = rx.recv().await.expect("queue left event");
        match left {
            ProxyCallEvent::QueueLeft { name, .. } => {
                assert_eq!(name, Some("support".to_string()))
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn shared_updates_phase_and_registry() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let shared = CallSessionShared::new(
            "test-session".to_string(),
            DialDirection::Inbound,
            Some("sip:alice@example.com".to_string()),
            Some("sip:bob@example.com".to_string()),
            Some(registry.clone()),
            None,
        );

        let (handle, _) = CallSessionHandle::with_shared(shared.clone());
        shared.register_active_call(handle.clone());

        assert!(shared.transition_to_ringing(false));
        shared.transition_to_answered();
        shared.mark_hangup(CallRecordHangupReason::ByCaller);

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.phase, ProxyCallPhase::Ended);
        assert_eq!(
            snapshot.hangup_reason,
            Some(CallRecordHangupReason::ByCaller)
        );

        let entry = registry.get("test-session").expect("registry entry");
        assert_eq!(entry.status, ActiveProxyCallStatus::Ringing);
    }

    #[tokio::test]
    async fn handle_forwards_session_actions() {
        let shared = CallSessionShared::new(
            "session-cmd".to_string(),
            DialDirection::Inbound,
            None,
            None,
            None,
            None,
        );
        let (handle, mut rx) = CallSessionHandle::with_shared(shared);

        handle
            .send_command(SessionAction::ExitQueue)
            .expect("command send");

        let action = rx.recv().await.expect("command received");
        assert_eq!(action, SessionAction::ExitQueue);
    }

    #[test]
    fn snapshot_tracks_routing_and_queue_state() {
        let shared = CallSessionShared::new(
            "session-snapshot".to_string(),
            DialDirection::Outbound,
            Some("sip:alice@example.com".to_string()),
            Some("sip:bob@example.com".to_string()),
            None,
            None,
        );

        shared.update_routed_parties(Some("sip:carol@example.com".to_string()), None);
        shared.set_queue_name(Some("sales".to_string()));
        shared.set_current_target(Some("sip:1001@example.com".to_string()));

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.caller, Some("sip:carol@example.com".to_string()));
        assert_eq!(snapshot.queue_name, Some("sales".to_string()));
        assert_eq!(
            snapshot.current_target,
            Some("sip:1001@example.com".to_string())
        );
    }

    #[test]
    fn registry_reflects_talking_when_answered() {
        let registry = Arc::new(ActiveProxyCallRegistry::new());
        let shared = CallSessionShared::new(
            "session-talking".to_string(),
            DialDirection::Inbound,
            Some("sip:alice@example.com".to_string()),
            Some("sip:bob@example.com".to_string()),
            Some(registry.clone()),
            None,
        );
        let (handle, _) = CallSessionHandle::with_shared(shared.clone());
        shared.register_active_call(handle);

        assert!(shared.transition_to_ringing(false));
        shared.transition_to_answered();

        let entry = registry.get("session-talking").expect("registry entry");
        assert_eq!(entry.status, ActiveProxyCallStatus::Talking);
    }
}
