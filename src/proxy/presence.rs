use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::Location;
use crate::call::TransactionCookie;
use crate::config::ProxyConfig;
use crate::models::presence;
use crate::proxy::cluster_event::EventSource;
use crate::proxy::cluster_sync::ClusterSync;
use crate::proxy::locator::LocatorEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::sip::prelude::{HeadersExt, ToTypedHeader};
use rsipstack::transaction::transaction::Transaction;
use sea_orm::{DatabaseConnection, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

// ── PIDF-XML (RFC 3863) and RPID (RFC 4480) support ─────────────────────
//
// Deserialization and serialization use separate structs because quick-xml
// strips XML namespace prefixes when matching element names on deserialize
// but emits the `rename` verbatim on serialize.  RPID input carries
// prefixed elements (`<rpid:activities>`) and we must output the same
// canonical prefixed form in NOTIFY bodies.

// ── Deserialization (inbound PUBLISH) ────────────────────────────────────
//
// quick-xml matches element local names, so RPID elements are matched
// by `activities`, `away`, `busy`, `on-the-phone`.  The `@entity` and
// `@xmlns` attributes are optional — cc-phone omits them in PUBLISH.

#[derive(Debug, Deserialize, Default)]
#[serde(rename = "presence")]
struct IncomingPresence {
    #[serde(rename = "tuple", default)]
    tuples: Vec<IncomingTuple>,
    #[serde(rename = "note", default)]
    notes: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct IncomingTuple {
    status: Option<IncomingStatus>,
    #[serde(rename = "note", default)]
    note: Option<String>,
    #[serde(rename = "activities", default)]
    activities: Option<RpidActivities>,
}

#[derive(Debug, Deserialize, Default)]
struct IncomingStatus {
    basic: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RpidActivities {
    #[serde(rename = "away", default)]
    away: Option<RpidEmpty>,
    #[serde(rename = "busy", default)]
    busy: Option<RpidEmpty>,
    #[serde(rename = "on-the-phone", default)]
    on_the_phone: Option<RpidEmpty>,
}

#[derive(Debug, Deserialize, Default)]
struct RpidEmpty {}

// ── Serialization (outbound NOTIFY PIDF-XML) ────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename = "presence")]
struct PidfPresence {
    #[serde(rename = "@xmlns")]
    xmlns: String,
    #[serde(
        rename = "@xmlns:rpid",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    xmlns_rpid: Option<String>,
    #[serde(rename = "@entity")]
    entity: String,
    #[serde(rename = "tuple", default)]
    tuples: Vec<PidfTuple>,
}

#[derive(Debug, Serialize)]
struct PidfTuple {
    #[serde(rename = "@id")]
    id: String,
    status: PidfStatus,
    #[serde(rename = "note", skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(rename = "contact", skip_serializing_if = "Option::is_none")]
    contact: Option<String>,
    #[serde(
        rename = "rpid:activities",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    activities: Option<OutputActivities>,
}

#[derive(Debug, Serialize)]
struct PidfStatus {
    basic: String,
}

#[derive(Debug, Serialize, Default)]
struct OutputActivities {
    #[serde(rename = "rpid:away", default, skip_serializing_if = "Option::is_none")]
    away: Option<RpidEmptySer>,
    #[serde(rename = "rpid:busy", default, skip_serializing_if = "Option::is_none")]
    busy: Option<RpidEmptySer>,
    #[serde(
        rename = "rpid:on-the-phone",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    on_the_phone: Option<RpidEmptySer>,
}

#[derive(Debug, Serialize, Default)]
struct RpidEmptySer {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
#[derive(Default)]
pub enum PresenceStatus {
    Idle,
    Busy,
    Ringing,
    Wrapup,
    Dnd,
    Away(String),
    #[default]
    Offline,
}

impl std::fmt::Display for PresenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceStatus::Idle => write!(f, "idle"),
            PresenceStatus::Busy => write!(f, "busy"),
            PresenceStatus::Ringing => write!(f, "ringing"),
            PresenceStatus::Wrapup => write!(f, "wrapup"),
            PresenceStatus::Dnd => write!(f, "dnd"),
            PresenceStatus::Away(_) => write!(f, "away"),
            PresenceStatus::Offline => write!(f, "offline"),
        }
    }
}

impl PresenceStatus {
    /// Parse a status string into a `PresenceStatus`.
    ///
    /// Accepts the canonical form (`away:<detail>`, `away`) as well as the
    /// legacy bare-detail (`lunch`) and `custom:<detail>` spellings so every
    /// inbound boundary tolerates historical payloads. Bare or prefixed detail
    /// values collapse to `Away(detail)`.
    pub fn normalize(status: &str) -> PresenceStatus {
        match status {
            "idle" | "online" | "available" => PresenceStatus::Idle,
            "dnd" => PresenceStatus::Dnd,
            "busy" => PresenceStatus::Busy,
            "ringing" => PresenceStatus::Ringing,
            "wrapup" | "wrap-up" | "wrap_up" => PresenceStatus::Wrapup,
            "offline" | "closed" | "" => PresenceStatus::Offline,
            "away" => PresenceStatus::Away(String::new()),
            s if s.starts_with("away:") => PresenceStatus::Away(s["away:".len()..].to_string()),
            s if s.starts_with("custom:") => PresenceStatus::Away(s["custom:".len()..].to_string()),
            other => PresenceStatus::Away(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceState {
    pub status: PresenceStatus,
    pub note: Option<String>,
    pub activity: Option<String>,
    pub last_updated: i64,
}

impl Default for PresenceState {
    fn default() -> Self {
        Self {
            status: PresenceStatus::Offline,
            note: None,
            activity: None,
            last_updated: chrono::Utc::now().timestamp(),
        }
    }
}

/// A SIP subscription record for PRESENCE (RFC 3856).
#[derive(Clone, Debug)]
pub struct Subscriber {
    pub aor: rsipstack::sip::Uri,
    pub dialog_id: DialogId,
    pub expires: std::time::Instant,
}

/// A SIP subscription record for MWI / message-summary (RFC 3842).
#[derive(Clone, Debug)]
pub struct MwiSubscriber {
    /// Address of the subscriber (used as To: in NOTIFY).
    pub aor: rsipstack::sip::Uri,
    pub dialog_id: DialogId,
    /// Account URI for the `Message-Account:` header (e.g. "sip:1001@pbx").
    pub account_uri: String,
    pub expires: std::time::Instant,
}

/// Internal message used to trigger an MWI NOTIFY from the voicemail layer.
#[derive(Clone, Debug)]
pub struct MwiTrigger {
    /// SIP extension / mailbox owner (e.g. "1001").
    pub extension: String,
    /// Number of new (unheard) voicemail messages.
    pub new_messages: u32,
    /// Number of old (heard) voicemail messages.
    pub old_messages: u32,
}

#[derive(Clone)]
pub struct PresenceManager {
    states: Arc<RwLock<HashMap<String, PresenceState>>>,
    /// PRESENCE (RFC 3856) subscriptions keyed by subscribed-to identity.
    subscribers: Arc<RwLock<HashMap<String, Vec<Subscriber>>>>,
    /// MWI (RFC 3842) subscriptions keyed by mailbox extension.
    mwi_subscribers: Arc<RwLock<HashMap<String, Vec<MwiSubscriber>>>>,
    database: Option<DatabaseConnection>,
    notify_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<String>>>>,
    /// Channel used by the voicemail layer to request MWI NOTIFY delivery.
    mwi_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<MwiTrigger>>>>,
    cluster_sync: std::sync::Arc<parking_lot::RwLock<Option<std::sync::Arc<ClusterSync>>>>,
}

impl PresenceManager {
    pub fn new(database: Option<DatabaseConnection>) -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            mwi_subscribers: Arc::new(RwLock::new(HashMap::new())),
            database,
            notify_tx: Arc::new(RwLock::new(None)),
            mwi_tx: Arc::new(RwLock::new(None)),
            cluster_sync: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    pub fn set_cluster_sync(&self, sync: ClusterSync) {
        *self.cluster_sync.write() = Some(std::sync::Arc::new(sync));
    }

    /// Get the cluster sync for direct access in tests.
    #[cfg(test)]
    pub fn cluster_sync(&self) -> Option<std::sync::Arc<ClusterSync>> {
        self.cluster_sync.read().clone()
    }

    pub fn set_notify_tx(&self, tx: tokio::sync::mpsc::Sender<String>) {
        let mut lock = self.notify_tx.write().unwrap();
        *lock = Some(tx);
    }

    /// Drop the notify sender so the dispatcher task observes channel-closed.
    /// Called from `PresenceModule::on_stop` to ensure deterministic shutdown.
    pub fn clear_notify_tx(&self) {
        let mut lock = self.notify_tx.write().unwrap();
        *lock = None;
    }

    pub async fn load_from_db(&self) -> Result<()> {
        if let Some(db) = &self.database {
            let states = presence::Entity::find().all(db).await?;
            let mut map = self.states.write().unwrap();
            for s in states {
                let status = PresenceStatus::normalize(&s.status);
                map.insert(
                    s.identity,
                    PresenceState {
                        status,
                        note: s.note,
                        activity: s.activity,
                        last_updated: s.last_updated,
                    },
                );
            }
        }
        Ok(())
    }

    pub fn states_len(&self) -> usize {
        self.states.read().unwrap().len()
    }

    pub fn subscribers_len(&self) -> usize {
        self.subscribers.read().unwrap().len()
    }

    /// Total presence subscription bindings (sum of all identity buckets).
    pub fn subscriber_bindings_len(&self) -> usize {
        self.subscribers
            .read()
            .unwrap()
            .values()
            .map(|v| v.len())
            .sum()
    }

    pub fn mwi_subscribers_len(&self) -> usize {
        self.mwi_subscribers.read().unwrap().len()
    }

    /// Total MWI subscription bindings (sum of all extension buckets).
    pub fn mwi_subscriber_bindings_len(&self) -> usize {
        self.mwi_subscribers
            .read()
            .unwrap()
            .values()
            .map(|v| v.len())
            .sum()
    }

    pub fn get_state(&self, identity: &str) -> PresenceState {
        let map = self.states.read().unwrap();
        map.get(identity).cloned().unwrap_or_default()
    }

    /// Update the presence state for an identity.
    ///
    /// Returns the previous state (if any). When `source.is_local()` the new
    /// state is persisted to DB and broadcast to cluster peers via AMI.
    pub async fn update_state(
        &self,
        identity: &str,
        state: PresenceState,
        source: &EventSource,
    ) -> Option<PresenceState> {
        let old_state = {
            let mut map = self.states.write().unwrap();
            map.insert(identity.to_string(), state.clone())
        };

        if source.is_local() {
            // Cluster sync first (parallel fire-and-forget) — no partial borrow issues
            let msg = crate::proxy::cluster_event::ClusterPresenceMessage::from((identity, &state));
            if let Some(ref sync) = *self.cluster_sync.read() {
                sync.broadcast("presence", &msg);
            }

            // Persist to DB
            if let Some(db) = &self.database {
                let active: presence::ActiveModel = presence::ActiveModel {
                    identity: Set(identity.to_string()),
                    status: Set(state.status.to_string()),
                    note: Set(state.note.clone()),
                    activity: Set(state.activity.clone()),
                    last_updated: Set(state.last_updated),
                };

                if let Err(e) = presence::Entity::insert(active)
                    .on_conflict(
                        sea_orm::sea_query::OnConflict::column(presence::Column::Identity)
                            .update_columns([
                                presence::Column::Status,
                                presence::Column::Note,
                                presence::Column::Activity,
                                presence::Column::LastUpdated,
                            ])
                            .to_owned(),
                    )
                    .exec(db)
                    .await
                {
                    tracing::error!("failed to persist presence state for {}: {}", identity, e);
                }
            }
        }

        // Notify subscribers (triggers NOTIFY messages)
        let tx = {
            let lock = self.notify_tx.read().unwrap();
            lock.clone()
        };
        if let Some(tx) = tx {
            let _ = tx.send(identity.to_string()).await;
        }

        old_state
    }

    pub fn add_subscriber(&self, identity: &str, sub: Subscriber) -> Vec<DialogId> {
        let mut map = self.subscribers.write().unwrap();
        let subs = map.entry(identity.to_string()).or_default();
        let mut replaced = Vec::new();
        let sub_key = Self::watcher_key(&sub.aor);
        subs.retain(|s| {
            let same_dialog = s.dialog_id == sub.dialog_id;
            let same_watcher = Self::watcher_key(&s.aor) == sub_key;
            if same_dialog || same_watcher {
                if s.dialog_id != sub.dialog_id {
                    replaced.push(s.dialog_id.clone());
                }
                false
            } else {
                true
            }
        });
        subs.push(sub);
        replaced
    }

    pub fn get_subscribers(&self, identity: &str) -> Vec<Subscriber> {
        let map = self.subscribers.read().unwrap();
        map.get(identity).cloned().unwrap_or_default()
    }

    /// Remove a presence subscription by dialog id. Returns true if removed.
    pub fn remove_subscriber_by_dialog(&self, dialog_id: &DialogId) -> bool {
        let mut map = self.subscribers.write().unwrap();
        let mut removed = false;
        map.retain(|_, subs| {
            let before = subs.len();
            subs.retain(|s| &s.dialog_id != dialog_id);
            if subs.len() != before {
                removed = true;
            }
            !subs.is_empty()
        });
        removed
    }

    /// Remove all presence subscriptions whose watcher (From) matches `user`.
    /// Returns dialog ids that were dropped so callers can free dialog_layer entries.
    pub fn remove_subscribers_for_watcher(&self, user: &str) -> Vec<DialogId> {
        let user = user.trim().to_ascii_lowercase();
        if user.is_empty() {
            return Vec::new();
        }
        let mut map = self.subscribers.write().unwrap();
        let mut removed = Vec::new();
        map.retain(|_, subs| {
            subs.retain(|s| {
                let watcher = s
                    .aor
                    .user()
                    .map(|u| u.to_ascii_lowercase())
                    .unwrap_or_default();
                if watcher == user {
                    removed.push(s.dialog_id.clone());
                    false
                } else {
                    true
                }
            });
            !subs.is_empty()
        });
        removed
    }

    pub fn cleanup_expired(&self) {
        let mut subscribers = self.subscribers.write().unwrap();
        let now = std::time::Instant::now();
        subscribers.retain(|_, subs| {
            subs.retain(|s| s.expires > now);
            !subs.is_empty()
        });
    }

    fn watcher_key(uri: &rsipstack::sip::Uri) -> String {
        format!(
            "{}@{}",
            uri.user().unwrap_or_default().to_ascii_lowercase(),
            uri.host().to_string().to_ascii_lowercase()
        )
    }

    // ── MWI (RFC 3842 message-summary) ────────────────────────────────────────

    /// Set the channel used by the MWI dispatch task.
    pub fn set_mwi_tx(&self, tx: tokio::sync::mpsc::Sender<MwiTrigger>) {
        let mut lock = self.mwi_tx.write().unwrap();
        *lock = Some(tx);
    }

    /// Drop the MWI sender so the dispatch task observes channel-closed.
    /// Called from `PresenceModule::on_stop` to ensure deterministic shutdown.
    pub fn clear_mwi_tx(&self) {
        let mut lock = self.mwi_tx.write().unwrap();
        *lock = None;
    }

    /// Add (or refresh) an MWI subscription for `extension`.
    /// Returns dialog ids replaced by the same watcher AOR.
    pub fn add_mwi_subscriber(&self, extension: &str, sub: MwiSubscriber) -> Vec<DialogId> {
        let mut map = self.mwi_subscribers.write().unwrap();
        let subs = map.entry(extension.to_string()).or_default();
        let mut replaced = Vec::new();
        let sub_key = Self::watcher_key(&sub.aor);
        subs.retain(|s| {
            let same_dialog = s.dialog_id == sub.dialog_id;
            let same_watcher = Self::watcher_key(&s.aor) == sub_key;
            if same_dialog || same_watcher {
                if s.dialog_id != sub.dialog_id {
                    replaced.push(s.dialog_id.clone());
                }
                false
            } else {
                true
            }
        });
        subs.push(sub);
        replaced
    }

    /// Return all live MWI subscribers for `extension`.
    pub fn get_mwi_subscribers(&self, extension: &str) -> Vec<MwiSubscriber> {
        let map = self.mwi_subscribers.read().unwrap();
        map.get(extension).cloned().unwrap_or_default()
    }

    /// Remove an MWI subscription by dialog id. Returns true if removed.
    pub fn remove_mwi_subscriber_by_dialog(&self, dialog_id: &DialogId) -> bool {
        let mut map = self.mwi_subscribers.write().unwrap();
        let mut removed = false;
        map.retain(|_, subs| {
            let before = subs.len();
            subs.retain(|s| &s.dialog_id != dialog_id);
            if subs.len() != before {
                removed = true;
            }
            !subs.is_empty()
        });
        removed
    }

    /// Remove all MWI subscriptions whose watcher matches `user`.
    pub fn remove_mwi_subscribers_for_watcher(&self, user: &str) -> Vec<DialogId> {
        let user = user.trim().to_ascii_lowercase();
        if user.is_empty() {
            return Vec::new();
        }
        let mut map = self.mwi_subscribers.write().unwrap();
        let mut removed = Vec::new();
        map.retain(|_, subs| {
            subs.retain(|s| {
                let watcher = s
                    .aor
                    .user()
                    .map(|u| u.to_ascii_lowercase())
                    .unwrap_or_default();
                if watcher == user {
                    removed.push(s.dialog_id.clone());
                    false
                } else {
                    true
                }
            });
            !subs.is_empty()
        });
        removed
    }

    /// Remove expired MWI subscriptions.
    pub fn cleanup_expired_mwi(&self) {
        let mut map = self.mwi_subscribers.write().unwrap();
        let now = std::time::Instant::now();
        map.retain(|_, subs| {
            subs.retain(|s| s.expires > now);
            !subs.is_empty()
        });
    }

    /// Enqueue an MWI trigger so the SIP layer sends NOTIFY to all subscribers
    /// of `extension`.  This is called from the voicemail notifier.
    pub async fn trigger_mwi(&self, extension: &str, new_messages: u32, old_messages: u32) {
        let tx = {
            let lock = self.mwi_tx.read().unwrap();
            lock.clone()
        };
        if let Some(tx) = tx {
            let _ = tx
                .send(MwiTrigger {
                    extension: extension.to_string(),
                    new_messages,
                    old_messages,
                })
                .await;
        } else {
            debug!(
                extension = %extension,
                "MWI trigger: no SIP stack attached, skipping NOTIFY"
            );
        }
    }

    fn get_user(loc: &Location) -> Option<String> {
        // Prefer canonical registered AoR (sip:1001@realm). WebRTC/WS contacts
        // often use ephemeral Contact users (e.g. sip:vsfbt0co@.invalid).
        if let Some(registered) = &loc.registered_aor
            && let Some(user) = registered.user()
        {
            let user = user.trim();
            if !user.is_empty() {
                return Some(user.to_string());
            }
        }
        loc.aor
            .user()
            .map(|u| u.to_string())
            .filter(|u| !u.is_empty())
    }

    /// Drop subscription dialogs owned by a watcher that just went offline.
    fn prune_watcher_subscriptions(&self, user: &str) -> Vec<DialogId> {
        let mut removed = self.remove_subscribers_for_watcher(user);
        removed.extend(self.remove_mwi_subscribers_for_watcher(user));
        if !removed.is_empty() {
            debug!(
                watcher = %user,
                count = removed.len(),
                "Pruned presence/MWI subscriptions for offline watcher"
            );
        }
        removed
    }

    // Process locator events.
    // Returns dialog ids whose subscriptions were dropped (caller should
    // `dialog_layer.remove_dialog`).
    pub async fn handle_locator_event(
        &self,
        event: LocatorEvent,
        source: &EventSource,
    ) -> Vec<DialogId> {
        let mut pruned = Vec::new();
        match event {
            LocatorEvent::Registered(loc) => {
                if let Some(user) = Self::get_user(&loc) {
                    let current = self.get_state(&user);
                    info!(
                        extension = %user,
                        destination = %loc.destination.as_ref().map(|d| d.to_string()).unwrap_or_default(),
                        user_agent = %loc.user_agent.as_deref().unwrap_or_default(),
                        status = %current.status,
                        "Presence: Registered"
                    );

                    // Extract X-CC-Presence from custom REGISTER headers,
                    // sent by cc-phone's JsSIP UA to set presence via SIP.
                    let header_status = loc.headers.as_ref().and_then(|headers| {
                        for h in headers {
                            if let rsipstack::sip::Header::Other(name, val) = h {
                                if name.eq_ignore_ascii_case("X-CC-Presence") {
                                    return Some(val.as_str());
                                }
                            }
                        }
                        None
                    });

                    let new_status = match header_status {
                        Some(s) => PresenceStatus::normalize(s),
                        None if current.status == PresenceStatus::Offline => PresenceStatus::Idle,
                        None => return pruned,
                    };

                    self.update_state(
                        &user,
                        PresenceState {
                            status: new_status,
                            last_updated: chrono::Utc::now().timestamp(),
                            ..current
                        },
                        source,
                    )
                    .await;
                }
            }
            LocatorEvent::Unregistered(loc) => {
                if let Some(user) = Self::get_user(&loc) {
                    // Drop this watcher's subscriptions before broadcasting Offline
                    // so we do not keep retrying NOTIFY on a dead transport.
                    pruned.extend(self.prune_watcher_subscriptions(&user));
                    self.update_state(
                        &user,
                        PresenceState {
                            status: PresenceStatus::Offline,
                            last_updated: chrono::Utc::now().timestamp(),
                            ..Default::default()
                        },
                        source,
                    )
                    .await;
                }
            }
            LocatorEvent::Offline(locs) => {
                for loc in locs {
                    if let Some(user) = Self::get_user(&loc) {
                        pruned.extend(self.prune_watcher_subscriptions(&user));
                        self.update_state(
                            &user,
                            PresenceState {
                                status: PresenceStatus::Offline,
                                last_updated: chrono::Utc::now().timestamp(),
                                ..Default::default()
                            },
                            source,
                        )
                        .await;
                    }
                }
            }
        }
        pruned
    }
}

#[derive(Clone)]
pub struct PresenceModule {
    manager: Arc<PresenceManager>,
    server: SipServerRef,
}

impl PresenceModule {
    pub fn create(server: SipServerRef, _config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let manager = server.presence_manager.clone();
        Ok(Box::new(PresenceModule { manager, server }))
    }
}

#[async_trait]
impl ProxyModule for PresenceModule {
    fn name(&self) -> &str {
        "presence"
    }
    fn allow_methods(&self) -> Vec<rsipstack::sip::Method> {
        vec![
            rsipstack::sip::Method::Subscribe,
            rsipstack::sip::Method::Publish,
            rsipstack::sip::Method::Notify,
        ]
    }
    async fn on_start(&mut self) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);
        self.manager.set_notify_tx(tx);

        // All background tasks below are tied to a child of the server's
        // cancel token so they shut down deterministically on `on_stop`.
        let cancel = self.server.cancel_token.child_token();

        // Spawn listener for notification requests (e.g. from UI or PUBLISH)
        let module_clone = self.clone();
        let cancel_notify = cancel.clone();
        crate::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_notify.cancelled() => break,
                    identity = rx.recv() => {
                        let Some(identity) = identity else { break };
                        let state = module_clone.manager.get_state(&identity);
                        let subscribers = module_clone.manager.get_subscribers(&identity);
                        for sub in subscribers {
                            if let Err(e) = module_clone.send_notify(&identity, &sub, &state).await {
                                debug!(
                                    dialog_id = %sub.dialog_id,
                                    error = %e,
                                    "Presence NOTIFY failed; subscription pruned"
                                );
                            }
                        }
                    }
                }
            }
        });

        // Spawn MWI dispatch task (RFC 3842 message-summary)
        let (mwi_tx, mut mwi_rx) = tokio::sync::mpsc::channel::<MwiTrigger>(100);
        self.manager.set_mwi_tx(mwi_tx);
        let mwi_module = self.clone();
        let cancel_mwi = cancel.clone();
        crate::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_mwi.cancelled() => break,
                    trigger = mwi_rx.recv() => {
                        let Some(trigger) = trigger else { break };
                        let subscribers = mwi_module.manager.get_mwi_subscribers(&trigger.extension);
                        for sub in subscribers {
                            if let Err(e) = mwi_module.send_mwi_notify(&trigger, &sub).await {
                                debug!(
                                    dialog_id = %sub.dialog_id,
                                    error = %e,
                                    "MWI NOTIFY failed; subscription pruned"
                                );
                            }
                        }
                    }
                }
            }
        });

        // Spawn listener for locator events
        let manager = self.manager.clone();
        let dialog_layer = self.server.dialog_layer.clone();
        let cancel_locator = cancel.clone();
        if let Some(mut rx) = self.server.locator_events.as_ref().map(|tx| tx.subscribe()) {
            crate::utils::spawn(async move {
                let source = EventSource::Local;
                loop {
                    tokio::select! {
                        _ = cancel_locator.cancelled() => break,
                        res = rx.recv() => {
                            if let Ok(event) = res {
                                let pruned = manager.handle_locator_event(event, &source).await;
                                for id in pruned {
                                    dialog_layer.remove_dialog(&id);
                                }
                            } else {
                                // channel closed; exit gracefully
                                break;
                            }
                        }
                    }
                }
            });
        }

        // Spawn background cleanup for expired subscriptions (presence + MWI)
        let manager_cleanup = self.manager.clone();
        let cancel_cleanup = cancel.clone();
        crate::utils::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel_cleanup.cancelled() => break,
                    _ = interval.tick() => {
                        manager_cleanup.cleanup_expired();
                        manager_cleanup.cleanup_expired_mwi();
                    }
                }
            }
        });

        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        // Cancelling the server token's children signals every spawned task
        // above to exit promptly. We also clear the channel senders held by
        // the manager so receivers observe channel-closed even without the
        // select! arms firing.
        self.manager.clear_notify_tx();
        self.manager.clear_mwi_tx();
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: tokio_util::sync::CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsipstack::sip::Method::Subscribe => {
                // Dispatch based on the Event header value.
                let event_val = tx
                    .original
                    .headers
                    .iter()
                    .find_map(|h| {
                        if let rsipstack::sip::Header::Event(ev) = h {
                            Some(ev.value().to_ascii_lowercase())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                if event_val.starts_with("message-summary") {
                    self.handle_mwi_subscribe(tx, &cookie).await?;
                } else {
                    self.handle_subscribe(tx, &cookie).await?;
                }
                Ok(ProxyAction::Abort)
            }
            rsipstack::sip::Method::Publish => {
                self.handle_publish(tx, &cookie).await?;
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }
}

impl PresenceModule {
    /// Watch subscription dialog termination and free both the subscriber
    /// record and the dialog_layer entry (rsipstack requires explicit remove).
    fn spawn_subscription_guard(
        &self,
        mut state_rx: tokio::sync::mpsc::UnboundedReceiver<DialogState>,
        dialog_id: DialogId,
        is_mwi: bool,
        dialog_cancel: tokio_util::sync::CancellationToken,
    ) {
        let manager = self.manager.clone();
        let dialog_layer = self.server.dialog_layer.clone();
        let server_cancel = self.server.cancel_token.child_token();
        crate::utils::spawn(async move {
            let prune = |id: &DialogId| {
                if is_mwi {
                    manager.remove_mwi_subscriber_by_dialog(id);
                } else {
                    manager.remove_subscriber_by_dialog(id);
                }
                dialog_layer.remove_dialog(id);
            };
            loop {
                tokio::select! {
                    _ = server_cancel.cancelled() => break,
                    _ = dialog_cancel.cancelled() => {
                        prune(&dialog_id);
                        break;
                    }
                    state = state_rx.recv() => {
                        match state {
                            Some(DialogState::Terminated(id, reason)) => {
                                debug!(
                                    dialog_id = %id,
                                    ?reason,
                                    is_mwi,
                                    "Subscription dialog terminated; pruning"
                                );
                                prune(&id);
                                break;
                            }
                            Some(_) => {}
                            None => {
                                prune(&dialog_id);
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    fn drop_replaced_dialogs(&self, replaced: Vec<DialogId>) {
        for id in replaced {
            // `on_remove` cancels the dialog token so the matching guard exits.
            self.server.dialog_layer.remove_dialog(&id);
        }
    }

    async fn handle_subscribe(
        &self,
        tx: &mut Transaction,
        _cookie: &TransactionCookie,
    ) -> Result<()> {
        let from = tx.original.from_header()?.typed()?;
        let to = tx.original.to_header()?.typed()?;
        // Extract identity from To URI (the person we want to watch)
        let identity = match to.uri.user() {
            Some(u) => u.to_string(),
            None => to.uri.host().to_string(),
        };

        debug!("Handle SUBSCRIBE for {}", identity);

        let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel();
        let dialog = self
            .server
            .dialog_layer
            .get_or_create_server_subscription(tx, state_tx, None, None)
            .map_err(|e| anyhow!("{:?}", e))?;

        let expires = tx
            .original
            .expires_header()
            .and_then(|h| h.value().parse::<u32>().ok())
            .unwrap_or(3600);

        let dialog_id = dialog.id().clone();
        if matches!(dialog.state(), DialogState::Calling(_)) {
            // Confirm the dialog so subsequent NOTIFY requests are allowed.
            // `accept` also sends the 200 OK via the transaction unit.
            if let Err(e) = dialog.accept(None, None) {
                warn!(error = %e, "Failed to accept presence SUBSCRIBE; falling back to tx.reply");
                tx.reply(rsipstack::sip::StatusCode::OK).await.ok();
            } else {
                tx.receive().await;
            }
            // The fresh state channel is only wired into a newly created
            // dialog; on an in-dialog refresh it is dropped, and a guard
            // spawned here would prune the live subscription.
            self.spawn_subscription_guard(
                state_rx,
                dialog_id.clone(),
                false,
                dialog.cancel_token().clone(),
            );
        } else {
            // In-dialog refresh or unsubscribe: reply on the current
            // transaction; the initial guard still owns this dialog.
            let reply = tx.reply(rsipstack::sip::StatusCode::OK).await;
            if let Err(error) = reply {
                if expires == 0
                    && matches!(
                        &error,
                        rsipstack::Error::Error(message) if message == "channel closed"
                    )
                {
                    debug!(
                        identity,
                        "SUBSCRIBE termination completed after the client transport closed"
                    );
                } else {
                    return Err(error.into());
                }
            }
        }

        if expires == 0 {
            // Explicit unsubscribe: drop any prior bindings for this watcher.
            let removed = self
                .manager
                .remove_subscribers_for_watcher(from.uri.user().unwrap_or_default());
            for id in removed {
                self.server.dialog_layer.remove_dialog(&id);
            }
            self.manager.remove_subscriber_by_dialog(&dialog_id);
            self.server.dialog_layer.remove_dialog(&dialog_id);
            return Ok(());
        }

        let sub = Subscriber {
            aor: from.uri.clone(),
            dialog_id: dialog_id.clone(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(expires as u64),
        };

        let replaced = self.manager.add_subscriber(&identity, sub.clone());
        self.drop_replaced_dialogs(replaced);

        // Initial NOTIFY must not block the SUBSCRIBE transaction (unit tests
        // have no UA to answer, and a stuck NOTIFY would stall dialplan events).
        let module = self.clone();
        let identity_notify = identity.clone();
        crate::utils::spawn(async move {
            let state = module.manager.get_state(&identity_notify);
            if let Err(e) = module.send_notify(&identity_notify, &sub, &state).await {
                debug!(
                    dialog_id = %sub.dialog_id,
                    error = %e,
                    "Initial presence NOTIFY failed; subscription pruned"
                );
            }
        });

        Ok(())
    }

    async fn handle_publish(&self, tx: &mut Transaction, cookie: &TransactionCookie) -> Result<()> {
        let auth_user = cookie.get_user();
        let from = tx.original.from_header()?.typed()?;

        // If authenticated, use the authenticated username to avoid spoofing
        // and support non-extension users.
        let identity = if let Some(user) = auth_user {
            user.username
        } else {
            match from.uri.user() {
                Some(u) => u.to_string(),
                None => return Err(anyhow!("Missing identity in From header")),
            }
        };

        let body = String::from_utf8_lossy(&tx.original.body);
        debug!("Handle PUBLISH for {}: {}", identity, body);

        let expires = tx
            .original
            .expires_header()
            .and_then(|h| h.value().parse::<u32>().ok())
            .unwrap_or(3600);

        let mut current = self.manager.get_state(&identity);
        current.last_updated = chrono::Utc::now().timestamp();

        if expires == 0 {
            current.status = PresenceStatus::Offline;
        } else if let Ok(pidf) = quick_xml::de::from_str::<IncomingPresence>(&body) {
            let mut status = PresenceStatus::Offline;
            let mut activity_note = None;

            for tuple in &pidf.tuples {
                if tuple.status.as_ref().and_then(|s| s.basic.as_deref()) == Some("open") {
                    status = PresenceStatus::Idle;

                    // Try to refine status from RPID activities
                    if let Some(activities) = &tuple.activities {
                        if activities.busy.is_some() || activities.on_the_phone.is_some() {
                            status = PresenceStatus::Busy;
                        } else if activities.away.is_some() {
                            // RPID <rpid:away/> implies an away state; the
                            // <note> carries the break detail. Accept both the
                            // canonical "away:<detail>" and legacy bare detail.
                            let note = tuple.note.clone().unwrap_or_default();
                            let detail = note
                                .strip_prefix("away:")
                                .or_else(|| note.strip_prefix("custom:"))
                                .unwrap_or(&note);
                            status = PresenceStatus::Away(detail.to_string());
                        }
                    }
                    // Allow clients to signal call-related states via the
                    // PIDF <note> element (e.g. "ringing", "wrapup").
                    if let Some(note) = &tuple.note {
                        match note.to_ascii_lowercase().as_str() {
                            "ringing" => status = PresenceStatus::Ringing,
                            "wrapup" | "wrap-up" | "wrap_up" => status = PresenceStatus::Wrapup,
                            _ => {}
                        }
                        activity_note = Some(note.clone());
                    }
                    break;
                }
            }

            if status == PresenceStatus::Offline && pidf.tuples.is_empty() {
                // Fallback to simple string check if XML parsed but no tuples found
                let lower = body.to_ascii_lowercase();
                if lower.contains("ringing") {
                    status = PresenceStatus::Ringing;
                } else if lower.contains("wrapup") || lower.contains("wrap-up") {
                    status = PresenceStatus::Wrapup;
                } else if lower.contains("busy") {
                    status = PresenceStatus::Busy;
                } else if lower.contains("away") {
                    status = PresenceStatus::Away(String::new());
                } else if lower.contains("idle")
                    || lower.contains("available")
                    || lower.contains("open")
                {
                    status = PresenceStatus::Idle;
                }
            }

            current.status = status;
            if let Some(note) = activity_note {
                current.note = Some(note);
            } else if !pidf.notes.is_empty() {
                current.note = Some(pidf.notes[0].clone());
            }
        } else {
            // Fallback for non-compliant or simplified clients
            let lower = body.to_ascii_lowercase();
            if lower.contains("ringing") {
                current.status = PresenceStatus::Ringing;
            } else if lower.contains("wrapup") || lower.contains("wrap-up") {
                current.status = PresenceStatus::Wrapup;
            } else if lower.contains("busy") {
                current.status = PresenceStatus::Busy;
            } else if lower.contains("away") {
                current.status = PresenceStatus::Away(String::new());
            } else if lower.contains("offline") {
                current.status = PresenceStatus::Offline;
            } else {
                current.status = PresenceStatus::Idle;
            }
        }

        let old_state = self.manager.get_state(&identity);

        self.manager
            .update_state(&identity, current.clone(), &EventSource::Local)
            .await;

        // Notify local addon handlers (CC addon subscribes to emit webhooks).
        // AMI cluster sync is handled internally by update_state.
        if let Some(hub) = &self.server.cluster_event_hub {
            hub.emit_presence_change(&identity, Some(&old_state), &current)
                .await;
        }

        tx.reply(rsipstack::sip::StatusCode::OK).await.ok();

        Ok(())
    }

    async fn send_notify(
        &self,
        identity: &str,
        sub: &Subscriber,
        state: &PresenceState,
    ) -> Result<()> {
        debug!(
            "Sending NOTIFY to {} for identity {} state {:?}",
            sub.aor, identity, state.status
        );

        let domain = sub.aor.host().to_string();
        let body = build_pidf_body(identity, &domain, state);

        let dialog = match self.server.dialog_layer.get_dialog(&sub.dialog_id) {
            Some(d) if !d.state().is_terminated() => d,
            _ => {
                self.manager.remove_subscriber_by_dialog(&sub.dialog_id);
                self.server.dialog_layer.remove_dialog(&sub.dialog_id);
                return Err(anyhow!("Dialog not found or terminated"));
            }
        };

        let expires_left = sub
            .expires
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        let headers = vec![
            rsipstack::sip::Header::Event(rsipstack::sip::headers::Event::new("presence")),
            rsipstack::sip::Header::SubscriptionState(
                rsipstack::sip::headers::SubscriptionState::new(format!(
                    "active;expires={}",
                    expires_left
                )),
            ),
            rsipstack::sip::Header::ContentType(rsipstack::sip::headers::ContentType::from(
                "application/pidf+xml",
            )),
        ];

        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dialog.request(
                rsipstack::sip::Method::Notify,
                Some(headers),
                Some(body.into_bytes()),
            ),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                self.manager.remove_subscriber_by_dialog(&sub.dialog_id);
                self.server.dialog_layer.remove_dialog(&sub.dialog_id);
                Err(anyhow!("{:?}", e))
            }
            Err(_) => {
                self.manager.remove_subscriber_by_dialog(&sub.dialog_id);
                self.server.dialog_layer.remove_dialog(&sub.dialog_id);
                Err(anyhow!("presence NOTIFY timed out"))
            }
        }
    }

    // ── MWI (RFC 3842 message-summary) ────────────────────────────────────────

    /// Handle a SUBSCRIBE for `Event: message-summary`.
    ///
    /// Accepts the subscription, stores it in `PresenceManager`, replies 200 OK,
    /// and immediately sends the current MWI state (zero messages as a safe
    /// default — the voicemail layer will push the real count via `trigger_mwi`).
    async fn handle_mwi_subscribe(
        &self,
        tx: &mut Transaction,
        _cookie: &TransactionCookie,
    ) -> Result<()> {
        let from = tx.original.from_header()?.typed()?;
        let to = tx.original.to_header()?.typed()?;

        // Extension being subscribed to (the mailbox owner).
        let extension = match to.uri.user() {
            Some(u) => u.to_string(),
            None => to.uri.host().to_string(),
        };
        let domain = to.uri.host().to_string();
        let account_uri = format!("sip:{}@{}", extension, domain);

        debug!("Handle MWI SUBSCRIBE for extension {}", extension);

        let expires = tx
            .original
            .expires_header()
            .and_then(|h| h.value().parse::<u32>().ok())
            .unwrap_or(3600);

        let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel();
        let dialog = self
            .server
            .dialog_layer
            .get_or_create_server_subscription(tx, state_tx, None, None)
            .map_err(|e| anyhow!("{:?}", e))?;

        let dialog_id = dialog.id().clone();
        if matches!(dialog.state(), DialogState::Calling(_)) {
            if let Err(e) = dialog.accept(None, None) {
                warn!(error = %e, "Failed to accept MWI SUBSCRIBE; falling back to tx.reply");
                tx.reply(rsipstack::sip::StatusCode::OK).await.ok();
            }
            // Guard only newly created dialogs; on an in-dialog refresh the
            // fresh state channel is dropped (see handle_subscribe).
            self.spawn_subscription_guard(
                state_rx,
                dialog_id.clone(),
                true,
                dialog.cancel_token().clone(),
            );
        } else {
            // In-dialog refresh or unsubscribe on the current transaction.
            tx.reply(rsipstack::sip::StatusCode::OK).await.ok();
        }

        if expires == 0 {
            let removed = self
                .manager
                .remove_mwi_subscribers_for_watcher(from.uri.user().unwrap_or_default());
            for id in removed {
                self.server.dialog_layer.remove_dialog(&id);
            }
            self.manager.remove_mwi_subscriber_by_dialog(&dialog_id);
            self.server.dialog_layer.remove_dialog(&dialog_id);
            return Ok(());
        }

        let sub = MwiSubscriber {
            aor: from.uri.clone(),
            dialog_id: dialog_id.clone(),
            account_uri: account_uri.clone(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(expires as u64),
        };

        let replaced = self.manager.add_mwi_subscriber(&extension, sub.clone());
        self.drop_replaced_dialogs(replaced);

        let module = self.clone();
        crate::utils::spawn(async move {
            let initial_trigger = MwiTrigger {
                extension: extension.clone(),
                new_messages: 0,
                old_messages: 0,
            };
            if let Err(e) = module.send_mwi_notify(&initial_trigger, &sub).await {
                debug!(
                    dialog_id = %sub.dialog_id,
                    error = %e,
                    "Initial MWI NOTIFY failed; subscription pruned"
                );
            }
        });

        Ok(())
    }

    /// Build and send a SIP NOTIFY for `Event: message-summary` (RFC 3842).
    ///
    /// The body follows the `application/simple-message-summary` format.
    async fn send_mwi_notify(&self, trigger: &MwiTrigger, sub: &MwiSubscriber) -> Result<()> {
        debug!(
            extension = %trigger.extension,
            new = trigger.new_messages,
            old = trigger.old_messages,
            "Sending MWI NOTIFY"
        );

        let waiting = if trigger.new_messages > 0 {
            "yes"
        } else {
            "no"
        };
        let body = format!(
            "Messages-Waiting: {}\r\nMessage-Account: {}\r\nVoice-Message: {}/{} (0/0)\r\n",
            waiting, sub.account_uri, trigger.new_messages, trigger.old_messages,
        );

        let dialog = match self.server.dialog_layer.get_dialog(&sub.dialog_id) {
            Some(d) if !d.state().is_terminated() => d,
            _ => {
                self.manager.remove_mwi_subscriber_by_dialog(&sub.dialog_id);
                self.server.dialog_layer.remove_dialog(&sub.dialog_id);
                return Err(anyhow!("MWI dialog not found for {}", trigger.extension));
            }
        };

        let expires_left = sub
            .expires
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();

        let headers = vec![
            rsipstack::sip::Header::Event(rsipstack::sip::headers::Event::new("message-summary")),
            rsipstack::sip::Header::SubscriptionState(
                rsipstack::sip::headers::SubscriptionState::new(format!(
                    "active;expires={}",
                    expires_left
                )),
            ),
            rsipstack::sip::Header::ContentType(rsipstack::sip::headers::ContentType::from(
                "application/simple-message-summary",
            )),
        ];

        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dialog.request(
                rsipstack::sip::Method::Notify,
                Some(headers),
                Some(body.into_bytes()),
            ),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                self.manager.remove_mwi_subscriber_by_dialog(&sub.dialog_id);
                self.server.dialog_layer.remove_dialog(&sub.dialog_id);
                Err(anyhow!("{:?}", e))
            }
            Err(_) => {
                self.manager.remove_mwi_subscriber_by_dialog(&sub.dialog_id);
                self.server.dialog_layer.remove_dialog(&sub.dialog_id);
                Err(anyhow!("MWI NOTIFY timed out"))
            }
        }
    }
}

/// Build a PIDF-XML body (RFC 3863 + RFC 4480 RPID) from a presence state.
/// Returns a complete XML document with declaration.
pub(crate) fn build_pidf_body(identity: &str, domain: &str, state: &PresenceState) -> String {
    let basic_status = if matches!(
        state.status,
        PresenceStatus::Idle
            | PresenceStatus::Busy
            | PresenceStatus::Ringing
            | PresenceStatus::Wrapup
            | PresenceStatus::Away(_)
            | PresenceStatus::Dnd
    ) {
        "open"
    } else {
        "closed"
    };

    let entity = format!("sip:{}@{}", identity, domain);

    let pidf = PidfPresence {
        xmlns: "urn:ietf:params:xml:ns:pidf".to_string(),
        xmlns_rpid: Some("urn:ietf:params:xml:ns:pidf:rpid".to_string()),
        entity,
        tuples: vec![PidfTuple {
            id: "t1".to_string(),
            status: PidfStatus {
                basic: basic_status.to_string(),
            },
            note: match &state.status {
                // Away states carry the canonical status string in the note
                // (e.g. "away:lunch") so subscribers see one consistent
                // vocabulary regardless of what the publisher sent.
                PresenceStatus::Away(_) => state.note.clone().or_else(|| Some("away".to_string())),
                _ => state
                    .note
                    .clone()
                    .or_else(|| Some(state.status.to_string())),
            },
            contact: Some(format!("sip:{}@{}", identity, domain)),
            activities: match state.status {
                PresenceStatus::Busy | PresenceStatus::Dnd => Some(OutputActivities {
                    busy: Some(RpidEmptySer {}),
                    ..Default::default()
                }),
                PresenceStatus::Ringing | PresenceStatus::Wrapup => Some(OutputActivities {
                    on_the_phone: Some(RpidEmptySer {}),
                    ..Default::default()
                }),
                PresenceStatus::Away(_) => Some(OutputActivities {
                    away: Some(RpidEmptySer {}),
                    ..Default::default()
                }),
                _ => None,
            },
        }],
    };

    match quick_xml::se::to_string(&pidf) {
        Ok(xml) => format!(r#"<?xml version="1.0" encoding="UTF-8"?>{}"#, xml),
        Err(e) => {
            tracing::error!("failed to serialize PIDF-XML: {}", e);
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::Location;
    use rsipstack::sip::Uri;

    #[tokio::test]
    async fn test_presence_manager_state() {
        let manager = PresenceManager::new(None);
        let ext = "1001";

        // Initial state
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Offline);

        // Update state manually
        let mut state = manager.get_state(ext);
        state.status = PresenceStatus::Idle;
        state.note = Some("On line".to_string());
        manager.update_state(ext, state, &EventSource::Local).await;

        let updated = manager.get_state(ext);
        assert_eq!(updated.status, PresenceStatus::Idle);
        assert_eq!(updated.note, Some("On line".to_string()));
    }

    #[tokio::test]
    async fn test_locator_events() {
        let manager = PresenceManager::new(None);
        let ext = "1002";
        let uri = Uri::try_from("sip:1002@localhost").unwrap();

        let loc = Location {
            aor: uri,
            ..Default::default()
        };

        // Test registration
        manager
            .handle_locator_event(LocatorEvent::Registered(loc.clone()), &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Idle);

        // Test unregistration
        manager
            .handle_locator_event(LocatorEvent::Unregistered(loc), &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Offline);
    }

    #[tokio::test]
    async fn test_subscriber_management() {
        let manager = PresenceManager::new(None);
        let ext = "1003";
        let sub_uri = Uri::try_from("sip:observer@localhost").unwrap();

        let sub = Subscriber {
            aor: sub_uri.clone(),
            dialog_id: rsipstack::dialog::DialogId {
                call_id: "test-call-id".into(),
                local_tag: "tag1".into(),
                remote_tag: "tag2".into(),
            },
            expires: std::time::Instant::now() + std::time::Duration::from_secs(60),
        };

        manager.add_subscriber(ext, sub);
        let subs = manager.get_subscribers(ext);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].aor, sub_uri);
        assert_eq!(manager.subscriber_bindings_len(), 1);
    }

    #[tokio::test]
    async fn test_subscriber_replaced_by_same_watcher() {
        let manager = PresenceManager::new(None);
        let ext = "1003";
        let sub_uri = Uri::try_from("sip:1001@192.168.3.227").unwrap();

        let old = Subscriber {
            aor: sub_uri.clone(),
            dialog_id: rsipstack::dialog::DialogId {
                call_id: "old-call".into(),
                local_tag: "l1".into(),
                remote_tag: "r1".into(),
            },
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };
        let new = Subscriber {
            aor: sub_uri.clone(),
            dialog_id: rsipstack::dialog::DialogId {
                call_id: "new-call".into(),
                local_tag: "l2".into(),
                remote_tag: "r2".into(),
            },
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        manager.add_subscriber(ext, old);
        let replaced = manager.add_subscriber(ext, new.clone());
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].call_id, "old-call");
        let subs = manager.get_subscribers(ext);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].dialog_id.call_id, "new-call");
    }

    #[tokio::test]
    async fn test_remove_subscribers_for_watcher() {
        let manager = PresenceManager::new(None);
        let watcher = Uri::try_from("sip:1001@pbx.local").unwrap();
        let other = Uri::try_from("sip:2002@pbx.local").unwrap();

        manager.add_subscriber(
            "1001",
            Subscriber {
                aor: watcher.clone(),
                dialog_id: rsipstack::dialog::DialogId {
                    call_id: "c1".into(),
                    local_tag: "l1".into(),
                    remote_tag: "r1".into(),
                },
                expires: std::time::Instant::now() + std::time::Duration::from_secs(60),
            },
        );
        manager.add_subscriber(
            "1001",
            Subscriber {
                aor: other,
                dialog_id: rsipstack::dialog::DialogId {
                    call_id: "c2".into(),
                    local_tag: "l2".into(),
                    remote_tag: "r2".into(),
                },
                expires: std::time::Instant::now() + std::time::Duration::from_secs(60),
            },
        );

        let removed = manager.remove_subscribers_for_watcher("1001");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].call_id, "c1");
        assert_eq!(manager.get_subscribers("1001").len(), 1);
        assert_eq!(manager.subscriber_bindings_len(), 1);
    }

    #[tokio::test]
    async fn test_get_user_prefers_registered_aor() {
        let contact = Uri::try_from("sip:vsfbt0co@gc1g9pmgn89n.invalid;transport=ws").unwrap();
        let registered = Uri::try_from("sip:1001@192.168.3.227").unwrap();
        let loc = Location {
            aor: contact,
            expires: 50,
            destination: None,
            last_modified: None,
            supports_webrtc: true,
            credential: None,
            headers: None,
            registered_aor: Some(registered),
            contact_raw: None,
            contact_params: None,
            path: None,
            service_route: None,
            instance_id: None,
            gruu: None,
            temp_gruu: None,
            reg_id: None,
            transport: None,
            user_agent: None,
            home_proxy: None,
        };
        assert_eq!(
            PresenceManager::get_user(&loc).as_deref(),
            Some("1001"),
            "WebRTC Contact user must not become the presence identity"
        );
    }

    #[tokio::test]
    async fn test_offline_prunes_watcher_subscriptions() {
        let manager = PresenceManager::new(None);
        let watcher = Uri::try_from("sip:1001@192.168.3.227").unwrap();
        manager.add_subscriber(
            "1001",
            Subscriber {
                aor: watcher.clone(),
                dialog_id: rsipstack::dialog::DialogId {
                    call_id: "sub1".into(),
                    local_tag: "l".into(),
                    remote_tag: "r".into(),
                },
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            },
        );
        assert_eq!(manager.subscriber_bindings_len(), 1);

        let loc = Location {
            aor: Uri::try_from("sip:vsfbt0co@gc1g9pmgn89n.invalid").unwrap(),
            expires: 0,
            destination: None,
            last_modified: None,
            supports_webrtc: true,
            credential: None,
            headers: None,
            registered_aor: Some(watcher),
            contact_raw: None,
            contact_params: None,
            path: None,
            service_route: None,
            instance_id: None,
            gruu: None,
            temp_gruu: None,
            reg_id: None,
            transport: None,
            user_agent: None,
            home_proxy: None,
        };
        let pruned = manager
            .handle_locator_event(LocatorEvent::Offline(vec![loc]), &EventSource::Local)
            .await;
        assert_eq!(pruned.len(), 1);
        assert_eq!(manager.subscriber_bindings_len(), 0);
        assert_eq!(manager.get_state("1001").status, PresenceStatus::Offline);
    }

    // ── build_pidf_body tests ──────────────────────────────────────────────

    fn make_state(status: PresenceStatus) -> PresenceState {
        PresenceState {
            status,
            note: None,
            activity: None,
            last_updated: 0,
        }
    }

    #[test]
    fn test_normalize_status_strings() {
        use PresenceStatus as P;
        assert_eq!(P::normalize("idle"), P::Idle);
        assert_eq!(P::normalize("available"), P::Idle);
        assert_eq!(P::normalize("dnd"), P::Dnd);
        assert_eq!(P::normalize("busy"), P::Busy);
        assert_eq!(P::normalize("ringing"), P::Ringing);
        assert_eq!(P::normalize("wrapup"), P::Wrapup);
        assert_eq!(P::normalize("offline"), P::Offline);
        assert_eq!(P::normalize("closed"), P::Offline);
        assert_eq!(P::normalize(""), P::Offline);
        assert_eq!(P::normalize("away"), P::Away(String::new()));
        // Canonical prefixed form.
        assert_eq!(P::normalize("away:lunch"), P::Away("lunch".to_string()));
        // Legacy bare and custom spellings.
        assert_eq!(P::normalize("lunch"), P::Away("lunch".to_string()));
        assert_eq!(P::normalize("custom:lunch"), P::Away("lunch".to_string()));
        // Bare/unknown detail without a prefix stays an away detail.
        assert_eq!(P::normalize("smoke"), P::Away("smoke".to_string()));
    }

    #[test]
    fn test_presence_status_display_canonical() {
        assert_eq!(PresenceStatus::Away(String::new()).to_string(), "away");
        assert_eq!(
            PresenceStatus::Away("lunch".to_string()).to_string(),
            "away"
        );
    }

    #[test]
    fn test_build_pidf_body_idle() {
        let body = build_pidf_body("1001", "pbx.example.com", &make_state(PresenceStatus::Idle));
        assert!(body.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(body.contains(r#"entity="sip:1001@pbx.example.com""#));
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>idle</note>"));
        assert!(!body.contains("rpid:away"));
        assert!(!body.contains("rpid:busy"));
        assert!(!body.contains("rpid:on-the-phone"));
    }

    #[test]
    fn test_build_pidf_body_busy() {
        let body = build_pidf_body("1001", "pbx.example.com", &make_state(PresenceStatus::Busy));
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>busy</note>"));
    }

    #[test]
    fn test_build_pidf_body_ringing() {
        let body = build_pidf_body(
            "1001",
            "pbx.example.com",
            &make_state(PresenceStatus::Ringing),
        );
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>ringing</note>"));
    }

    #[test]
    fn test_build_pidf_body_wrapup() {
        let body = build_pidf_body(
            "1001",
            "pbx.example.com",
            &make_state(PresenceStatus::Wrapup),
        );
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>wrapup</note>"));
    }

    #[test]
    fn test_build_pidf_body_away_with_detail() {
        let mut state = make_state(PresenceStatus::Away("lunch".to_string()));
        state.note = Some("away:lunch".to_string());
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>away:lunch</note>"));
    }

    #[test]
    fn test_build_pidf_body_away_empty() {
        let state = make_state(PresenceStatus::Away(String::new()));
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>away</note>"));
    }

    #[test]
    fn test_build_pidf_body_dnd() {
        let body = build_pidf_body("1001", "pbx.example.com", &make_state(PresenceStatus::Dnd));
        assert!(body.contains("<basic>open</basic>"));
        assert!(body.contains("<note>dnd</note>"));
    }

    #[test]
    fn test_build_pidf_body_offline() {
        let body = build_pidf_body(
            "1001",
            "pbx.example.com",
            &make_state(PresenceStatus::Offline),
        );
        assert!(body.contains("<basic>closed</basic>"));
        assert!(body.contains("<note>offline</note>"));
    }

    #[test]
    fn test_build_pidf_body_entity_uri() {
        let body = build_pidf_body(
            "agent42",
            "sip.example.net",
            &make_state(PresenceStatus::Idle),
        );
        assert!(body.contains(r#"entity="sip:agent42@sip.example.net""#));
    }

    // ── PIDF parse tests ──────────────────────────────────────────────────

    /// The exact PIDF body from a cc-phone PUBLISH (no `entity`, prefixed
    /// RPID elements).  The parser must extract the detail from the <note>
    /// and produce `Away("meeting")`.
    #[test]
    fn test_parse_publish_away_with_detail() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>away:meeting</note></tuple></presence>"#;
        let pidf = quick_xml::de::from_str::<IncomingPresence>(body)
            .expect("should parse cc-phone PIDF with no entity attrib");
        assert_eq!(pidf.tuples.len(), 1);
        assert_eq!(
            pidf.tuples[0]
                .status
                .as_ref()
                .and_then(|s| s.basic.as_deref()),
            Some("open")
        );
        assert_eq!(pidf.tuples[0].note.as_deref(), Some("away:meeting"));
        assert!(
            pidf.tuples[0]
                .activities
                .as_ref()
                .and_then(|a| a.away.as_ref())
                .is_some()
        );
        // Verify the detail extraction logic matches
        let note = pidf.tuples[0].note.clone().unwrap_or_default();
        let detail = note
            .strip_prefix("away:")
            .or_else(|| note.strip_prefix("custom:"))
            .unwrap_or(&note);
        assert_eq!(detail, "meeting");
    }

    /// PIDF with missing `entity` and `xmlns` attributes parses cleanly.
    #[test]
    fn test_parse_publish_away_bare() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><presence><tuple id="presence"><status><basic>open</basic></status><note>away:lunch</note></tuple></presence>"#;
        let pidf =
            quick_xml::de::from_str::<IncomingPresence>(body).expect("should parse minimal PIDF");
        assert_eq!(pidf.tuples[0].note.as_deref(), Some("away:lunch"));
        assert!(pidf.tuples[0].status.as_ref().is_some());
    }

    /// `<rpid:busy/>` maps correctly through RPID activities.
    #[test]
    fn test_parse_publish_busy_with_rpid() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:busy/></rpid:activities></tuple></presence>"#;
        let pidf =
            quick_xml::de::from_str::<IncomingPresence>(body).expect("should parse RPID busy");
        assert!(
            pidf.tuples[0]
                .activities
                .as_ref()
                .and_then(|a| a.busy.as_ref())
                .is_some()
        );
    }

    /// `<rpid:on-the-phone/>` maps correctly.
    #[test]
    fn test_parse_publish_on_the_phone_with_rpid() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:on-the-phone/></rpid:activities></tuple></presence>"#;
        let pidf = quick_xml::de::from_str::<IncomingPresence>(body)
            .expect("should parse RPID on-the-phone");
        assert!(
            pidf.tuples[0]
                .activities
                .as_ref()
                .and_then(|a| a.on_the_phone.as_ref())
                .is_some()
        );
    }

    /// The full Publish flow strips the `away:` prefix from the note and
    /// stores the bare detail.  Simulates what `handle_publish` does.
    #[test]
    fn test_full_handle_publish_away_detail_flow() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>away:meeting</note></tuple></presence>"#;

        let pidf = quick_xml::de::from_str::<IncomingPresence>(body).unwrap();

        let mut status = PresenceStatus::Offline;
        let mut activity_note: Option<String> = None;

        for tuple in &pidf.tuples {
            if tuple.status.as_ref().and_then(|s| s.basic.as_deref()) == Some("open") {
                status = PresenceStatus::Idle;
                if let Some(activities) = &tuple.activities {
                    if activities.busy.is_some() || activities.on_the_phone.is_some() {
                        status = PresenceStatus::Busy;
                    } else if activities.away.is_some() {
                        let note = tuple.note.clone().unwrap_or_default();
                        let detail = note
                            .strip_prefix("away:")
                            .or_else(|| note.strip_prefix("custom:"))
                            .unwrap_or(&note);
                        status = PresenceStatus::Away(detail.to_string());
                    }
                }
                if let Some(note) = &tuple.note {
                    activity_note = Some(note.clone());
                }
                break;
            }
        }

        assert!(matches!(status, PresenceStatus::Away(ref d) if d == "meeting"));
        assert_eq!(activity_note.as_deref(), Some("away:meeting"));
    }

    /// A PUBLISH with a bare custom note (no prefix) is treated as detail.
    #[test]
    fn test_full_handle_publish_away_custom_detail() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><presence xmlns="urn:ietf:params:xml:ns:pidf" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid"><tuple id="presence"><status><basic>open</basic></status><rpid:activities><rpid:away/></rpid:activities><note>lunch</note></tuple></presence>"#;

        let pidf = quick_xml::de::from_str::<IncomingPresence>(body).unwrap();
        let tuple = &pidf.tuples[0];
        let note = tuple.note.clone().unwrap_or_default();
        let detail = note
            .strip_prefix("away:")
            .or_else(|| note.strip_prefix("custom:"))
            .unwrap_or(&note);
        assert_eq!(detail, "lunch");
    }

    /// Verify that the serialized NOTIFY body for an away state only
    /// contains `<rpid:away/>` — no `<rpid:busy/>` or `<rpid:on-the-phone/>`.
    #[test]
    fn test_build_pidf_body_away_activities_exclusive() {
        let state = make_state(PresenceStatus::Away("meeting".to_string()));
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<rpid:away/>"));
        assert!(!body.contains("rpid:busy"));
        assert!(!body.contains("rpid:on-the-phone"));
    }

    /// Verify that the serialized NOTIFY body for a busy state only
    /// contains `<rpid:busy/>`.
    #[test]
    fn test_build_pidf_body_busy_activities_exclusive() {
        let state = make_state(PresenceStatus::Busy);
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<rpid:busy/>"));
        assert!(!body.contains("rpid:away"));
        assert!(!body.contains("rpid:on-the-phone"));
    }
}
