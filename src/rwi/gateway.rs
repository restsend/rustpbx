use crate::rwi::auth::RwiIdentity;
use crate::rwi::event::{FlatEvent, RwiEventSpec, merge_event_context};
use crate::rwi::proto::{CallMetaStore, RwiEvent};
use crate::rwi::session::{OwnershipMode, RwiSession, SupervisorMode};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Arc as StdArc, Mutex};
use tokio::sync::{broadcast, mpsc};

pub type SessionId = String;
pub type CallId = String;
pub type Context = String;

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventCacheEntry {
    pub sequence: u64,
    pub cached_at: DateTime<Utc>,
    pub call_id: CallId,
    pub event: RwiEvent,
}

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
    event_cache: Mutex<EventCacheState>,
    max_cache_size: usize,
    max_cache_age_secs: u64,
    /// Per-call DTMF taps for active DtmfCollect operations.
    dtmf_taps: Mutex<HashMap<CallId, tokio::sync::mpsc::UnboundedSender<(Option<String>, char)>>>,
    /// Per-call channel variables (key/value store).
    call_vars: HashMap<CallId, HashMap<String, String>>,
    /// Per-session event type filter; if set, only events whose type name is in the set are delivered.
    session_event_filters: HashMap<SessionId, HashSet<String>>,
    /// Optional broadcast sender for the RWI webhook handler.
    webhook_tx: Option<broadcast::Sender<EventCacheEntry>>,
    /// In-memory call context store for event enrichment.
    pub meta_store: Arc<CallMetaStore>,
}

#[derive(Debug)]
struct EventCacheState {
    cache: VecDeque<EventCacheEntry>,
    next_sequence: u64,
}

#[derive(Debug, Clone)]
pub struct RwiEventMessage {
    pub call_id: CallId,
    pub event: RwiEvent,
    pub target_sessions: Vec<SessionId>,
}

impl RwiGateway {
    pub fn new() -> Self {
        Self::with_config(1000, 60) // Default: 1000 events, 60 seconds
    }

    /// Create gateway with custom cache configuration
    ///
    /// # Arguments
    /// * `max_cache_size` - Maximum number of events to cache
    /// * `max_cache_age_secs` - Maximum age of cached events in seconds
    pub fn with_config(max_cache_size: usize, max_cache_age_secs: u64) -> Self {
        Self {
            sessions: HashMap::new(),
            session_event_senders: HashMap::new(),
            context_subscriptions: HashMap::new(),
            call_ownership: HashMap::new(),
            supervisor_calls: HashMap::new(),
            event_cache: Mutex::new(EventCacheState {
                cache: VecDeque::new(),
                next_sequence: 1,
            }),
            max_cache_size,
            max_cache_age_secs,
            dtmf_taps: Mutex::new(HashMap::new()),
            call_vars: HashMap::new(),
            session_event_filters: HashMap::new(),
            webhook_tx: None,
            meta_store: CallMetaStore::new(),
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

    /// Set the broadcast sender for the RWI webhook handler.
    pub fn set_webhook_tx(&mut self, tx: broadcast::Sender<EventCacheEntry>) {
        self.webhook_tx = Some(tx);
    }

    pub fn remove_session(&mut self, session_id: &SessionId) {
        self.session_event_senders.remove(session_id);
        self.session_event_filters.remove(session_id);
        if let Some(session) = self.sessions.remove(session_id) {
            let session = session.read();
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

    pub fn subscribe(
        &mut self,
        session_id: &SessionId,
        contexts: Vec<Context>,
        events: Option<Vec<String>>,
    ) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write();
            session.subscribe(contexts.clone());
            for ctx in contexts {
                self.context_subscriptions
                    .entry(ctx)
                    .or_default()
                    .insert(session_id.clone());
            }
            // Store event type filter if provided
            match events {
                Some(ev) if !ev.is_empty() => {
                    self.session_event_filters
                        .insert(session_id.clone(), ev.into_iter().collect());
                }
                _ => {
                    // No filter (or empty list) means receive all events
                    self.session_event_filters.remove(session_id);
                }
            }
            true
        } else {
            false
        }
    }

    pub fn unsubscribe(&mut self, session_id: &SessionId, contexts: &[Context]) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write();
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

    pub fn claim_call_ownership(
        &mut self,
        session_id: &SessionId,
        call_id: CallId,
        mode: OwnershipMode,
    ) -> Result<(), ClaimError> {
        if let Some(current_owner) = self.call_ownership.get(&call_id)
            && current_owner != session_id
        {
            return Err(ClaimError::AlreadyOwned);
        }

        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write();
            if session.claim_call(call_id.clone(), mode) {
                self.call_ownership.insert(call_id, session_id.clone());
                return Ok(());
            }
            Err(ClaimError::AlreadyOwned)
        } else {
            Err(ClaimError::SessionNotFound)
        }
    }

    pub fn release_call_ownership(&mut self, session_id: &SessionId, call_id: &CallId) -> bool {
        if let Some(current_owner) = self.call_ownership.get(call_id)
            && current_owner != session_id
        {
            return false;
        }

        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write();
            if session.release_call(call_id) {
                self.call_ownership.remove(call_id);
                return true;
            }
        }
        false
    }

    pub fn attach_supervisor(
        &mut self,
        session_id: &SessionId,
        target_call_id: CallId,
        mode: SupervisorMode,
    ) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write();
            session.add_supervisor_target(target_call_id.clone(), mode);
            self.supervisor_calls
                .insert(target_call_id, session_id.clone());
            true
        } else {
            false
        }
    }

    pub fn detach_supervisor(&mut self, session_id: &SessionId, target_call_id: &CallId) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let mut session = session.write();
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

    pub fn session_owns_call(&self, session_id: &SessionId, call_id: &CallId) -> bool {
        self.call_ownership
            .get(call_id)
            .map(|owner| owner == session_id)
            .unwrap_or(false)
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

    /// Send a pre-serialized flat JSON value to a single session.
    /// Applies per-session event type filter before sending.
    fn send_json_to_session(
        &self,
        session_id: &SessionId,
        value: &serde_json::Value,
        event_type: &str,
    ) {
        if let Some(sender) = self.session_event_senders.get(session_id) {
            if let Some(filter) = self.session_event_filters.get(session_id) {
                if !filter.contains(event_type) {
                    return;
                }
            }
            let _ = sender.send(value.clone());
        }
    }

    /// Send an event to a single session by session_id.
    /// Auto-enriches from `CallMetaStore`, flattens to remove variant wrapper.
    pub fn send_event_to_session(&self, session_id: &SessionId, event: &RwiEvent) {
        let enriched = if let Some(call_id) = event.call_id() {
            self.enrich_event(call_id, event)
        } else {
            event.clone()
        };
        let (value, event_type) = enriched.to_flat_value();
        self.send_json_to_session(session_id, &value, event_type);
    }

    /// Set a channel variable for the given call.
    pub fn set_call_var(&mut self, call_id: &CallId, key: String, value: String) {
        self.call_vars
            .entry(call_id.clone())
            .or_default()
            .insert(key, value);
    }

    /// Get a channel variable for the given call. Returns `None` if not set.
    pub fn get_call_var(&self, call_id: &CallId, key: &str) -> Option<String> {
        self.call_vars
            .get(call_id)
            .and_then(|vars| vars.get(key).cloned())
    }

    /// Remove all channel variables for the given call (call hangup cleanup).
    pub fn remove_call_vars(&mut self, call_id: &CallId) {
        self.call_vars.remove(call_id);
    }

    /// Enrich an event with call context from `CallMetaStore`.
    /// If no metadata is found, returns the event unchanged.
    fn enrich_event(&self, call_id: &str, event: &RwiEvent) -> RwiEvent {
        let event = event.clone();
        if let Some(meta) = self.meta_store.get_sync(call_id) {
            event.enrich(meta.into())
        } else {
            event
        }
    }

    pub fn cache_event(&self, call_id: &CallId, event: &RwiEvent) -> u64 {
        let mut cache_state = self
            .event_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let max_age = self.max_cache_age_secs;
        let now = chrono::Utc::now();
        while let Some(front) = cache_state.cache.front() {
            if now.signed_duration_since(front.cached_at).num_seconds() as u64 > max_age {
                cache_state.cache.pop_front();
            } else {
                break;
            }
        }

        let sequence = cache_state.next_sequence;
        cache_state.next_sequence += 1;

        let entry = EventCacheEntry {
            sequence,
            cached_at: now,
            call_id: call_id.clone(),
            event: event.clone(),
        };

        cache_state.cache.push_back(entry);

        // Remove oldest events if cache is too large
        while cache_state.cache.len() > self.max_cache_size {
            cache_state.cache.pop_front();
        }

        sequence
    }

    /// Get events for a call since a given sequence number
    /// Used for session resumption after disconnect
    pub fn get_events_since(&self, last_sequence: u64) -> Vec<EventCacheEntry> {
        let cache_state = self
            .event_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        cache_state
            .cache
            .iter()
            .filter(|entry| entry.sequence > last_sequence)
            .cloned()
            .collect()
    }

    /// Get events for a specific call since a given sequence number
    pub fn get_events_for_call_since(
        &self,
        call_id: &CallId,
        last_sequence: u64,
    ) -> Vec<EventCacheEntry> {
        let cache_state = self
            .event_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        cache_state
            .cache
            .iter()
            .filter(|entry| entry.call_id == *call_id && entry.sequence > last_sequence)
            .cloned()
            .collect()
    }

    /// Check if event is still in cache window
    pub fn is_sequence_in_cache(&self, sequence: u64) -> bool {
        let cache_state = self
            .event_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if cache_state.cache.is_empty() {
            return false;
        }

        let min_sequence = cache_state.cache.front().map(|e| e.sequence).unwrap_or(0);
        sequence >= min_sequence && sequence < cache_state.next_sequence
    }

    /// Get current sequence number
    pub fn current_sequence(&self) -> u64 {
        let cache_state = self
            .event_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache_state.next_sequence
    }

    /// Forward a cached event to the webhook broadcast channel, if configured.
    fn forward_to_webhook(&self, call_id: &CallId, event: &RwiEvent, sequence: u64) {
        if let Some(tx) = &self.webhook_tx {
            let entry = EventCacheEntry {
                sequence,
                cached_at: chrono::Utc::now(),
                call_id: call_id.clone(),
                event: event.clone(),
            };
            let _ = tx.send(entry);
        }
    }

    /// Send an event to the owner of a call_id (if any).
    /// Also caches the event for session resumption.
    pub fn send_event_to_call_owner(&self, call_id: &CallId, event: &RwiEvent) {
        // Feed DTMF digits to any active DtmfCollect tap for this call.
        if let RwiEvent::Custom(flat) = event
            && flat.event_type == "dtmf"
        {
            let digit_char = flat.payload.get("digit").and_then(|v| v.as_str()).and_then(|s| s.chars().next());
            let leg_id = flat.payload.get("leg_id").and_then(|v| v.as_str()).map(ToOwned::to_owned);
            if let Some(c) = digit_char {
                if let Ok(taps) = self.dtmf_taps.lock() {
                    if let Some(tx) = taps.get(call_id) {
                        let _ = tx.send((leg_id, c));
                    }
                }
            }
        }

        // Cache the event first
        let sequence = self.cache_event(call_id, event);

        // Forward to webhook handler (if configured)
        self.forward_to_webhook(call_id, event, sequence);

        // Send to owner
        if let Some(owner_id) = self.call_ownership.get(call_id) {
            self.send_event_to_session(owner_id, event);
        }
    }

    /// Register a DTMF tap for an active DtmfCollect on `call_id`.
    pub fn add_dtmf_tap(
        &self,
        call_id: CallId,
        tx: tokio::sync::mpsc::UnboundedSender<(Option<String>, char)>,
    ) {
        if let Ok(mut taps) = self.dtmf_taps.lock() {
            taps.insert(call_id, tx);
        }
    }

    /// Remove the DTMF tap for `call_id` (called when collection completes).
    pub fn remove_dtmf_tap(&self, call_id: &CallId) {
        if let Ok(mut taps) = self.dtmf_taps.lock() {
            taps.remove(call_id);
        }
    }

    /// Fan-out an event to all sessions subscribed to a context.
    /// Used for inbound `call.incoming` notifications.
    /// Also caches the event for session resumption.
    pub fn fan_out_event_to_context(
        &self,
        context: &str,
        event: &RwiEvent,
        call_id: &CallId,
    ) {
        self.fan_out_event_to_context_excluding(context, event, call_id, None);
    }

    pub fn fan_out_event_to_context_excluding(
        &self,
        context: &str,
        event: &RwiEvent,
        call_id: &CallId,
        exclude: Option<&SessionId>,
    ) {
        let sequence = self.cache_event(call_id, event);

        self.forward_to_webhook(call_id, event, sequence);

        if let Some(subscribers) = self.context_subscriptions.get(context) {
            for session_id in subscribers {
                if exclude.map_or(false, |ex| ex == session_id) {
                    continue;
                }
                self.send_event_to_session(session_id, event);
            }
        }
    }

    /// Send an event to every known session (broadcast).
    /// Note: Broadcasting does not cache events as there's no specific call_id.
    pub fn broadcast_event(&self, event: &RwiEvent) {
        // Forward to webhook handler (if configured).
        // Use an empty call_id and zero sequence since broadcast events are not cached.
        if let Some(tx) = &self.webhook_tx {
            let entry = EventCacheEntry {
                sequence: 0,
                cached_at: chrono::Utc::now(),
                call_id: String::new(),
                event: event.clone(),
            };
            let _ = tx.send(entry);
        }

        for session_id in self.session_event_senders.keys() {
            self.send_event_to_session(session_id, event);
        }
    }

    /// Resume a session after disconnect
    ///
    /// Returns events that need to be replayed to the session
    /// and the current sequence number for the session to track
    pub fn resume_session(&self, last_sequence: Option<u64>) -> (Vec<EventCacheEntry>, u64) {
        let events = match last_sequence {
            Some(seq) => self.get_events_since(seq),
            None => {
                let cache_state = self
                    .event_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache_state.cache.iter().cloned().collect()
            }
        };

        (events, self.current_sequence())
    }

    /// Resume a specific call after disconnect
    ///
    /// Returns events for the call that need to be replayed
    /// and the current sequence number for the session to track
    pub fn resume_call(
        &self,
        call_id: &CallId,
        last_sequence: Option<u64>,
    ) -> (Vec<EventCacheEntry>, u64) {
        let events = match last_sequence {
            Some(seq) => self.get_events_for_call_since(call_id, seq),
            None => {
                let cache_state = self
                    .event_cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache_state
                    .cache
                    .iter()
                    .filter(|entry| entry.call_id == *call_id)
                    .cloned()
                    .collect()
            }
        };

        (events, self.current_sequence())
    }

    fn enrich_flat_event(&self, flat: &FlatEvent) -> FlatEvent {
        if let Some(call_id) = &flat.call_id
            && let Some(meta) = self.meta_store.get_sync(call_id)
        {
            let mut payload = flat.payload.clone();
            let ctx = crate::rwi::proto::EventCallContext::from(meta);
            merge_event_context(&mut payload, Some(&ctx));
            return FlatEvent { event_type: flat.event_type, call_id: flat.call_id.clone(), payload };
        }
        flat.clone()
    }

    fn dispatch_flat(&self, flat: &FlatEvent) {
        let enriched = self.enrich_flat_event(flat);
        for session_id in self.session_event_senders.keys() {
            self.send_flat_to_session(session_id, &enriched);
        }
    }

    fn send_flat_to_session(&self, session_id: &SessionId, flat: &FlatEvent) {
        if let Some(sender) = self.session_event_senders.get(session_id) {
            if let Some(filter) = self.session_event_filters.get(session_id) {
                if !filter.contains(flat.event_type) { return; }
            }
            let _ = sender.send(flat.payload.clone());
        }
    }

    fn cache_flat_event(&self, call_id: &CallId, flat: &FlatEvent) -> u64 {
        let mut cache_state = self.event_cache.lock().unwrap_or_else(|p| p.into_inner());
        let now = chrono::Utc::now();
        while let Some(front) = cache_state.cache.front() {
            if now.signed_duration_since(front.cached_at).num_seconds() as u64 > self.max_cache_age_secs {
                cache_state.cache.pop_front();
            } else { break; }
        }
        let seq = cache_state.next_sequence;
        cache_state.next_sequence += 1;
        cache_state.cache.push_back(EventCacheEntry {
            sequence: seq, cached_at: now, call_id: call_id.clone(),
            event: RwiEvent::Custom(flat.clone()),
        });
        seq
    }

    pub fn broadcast<E: RwiEventSpec>(&self, event: &E) {
        let flat = FlatEvent::from_spec(event, None);
        if let Some(tx) = &self.webhook_tx {
            let mut cs = self.event_cache.lock().unwrap_or_else(|p| p.into_inner());
            let seq = cs.next_sequence; cs.next_sequence += 1;
            let _ = tx.send(EventCacheEntry { sequence: seq, cached_at: chrono::Utc::now(), call_id: String::new(), event: RwiEvent::Custom(flat.clone()) });
        }
        self.dispatch_flat(&flat);
    }

    pub fn send_to_owner<E: RwiEventSpec>(&self, event: &E) {
        let flat = FlatEvent::from_spec(event, None);
        let cid = event.call_id().expect("send_to_owner requires event.call_id()").to_string();
        let seq = self.cache_flat_event(&cid, &flat);
        let enriched = self.enrich_flat_event(&flat);
        if let Some(tx) = &self.webhook_tx {
            let _ = tx.send(EventCacheEntry { sequence: seq, cached_at: chrono::Utc::now(), call_id: cid.clone(), event: RwiEvent::Custom(enriched.clone()) });
        }
        if let Some(owner_id) = self.call_ownership.get(&cid) {
            self.send_flat_to_session(owner_id, &enriched);
        }
    }

    pub fn fan_out<E: RwiEventSpec>(&self, context: &str, event: &E) {
        let flat = FlatEvent::from_spec(event, None);
        let cid = event.call_id().expect("fan_out requires event.call_id()").to_string();
        let seq = self.cache_flat_event(&cid, &flat);
        let enriched = self.enrich_flat_event(&flat);
        if let Some(tx) = &self.webhook_tx {
            let _ = tx.send(EventCacheEntry { sequence: seq, cached_at: chrono::Utc::now(), call_id: cid.clone(), event: RwiEvent::Custom(enriched.clone()) });
        }
        if let Some(subscribers) = self.context_subscriptions.get(context) {
            for session_id in subscribers { self.send_flat_to_session(session_id, &enriched); }
        }
    }

    pub fn send_to_session<E: RwiEventSpec>(&self, session_id: &SessionId, event: &E) {
        let flat = FlatEvent::from_spec(event, None);
        let enriched = self.enrich_flat_event(&flat);
        self.send_flat_to_session(session_id, &enriched);
    }

    pub fn fan_out_excluding<E: RwiEventSpec>(&self, context: &str, event: &E, exclude: Option<&SessionId>) {
        let flat = FlatEvent::from_spec(event, None);
        let cid = event.call_id().expect("fan_out_excluding requires event.call_id()").to_string();
        let seq = self.cache_flat_event(&cid, &flat);
        let enriched = self.enrich_flat_event(&flat);
        if let Some(tx) = &self.webhook_tx {
            let _ = tx.send(EventCacheEntry { sequence: seq, cached_at: chrono::Utc::now(), call_id: cid.clone(), event: RwiEvent::Custom(enriched.clone()) });
        }
        if let Some(subscribers) = self.context_subscriptions.get(context) {
            for session_id in subscribers {
                if exclude.map_or(false, |e| e == session_id) { continue; }
                self.send_flat_to_session(session_id, &enriched);
            }
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

    fn create_identity() -> RwiIdentity { RwiIdentity { token: "t".into(), scopes: vec![] } }

    #[tokio::test]
    async fn test_broadcast_generic() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);
        gw.broadcast(&crate::rwi::CallRinging { call_id: "c1".into() });
        let v = rx.recv().await.unwrap();
        assert!(v.to_string().contains("call_ringing"));
    }

    #[tokio::test]
    async fn test_send_to_owner_generic() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);
        gw.claim_call_ownership(&sid, "c1".into(), OwnershipMode::Control).unwrap();
        gw.send_to_owner(&crate::rwi::CallAnswered { call_id: "c1".into() });
        let v = rx.recv().await.unwrap();
        assert!(v.to_string().contains("call_answered"));
    }
}
