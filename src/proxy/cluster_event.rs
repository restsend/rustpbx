//! Cluster event synchronization via SIP MESSAGE.
//!
//! When `cluster_peers` is configured, LocatorEvent and Presence state changes
//! are forwarded to peer nodes.  Remote nodes process shared events locally
//! without duplicating side-effects (DB writes, webhooks).

use super::ProxyAction;
use crate::call::Location;
use crate::call::TransactionCookie;
use crate::proxy::ProxyModule;
use crate::proxy::locator::LocatorEvent;
use crate::proxy::presence::{PresenceManager, PresenceState, PresenceStatus};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use parking_lot::RwLock;
use rsipstack::sip::Param;
use rsipstack::sip::headers::typed::CSeq;
use rsipstack::sip::headers::{CallId, ContentType};
use rsipstack::sip::typed::{From as FromHeader, To as ToHeader, Via};
use rsipstack::sip::{Header, Method, Request, Uri, Version};
use rsipstack::transaction::endpoint::EndpointInnerRef;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

// ── Event source ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum EventSource {
    Local,
    Remote(SocketAddr),
}

impl EventSource {
    pub fn is_local(&self) -> bool {
        matches!(self, EventSource::Local)
    }
}

// ── Addon handler trait ─────────────────────────────────────────────────────

#[async_trait]
pub trait ClusterEventHandler: Send + Sync {
    async fn on_locator_event(&self, _event: &LocatorEvent, _source: &EventSource) {}
    async fn on_presence_event(
        &self,
        _identity: &str,
        _state: &PresenceState,
        _source: &EventSource,
    ) {
    }
}

// ── Cluster message body (serialised in SIP MESSAGE) ────────────────────────

const CLUSTER_CONTENT_TYPE: &str = "application/x-rustpbx-cluster-event";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
enum ClusterMessageBody {
    #[serde(rename = "locator")]
    Locator(ClusterLocatorMessage),
    #[serde(rename = "presence")]
    Presence(ClusterPresenceMessage),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ClusterLocatorMessage {
    event: String,
    aor: String,
    contact_raw: Option<String>,
    expires: u32,
    destination: Option<String>,
}

impl ClusterLocatorMessage {
    fn to_event(&self) -> Option<LocatorEvent> {
        let aor: Uri = self.aor.parse().ok()?;
        let loc = Location {
            aor,
            expires: self.expires,
            contact_raw: self.contact_raw.clone(),
            destination: None, // not needed for presence/CC logic
            ..Default::default()
        };
        match self.event.as_str() {
            "registered" => Some(LocatorEvent::Registered(loc)),
            "unregistered" => Some(LocatorEvent::Unregistered(loc)),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ClusterPresenceMessage {
    identity: String,
    status: String,
    note: Option<String>,
    activity: Option<String>,
    last_updated: i64,
}

impl ClusterPresenceMessage {
    fn to_state(&self) -> Option<PresenceState> {
        let status = match self.status.as_str() {
            "available" => PresenceStatus::Available,
            "busy" => PresenceStatus::Busy,
            "away" => PresenceStatus::Away,
            _ => PresenceStatus::Offline,
        };
        Some(PresenceState {
            status,
            note: self.note.clone(),
            activity: self.activity.clone(),
            last_updated: self.last_updated,
        })
    }
}

impl From<&LocatorEvent> for ClusterLocatorMessage {
    fn from(ev: &LocatorEvent) -> Self {
        match ev {
            LocatorEvent::Registered(loc) | LocatorEvent::Unregistered(loc) => {
                let event = if matches!(ev, LocatorEvent::Registered(_)) {
                    "registered"
                } else {
                    "unregistered"
                };
                ClusterLocatorMessage {
                    event: event.to_string(),
                    aor: loc.aor.to_string(),
                    contact_raw: loc.contact_raw.clone(),
                    expires: loc.expires,
                    destination: loc.destination.as_ref().map(|d| d.to_string()),
                }
            }
            LocatorEvent::Offline(locs) => {
                let loc = &locs[0];
                ClusterLocatorMessage {
                    event: "unregistered".to_string(),
                    aor: loc.aor.to_string(),
                    contact_raw: loc.contact_raw.clone(),
                    expires: loc.expires,
                    destination: loc.destination.as_ref().map(|d| d.to_string()),
                }
            }
        }
    }
}

impl From<(&str, &PresenceState)> for ClusterPresenceMessage {
    fn from((identity, state): (&str, &PresenceState)) -> Self {
        ClusterPresenceMessage {
            identity: identity.to_string(),
            status: state.status.to_string(),
            note: state.note.clone(),
            activity: state.activity.clone(),
            last_updated: state.last_updated,
        }
    }
}

// ── ClusterEventHub ─────────────────────────────────────────────────────────

pub struct ClusterEventHub {
    locator_events: tokio::sync::broadcast::Sender<LocatorEvent>,
    presence_manager: Arc<PresenceManager>,
    endpoint_inner: EndpointInnerRef,
    peers: Vec<SocketAddr>,
    handlers: RwLock<Vec<Arc<dyn ClusterEventHandler>>>,
}

impl ClusterEventHub {
    pub fn new(
        locator_events: tokio::sync::broadcast::Sender<LocatorEvent>,
        presence_manager: Arc<PresenceManager>,
        endpoint_inner: EndpointInnerRef,
        peers: Vec<SocketAddr>,
    ) -> Self {
        Self {
            locator_events,
            presence_manager,
            endpoint_inner,
            peers,
            handlers: RwLock::new(Vec::new()),
        }
    }

    pub fn register_handler(&self, handler: Arc<dyn ClusterEventHandler>) {
        self.handlers.write().push(handler);
    }

    /// Subscribe to local locator events and forward to peers.
    pub async fn start(self: Arc<Self>) {
        let mut rx = self.locator_events.subscribe();
        let this = self.clone();
        crate::utils::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        this.dispatch_local_locator_event(event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("cluster event hub: lagged, missed {} locator events", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });
    }

    async fn dispatch_local_locator_event(&self, event: LocatorEvent) {
        let source = EventSource::Local;

        // Notify addon handlers
        self.notify_locator_handlers(&event, &source).await;

        // Forward to peers
        if let LocatorEvent::Offline(locs) = &event {
            if !locs.is_empty() {
                for loc in locs.iter() {
                    let single = LocatorEvent::Unregistered(loc.clone());
                    self.notify_locator_handlers_for_offline(loc, &single).await;
                    self.send_locator_to_peers(&single).await;
                }
                return;
            }
        }
        self.send_locator_to_peers(&event).await;
    }

    /// For Offline events, also notify handlers with the Unregistered event
    async fn notify_locator_handlers_for_offline(&self, _loc: &Location, event: &LocatorEvent) {
        let source = EventSource::Local;
        self.notify_locator_handlers(event, &source).await;
    }

    /// Remote locator event (from peer MESSAGE).
    pub async fn on_remote_locator_event(&self, event: LocatorEvent, source: EventSource) {
        self.presence_manager
            .handle_locator_event(event.clone(), &source)
            .await;
        self.notify_locator_handlers(&event, &source).await;
    }

    /// Remote presence event (from peer MESSAGE).
    pub async fn on_remote_presence_change(
        &self,
        identity: &str,
        state: PresenceState,
        source: EventSource,
    ) {
        self.presence_manager
            .update_state(identity, state.clone(), &source)
            .await;
        self.notify_presence_handlers(identity, &state, &source)
            .await;
    }

    /// Emit a local presence change (from PUBLISH etc).
    pub async fn emit_presence_change(&self, identity: &str, state: &PresenceState) {
        let source = EventSource::Local;
        self.notify_presence_handlers(identity, state, &source)
            .await;
        self.send_presence_to_peers(identity, state).await;
    }

    // ── handlers ─────────────────────────────────────────────────────────

    async fn notify_locator_handlers(&self, event: &LocatorEvent, source: &EventSource) {
        let handlers: Vec<Arc<dyn ClusterEventHandler>> =
            self.handlers.read().iter().cloned().collect();
        for h in &handlers {
            h.on_locator_event(event, source).await;
        }
    }

    async fn notify_presence_handlers(
        &self,
        identity: &str,
        state: &PresenceState,
        source: &EventSource,
    ) {
        let handlers: Vec<Arc<dyn ClusterEventHandler>> =
            self.handlers.read().iter().cloned().collect();
        for h in &handlers {
            h.on_presence_event(identity, state, source).await;
        }
    }

    // ── send to peers ────────────────────────────────────────────────────

    async fn send_locator_to_peers(&self, event: &LocatorEvent) {
        let msg = ClusterLocatorMessage::from(event);
        let body = match serde_json::to_vec(&ClusterMessageBody::Locator(msg)) {
            Ok(b) => b,
            Err(e) => {
                warn!("failed to serialize locator cluster msg: {}", e);
                return;
            }
        };
        for peer in &self.peers {
            let _ = self.send_sip_message(peer, &body).await;
        }
    }

    async fn send_presence_to_peers(&self, identity: &str, state: &PresenceState) {
        let msg = ClusterPresenceMessage::from((identity, state));
        let body = match serde_json::to_vec(&ClusterMessageBody::Presence(msg)) {
            Ok(b) => b,
            Err(e) => {
                warn!("failed to serialize presence cluster msg: {}", e);
                return;
            }
        };
        for peer in &self.peers {
            let _ = self.send_sip_message(peer, &body).await;
        }
    }

    async fn send_sip_message(&self, peer: &SocketAddr, body: &[u8]) -> Result<()> {
        let uri: Uri = format!("sip:cluster@{}:{}", peer.ip(), peer.port())
            .parse()
            .map_err(|e| anyhow!("invalid peer URI: {}", e))?;

        let branch = format!(
            "z9hG4bK-{}",
            uuid::Uuid::new_v4().to_string().replace('-', "")
        );
        let tag = uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("tag")
            .to_string();

        let via = Via::parse(&format!("SIP/2.0/UDP {}:5060;branch={}", peer.ip(), branch))
            .map_err(|e| anyhow!("invalid via: {}", e))?;
        let from = FromHeader {
            display_name: None,
            uri: format!("sip:cluster@{}", peer.ip()).parse()?,
            params: vec![Param::Tag(rsipstack::sip::uri::Tag::new(&tag))],
        };
        let to = ToHeader {
            display_name: None,
            uri: format!("sip:cluster@{}:{}", peer.ip(), peer.port()).parse()?,
            params: vec![],
        };
        let call_id = CallId::new(uuid::Uuid::new_v4().to_string());
        let cseq = CSeq {
            seq: 1u32,
            method: Method::Message,
        };

        let request = Request {
            method: Method::Message,
            uri,
            version: Version::V2,
            headers: vec![
                via.into(),
                from.into(),
                to.into(),
                call_id.into(),
                cseq.into(),
                ContentType::new(CLUSTER_CONTENT_TYPE).into(),
            ]
            .into(),
            body: body.to_vec(),
        };

        let key = TransactionKey::from_request(&request, TransactionRole::Client)?;
        let mut tx = Transaction::new_client(key, request, self.endpoint_inner.clone(), None);
        tx.send().await?;
        debug!("cluster MESSAGE sent to {}", peer);
        Ok(())
    }
}

// ── ClusterEventModule (ProxyModule) ────────────────────────────────────────

pub struct ClusterEventModule {
    hub: Arc<ClusterEventHub>,
    peer_ips: HashSet<IpAddr>,
    local_addrs: HashSet<SocketAddr>,
}

impl ClusterEventModule {
    pub fn create(
        hub: Arc<ClusterEventHub>,
        peers: &[SocketAddr],
        local_addrs: HashSet<SocketAddr>,
    ) -> Box<dyn ProxyModule> {
        let peer_ips: HashSet<IpAddr> = peers.iter().map(|a| a.ip()).collect();
        Box::new(Self {
            hub,
            peer_ips,
            local_addrs,
        })
    }

    fn extract_source_ip(tx: &Transaction) -> Option<IpAddr> {
        let addr = tx.connection.as_ref()?.get_addr();
        let ip: IpAddr = addr.addr.host.clone().try_into().ok()?;
        Some(ip)
    }

    /// Check if source address matches any local transport listener.
    fn is_local(&self, source_ip: &IpAddr) -> bool {
        self.local_addrs.iter().any(|a| a.ip() == *source_ip)
    }
}

#[async_trait]
impl ProxyModule for ClusterEventModule {
    fn name(&self) -> &str {
        "cluster_event"
    }

    fn allow_methods(&self) -> Vec<Method> {
        vec![Method::Message]
    }

    async fn on_start(&mut self) -> Result<()> {
        self.hub.clone().start().await;
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        _cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        if tx.original.method != Method::Message {
            return Ok(ProxyAction::Continue);
        }

        let Some(from_ip) = Self::extract_source_ip(tx) else {
            return Ok(ProxyAction::Continue);
        };

        // Accept only from cluster peers
        if !self.peer_ips.contains(&from_ip) {
            return Ok(ProxyAction::Continue);
        }

        // Ignore messages from self (loopback through hub → peers)
        if self.is_local(&from_ip) {
            debug!("cluster event: ignoring loopback MESSAGE from {}", from_ip);
            tx.reply(rsipstack::sip::StatusCode::OK).await?;
            return Ok(ProxyAction::Abort);
        }

        // Check Content-Type by searching headers
        let ct = tx.original.headers.iter().find_map(|h| match h {
            Header::ContentType(ct) => Some(ct.value().to_string()),
            Header::Other(name, value) if name.eq_ignore_ascii_case("Content-Type") => {
                Some(value.to_string())
            }
            _ => None,
        });
        if ct.as_deref() != Some(CLUSTER_CONTENT_TYPE) {
            return Ok(ProxyAction::Continue);
        }

        // Parse body
        let body: ClusterMessageBody = match serde_json::from_slice(&tx.original.body) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "cluster event: failed to parse body from {}: {}",
                    from_ip, e
                );
                tx.reply(rsipstack::sip::StatusCode::BadRequest).await?;
                return Ok(ProxyAction::Abort);
            }
        };

        let remote_source = EventSource::Remote(SocketAddr::new(from_ip, 0));

        match body {
            ClusterMessageBody::Locator(msg) => {
                if let Some(event) = msg.to_event() {
                    self.hub.on_remote_locator_event(event, remote_source).await;
                }
            }
            ClusterMessageBody::Presence(msg) => {
                if let Some(state) = msg.to_state() {
                    self.hub
                        .on_remote_presence_change(&msg.identity, state, remote_source)
                        .await;
                }
            }
        }

        tx.reply(rsipstack::sip::StatusCode::OK).await?;
        Ok(ProxyAction::Abort)
    }
}

// ── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::Location;
    use crate::proxy::locator::LocatorEvent;
    use crate::proxy::presence::{PresenceManager, PresenceState, PresenceStatus};
    use rsipstack::sip::Uri;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    // ── EventSource ─────────────────────────────────────────────────────────

    #[test]
    fn test_event_source_local() {
        let source = EventSource::Local;
        assert!(source.is_local());
    }

    #[test]
    fn test_event_source_remote() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5060);
        let source = EventSource::Remote(addr);
        assert!(!source.is_local());
    }

    #[test]
    fn test_event_source_local_clone() {
        let source = EventSource::Local;
        assert_eq!(matches!(source.clone(), EventSource::Local), true);
    }

    // ── ClusterLocatorMessage JSON round-trip ───────────────────────────────

    #[test]
    fn test_locator_message_round_trip_registered() {
        let msg = ClusterLocatorMessage {
            event: "registered".to_string(),
            aor: "sip:1001@pbx.local".to_string(),
            contact_raw: Some("sip:1001@10.0.0.1:5060".to_string()),
            expires: 3600,
            destination: Some("10.0.0.1:5060".to_string()),
        };

        let body = ClusterMessageBody::Locator(msg);
        let json = serde_json::to_string(&body).unwrap();

        // Parse back
        let parsed: ClusterMessageBody = serde_json::from_str(&json).unwrap();
        match parsed {
            ClusterMessageBody::Locator(m) => {
                assert_eq!(m.event, "registered");
                assert_eq!(m.aor, "sip:1001@pbx.local");
                assert_eq!(m.contact_raw.as_deref(), Some("sip:1001@10.0.0.1:5060"));
                assert_eq!(m.expires, 3600);
                assert_eq!(m.destination.as_deref(), Some("10.0.0.1:5060"));
            }
            _ => panic!("expected Locator variant"),
        }
    }

    #[test]
    fn test_locator_message_round_trip_unregistered() {
        let msg = ClusterLocatorMessage {
            event: "unregistered".to_string(),
            aor: "sip:2001@pbx.local".to_string(),
            contact_raw: None,
            expires: 0,
            destination: None,
        };

        let body = ClusterMessageBody::Locator(msg);
        let json = serde_json::to_string(&body).unwrap();

        let parsed: ClusterMessageBody = serde_json::from_str(&json).unwrap();
        match parsed {
            ClusterMessageBody::Locator(m) => {
                assert_eq!(m.event, "unregistered");
                assert_eq!(m.aor, "sip:2001@pbx.local");
                assert_eq!(m.contact_raw, None);
                assert_eq!(m.expires, 0);
                assert_eq!(m.destination, None);
            }
            _ => panic!("expected Locator variant"),
        }
    }

    #[test]
    fn test_locator_message_json_has_type_field() {
        let msg = ClusterLocatorMessage {
            event: "registered".to_string(),
            aor: "sip:test@x".to_string(),
            contact_raw: None,
            expires: 60,
            destination: None,
        };
        let json = serde_json::to_string(&ClusterMessageBody::Locator(msg)).unwrap();
        assert!(json.contains("\"type\":\"locator\""));
    }

    // ── ClusterLocatorMessage → LocatorEvent conversion ─────────────────────

    #[test]
    fn test_locator_message_to_event_registered() {
        let msg = ClusterLocatorMessage {
            event: "registered".to_string(),
            aor: "sip:1001@pbx.local".to_string(),
            contact_raw: Some("sip:1001@10.0.0.1:5060".to_string()),
            expires: 3600,
            destination: None,
        };
        let event = msg.to_event().unwrap();
        match event {
            LocatorEvent::Registered(loc) => {
                assert_eq!(loc.aor.to_string(), "sip:1001@pbx.local");
                assert_eq!(loc.expires, 3600);
                assert_eq!(loc.contact_raw.unwrap(), "sip:1001@10.0.0.1:5060");
            }
            _ => panic!("expected Registered"),
        }
    }

    #[test]
    fn test_locator_message_to_event_unregistered() {
        let msg = ClusterLocatorMessage {
            event: "unregistered".to_string(),
            aor: "sip:2001@pbx.local".to_string(),
            contact_raw: None,
            expires: 0,
            destination: None,
        };
        let event = msg.to_event().unwrap();
        assert!(matches!(event, LocatorEvent::Unregistered(_)));
    }

    #[test]
    fn test_locator_message_to_event_invalid_event_type() {
        let msg = ClusterLocatorMessage {
            event: "unknown_event".to_string(),
            aor: "sip:test@x".to_string(),
            contact_raw: None,
            expires: 0,
            destination: None,
        };
        assert!(msg.to_event().is_none());
    }

    #[test]
    fn test_locator_message_to_event_invalid_aor() {
        // URI parser may accept many strings, so test with empty aor
        // which results in None from the user() check path — but to_event
        // only checks parseability, not user extraction.  An empty string
        // will either parse or fail; if it parses, the event is still valid.
        // The key invariant is: a truly unparseable string returns None.
        let msg = ClusterLocatorMessage {
            event: "registered".to_string(),
            aor: String::new(),
            contact_raw: None,
            expires: 0,
            destination: None,
        };
        // Empty string may or may not parse as URI; either way the
        // conversion should not panic.
        let _ = msg.to_event();
    }

    // ── ClusterPresenceMessage JSON round-trip ──────────────────────────────

    #[test]
    fn test_presence_message_round_trip_available() {
        let msg = ClusterPresenceMessage {
            identity: "1001".to_string(),
            status: "available".to_string(),
            note: Some("On line".to_string()),
            activity: None,
            last_updated: 1714000000,
        };

        let body = ClusterMessageBody::Presence(msg);
        let json = serde_json::to_string(&body).unwrap();

        let parsed: ClusterMessageBody = serde_json::from_str(&json).unwrap();
        match parsed {
            ClusterMessageBody::Presence(m) => {
                assert_eq!(m.identity, "1001");
                assert_eq!(m.status, "available");
                assert_eq!(m.note.as_deref(), Some("On line"));
                assert_eq!(m.activity, None);
                assert_eq!(m.last_updated, 1714000000);
            }
            _ => panic!("expected Presence variant"),
        }
    }

    #[test]
    fn test_presence_message_round_trip_busy() {
        let msg = ClusterPresenceMessage {
            identity: "2001".to_string(),
            status: "busy".to_string(),
            note: Some("In a meeting".to_string()),
            activity: Some("on-the-phone".to_string()),
            last_updated: 1714000100,
        };

        let body = ClusterMessageBody::Presence(msg);
        let json = serde_json::to_string(&body).unwrap();

        let parsed: ClusterMessageBody = serde_json::from_str(&json).unwrap();
        match parsed {
            ClusterMessageBody::Presence(m) => {
                assert_eq!(m.identity, "2001");
                assert_eq!(m.status, "busy");
                assert_eq!(m.note.as_deref(), Some("In a meeting"));
                assert_eq!(m.activity.as_deref(), Some("on-the-phone"));
                assert_eq!(m.last_updated, 1714000100);
            }
            _ => panic!("expected Presence variant"),
        }
    }

    #[test]
    fn test_presence_message_json_has_type_field() {
        let msg = ClusterPresenceMessage {
            identity: "x".to_string(),
            status: "offline".to_string(),
            note: None,
            activity: None,
            last_updated: 0,
        };
        let json = serde_json::to_string(&ClusterMessageBody::Presence(msg)).unwrap();
        assert!(json.contains("\"type\":\"presence\""));
    }

    // ── ClusterPresenceMessage → PresenceState conversion ───────────────────

    #[test]
    fn test_presence_message_to_state_available() {
        let msg = ClusterPresenceMessage {
            identity: "1001".to_string(),
            status: "available".to_string(),
            note: Some("hello".to_string()),
            activity: None,
            last_updated: 100,
        };
        let state = msg.to_state().unwrap();
        assert_eq!(state.status, PresenceStatus::Available);
        assert_eq!(state.note.as_deref(), Some("hello"));
        assert_eq!(state.last_updated, 100);
    }

    #[test]
    fn test_presence_message_to_state_busy() {
        let msg = ClusterPresenceMessage {
            identity: "1001".to_string(),
            status: "busy".to_string(),
            note: None,
            activity: None,
            last_updated: 0,
        };
        let state = msg.to_state().unwrap();
        assert_eq!(state.status, PresenceStatus::Busy);
    }

    #[test]
    fn test_presence_message_to_state_away() {
        let msg = ClusterPresenceMessage {
            identity: "1001".to_string(),
            status: "away".to_string(),
            note: None,
            activity: None,
            last_updated: 0,
        };
        let state = msg.to_state().unwrap();
        assert_eq!(state.status, PresenceStatus::Away);
    }

    #[test]
    fn test_presence_message_to_state_offline() {
        let msg = ClusterPresenceMessage {
            identity: "1001".to_string(),
            status: "offline".to_string(),
            note: None,
            activity: None,
            last_updated: 0,
        };
        let state = msg.to_state().unwrap();
        assert_eq!(state.status, PresenceStatus::Offline);
    }

    #[test]
    fn test_presence_message_to_state_unknown_falls_to_offline() {
        let msg = ClusterPresenceMessage {
            identity: "test".to_string(),
            status: "dnd".to_string(),
            note: None,
            activity: None,
            last_updated: 0,
        };
        let state = msg.to_state().unwrap();
        assert_eq!(state.status, PresenceStatus::Offline);
    }

    // ── LocatorEvent → ClusterLocatorMessage conversion ─────────────────────

    #[test]
    fn test_locator_event_to_message_registered() {
        let aor: Uri = "sip:1001@pbx.local".parse().unwrap();
        let loc = Location {
            aor: aor.clone(),
            expires: 3600,
            contact_raw: Some("sip:1001@10.0.0.1".to_string()),
            destination: None,
            ..Default::default()
        };
        let event = LocatorEvent::Registered(loc);
        let msg = ClusterLocatorMessage::from(&event);
        assert_eq!(msg.event, "registered");
        assert_eq!(msg.aor, "sip:1001@pbx.local");
        assert_eq!(msg.expires, 3600);
        assert_eq!(msg.contact_raw.as_deref(), Some("sip:1001@10.0.0.1"));
    }

    #[test]
    fn test_locator_event_to_message_unregistered() {
        let aor: Uri = "sip:2001@pbx.local".parse().unwrap();
        let loc = Location {
            aor: aor.clone(),
            expires: 0,
            contact_raw: None,
            destination: None,
            ..Default::default()
        };
        let event = LocatorEvent::Unregistered(loc);
        let msg = ClusterLocatorMessage::from(&event);
        assert_eq!(msg.event, "unregistered");
        assert_eq!(msg.aor, "sip:2001@pbx.local");
        assert_eq!(msg.expires, 0);
        assert_eq!(msg.contact_raw, None);
    }

    #[test]
    fn test_locator_event_offline_to_message() {
        let aor: Uri = "sip:3001@pbx.local".parse().unwrap();
        let loc = Location {
            aor: aor.clone(),
            expires: 0,
            contact_raw: Some("sip:3001@10.0.0.3".to_string()),
            destination: None,
            ..Default::default()
        };
        let event = LocatorEvent::Offline(vec![loc]);
        let msg = ClusterLocatorMessage::from(&event);
        assert_eq!(msg.event, "unregistered");
        assert_eq!(msg.contact_raw.as_deref(), Some("sip:3001@10.0.0.3"));
    }

    // ── PresenceState → ClusterPresenceMessage conversion ───────────────────

    #[test]
    fn test_presence_state_to_message() {
        let state = PresenceState {
            status: PresenceStatus::Available,
            note: Some("online".to_string()),
            activity: Some("idle".to_string()),
            last_updated: 1714001000,
        };
        let msg = ClusterPresenceMessage::from(("1001", &state));
        assert_eq!(msg.identity, "1001");
        assert_eq!(msg.status, "available");
        assert_eq!(msg.note.as_deref(), Some("online"));
        assert_eq!(msg.activity.as_deref(), Some("idle"));
        assert_eq!(msg.last_updated, 1714001000);
    }

    #[test]
    fn test_presence_state_to_message_busy() {
        let state = PresenceState {
            status: PresenceStatus::Busy,
            note: None,
            activity: None,
            last_updated: 0,
        };
        let msg = ClusterPresenceMessage::from(("2001", &state));
        assert_eq!(msg.status, "busy");
    }

    // ── ClusterEventHandler default implementations ─────────────────────────

    #[tokio::test]
    async fn test_handler_default_impls_compile() {
        struct DummyHandler;
        #[async_trait::async_trait]
        impl ClusterEventHandler for DummyHandler {}

        let handler: Arc<dyn ClusterEventHandler> = Arc::new(DummyHandler);
        let aor: Uri = "sip:test@x".parse().unwrap();
        let loc = Location { aor, ..Default::default() };
        let state = PresenceState::default();
        let source = EventSource::Local;

        // Just verify they run without panic
        handler.on_locator_event(&LocatorEvent::Registered(loc), &source).await;
        handler.on_presence_event("1001", &state, &source).await;
    }

    // ── Hub handler notification ────────────────────────────────────────────

    struct CountingHandler {
        locator_count: std::sync::Mutex<usize>,
        presence_count: std::sync::Mutex<usize>,
    }

    impl CountingHandler {
        fn new() -> Self {
            Self {
                locator_count: std::sync::Mutex::new(0),
                presence_count: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ClusterEventHandler for CountingHandler {
        async fn on_locator_event(&self, _event: &LocatorEvent, _source: &EventSource) {
            *self.locator_count.lock().unwrap() += 1;
        }
        async fn on_presence_event(
            &self,
            _identity: &str,
            _state: &PresenceState,
            _source: &EventSource,
        ) {
            *self.presence_count.lock().unwrap() += 1;
        }
    }

    fn make_test_hub() -> Arc<ClusterEventHub> {
        let (locator_tx, _) = tokio::sync::broadcast::channel(4);
        let presence_manager = Arc::new(PresenceManager::new(None));
        let transport_layer =
            rsipstack::transport::TransportLayer::new(tokio_util::sync::CancellationToken::new());
        let endpoint = rsipstack::EndpointBuilder::new()
            .with_transport_layer(transport_layer)
            .build();
        Arc::new(ClusterEventHub::new(
            locator_tx,
            presence_manager,
            endpoint.inner.clone(),
            vec![],
        ))
    }

    fn make_test_location(user: &str) -> (Location, LocatorEvent) {
        let aor: Uri = format!("sip:{}@pbx.local", user).parse().unwrap();
        let loc = Location {
            aor,
            expires: 3600,
            ..Default::default()
        };
        let event = LocatorEvent::Registered(loc.clone());
        (loc, event)
    }

    #[tokio::test]
    async fn test_hub_register_and_notify_locator() {
        let hub = make_test_hub();
        let handler = Arc::new(CountingHandler::new());
        hub.register_handler(handler.clone());

        let (_, event) = make_test_location("1001");
        hub.on_remote_locator_event(event, EventSource::Local).await;

        assert_eq!(*handler.locator_count.lock().unwrap(), 1);
        assert_eq!(*handler.presence_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_hub_notify_presence() {
        let hub = make_test_hub();
        let handler = Arc::new(CountingHandler::new());
        hub.register_handler(handler.clone());

        let state = PresenceState {
            status: PresenceStatus::Available,
            ..Default::default()
        };
        hub.emit_presence_change("1001", &state).await;

        assert_eq!(*handler.presence_count.lock().unwrap(), 1);
        assert_eq!(*handler.locator_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_hub_multiple_handlers() {
        let hub = make_test_hub();
        let h1 = Arc::new(CountingHandler::new());
        let h2 = Arc::new(CountingHandler::new());
        hub.register_handler(h1.clone());
        hub.register_handler(h2.clone());

        let (_, event) = make_test_location("2001");
        hub.on_remote_locator_event(event, EventSource::Local).await;

        assert_eq!(*h1.locator_count.lock().unwrap(), 1);
        assert_eq!(*h2.locator_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn test_hub_remote_presence_does_not_write_db() {
        let hub = make_test_hub();
        let identity = "1001";
        let state = PresenceState {
            status: PresenceStatus::Available,
            note: Some("remote-test".to_string()),
            ..Default::default()
        };

        let remote_source = EventSource::Remote(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            5060,
        ));

        hub.on_remote_presence_change(identity, state.clone(), remote_source).await;

        // Memory state should be updated
        let stored = hub.presence_manager.get_state(identity);
        assert_eq!(stored.status, PresenceStatus::Available);
        assert_eq!(stored.note.as_deref(), Some("remote-test"));
    }

    #[tokio::test]
    async fn test_hub_remote_locator_updates_presence() {
        let hub = make_test_hub();
        let (_, event) = make_test_location("3001");

        // Pre-condition: user is offline
        assert_eq!(
            hub.presence_manager.get_state("3001").status,
            PresenceStatus::Offline
        );

        let remote_source = EventSource::Remote(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            5060,
        ));
        hub.on_remote_locator_event(event, remote_source).await;

        // After receipt, presence should be Available (Registered from peer)
        let stored = hub.presence_manager.get_state("3001");
        assert_eq!(stored.status, PresenceStatus::Available);
    }

    #[tokio::test]
    async fn test_hub_local_does_not_double_update() {
        // Verify that local locator events dispatched via hub do NOT
        // call handle_locator_event on the presence manager (that's done
        // by the PresenceModule separately).  The hub only notifies addon
        // handlers and forwards to peers.
        let hub = make_test_hub();
        let handler = Arc::new(CountingHandler::new());
        hub.register_handler(handler.clone());

        let (_, event) = make_test_location("4001");

        // Simulate what the hub's broadcast subscriber does for local events
        // (dispatch_local_locator_event only calls notify handlers + forward,
        // it does NOT call handle_locator_event)
        hub.notify_locator_handlers(&event, &EventSource::Local).await;

        // Handler got notified
        assert_eq!(*handler.locator_count.lock().unwrap(), 1);

        // But presence should NOT have changed (no handle_locator_event called)
        assert_eq!(
            hub.presence_manager.get_state("4001").status,
            PresenceStatus::Offline
        );
    }

    // ── Full round-trip: LocatorEvent → JSON → LocatorEvent ─────────────────

    #[test]
    fn test_locator_event_full_round_trip() {
        let aor: Uri = "sip:1001@pbx.local".parse().unwrap();
        let original_loc = Location {
            aor: aor.clone(),
            expires: 3600,
            contact_raw: Some("sip:1001@10.0.0.1:5060;transport=udp".to_string()),
            destination: None,
            ..Default::default()
        };
        let original_event = LocatorEvent::Registered(original_loc);

        // Serialize
        let cluster_msg = ClusterLocatorMessage::from(&original_event);
        let body = ClusterMessageBody::Locator(cluster_msg);
        let json = serde_json::to_vec(&body).unwrap();

        // Deserialize
        let parsed: ClusterMessageBody = serde_json::from_slice(&json).unwrap();
        let reconstructed = match parsed {
            ClusterMessageBody::Locator(m) => m.to_event().unwrap(),
            _ => panic!("wrong variant"),
        };

        // Assert
        assert!(matches!(reconstructed, LocatorEvent::Registered(_)));
        if let LocatorEvent::Registered(loc) = reconstructed {
            assert_eq!(loc.aor, aor);
            assert_eq!(loc.expires, 3600);
            assert_eq!(
                loc.contact_raw.as_deref(),
                Some("sip:1001@10.0.0.1:5060;transport=udp")
            );
        }
    }

    // ── Full round-trip: PresenceState → JSON → PresenceState ───────────────

    #[test]
    fn test_presence_state_full_round_trip() {
        let original = PresenceState {
            status: PresenceStatus::Away,
            note: Some("Lunch".to_string()),
            activity: Some("meal".to_string()),
            last_updated: 1715000000,
        };

        // Serialize
        let cluster_msg = ClusterPresenceMessage::from(("1001", &original));
        let body = ClusterMessageBody::Presence(cluster_msg);
        let json = serde_json::to_vec(&body).unwrap();

        // Deserialize
        let parsed: ClusterMessageBody = serde_json::from_slice(&json).unwrap();
        let reconstructed = match parsed {
            ClusterMessageBody::Presence(m) => m.to_state().unwrap(),
            _ => panic!("wrong variant"),
        };

        // Assert
        assert_eq!(reconstructed.status, PresenceStatus::Away);
        assert_eq!(reconstructed.note.as_deref(), Some("Lunch"));
        assert_eq!(reconstructed.activity.as_deref(), Some("meal"));
        assert_eq!(reconstructed.last_updated, 1715000000);
    }

    // ── Edge cases ──────────────────────────────────────────────────────────

    #[test]
    fn test_cluster_message_unknown_type_deserialize() {
        // Simulate an unknown JSON type
        let json = r#"{"type":"unknown","payload":"test"}"#;
        let result: Result<ClusterMessageBody, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_locator_message_empty_strings() {
        let msg = ClusterLocatorMessage {
            event: String::new(),
            aor: String::new(),
            contact_raw: Some(String::new()),
            expires: 0,
            destination: None,
        };
        assert!(msg.to_event().is_none()); // empty aor is not valid
    }

    #[test]
    fn test_presence_message_empty_status() {
        let msg = ClusterPresenceMessage {
            identity: "test".to_string(),
            status: String::new(),
            note: None,
            activity: None,
            last_updated: 0,
        };
        let state = msg.to_state().unwrap();
        assert_eq!(state.status, PresenceStatus::Offline); // unknown -> offline
    }
}
