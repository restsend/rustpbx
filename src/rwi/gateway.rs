use crate::rwi::auth::RwiIdentity;
use crate::rwi::proto::RwiEvent;
use crate::rwi::session::{OwnershipMode, RwiSession, SupervisorMode};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Arc as StdArc};
use tokio::sync::{RwLock, mpsc};

pub type SessionId = String;
pub type CallId = String;
pub type Context = String;

/// Sender for pushing JSON-serialized events to a WebSocket session.
pub type WsEventSender = mpsc::UnboundedSender<serde_json::Value>;

#[derive(Debug, Clone)]
pub struct GatewayState {
    pub session_id: SessionId,
    pub call_id: CallId,
    pub context: Option<Context>,
    pub ownership: Option<OwnershipMode>,
    pub supervisor_mode: Option<SupervisorMode>,
}

pub type RwiGatewayRef = StdArc<RwLock<RwiGateway>>;

pub struct RwiGateway {
    sessions: HashMap<SessionId, Arc<RwLock<RwiSession>>>,
    /// Per-session WebSocket event senders.
    session_event_senders: HashMap<SessionId, WsEventSender>,
    context_subscriptions: HashMap<Context, HashSet<SessionId>>,
    call_ownership: HashMap<CallId, SessionId>,
    supervisor_calls: HashMap<CallId, SessionId>,
}

#[derive(Debug, Clone)]
pub struct RwiEventMessage {
    pub call_id: CallId,
    pub event: RwiEvent,
    pub target_sessions: Vec<SessionId>,
}

impl RwiGateway {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            session_event_senders: HashMap::new(),
            context_subscriptions: HashMap::new(),
            call_ownership: HashMap::new(),
            supervisor_calls: HashMap::new(),
        }
    }

    /// Create a new RWI session and return the Arc handle.
    /// The caller must call [`set_session_event_sender`] with the WS sender after this.
    pub fn create_session(&mut self, identity: RwiIdentity) -> Arc<RwLock<RwiSession>> {
        let (command_tx, _command_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = RwiSession::new(identity, command_tx);
        let session_id = session.id.clone();
        let session = Arc::new(RwLock::new(session));
        self.sessions.insert(session_id.clone(), session.clone());
        session
    }

    /// Register the WebSocket event sender for a session so that `send_event`
    /// and `fan_out_event_to_context` can deliver events to it.
    pub fn set_session_event_sender(&mut self, session_id: &SessionId, sender: WsEventSender) {
        self.session_event_senders
            .insert(session_id.clone(), sender);
    }

    pub async fn remove_session(&mut self, session_id: &SessionId) {
        self.session_event_senders.remove(session_id);
        if let Some(session) = self.sessions.remove(session_id) {
            let session = session.read().await;
            for ctx in &session.subscribed_contexts {
                if let Some(subs) = self.context_subscriptions.get_mut(ctx) {
                    subs.remove(session_id);
                }
            }
            for call_id in session.owned_calls.keys() {
                self.call_ownership.remove(call_id);
            }
            for call_id in session.supervisor_targets.keys() {
                self.supervisor_calls.remove(call_id);
            }
        }
    }

    pub async fn subscribe(&mut self, session_id: &SessionId, contexts: Vec<Context>) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write().await;
            session.subscribe(contexts.clone());
            for ctx in contexts {
                self.context_subscriptions
                    .entry(ctx)
                    .or_insert_with(HashSet::new)
                    .insert(session_id.clone());
            }
            true
        } else {
            false
        }
    }

    pub async fn unsubscribe(&mut self, session_id: &SessionId, contexts: &[Context]) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write().await;
            session.unsubscribe(contexts);
            for ctx in contexts {
                if let Some(subs) = self.context_subscriptions.get_mut(ctx) {
                    subs.remove(session_id);
                }
            }
            true
        } else {
            false
        }
    }

    pub async fn claim_call_ownership(
        &mut self,
        session_id: &SessionId,
        call_id: CallId,
        mode: OwnershipMode,
    ) -> Result<(), ClaimError> {
        if let Some(current_owner) = self.call_ownership.get(&call_id) {
            if current_owner != session_id {
                return Err(ClaimError::AlreadyOwned);
            }
        }

        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write().await;
            if session.claim_call(call_id.clone(), mode) {
                self.call_ownership.insert(call_id, session_id.clone());
                return Ok(());
            }
            Err(ClaimError::AlreadyOwned)
        } else {
            Err(ClaimError::SessionNotFound)
        }
    }

    pub async fn release_call_ownership(
        &mut self,
        session_id: &SessionId,
        call_id: &CallId,
    ) -> bool {
        if let Some(current_owner) = self.call_ownership.get(call_id) {
            if current_owner != session_id {
                return false;
            }
        }

        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write().await;
            if session.release_call(call_id) {
                self.call_ownership.remove(call_id);
                return true;
            }
        }
        false
    }

    pub async fn attach_supervisor(
        &mut self,
        session_id: &SessionId,
        target_call_id: CallId,
        mode: SupervisorMode,
    ) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write().await;
            session.add_supervisor_target(target_call_id.clone(), mode);
            self.supervisor_calls
                .insert(target_call_id, session_id.clone());
            true
        } else {
            false
        }
    }

    pub async fn detach_supervisor(
        &mut self,
        session_id: &SessionId,
        target_call_id: &CallId,
    ) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write().await;
            if session.remove_supervisor_target(target_call_id) {
                self.supervisor_calls.remove(target_call_id);
                return true;
            }
        }
        false
    }

    pub fn get_call_owner(&self, call_id: &CallId) -> Option<SessionId> {
        self.call_ownership.get(call_id).cloned()
    }

    pub fn is_supervisor(&self, call_id: &CallId) -> bool {
        self.supervisor_calls.contains_key(call_id)
    }

    pub fn get_supervisor_session(&self, call_id: &CallId) -> Option<SessionId> {
        self.supervisor_calls.get(call_id).cloned()
    }

    pub fn get_sessions_subscribed_to_context(&self, context: &str) -> Vec<SessionId> {
        self.context_subscriptions
            .get(context)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get_all_sessions(&self) -> Vec<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn call_count(&self) -> usize {
        self.call_ownership.len()
    }

    /// Send an event to a single session by session_id.
    pub fn send_event_to_session(&self, session_id: &SessionId, event: &RwiEvent) {
        if let Some(sender) = self.session_event_senders.get(session_id) {
            if let Ok(value) = serde_json::to_value(event) {
                let _ = sender.send(value);
            }
        }
    }

    /// Send an event to the owner of a call_id (if any).
    pub fn send_event_to_call_owner(&self, call_id: &CallId, event: &RwiEvent) {
        if let Some(owner_id) = self.call_ownership.get(call_id) {
            self.send_event_to_session(owner_id, event);
        }
    }

    /// Fan-out an event to all sessions subscribed to a context.
    /// Used for inbound `call.incoming` notifications.
    pub fn fan_out_event_to_context(&self, context: &str, event: &RwiEvent) {
        if let Some(subscribers) = self.context_subscriptions.get(context) {
            for session_id in subscribers {
                self.send_event_to_session(session_id, event);
            }
        }
    }

    /// Send an event to every known session (broadcast).
    pub fn broadcast_event(&self, event: &RwiEvent) {
        for session_id in self.session_event_senders.keys() {
            self.send_event_to_session(session_id, event);
        }
    }
}

impl Default for RwiGateway {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaimError {
    AlreadyOwned,
    SessionNotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rwi::auth::RwiIdentity;

    fn create_test_identity() -> RwiIdentity {
        RwiIdentity {
            token: "test-token".to_string(),
            scopes: vec!["call.control".to_string()],
        }
    }

    #[tokio::test]
    async fn test_create_and_remove_session() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();

        assert_eq!(gateway.session_count(), 0);

        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();
        assert_eq!(gateway.session_count(), 1);

        gateway.remove_session(&session_id).await;
        assert_eq!(gateway.session_count(), 0);
    }

    #[tokio::test]
    async fn test_subscribe_unsubscribe() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let contexts = vec!["context1".to_string(), "context2".to_string()];
        gateway.subscribe(&session_id, contexts.clone()).await;

        assert_eq!(
            gateway.get_sessions_subscribed_to_context("context1"),
            vec![session_id.clone()]
        );
        assert_eq!(
            gateway.get_sessions_subscribed_to_context("context2"),
            vec![session_id.clone()]
        );

        gateway
            .unsubscribe(&session_id, &["context1".to_string()])
            .await;
        assert!(
            gateway
                .get_sessions_subscribed_to_context("context1")
                .is_empty()
        );
        assert_eq!(
            gateway.get_sessions_subscribed_to_context("context2"),
            vec![session_id]
        );
    }

    #[tokio::test]
    async fn test_claim_call_ownership() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let call_id = "call_001".to_string();
        let result = gateway
            .claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control)
            .await;
        assert!(result.is_ok());

        assert_eq!(gateway.get_call_owner(&call_id), Some(session_id.clone()));

        let result2 = gateway
            .claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control)
            .await;
        assert!(result2.is_err());
    }

    #[tokio::test]
    async fn test_claim_call_already_owned() {
        let mut gateway = RwiGateway::new();

        let identity1 = RwiIdentity {
            token: "token1".to_string(),
            scopes: vec!["call.control".to_string()],
        };
        let identity2 = RwiIdentity {
            token: "token2".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        let session1 = gateway.create_session(identity1);
        let session1_id = session1.read().await.id.clone();
        let session2 = gateway.create_session(identity2);
        let session2_id = session2.read().await.id.clone();

        let call_id = "call_001".to_string();
        gateway
            .claim_call_ownership(&session1_id, call_id.clone(), OwnershipMode::Control)
            .await
            .unwrap();

        let result = gateway
            .claim_call_ownership(&session2_id, call_id, OwnershipMode::Control)
            .await;
        assert!(matches!(result, Err(ClaimError::AlreadyOwned)));
    }

    #[tokio::test]
    async fn test_release_call_ownership() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let call_id = "call_001".to_string();
        gateway
            .claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control)
            .await
            .unwrap();

        assert_eq!(gateway.get_call_owner(&call_id), Some(session_id.clone()));

        gateway.release_call_ownership(&session_id, &call_id).await;
        assert_eq!(gateway.get_call_owner(&call_id), None);
    }

    #[tokio::test]
    async fn test_supervisor_attach_detach() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let target_call = "call_001".to_string();

        let result = gateway
            .attach_supervisor(&session_id, target_call.clone(), SupervisorMode::Listen)
            .await;
        assert!(result);
        assert!(gateway.is_supervisor(&target_call));
        assert_eq!(
            gateway.get_supervisor_session(&target_call),
            Some(session_id.clone())
        );

        gateway.detach_supervisor(&session_id, &target_call).await;
        assert!(!gateway.is_supervisor(&target_call));
    }

    #[tokio::test]
    async fn test_fanout_to_context() {
        let mut gateway = RwiGateway::new();

        let identity1 = RwiIdentity {
            token: "token1".to_string(),
            scopes: vec!["call.control".to_string()],
        };
        let identity2 = RwiIdentity {
            token: "token2".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        let session1 = gateway.create_session(identity1);
        let session1_id = session1.read().await.id.clone();
        let session2 = gateway.create_session(identity2);
        let session2_id = session2.read().await.id.clone();

        gateway
            .subscribe(&session1_id, vec!["context1".to_string()])
            .await;
        gateway
            .subscribe(
                &session2_id,
                vec!["context1".to_string(), "context2".to_string()],
            )
            .await;

        let subscribers = gateway.get_sessions_subscribed_to_context("context1");
        assert_eq!(subscribers.len(), 2);
        assert!(subscribers.contains(&session1_id));
        assert!(subscribers.contains(&session2_id));

        let subscribers2 = gateway.get_sessions_subscribed_to_context("context2");
        assert_eq!(subscribers2.len(), 1);
        assert_eq!(subscribers2[0], session2_id);
    }

    #[tokio::test]
    async fn test_remove_session_cleans_up_subscriptions() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        gateway
            .subscribe(&session_id, vec!["context1".to_string()])
            .await;

        assert_eq!(
            gateway.get_sessions_subscribed_to_context("context1"),
            vec![session_id.clone()]
        );

        gateway.remove_session(&session_id).await;

        assert!(
            gateway
                .get_sessions_subscribed_to_context("context1")
                .is_empty()
        );
        assert!(gateway.sessions.get(&session_id).is_none());
    }

    #[tokio::test]
    async fn test_remove_session_cleans_up_ownership() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        gateway
            .claim_call_ownership(&session_id, "call_001".to_string(), OwnershipMode::Control)
            .await
            .unwrap();

        assert_eq!(
            gateway.get_call_owner(&"call_001".to_string()),
            Some(session_id.clone())
        );

        gateway.remove_session(&session_id).await;

        assert_eq!(gateway.get_call_owner(&"call_001".to_string()), None);
    }

    #[tokio::test]
    async fn test_send_event_to_session() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let (tx, mut rx) = mpsc::unbounded_channel();
        gateway.set_session_event_sender(&session_id, tx);

        let event = RwiEvent::CallAnswered {
            call_id: "call_001".to_string(),
        };
        gateway.send_event_to_session(&session_id, &event);

        let received = rx.recv().await.expect("should receive event");
        assert!(received.is_object());
    }

    #[tokio::test]
    async fn test_send_event_to_call_owner() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let (tx, mut rx) = mpsc::unbounded_channel();
        gateway.set_session_event_sender(&session_id, tx);

        let call_id = "call_999".to_string();
        gateway
            .claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control)
            .await
            .unwrap();

        let event = RwiEvent::CallHangup {
            call_id: call_id.clone(),
            reason: None,
            sip_status: None,
        };
        gateway.send_event_to_call_owner(&call_id, &event);

        let received = rx.recv().await.expect("should receive event");
        assert!(received.is_object());
    }

    #[tokio::test]
    async fn test_fan_out_event_to_context() {
        let mut gateway = RwiGateway::new();

        let id1 = RwiIdentity {
            token: "t1".into(),
            scopes: vec![],
        };
        let id2 = RwiIdentity {
            token: "t2".into(),
            scopes: vec![],
        };

        let s1 = gateway.create_session(id1);
        let s1_id = s1.read().await.id.clone();
        let s2 = gateway.create_session(id2);
        let s2_id = s2.read().await.id.clone();

        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        gateway.set_session_event_sender(&s1_id, tx1);
        gateway.set_session_event_sender(&s2_id, tx2);

        gateway.subscribe(&s1_id, vec!["ctx".into()]).await;
        gateway.subscribe(&s2_id, vec!["ctx".into()]).await;

        let event = RwiEvent::CallRinging {
            call_id: "c1".into(),
        };
        gateway.fan_out_event_to_context("ctx", &event);

        assert!(rx1.recv().await.is_some());
        assert!(rx2.recv().await.is_some());
    }

    #[tokio::test]
    async fn test_remove_session_cleans_up_event_sender() {
        let mut gateway = RwiGateway::new();
        let identity = create_test_identity();
        let session = gateway.create_session(identity);
        let session_id = session.read().await.id.clone();

        let (tx, _rx) = mpsc::unbounded_channel();
        gateway.set_session_event_sender(&session_id, tx);

        assert_eq!(gateway.session_event_senders.len(), 1);

        gateway.remove_session(&session_id).await;

        assert_eq!(gateway.session_event_senders.len(), 0);
    }
}
