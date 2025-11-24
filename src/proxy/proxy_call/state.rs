use crate::call::DialDirection;
use crate::callrecord::CallRecordHangupReason;
use crate::proxy::active_call_registry::{
    ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus, normalize_direction,
};
use chrono::{DateTime, Utc};
use rsip::StatusCode;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyCallPhase {
    Initializing,
    Ringing,
    EarlyMedia,
    Bridged,
    Terminating,
    Failed,
    Ended,
}

impl ProxyCallPhase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Initializing => "initializing",
            Self::Ringing => "ringing",
            Self::EarlyMedia => "early_media",
            Self::Bridged => "bridged",
            Self::Terminating => "terminating",
            Self::Failed => "failed",
            Self::Ended => "ended",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProxyCallStateSnapshot {
    pub session_id: String,
    pub phase: ProxyCallPhase,
    pub started_at: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub last_error_code: Option<u16>,
    pub last_error_reason: Option<String>,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub current_target: Option<String>,
    pub queue_name: Option<String>,
}

struct ProxyCallStateInner {
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
}

#[derive(Clone)]
pub struct ProxyCallState {
    inner: Arc<RwLock<ProxyCallStateInner>>,
    registry: Option<Arc<ActiveProxyCallRegistry>>,
    events: Arc<RwLock<Option<ProxyCallEventSender>>>,
}

impl ProxyCallState {
    pub fn new(
        session_id: String,
        direction: DialDirection,
        caller: Option<String>,
        callee: Option<String>,
        registry: Option<Arc<ActiveProxyCallRegistry>>,
    ) -> Self {
        let started_at = Utc::now();
        let inner = ProxyCallStateInner {
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
            direction: normalize_direction(&direction),
        };
        Self {
            inner: Arc::new(RwLock::new(inner)),
            registry,
            events: Arc::new(RwLock::new(None)),
        }
    }

    pub fn snapshot(&self) -> ProxyCallStateSnapshot {
        let inner = self.inner.read().unwrap();
        ProxyCallStateSnapshot {
            session_id: inner.session_id.clone(),
            phase: inner.phase,
            started_at: inner.started_at,
            ring_time: inner.ring_time,
            answer_time: inner.answer_time,
            hangup_reason: inner.hangup_reason.clone(),
            last_error_code: inner.last_error_code,
            last_error_reason: inner.last_error_reason.clone(),
            caller: inner.caller.clone(),
            callee: inner.callee.clone(),
            current_target: inner.current_target.clone(),
            queue_name: inner.queue_name.clone(),
        }
    }

    pub fn register_active_call(&self) {
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
            };
            registry.upsert(entry);
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
        F: FnOnce(&mut ProxyCallStateInner) -> bool,
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

const PROXY_CALL_COMMAND_CAPACITY: usize = 32;
const PROXY_CALL_EVENT_CAPACITY: usize = 64;

pub type ProxyCallCommandSender = broadcast::Sender<ProxyCallCommand>;
pub type ProxyCallCommandReceiver = broadcast::Receiver<ProxyCallCommand>;
pub type ProxyCallEventSender = broadcast::Sender<ProxyCallEvent>;
pub type ProxyCallEventReceiver = broadcast::Receiver<ProxyCallEvent>;

#[derive(Clone, Debug)]
pub enum ProxyCallCommand {
    StartRinging {
        ringback: Option<String>,
        passthrough: bool,
    },
    ProvideEarlyMedia {
        sdp: Option<String>,
    },
    Answer {
        callee: Option<String>,
        sdp: Option<String>,
    },
    Hangup {
        reason: Option<CallRecordHangupReason>,
        code: Option<u16>,
        initiator: Option<String>,
    },
    EnterQueue {
        name: String,
    },
    ExitQueue,
    EnterIvr {
        reference: String,
    },
    Transfer {
        target: String,
    },
}

#[derive(Clone, Debug)]
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

#[derive(Clone)]
pub struct ProxyCallHandle {
    session_id: String,
    state: ProxyCallState,
    cmd_tx: ProxyCallCommandSender,
    event_tx: ProxyCallEventSender,
}

impl ProxyCallHandle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn snapshot(&self) -> ProxyCallStateSnapshot {
        self.state.snapshot()
    }

    pub fn send_command(
        &self,
        command: ProxyCallCommand,
    ) -> Result<(), broadcast::error::SendError<ProxyCallCommand>> {
        self.cmd_tx.send(command).map(|_| ())
    }

    pub fn subscribe_events(&self) -> ProxyCallEventReceiver {
        self.event_tx.subscribe()
    }

    pub fn command_sender(&self) -> ProxyCallCommandSender {
        self.cmd_tx.clone()
    }
}

#[allow(dead_code)]
pub struct ProxyCallController {
    session_id: String,
    state: ProxyCallState,
    cmd_tx: ProxyCallCommandSender,
    event_tx: ProxyCallEventSender,
}

#[allow(dead_code)]
impl ProxyCallController {
    pub fn new(state: ProxyCallState) -> (Self, ProxyCallHandle) {
        let snapshot = state.snapshot();
        let session_id = snapshot.session_id.clone();
        let state_for_controller = state.clone();
        let state_for_handle = state.clone();
        let (cmd_tx, _) = broadcast::channel(PROXY_CALL_COMMAND_CAPACITY);
        let (event_tx, _) = broadcast::channel(PROXY_CALL_EVENT_CAPACITY);
        state.set_event_sender(event_tx.clone());
        let controller = Self {
            session_id: session_id.clone(),
            state: state_for_controller,
            cmd_tx: cmd_tx.clone(),
            event_tx: event_tx.clone(),
        };
        let handle = ProxyCallHandle {
            session_id,
            state: state_for_handle,
            cmd_tx,
            event_tx,
        };
        (controller, handle)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn state(&self) -> &ProxyCallState {
        &self.state
    }

    pub fn subscribe_commands(&self) -> ProxyCallCommandReceiver {
        self.cmd_tx.subscribe()
    }

    pub fn emit_event(&self, event: ProxyCallEvent) {
        let _ = self.event_tx.send(event);
    }

    pub fn notify_phase_change(&self) {
        let snapshot = self.state.snapshot();
        let _ = self.event_tx.send(ProxyCallEvent::PhaseChanged {
            session_id: snapshot.session_id,
            phase: snapshot.phase,
        });
    }

    pub fn command_sender(&self) -> ProxyCallCommandSender {
        self.cmd_tx.clone()
    }

    pub fn event_sender(&self) -> ProxyCallEventSender {
        self.event_tx.clone()
    }
}
