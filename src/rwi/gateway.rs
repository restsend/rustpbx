use crate::rwi::auth::RwiIdentity;
use crate::rwi::event::{RwiEvent, RwiEventSpec, merge_event_context};
use crate::rwi::proto::CallMetaStore;
use crate::rwi::session::{OwnershipMode, RwiSession};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Arc as StdArc, Weak as StdWeak};
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

pub type SessionId = String;
pub type CallId = String;
pub type Context = String;

#[derive(Debug, Clone, serde::Serialize)]
pub struct EventCacheEntry {
    pub cached_at: DateTime<Utc>,
    pub call_id: CallId,
    pub event: RwiEvent,
}

/// Sender for pushing JSON-serialized events to a WebSocket session.
pub type WsEventSender = mpsc::UnboundedSender<serde_json::Value>;

pub type RwiGatewayRef = StdArc<RwLock<RwiGateway>>;

/// Keeps gateway-owned call state alive until the `CallRecord` and all of its
/// completion hooks have finished. `CallRecord.extensions` requires cloneable
/// values, so cleanup belongs to the shared inner value and runs exactly once
/// when its final guard is dropped.
#[derive(Clone)]
pub struct RwiCallRecordGuard {
    _inner: StdArc<RwiCallRecordGuardInner>,
}

struct RwiCallRecordGuardInner {
    gateway: StdWeak<RwLock<RwiGateway>>,
    call_id: CallId,
}

impl RwiCallRecordGuard {
    pub fn new(gateway: &RwiGatewayRef, call_id: CallId) -> Self {
        Self {
            _inner: StdArc::new(RwiCallRecordGuardInner {
                gateway: StdArc::downgrade(gateway),
                call_id,
            }),
        }
    }
}

impl Drop for RwiCallRecordGuardInner {
    fn drop(&mut self) {
        if let Some(gateway) = self.gateway.upgrade() {
            gateway.write().call_finished(&self.call_id);
        }
    }
}

pub struct RwiGateway {
    sessions: HashMap<SessionId, Arc<RwLock<RwiSession>>>,
    /// Per-session WebSocket event senders.
    session_event_senders: HashMap<SessionId, WsEventSender>,
    context_subscriptions: HashMap<Context, HashSet<SessionId>>,
    call_ownership: HashMap<CallId, SessionId>,
    event_cache: Mutex<EventCacheState>,
    max_cache_size: usize,
    max_cache_age_secs: u64,
    /// Per-call DTMF taps for active DtmfCollect operations.
    dtmf_taps: DashMap<CallId, tokio::sync::mpsc::UnboundedSender<(Option<String>, char)>>,
    /// Per-call channel variables (key/value store).
    call_vars: HashMap<CallId, HashMap<String, String>>,
    /// Per-session event type filter; if set, only events whose type name is in the set are delivered.
    session_event_filters: HashMap<SessionId, HashSet<String>>,
    /// Optional broadcast sender for the RWI webhook handler.
    webhook_tx: Option<broadcast::Sender<EventCacheEntry>>,
    /// Always-on event tap — every cached event is fanned out here.
    /// Used by the outbound SSE interface to subscribe to call progress events.
    event_tap: broadcast::Sender<EventCacheEntry>,
    /// In-memory call context store for event enrichment.
    pub meta_store: Arc<CallMetaStore>,
    /// This node's cluster-internal IP, injected into every dispatched event as
    /// `src_ip`. `None` in single-node mode (cluster disabled or no self peer
    /// match) — no injection happens then.
    src_ip: Option<String>,
    /// Optional source of the agent's client IP. When set, events whose payload
    /// carries an `agent_id` get a `client_ip` field injected (looked up via the
    /// CC AgentRegistry at dispatch time — no per-event locator/DB query).
    client_ip_lookup: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
}

#[derive(Debug)]
struct EventCacheState {
    cache: VecDeque<EventCacheEntry>,
}

/// Delivery target for the unified [`RwiGateway::dispatch`] primitive.
#[derive(Debug, Clone, Copy)]
enum DispatchTarget<'a> {
    /// Deliver to the session that owns the routed `call_id`.
    Owner(&'a CallId),
    /// Deliver to every session subscribed to `context`, optionally excluding
    /// one session.
    FanOut(&'a str, Option<&'a SessionId>),
    /// Deliver to every online session (global events).
    Broadcast,
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
        let (event_tap, _) = broadcast::channel(512);
        Self {
            sessions: HashMap::new(),
            session_event_senders: HashMap::new(),
            context_subscriptions: HashMap::new(),
            call_ownership: HashMap::new(),
            event_cache: Mutex::new(EventCacheState {
                cache: VecDeque::new(),
            }),
            max_cache_size,
            max_cache_age_secs,
            dtmf_taps: DashMap::new(),
            call_vars: HashMap::new(),
            session_event_filters: HashMap::new(),
            webhook_tx: None,
            event_tap,
            meta_store: CallMetaStore::new(),
            src_ip: None,
            client_ip_lookup: None,
        }
    }

    /// Set this node's cluster-internal IP (`src_ip`). Called once at startup
    /// when cluster is enabled. `None` disables injection.
    pub fn set_src_ip(&mut self, ip: Option<String>) {
        self.src_ip = ip;
    }

    /// Set an optional agent client-IP lookup used to enrich `agent_id`-carrying
    /// events with a `client_ip` field. `None` disables client-IP injection.
    pub fn set_client_ip_lookup(
        &mut self,
        lookup: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    ) {
        self.client_ip_lookup = lookup;
    }

    /// Create a new RWI session and return the Arc handle.
    /// The caller must call [`set_session_event_sender`] with the WS sender after this.
    pub fn create_session(&mut self, identity: RwiIdentity) -> Arc<RwLock<RwiSession>> {
        let session = RwiSession::new(identity);
        let session_id = session.id.clone();
        let session = Arc::new(RwLock::new(session));
        self.sessions.insert(session_id.clone(), session.clone());
        session
    }

    /// Register the WebSocket event sender for a session so that events can be
    /// delivered to it.
    pub fn set_session_event_sender(&mut self, session_id: &SessionId, sender: WsEventSender) {
        self.session_event_senders
            .insert(session_id.clone(), sender);
    }

    /// Set the broadcast sender for the RWI webhook handler.
    pub fn set_webhook_tx(&mut self, tx: broadcast::Sender<EventCacheEntry>) {
        self.webhook_tx = Some(tx);
    }

    /// Subscribe to the always-on event tap.
    ///
    /// Every event that flows through the gateway (via `send_to_owner`,
    /// `send_to_owner_at`, `fan_out`, `broadcast`, and `broadcast_event`) is
    /// fanned out to this broadcast channel.
    ///
    /// Callers should filter by `call_id` and handle `RecvError::Lagged`
    /// gracefully (e.g. skip missed events and continue).
    pub fn subscribe_events(&self) -> broadcast::Receiver<EventCacheEntry> {
        self.event_tap.subscribe()
    }

    /// Returns true if a webhook handler is configured and wired.
    pub fn webhook_configured(&self) -> bool {
        self.webhook_tx.is_some()
    }

    pub fn remove_session(&mut self, session_id: &SessionId) -> Vec<CallId> {
        self.session_event_senders.remove(session_id);
        self.session_event_filters.remove(session_id);
        let mut cleanup_call_ids = Vec::new();
        if let Some(session) = self.sessions.remove(session_id) {
            let session = session.read();
            for ctx in &session.subscribed_contexts {
                if let Some(subs) = self.context_subscriptions.get_mut(ctx) {
                    subs.remove(session_id);
                }
            }
            for call_id in session.owned_calls.keys() {
                self.call_ownership.remove(call_id);
                self.remove_call_vars(call_id);
                cleanup_call_ids.push(call_id.clone());
            }
        }
        cleanup_call_ids
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

        if self.call_ownership.contains_key(call_id) {
            return self.call_finished(call_id);
        }

        let released = self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.write().release_call(call_id));
        if released {
            self.remove_call_vars(call_id);
            self.dtmf_taps.remove(call_id);
            return true;
        }
        false
    }

    /// Remove all gateway-owned state after the call-record completion guard
    /// is dropped, or immediately when ownership is explicitly detached.
    pub fn call_finished(&mut self, call_id: &CallId) -> bool {
        let owner_id = self.call_ownership.remove(call_id);
        let released = owner_id
            .as_ref()
            .and_then(|session_id| self.sessions.get(session_id))
            .is_some_and(|session| session.write().release_call(call_id));

        self.remove_call_vars(call_id);
        self.dtmf_taps.remove(call_id);
        self.meta_store.remove(call_id);

        owner_id.is_some() || released
    }

    /// Fan an event entry out to the RWI webhook handler (if configured) and
    /// the always-on event tap.
    fn fanout_webhook_tap(&self, entry: &EventCacheEntry) {
        if let Some(tx) = &self.webhook_tx {
            let _ = tx.send(entry.clone());
            metrics::counter!(
                "rwi_event_enqueued_total",
                "event_type" => entry.event.event_type
            )
            .increment(1);
        }
        let _ = self.event_tap.send(entry.clone());
    }

    /// Single dispatch primitive shared by every event path.
    ///
    /// Owner / FanOut targets cache the (raw) event for session resume, enrich
    /// it from the `CallMetaStore`, and forward one enriched entry to both the
    /// webhook handler and the event tap. Broadcast targets do not cache —
    /// they are direct global sends.
    fn dispatch(&self, dispatch_call_id: &CallId, event: &RwiEvent, target: DispatchTarget) {
        match target {
            DispatchTarget::Broadcast => {
                let enriched = self.enrich_flat_event(event);
                let entry = EventCacheEntry {
                    cached_at: chrono::Utc::now(),
                    call_id: enriched.call_id.clone().unwrap_or_default(),
                    event: enriched.clone(),
                };
                self.fanout_webhook_tap(&entry);
                for session_id in self.session_event_senders.keys() {
                    self.send_flat_to_session(session_id, &enriched);
                }
            }
            DispatchTarget::Owner(owner_call_id) => {
                // Feed DTMF digits to any active DtmfCollect tap for this call.
                if event.event_type == "dtmf" {
                    let digit_char = event
                        .payload
                        .get("digit")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.chars().next());
                    let leg_id = event
                        .payload
                        .get("leg_id")
                        .and_then(|v| v.as_str())
                        .map(ToOwned::to_owned);
                    if let Some(c) = digit_char {
                        if let Some(tx) = self.dtmf_taps.get(owner_call_id) {
                            let _ = tx.send((leg_id, c));
                        }
                    }
                }

                self.cache_event(dispatch_call_id, event);
                let enriched = self.enrich_flat_event(event);
                let entry = EventCacheEntry {
                    cached_at: chrono::Utc::now(),
                    call_id: dispatch_call_id.clone(),
                    event: enriched.clone(),
                };
                self.fanout_webhook_tap(&entry);
                if let Some(owner_id) = self.call_ownership.get(owner_call_id) {
                    self.send_flat_to_session(owner_id, &enriched);
                }
            }
            DispatchTarget::FanOut(context, exclude) => {
                self.cache_event(dispatch_call_id, event);
                let enriched = self.enrich_flat_event(event);
                let entry = EventCacheEntry {
                    cached_at: chrono::Utc::now(),
                    call_id: dispatch_call_id.clone(),
                    event: enriched.clone(),
                };
                self.fanout_webhook_tap(&entry);
                if let Some(subscribers) = self.context_subscriptions.get(context) {
                    for session_id in subscribers {
                        if exclude.map_or(false, |e| e == session_id) {
                            continue;
                        }
                        self.send_flat_to_session(session_id, &enriched);
                    }
                }
            }
        }
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

    /// Cache an event for later session/call resume replay.
    pub fn cache_event(&self, call_id: &CallId, event: &RwiEvent) {
        let mut cache_state = self.event_cache.lock();
        let max_age = self.max_cache_age_secs;
        let now = chrono::Utc::now();
        while let Some(front) = cache_state.cache.front() {
            if now.signed_duration_since(front.cached_at).num_seconds() as u64 > max_age {
                cache_state.cache.pop_front();
            } else {
                break;
            }
        }

        let entry = EventCacheEntry {
            cached_at: now,
            call_id: call_id.clone(),
            event: event.clone(),
        };

        cache_state.cache.push_back(entry);

        // Remove oldest events if cache is too large
        while cache_state.cache.len() > self.max_cache_size {
            cache_state.cache.pop_front();
        }
    }

    /// Get all cached events for a specific call.
    pub fn get_events_for_call(&self, call_id: &CallId) -> Vec<EventCacheEntry> {
        let cache_state = self.event_cache.lock();

        cache_state
            .cache
            .iter()
            .filter(|entry| entry.call_id == *call_id)
            .cloned()
            .collect()
    }

    /// Register a DTMF tap for an active DtmfCollect on `call_id`.
    pub fn add_dtmf_tap(
        &self,
        call_id: CallId,
        tx: tokio::sync::mpsc::UnboundedSender<(Option<String>, char)>,
    ) {
        self.dtmf_taps.insert(call_id, tx);
    }

    /// Remove the DTMF tap for `call_id` (called when collection completes).
    pub fn remove_dtmf_tap(&self, call_id: &CallId) {
        self.dtmf_taps.remove(call_id);
    }

    /// Send an event to every known session (broadcast).
    /// Call-scoped events (with `call_id`) carry it in the envelope so webhook
    /// consumers can correlate. Truly global events (agent_state_changed, etc.)
    /// have `call_id = None` and the envelope field is empty.
    ///
    /// This is the dispatch path used by addons that construct events
    /// dynamically (e.g. the CC addon's agent / queue / skill-group events).
    /// Call-scoped broadcasts are enriched with flat context (caller/callee/
    /// names/direction) from the `CallMetaStore` exactly like call_owner events.
    pub fn broadcast_event(&self, event: &RwiEvent) {
        self.dispatch(&String::new(), event, DispatchTarget::Broadcast);
    }

    /// Resume a session after disconnect.
    ///
    /// Returns all cached events (bounded by the cache's size/age window) for
    /// replay to the reconnecting session.
    pub fn resume_session(&self) -> Vec<EventCacheEntry> {
        let cache_state = self.event_cache.lock();
        cache_state.cache.iter().cloned().collect()
    }

    /// Resume a specific call after disconnect.
    ///
    /// Returns the call's cached events for replay to the reconnecting session.
    pub fn resume_call(&self, call_id: &CallId) -> Vec<EventCacheEntry> {
        self.get_events_for_call(call_id)
    }

    fn enrich_flat_event(&self, flat: &RwiEvent) -> RwiEvent {
        let mut payload = if let Some(call_id) = &flat.call_id
            && let Some(meta) = self.meta_store.get_sync(call_id)
        {
            let mut payload = flat.payload.clone();
            let ctx = crate::rwi::proto::EventCallContext::from(meta);
            merge_event_context(&mut payload, Some(&ctx));
            payload
        } else {
            flat.payload.clone()
        };

        self.inject_origin_fields(&mut payload);

        RwiEvent {
            event_type: flat.event_type,
            call_id: flat.call_id.clone(),
            payload,
        }
    }

    /// Stamp origin fields onto an event payload:
    /// - `src_ip` — this node's cluster IP (when cluster is enabled).
    /// - `client_ip` — the agent's registered client IP, when the payload
    ///   carries an `agent_id` and a lookup is configured.
    ///
    /// Existing keys are never overwritten (event's own field wins), matching
    /// the `merge_event_context` convention.
    fn inject_origin_fields(&self, payload: &mut serde_json::Value) {
        if let Some(ip) = &self.src_ip
            && let Some(obj) = payload.as_object_mut()
            && !obj.contains_key("src_ip")
        {
            obj.insert("src_ip".to_string(), serde_json::Value::String(ip.clone()));
        }
        if let Some(lookup) = &self.client_ip_lookup
            && let Some(obj) = payload.as_object_mut()
            && let Some(agent_id) = obj.get("agent_id").and_then(|v| v.as_str())
            && !obj.contains_key("client_ip")
            && let Some(ip) = lookup(agent_id)
        {
            obj.insert(
                "client_ip".to_string(),
                serde_json::Value::String(ip.to_string()),
            );
        }
    }

    fn send_flat_to_session(&self, session_id: &SessionId, flat: &RwiEvent) {
        if let Some(sender) = self.session_event_senders.get(session_id) {
            if let Some(filter) = self.session_event_filters.get(session_id) {
                if !filter.contains(flat.event_type) {
                    return;
                }
            }
            let _ = sender.send(flat.payload.clone());
        }
    }

    pub fn broadcast<E: RwiEventSpec>(&self, event: &E) {
        let flat = RwiEvent::from_spec(event, None);
        self.dispatch(&String::new(), &flat, DispatchTarget::Broadcast);
    }

    pub fn send_to_owner<E: RwiEventSpec>(&self, event: &E) {
        let Some(cid) = event.call_id().map(ToOwned::to_owned) else {
            warn!("send_to_owner: event has no call_id, skipping");
            return;
        };
        let flat = RwiEvent::from_spec(event, None);
        self.dispatch(&cid, &flat, DispatchTarget::Owner(&cid));
    }

    /// Send an event to the owner of an explicit `call_id` regardless of the
    /// event's own `call_id()`. Used by supervisor / ringback flows that fan a
    /// single event out to several call owners. Caches and forwards to the
    /// webhook/tap exactly like [`Self::send_to_owner`].
    pub fn send_to_owner_at<E: RwiEventSpec>(&self, call_id: &CallId, event: &E) {
        let flat = RwiEvent::from_spec(event, None);
        self.dispatch(call_id, &flat, DispatchTarget::Owner(call_id));
    }

    pub fn fan_out<E: RwiEventSpec>(&self, context: &str, event: &E) {
        self.fan_out_excluding(context, event, None);
    }

    pub fn fan_out_excluding<E: RwiEventSpec>(
        &self,
        context: &str,
        event: &E,
        exclude: Option<&SessionId>,
    ) {
        let Some(cid) = event.call_id().map(ToOwned::to_owned) else {
            warn!("fan_out: event has no call_id, skipping");
            return;
        };
        let flat = RwiEvent::from_spec(event, None);
        self.dispatch(&cid, &flat, DispatchTarget::FanOut(context, exclude));
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

    fn create_identity() -> RwiIdentity {
        RwiIdentity {
            token: "t".into(),
            scopes: vec![],
        }
    }

    #[tokio::test]
    async fn test_broadcast_generic() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);
        gw.broadcast(&crate::rwi::CallRinging {
            call_id: "c1".into(),
        });
        let v = rx.recv().await.unwrap();
        assert!(v.to_string().contains("call_ringing"));
    }

    #[tokio::test]
    async fn test_send_to_owner_generic() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);
        gw.claim_call_ownership(&sid, "c1".into(), OwnershipMode::Control)
            .unwrap();
        gw.meta_store.insert("c1".into(), Default::default());
        gw.send_to_owner(&crate::rwi::CallAnswered {
            call_id: "c1".into(),
        });
        let v = rx.recv().await.unwrap();
        assert!(v.to_string().contains("call_answered"));
    }

    #[tokio::test]
    async fn test_call_finished_releases_both_ownership_indexes() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);
        gw.claim_call_ownership(&sid, "c1".into(), OwnershipMode::Control)
            .unwrap();

        // The final SipSession event is routed before the finished
        // notification removes ownership.
        gw.send_to_owner(&crate::rwi::CallHangup {
            call_id: "c1".into(),
            reason: Some("normal".into()),
            hangup_by: Some("callee".into()),
            sip_status: None,
        });
        assert_eq!(rx.recv().await.unwrap()["event_type"], "call_hangup");

        assert!(gw.call_finished(&"c1".to_string()));
        assert!(!gw.call_ownership.contains_key("c1"));
        assert!(!gw.sessions[&sid].read().owns_call("c1"));
        assert!(gw.meta_store.get_sync("c1").is_none());
        assert!(!gw.call_finished(&"c1".to_string()));
    }

    #[tokio::test]
    async fn test_call_record_guard_defers_cleanup_until_drop() {
        let gateway = StdArc::new(RwLock::new(RwiGateway::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sid = {
            let mut gateway = gateway.write();
            let sid = gateway.create_session(create_identity()).read().id.clone();
            gateway.set_session_event_sender(&sid, tx);
            gateway
                .claim_call_ownership(&sid, "c1".into(), OwnershipMode::Control)
                .unwrap();
            gateway.meta_store.insert(
                "c1".into(),
                crate::rwi::proto::CallMeta {
                    caller: Some("sip:caller@example.com".into()),
                    ..Default::default()
                },
            );
            sid
        };
        let guard = RwiCallRecordGuard::new(&gateway, "c1".into());

        gateway.read().send_to_owner(&crate::rwi::RecordEnd {
            call_id: "c1".into(),
            url: Some("sipflow://c1".into()),
            duration_secs: 12,
            file_size: 1024,
        });

        let event = rx.recv().await.expect("final event must reach RWI session");
        assert_eq!(event["event_type"], "record_end");
        assert_eq!(event["caller"], "sip:caller@example.com");

        {
            let gateway = gateway.read();
            assert!(gateway.call_ownership.contains_key("c1"));
            assert!(gateway.sessions[&sid].read().owns_call("c1"));
            assert!(gateway.meta_store.get_sync("c1").is_some());
        }

        drop(guard);

        let gateway = gateway.read();
        assert!(!gateway.call_ownership.contains_key("c1"));
        assert!(!gateway.sessions[&sid].read().owns_call("c1"));
        assert!(gateway.meta_store.get_sync("c1").is_none());
    }

    /// `send_to_owner` must enrich the event payload with `agent_id` /
    /// Verify send_to_owner delivers a RecordEnd event to the webhook
    /// with the payload fields intact.
    #[tokio::test]
    async fn test_send_to_owner_delivers_record_end_to_webhook() {
        let mut gw = RwiGateway::new();
        let (tx, mut rx) = broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);

        gw.send_to_owner(&crate::rwi::RecordEnd {
            call_id: "call-1".to_string(),
            url: Some("https://example.com/rec.wav".to_string()),
            duration_secs: 12,
            file_size: 1024,
        });

        let entry = rx.recv().await.expect("webhook must receive record_end");
        assert_eq!(entry.event.event_type, "record_end");
        assert_eq!(
            entry.event.payload["url"].as_str(),
            Some("https://example.com/rec.wav")
        );
        assert_eq!(entry.event.payload["duration_secs"].as_u64(), Some(12));
    }

    /// When no agent context exists for a call, enrichment must leave the
    /// payload untouched (no spurious null agent fields).
    #[tokio::test]
    async fn test_send_to_owner_no_enrichment_without_meta() {
        let mut gw = RwiGateway::new();
        let (tx, mut rx) = broadcast::channel::<EventCacheEntry>(16);
        gw.set_webhook_tx(tx);

        gw.send_to_owner(&crate::rwi::RecordEnd {
            call_id: "call-unknown".to_string(),
            url: None,
            duration_secs: 0,
            file_size: 0,
        });

        let entry = rx.recv().await.unwrap();
        assert_eq!(entry.event.event_type, "record_end");
        assert!(
            entry.event.payload.get("agent_id").is_none(),
            "no agent fields when meta absent, got: {}",
            entry.event.payload
        );
    }

    /// `broadcast_event` must enrich call-scoped broadcasts with flat context
    /// (caller/callee/names/direction) from the CallMetaStore — same as
    /// call_owner events — so cc_*/queue_*/skill_group_* carry primary call info.
    #[tokio::test]
    async fn test_broadcast_event_enriches_with_meta() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.meta_store.insert(
            "c1".to_string(),
            crate::rwi::CallMeta {
                caller: Some("sip:alice@localhost".to_string()),
                callee: Some("sip:4000@localhost".to_string()),
                caller_name: Some("alice".to_string()),
                callee_name: Some("4000".to_string()),
                direction: Some("inbound".to_string()),
                ..Default::default()
            },
        );

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallAnswered {
                call_id: "c1".into(),
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert_eq!(v["caller"].as_str(), Some("sip:alice@localhost"));
        assert_eq!(v["callee"].as_str(), Some("sip:4000@localhost"));
        assert_eq!(v["caller_name"].as_str(), Some("alice"));
        assert_eq!(v["callee_name"].as_str(), Some("4000"));
        assert_eq!(v["direction"].as_str(), Some("inbound"));
    }

    /// Broadcast without a matching meta entry must leave the payload untouched.
    #[tokio::test]
    async fn test_broadcast_event_no_meta_untouched() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallAnswered {
                call_id: "call-unknown".into(),
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert!(v.get("caller").is_none(), "no caller injected without meta");
        assert!(v.get("callee").is_none(), "no callee injected without meta");
    }

    /// Call-scoped broadcasts must carry the `root` block (root call identity)
    /// when the CallMetaStore has it.
    #[tokio::test]
    async fn test_broadcast_event_enriches_with_root() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.meta_store.insert(
            "c1".to_string(),
            crate::rwi::CallMeta {
                caller: Some("sip:alice@localhost".to_string()),
                callee: Some("sip:4000@localhost".to_string()),
                root: Some(crate::rwi::RootCallInfo {
                    caller: Some("sip:alice@localhost".to_string()),
                    caller_name: Some("alice".to_string()),
                    callee: Some("sip:4000@localhost".to_string()),
                    callee_name: Some("4000".to_string()),
                    call_id: Some("c1".to_string()),
                    start_time: Some("2026-01-01T00:00:00Z".to_string()),
                }),
                ..Default::default()
            },
        );

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallAnswered {
                call_id: "c1".into(),
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert_eq!(v["root"]["call_id"].as_str(), Some("c1"));
        assert_eq!(v["root"]["caller"].as_str(), Some("sip:alice@localhost"));
        assert_eq!(v["root"]["callee_name"].as_str(), Some("4000"));
        assert_eq!(
            v["root"]["start_time"].as_str(),
            Some("2026-01-01T00:00:00Z")
        );
    }

    /// Call-scoped events must carry enrichment `session_id` from CallMetaStore
    /// so webhook consumers can correlate legs without the retired `root_call_id`.
    #[tokio::test]
    async fn test_broadcast_event_enriches_with_session_id() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.meta_store.insert(
            "leg-child".to_string(),
            crate::rwi::CallMeta {
                session_id: Some("root-call-42".to_string()),
                caller: Some("sip:alice@localhost".to_string()),
                callee: Some("sip:agent@localhost".to_string()),
                ..Default::default()
            },
        );

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallAnswered {
                call_id: "leg-child".into(),
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert_eq!(v["call_id"].as_str(), Some("leg-child"));
        assert_eq!(
            v["session_id"].as_str(),
            Some("root-call-42"),
            "session_id must be enriched from CallMeta for webhook correlation"
        );
    }

    /// When the event already carries its own `caller` field (e.g. cc_ringing
    /// or call_created), enrichment must not overwrite it with the context value.
    #[tokio::test]
    async fn test_broadcast_event_preserves_explicit_caller() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.meta_store.insert(
            "c1".to_string(),
            crate::rwi::CallMeta {
                caller: Some("sip:ctx-caller@localhost".to_string()),
                callee: Some("sip:ctx-callee@localhost".to_string()),
                ..Default::default()
            },
        );

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallCreated {
                call_id: "c1".into(),
                context: "default".into(),
                caller: "sip:explicit-caller@localhost".into(),
                callee: "sip:explicit-callee@localhost".into(),
                trunk: None,
                sip_headers: Default::default(),
                caller_name: None,
                callee_name: None,
                called_phone: None,
                app_id: None,
                routing_target: None,
                uuid: None,
                routing_path: None,
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert_eq!(
            v["caller"].as_str(),
            Some("sip:explicit-caller@localhost"),
            "event's own caller must win over context"
        );
        assert_eq!(
            v["callee"].as_str(),
            Some("sip:explicit-callee@localhost"),
            "event's own callee must win over context"
        );
    }

    /// When `src_ip` is configured, every dispatched event must carry it —
    /// including broadcast events with no call context.
    #[tokio::test]
    async fn test_src_ip_injected_into_broadcast_event() {
        let mut gw = RwiGateway::new();
        gw.set_src_ip(Some("10.0.0.1".to_string()));
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallAnswered {
                call_id: "c1".into(),
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert_eq!(v["src_ip"].as_str(), Some("10.0.0.1"));
    }

    /// `src_ip` must also be injected for call-scoped owner events (no meta).
    #[tokio::test]
    async fn test_src_ip_injected_into_owner_event() {
        let mut gw = RwiGateway::new();
        gw.set_src_ip(Some("10.0.0.2".to_string()));
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);
        gw.claim_call_ownership(&sid, "c1".into(), OwnershipMode::Control)
            .unwrap();

        gw.send_to_owner(&crate::rwi::CallRinging {
            call_id: "c1".into(),
        });

        let v = rx.recv().await.unwrap();
        assert_eq!(v["src_ip"].as_str(), Some("10.0.0.2"));
    }

    /// No `src_ip` configured → no injection (single-node mode).
    #[tokio::test]
    async fn test_src_ip_absent_when_not_configured() {
        let mut gw = RwiGateway::new();
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.broadcast_event(&crate::rwi::event::to_legacy_event(
            &crate::rwi::CallAnswered {
                call_id: "c1".into(),
            },
            None,
        ));

        let v = rx.recv().await.unwrap();
        assert!(v.get("src_ip").is_none());
    }

    /// An event that already carries `src_ip` must not be overwritten.
    #[tokio::test]
    async fn test_src_ip_not_overwritten() {
        let mut gw = RwiGateway::new();
        gw.set_src_ip(Some("10.0.0.1".to_string()));
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        let mut payload = serde_json::json!({
            "event_type": "cc_ringing",
            "call_id": "c1",
            "agent_id": "agent-1",
            "src_ip": "192.168.1.9",
        });
        payload["event_type"] = "cc_ringing".into();
        gw.broadcast_event(&crate::rwi::event::RwiEvent {
            event_type: "cc_ringing",
            call_id: Some("c1".into()),
            payload,
        });

        let v = rx.recv().await.unwrap();
        assert_eq!(
            v["src_ip"].as_str(),
            Some("192.168.1.9"),
            "event's own src_ip must win"
        );
    }

    /// With a `client_ip_lookup` configured, events carrying `agent_id` get a
    /// `client_ip` stamped from the lookup.
    #[tokio::test]
    async fn test_client_ip_injected_for_agent_events() {
        let mut gw = RwiGateway::new();
        gw.set_client_ip_lookup(Some(std::sync::Arc::new(|agent_id: &str| {
            if agent_id == "agent-1" {
                Some("192.168.1.100".to_string())
            } else {
                None
            }
        })));
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.broadcast_event(&crate::rwi::event::RwiEvent {
            event_type: "agent_state_changed",
            call_id: None,
            payload: serde_json::json!({
                "event_type": "agent_state_changed",
                "agent_id": "agent-1",
                "from_status": "offline",
                "to_status": "idle",
            }),
        });

        let v = rx.recv().await.unwrap();
        assert_eq!(v["client_ip"].as_str(), Some("192.168.1.100"));
        assert_eq!(v["agent_id"].as_str(), Some("agent-1"));
    }

    /// Events without an `agent_id` must not get a `client_ip`, and unknown
    /// agents (lookup returns None) must leave the payload untouched.
    #[tokio::test]
    async fn test_client_ip_absent_for_unknown_agent() {
        let mut gw = RwiGateway::new();
        gw.set_client_ip_lookup(Some(std::sync::Arc::new(|_| None)));
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.broadcast_event(&crate::rwi::event::RwiEvent {
            event_type: "agent_state_changed",
            call_id: None,
            payload: serde_json::json!({
                "event_type": "agent_state_changed",
                "agent_id": "unknown-agent",
                "to_status": "idle",
            }),
        });

        let v = rx.recv().await.unwrap();
        assert!(
            v.get("client_ip").is_none(),
            "no client_ip injected for unknown agent"
        );
    }

    /// An event that already carries `client_ip` must not be overwritten.
    #[tokio::test]
    async fn test_client_ip_not_overwritten() {
        let mut gw = RwiGateway::new();
        gw.set_client_ip_lookup(Some(std::sync::Arc::new(|_| Some("10.9.9.9".to_string()))));
        let sid = gw.create_session(create_identity()).read().id.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        gw.set_session_event_sender(&sid, tx);

        gw.broadcast_event(&crate::rwi::event::RwiEvent {
            event_type: "cc_ringing",
            call_id: Some("c1".into()),
            payload: serde_json::json!({
                "event_type": "cc_ringing",
                "call_id": "c1",
                "agent_id": "agent-1",
                "client_ip": "192.168.1.50",
            }),
        });

        let v = rx.recv().await.unwrap();
        assert_eq!(
            v["client_ip"].as_str(),
            Some("192.168.1.50"),
            "event's own client_ip must win"
        );
    }
}
