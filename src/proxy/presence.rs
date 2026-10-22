use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::Location;
use crate::call::TransactionCookie;
use crate::config::ProxyConfig;
use crate::models::presence;
use crate::proxy::cluster_event::{EventSource, event_etags};
use crate::proxy::cluster_sync::ClusterSync;
use crate::proxy::locator::{Locator, LocatorEvent, LocatorEventLock};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::DialogState;
use rsipstack::sip::prelude::{HeadersExt, ToTypedHeader};
use rsipstack::transaction::transaction::Transaction;
use sea_orm::{DatabaseConnection, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
// parking_lot (not std): guards are poison-free and the SIP REGISTER/SUBSCRIBE
// hot path must never turn a panicked holder into permanent lock poisoning.
use parking_lot::RwLock;
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
    /// RFC 4479 data-model person carrying standard RPID metadata (reason).
    #[serde(rename = "person", default)]
    persons: Vec<IncomingPerson>,
}

#[derive(Debug, Deserialize, Default)]
struct IncomingPerson {
    #[serde(rename = "activities", default)]
    activities: Option<RpidActivities>,
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
    /// RFC 4480 human-readable activity note (`<rpid:note>`).
    #[serde(rename = "note", default)]
    notes: Vec<String>,
    /// RFC 4480 application-specific activity text (`<rpid:other>`); CC uses
    /// the exact `ring-timeout-ack <token>` grammar.
    #[serde(rename = "other", default)]
    other: Option<String>,
    #[serde(rename = "away", default)]
    away: Option<RpidEmpty>,
    #[serde(rename = "busy", default)]
    busy: Option<RpidEmpty>,
    #[serde(rename = "on-the-phone", default)]
    on_the_phone: Option<RpidEmpty>,
}

#[derive(Debug, Deserialize, Default)]
struct RpidEmpty {}

/// First non-empty standard RPID reason (`dm:person/rpid:activities/rpid:note`).
fn incoming_reason(pidf: &IncomingPresence) -> Option<String> {
    pidf.persons
        .iter()
        .filter_map(|person| person.activities.as_ref())
        .flat_map(|activities| activities.notes.iter())
        .map(|note| note.trim())
        .find(|note| !note.is_empty())
        .map(str::to_string)
}

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
    /// Human-readable reason from standard RPID `activities/note` (e.g. a
    /// 小休申请理由). Never used to derive the machine status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub activity: Option<String>,
    pub last_updated: i64,
    /// PUBLISH TTL deadline (epoch secs). `None` = no TTL enforced — states
    /// set outside a SIP PUBLISH (REGISTER header, REST, locator events)
    /// never lapse on their own. This keeps ordinary phones that never
    /// publish presence completely unaffected: only an identity that has
    /// actually PUBLISHed can be flipped offline by the TTL sweeper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Why the state went offline — set by the PUBLISH-TTL sweeper
    /// (`"expired"`) and read by the CC registrar bridge to stamp
    /// `reason_code` on agent events. Always `None` on live states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline_reason: Option<String>,
}

impl Default for PresenceState {
    fn default() -> Self {
        Self {
            status: PresenceStatus::Offline,
            note: None,
            reason: None,
            activity: None,
            last_updated: chrono::Utc::now().timestamp(),
            expires_at: None,
            offline_reason: None,
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

/// Reverse business-status source: on a REGISTER that recovers from Offline the
/// presence plane asks the business plane (the CC addon) for the extension's
/// **persisted** status (break/away/…) instead of blindly defaulting to Idle.
///
/// Motivation (e2e S18 C6): after a node restart the in-memory presence store is
/// empty, so a re-REGISTER hit the hard-coded `Offline → Idle` recovery and
/// emitted a ghost `idle` NOTIFY while the persisted CC registry still held
/// `away:2` — the soft-phone follows SIP presence and flipped 小休→空闲.
///
/// `None` = no business context (plain SIP phone); callers fall back to the
/// historical `Idle` recovery.
#[async_trait]
pub trait BusinessStatusProvider: Send + Sync {
    async fn business_status(&self, extension: &str) -> Option<PresenceState>;
}

/// Generic hook for raw SIP PUBLISH bodies. Core parses the standard PIDF
/// status/note and then hands the untouched body to registered addons; all
/// business-specific XML extensions (e.g. CC ring-timeout acknowledgements)
/// stay in the addon. Core never interprets those extensions.
///
/// Returning `true` means the publication was fully consumed as an addon
/// command (not a manual presence update); core replies 200 but skips its
/// normal admission/persistence/NOTIFY path. This is required for CC ring
/// acknowledgements, whose PIDF note is an internal marker rather than a
/// state the agent meant to publish.
#[async_trait]
pub trait PresencePublishHook: Send + Sync {
    async fn on_publish(&self, identity: &str, state: &PresenceState, body: &[u8]) -> bool;
}

/// Decision for a REGISTER recovering from Offline (pure, unit-tested): prefer
/// the business plane's non-offline state (e.g. `away:2`); only fall back to the
/// historical `Idle` when there is no business status.
fn register_recovery_status(business: Option<&PresenceState>) -> PresenceStatus {
    match business {
        Some(st) if st.status != PresenceStatus::Offline => st.status.clone(),
        _ => PresenceStatus::Idle,
    }
}

#[derive(Clone)]
pub struct PresenceManager {
    /// Shared with REGISTER and transport cleanup; used when consuming delayed Offline events.
    registration_guard: Option<(Arc<Box<dyn Locator>>, LocatorEventLock)>,
    states: Arc<RwLock<HashMap<String, PresenceState>>>,
    publication_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// PRESENCE (RFC 3856) subscriptions keyed by subscribed-to identity.
    subscribers: Arc<RwLock<HashMap<String, Vec<Subscriber>>>>,
    /// MWI (RFC 3842) subscriptions keyed by mailbox extension.
    mwi_subscribers: Arc<RwLock<HashMap<String, Vec<MwiSubscriber>>>>,
    database: Option<DatabaseConnection>,
    notify_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<String>>>>,
    /// Channel used by the voicemail layer to request MWI NOTIFY delivery.
    mwi_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<MwiTrigger>>>>,
    cluster_sync: std::sync::Arc<parking_lot::RwLock<Option<std::sync::Arc<ClusterSync>>>>,
    /// Presence NOTIFYs acknowledged with a successful SIP response and the
    /// last `(identity, status)` they carried.
    notifies_sent: Arc<std::sync::atomic::AtomicU64>,
    last_notified: Arc<RwLock<Option<(String, String)>>>,
    /// Reverse business-status source (registered by the CC addon). See
    /// [`BusinessStatusProvider`].
    business_status_provider: Arc<RwLock<Option<Arc<dyn BusinessStatusProvider>>>>,
    /// Addon hooks receiving the raw PUBLISH body. See [`PresencePublishHook`].
    publish_hooks: Arc<RwLock<Vec<Arc<dyn PresencePublishHook>>>>,
}

impl PresenceManager {
    pub fn new(database: Option<DatabaseConnection>) -> Self {
        Self {
            registration_guard: None,
            states: Arc::new(RwLock::new(HashMap::new())),
            publication_locks: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            mwi_subscribers: Arc::new(RwLock::new(HashMap::new())),
            database,
            notify_tx: Arc::new(RwLock::new(None)),
            mwi_tx: Arc::new(RwLock::new(None)),
            cluster_sync: std::sync::Arc::new(parking_lot::RwLock::new(None)),
            notifies_sent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_notified: Arc::new(RwLock::new(None)),
            business_status_provider: Arc::new(RwLock::new(None)),
            publish_hooks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Configure the same registration authority and lock used by this SIP server.
    pub(crate) fn with_registration_guard(
        mut self,
        locator: Arc<Box<dyn Locator>>,
        event_lock: LocatorEventLock,
    ) -> Self {
        self.registration_guard = Some((locator, event_lock));
        self
    }

    /// Register an addon hook for raw SIP PUBLISH bodies. Hooks run in
    /// registration order before presence admission/persistence.
    pub fn add_publish_hook(&self, hook: Arc<dyn PresencePublishHook>) {
        self.publish_hooks.write().push(hook);
    }

    fn publish_hooks(&self) -> Vec<Arc<dyn PresencePublishHook>> {
        self.publish_hooks.read().clone()
    }

    /// Register the reverse business-status source (the CC addon calls this once
    /// at startup; see [`BusinessStatusProvider`]).
    pub fn set_business_status_provider(&self, provider: Arc<dyn BusinessStatusProvider>) {
        *self.business_status_provider.write() = Some(provider);
    }

    /// Query the business plane for an extension's persisted status. `None`
    /// when no provider is registered.
    async fn business_status(&self, extension: &str) -> Option<PresenceState> {
        let provider = self.business_status_provider.read().clone();
        match provider {
            Some(p) => p.business_status(extension).await,
            None => None,
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
        let mut lock = self.notify_tx.write();
        *lock = Some(tx);
    }

    /// Drop the notify sender so the dispatcher task observes channel-closed.
    /// Called from `PresenceModule::on_stop` to ensure deterministic shutdown.
    pub fn clear_notify_tx(&self) {
        let mut lock = self.notify_tx.write();
        *lock = None;
    }

    pub async fn load_from_db(&self) -> Result<()> {
        if let Some(db) = &self.database {
            let states = presence::Entity::find().all(db).await?;
            let mut map = self.states.write();
            for s in states {
                let status = PresenceStatus::normalize(&s.status);
                map.insert(
                    s.identity,
                    PresenceState {
                        status,
                        note: s.note,
                        activity: s.activity,
                        last_updated: s.last_updated,
                        ..Default::default()
                    },
                );
            }
        }
        Ok(())
    }

    pub fn states_len(&self) -> usize {
        self.states.read().len()
    }

    pub fn subscribers_len(&self) -> usize {
        self.subscribers.read().len()
    }

    /// Total presence subscription bindings (sum of all identity buckets).
    pub fn subscriber_bindings_len(&self) -> usize {
        self.subscribers.read().values().map(|v| v.len()).sum()
    }

    pub fn mwi_subscribers_len(&self) -> usize {
        self.mwi_subscribers.read().len()
    }

    /// Total MWI subscription bindings (sum of all extension buckets).
    pub fn mwi_subscriber_bindings_len(&self) -> usize {
        self.mwi_subscribers.read().values().map(|v| v.len()).sum()
    }

    /// Number of presence NOTIFYs acknowledged with a successful SIP response.
    pub fn notifies_sent(&self) -> u64 {
        self.notifies_sent
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Last successfully acknowledged presence NOTIFY's `(identity, status)`.
    pub fn last_notified(&self) -> Option<(String, String)> {
        self.last_notified.read().clone()
    }

    /// Record a successfully acknowledged presence NOTIFY.
    fn record_notify(&self, identity: &str, status: &str) {
        self.notifies_sent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.last_notified.write() = Some((identity.to_string(), status.to_string()));
    }

    pub fn get_state(&self, identity: &str) -> PresenceState {
        let map = self.states.read();
        map.get(identity).cloned().unwrap_or_default()
    }

    pub(crate) fn publication_lock(&self, identity: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.publication_locks
            .write()
            .entry(identity.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Rearm an accepted publication without replacing its authoritative status.
    pub(crate) fn update_publication_expiry(&self, identity: &str, expires_at: Option<i64>) {
        if let Some(state) = self.states.write().get_mut(identity) {
            state.expires_at = expires_at;
        }
    }

    /// Refresh only a live publication, preserving the latest business projection.
    fn renew_publication(&self, identity: &str, expires: u32) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut states = self.states.write();
        let Some(current) = states.get_mut(identity) else {
            return false;
        };
        if current.status == PresenceStatus::Offline
            || !current.expires_at.is_some_and(|deadline| deadline > now)
        {
            return false;
        }
        current.expires_at = Some(now + i64::from(expires));
        true
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
        self.update_state_internal(identity, state, source, false, |_| true)
            .await
    }

    /// Business-state projections must retain the latest publication lease atomically.
    pub(crate) async fn update_projected_state(
        &self,
        identity: &str,
        state: PresenceState,
        source: &EventSource,
    ) -> Option<PresenceState> {
        self.update_state_internal(identity, state, source, true, |_| true)
            .await
    }

    async fn update_state_internal(
        &self,
        identity: &str,
        mut state: PresenceState,
        source: &EventSource,
        preserve_publication: bool,
        can_update: impl FnOnce(Option<&PresenceState>) -> bool,
    ) -> Option<PresenceState> {
        let old_state = {
            let mut map = self.states.write();
            // Recheck implicit registration recovery against the latest business
            // state under the same lock that installs the recovered state.
            if !can_update(map.get(identity)) {
                return None;
            }
            if preserve_publication && state.status != PresenceStatus::Offline {
                state.expires_at = map.get(identity).and_then(|current| current.expires_at);
            }
            map.insert(identity.to_string(), state.clone())
        };

        if source.is_local() {
            // Cluster sync first (parallel fire-and-forget) — no partial borrow issues
            let msg = crate::proxy::cluster_event::ClusterPresenceMessage::from((identity, &state));
            if let Some(ref sync) = *self.cluster_sync.read() {
                sync.broadcast(event_etags::PRESENCE, identity, &msg);
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
                    // Identical-value re-save is a no-op upsert, not a failure.
                    if crate::db_report::is_noop_upsert(&e) {
                        tracing::debug!(
                            identity = %identity,
                            "presence upsert matched an identical row"
                        );
                    } else {
                        crate::db_report::report_db_write_failure_with_detail(
                            "presence",
                            "upsert",
                            None,
                            &e,
                            Some(serde_json::json!({ "identity": identity })),
                            crate::db_report::THROTTLE_COOLDOWN,
                        );
                    }
                }
            }
        }

        // Notify subscribers (triggers NOTIFY messages)
        let tx = {
            let lock = self.notify_tx.read();
            lock.clone()
        };
        if let Some(tx) = tx {
            let _ = tx.send(identity.to_string()).await;
        }

        old_state
    }

    pub fn add_subscriber(&self, identity: &str, sub: Subscriber) -> Vec<DialogId> {
        let mut map = self.subscribers.write();
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
        let map = self.subscribers.read();
        map.get(identity).cloned().unwrap_or_default()
    }

    /// Remove a presence subscription by dialog id. Returns true if removed.
    pub fn remove_subscriber_by_dialog(&self, dialog_id: &DialogId) -> bool {
        let mut map = self.subscribers.write();
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
        let mut map = self.subscribers.write();
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
        let mut subscribers = self.subscribers.write();
        let now = std::time::Instant::now();
        subscribers.retain(|_, subs| {
            subs.retain(|s| s.expires > now);
            !subs.is_empty()
        });
    }

    /// Flip published-presence states whose PUBLISH TTL lapsed to Offline.
    ///
    /// Long-absence detection for softphones: a client that PUBLISHed a
    /// status with an `Expires` but then vanished (tab frozen, network
    /// changed) stops refreshing; once the deadline passes the presence
    /// returns to Offline so CC agents do not stay schedulable forever.
    ///
    /// Only identities that actually PUBLISHed participate (`expires_at`
    /// set) — a REGISTER-only phone never gets a presence TTL and is
    /// untouched. Callers must notify addon handlers afterwards (the
    /// manager itself only owns the state + cluster mirror).
    pub async fn expire_stale_publishes(&self) -> Vec<String> {
        self.expire_stale_publishes_via_hub(None, None).await
    }

    /// `locator` is the registration-liveness authority. A lapsed PUBLISH
    /// alone must NOT log an agent out: restsend-call publishes once at
    /// login and never re-publishes, so its soft state goes stale after
    /// ~5 minutes while the REGISTER keeps refreshing every ~25s. When the
    /// extension still has a live binding the stale publication is treated
    /// as expired-but-superseded: the last published status stands, the TTL
    /// is dropped, and nothing is broadcast (no NOTIFY, no CC transition).
    /// Only a truly gone client (no binding) flips offline with
    /// `offline_reason = "expired"`. A failed lookup skips the identity for
    /// this round (fail-open — the CC offline watchdog reconciles later).
    pub(crate) async fn expire_stale_publishes_via_hub(
        &self,
        hub: Option<&crate::proxy::cluster_event::ClusterEventHub>,
        locator: Option<&dyn Locator>,
    ) -> Vec<String> {
        let now = chrono::Utc::now().timestamp();
        let stale: Vec<(String, PresenceState)> = {
            let states = self.states.read();
            states
                .iter()
                .filter(|(_, s)| {
                    s.status != PresenceStatus::Offline
                        && s.expires_at.is_some_and(|exp| exp <= now)
                })
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect()
        };
        let mut expired = Vec::with_capacity(stale.len());
        for (identity, mut state) in stale {
            let publication_lock = self.publication_lock(&identity);
            let _publication = publication_lock.lock().await;
            // A renewal after the sweep snapshot invalidates this expiry.
            let current = self.get_state(&identity);
            if current.expires_at != state.expires_at
                || !current.expires_at.is_some_and(|deadline| deadline <= now)
                || current.status == PresenceStatus::Offline
            {
                continue;
            }
            if let Some(locator) = locator {
                match locator.has_active_binding_for_user(&identity).await {
                    Ok(true) => {
                        // Still registered: keep the published status, just
                        // retire the dead soft-state timer. Silent on purpose
                        // — a status change here would be the very false
                        // "offline while connected" NOTIFY this guard exists
                        // to prevent.
                        {
                            let mut states = self.states.write();
                            // Retire only this lease; CC may project a new status during lookup.
                            if let Some(current) = states.get_mut(&identity)
                                && current.expires_at == state.expires_at
                            {
                                current.expires_at = None;
                                current.offline_reason = None;
                            }
                        }
                        debug!(
                            identity = %identity,
                            "PUBLISH TTL lapsed but registration is alive; keeping published presence"
                        );
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(
                            identity = %identity,
                            error = %e,
                            "PUBLISH TTL sweep: binding lookup failed, skipping this round"
                        );
                        continue;
                    }
                }
            }
            state.status = PresenceStatus::Offline;
            state.note = None;
            state.activity = None;
            state.last_updated = now;
            state.expires_at = None;
            state.offline_reason = Some("expired".to_string());
            // An addon may defer logout or reject the write. Admit expiry
            // before notifying subscribers, just like a client PUBLISH.
            if let Some(hub) = hub {
                hub.emit_presence_change_locked(&identity, None, &state)
                    .await;
            } else {
                self.update_state(&identity, state, &EventSource::Local)
                    .await;
            }
            expired.push(identity);
        }
        expired
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
        let mut lock = self.mwi_tx.write();
        *lock = Some(tx);
    }

    /// Drop the MWI sender so the dispatch task observes channel-closed.
    /// Called from `PresenceModule::on_stop` to ensure deterministic shutdown.
    pub fn clear_mwi_tx(&self) {
        let mut lock = self.mwi_tx.write();
        *lock = None;
    }

    /// Add (or refresh) an MWI subscription for `extension`.
    /// Returns dialog ids replaced by the same watcher AOR.
    pub fn add_mwi_subscriber(&self, extension: &str, sub: MwiSubscriber) -> Vec<DialogId> {
        let mut map = self.mwi_subscribers.write();
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
        let map = self.mwi_subscribers.read();
        map.get(extension).cloned().unwrap_or_default()
    }

    /// Remove an MWI subscription by dialog id. Returns true if removed.
    pub fn remove_mwi_subscriber_by_dialog(&self, dialog_id: &DialogId) -> bool {
        let mut map = self.mwi_subscribers.write();
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
        let mut map = self.mwi_subscribers.write();
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
        let mut map = self.mwi_subscribers.write();
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
            let lock = self.mwi_tx.read();
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
        let is_refresh = matches!(event, LocatorEvent::Refreshed(_));
        match event {
            LocatorEvent::Registered(loc) | LocatorEvent::Refreshed(loc) => {
                if let Some(user) = Self::get_user(&loc) {
                    let publication_lock = self.publication_lock(&user);
                    let _publication = publication_lock.lock().await;
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

                    let mut recovered: Option<PresenceState> = None;
                    let new_status = match header_status {
                        Some(s) => PresenceStatus::normalize(s),
                        // Registration may recover Offline; a keepalive only
                        // recovers transport/lease failures, never explicit logout.
                        // The locked predicate below also preserves newer states.
                        None if current.status == PresenceStatus::Offline => {
                            // Prefer the business plane's persisted status (break/
                            // away) over a hard-coded Idle — a node restart empties
                            // this store, so the old `Offline → Idle` recovery
                            // emitted a ghost `idle` NOTIFY that desynced SIP
                            // presence from the (DB-persisted) CC registry and
                            // flipped a soft-phone's 小休 to 空闲 (e2e S18 C6).
                            let business = self.business_status(&user).await;
                            recovered = business.clone();
                            register_recovery_status(business.as_ref())
                        }
                        None => return pruned,
                    };

                    let mut next = PresenceState {
                        status: new_status,
                        last_updated: chrono::Utc::now().timestamp(),
                        // A fresh REGISTER proves the client is alive: a
                        // PUBLISH TTL armed by an earlier publication
                        // must not later flip this user offline.
                        expires_at: None,
                        offline_reason: None,
                        ..current
                    };
                    if let Some(st) = recovered {
                        // Carry the business detail (e.g. `away:2`); otherwise the
                        // reconcile NOTIFY would degrade to a bare `away`.
                        next.note = st.note;
                    }

                    self.update_state_internal(&user, next, source, false, |latest| {
                        header_status.is_some()
                            || latest.map_or(!is_refresh, |state| {
                                state.status == PresenceStatus::Offline
                                    && (!is_refresh
                                        || matches!(
                                            state.offline_reason.as_deref(),
                                            Some("expired" | "offline")
                                        ))
                            })
                    })
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
                            // Explicit REGISTER expires=0 — the client said
                            // goodbye; stamp the cause so NOTIFY bodies and
                            // downstream consumers can tell it apart from a
                            // transport drop or a TTL sweep.
                            offline_reason: Some("unregistered".to_string()),
                            ..Default::default()
                        },
                        source,
                    )
                    .await;
                }
            }
            LocatorEvent::Offline(locs, reason) => {
                let reason = reason.as_str().to_string();
                for loc in locs {
                    if let Some(user) = Self::get_user(&loc) {
                        // The event may have waited behind another identity or a peer
                        // update. Recheck under the REGISTER writer's lock, and keep
                        // the decision and its effects in that same critical section.
                        let _registration = match self.registration_guard.as_ref() {
                            Some((_, lock)) => Some(lock.lock().await),
                            None => None,
                        };
                        if let Some((locator, _)) = self.registration_guard.as_ref() {
                            match locator.has_active_binding_for_user(&user).await {
                                Ok(false) => {},
                                Ok(true) => continue,
                                Err(error) => {
                                    warn!(extension = %user, %error, "Skipping Offline presence cleanup: registration lookup failed");
                                    continue;
                                }
                            }
                        }
                        pruned.extend(self.prune_watcher_subscriptions(&user));
                        self.update_state(
                            &user,
                            PresenceState {
                                status: PresenceStatus::Offline,
                                last_updated: chrono::Utc::now().timestamp(),
                                // `"offline"` (transport closed) or
                                // `"expired"` (REGISTER TTL lapsed) — rides
                                // into the NOTIFY PIDF note and the CC
                                // registrar bridge's `reason_code`.
                                offline_reason: Some(reason.clone()),
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
                            if let Err(e) = module_clone.send_notify(&identity, &sub, &state, false).await {
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
        // and for lapsed PUBLISH presence TTLs. The TTL sweep must notify
        // addon handlers afterwards (CC registrar bridge → agent offline +
        // RWI events), which the manager alone cannot do.
        let manager_cleanup = self.manager.clone();
        let manager_ttl = self.manager.clone();
        let ttl_hub = self.server.cluster_event_hub.clone();
        // The TTL sweep consults the locator before flipping an identity
        // offline: a lapsed PUBLISH with a live REGISTER is a stale soft
        // state, not a logout (restsend-call publishes once, never refreshes).
        let ttl_locator = self.server.locator.clone();
        // Cadence is config-driven (default 60s); e2e shrinks it to bound
        // TTL-related waits.
        let ttl_interval = std::time::Duration::from_secs(
            self.server
                .proxy_config
                .load()
                .clamped_presence_sweep_secs() as u64,
        );
        let cancel_cleanup = cancel.clone();
        crate::utils::spawn(async move {
            let mut interval = tokio::time::interval(ttl_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel_cleanup.cancelled() => break,
                    _ = interval.tick() => {
                        manager_cleanup.cleanup_expired();
                        manager_cleanup.cleanup_expired_mwi();
                        manager_ttl
                            .expire_stale_publishes_via_hub(
                                ttl_hub.as_deref(),
                                Some(ttl_locator.as_ref().as_ref()),
                            )
                            .await;
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

        // Cap the granted expiry and echo it in the 200 OK so clients that
        // only parse the response `Expires` header refresh at the right
        // cadence; `Subscriber.expires` stores the same granted value so the
        // NOTIFY `Subscription-State` stays consistent with what we advertised.
        let max_expires = self
            .server
            .proxy_config
            .load()
            .clamped_max_subscription_expires();
        let expires = tx
            .original
            .expires_header()
            .and_then(|h| h.value().parse::<u32>().ok())
            .unwrap_or(3600)
            .min(max_expires);

        let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel();
        let dialog = self
            .server
            .dialog_layer
            .get_or_create_server_subscription(tx, state_tx, None, None)
            .map_err(|e| anyhow!("{:?}", e))?;

        let dialog_id = dialog.id().clone();
        if matches!(dialog.state(), DialogState::Calling(_)) {
            // Confirm the dialog so subsequent NOTIFY requests are allowed.
            // `accept` also sends the 200 OK via the transaction unit.
            let expires_header = rsipstack::sip::Header::Expires(expires.into());
            if let Err(e) = dialog.accept(Some(vec![expires_header]), None) {
                warn!(error = %e, "Failed to accept presence SUBSCRIBE; falling back to tx.reply");
                tx.reply_with(
                    rsipstack::sip::StatusCode::OK,
                    vec![rsipstack::sip::Header::Expires(expires.into())],
                    None,
                )
                .await
                .ok();
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
            let reply = tx
                .reply_with(
                    rsipstack::sip::StatusCode::OK,
                    vec![rsipstack::sip::Header::Expires(expires.into())],
                    None,
                )
                .await;
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
            if let Err(e) = module
                .send_notify(&identity_notify, &sub, &state, true)
                .await
            {
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

        if expires > 0 && tx.original.body.is_empty() {
            // Share expiry/logout ordering without entering manual CC admission.
            let publication_lock = self.manager.publication_lock(&identity);
            let _publication = publication_lock.lock().await;
            let status = if self.manager.renew_publication(&identity, expires) {
                rsipstack::sip::StatusCode::OK
            } else {
                rsipstack::sip::StatusCode::ConditionalRequestFailed
            };
            tx.reply(status).await.ok();
            return Ok(());
        }

        let mut current = self.manager.get_state(&identity);
        current.last_updated = chrono::Utc::now().timestamp();
        // Every successful PUBLISH rearms the presence TTL and clears any
        // stale offline-reason stamp. expires=0 closes the publication.
        current.offline_reason = None;
        current.expires_at = if expires == 0 {
            None
        } else {
            Some(chrono::Utc::now().timestamp() + expires as i64)
        };

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

            // Standard RPID reason: <dm:person><rpid:activities><rpid:note>.
            // Only the person extension is used; tuple notes remain the
            // machine-readable state and must not be reinterpreted.
            let reason = incoming_reason(&pidf);

            current.status = status;
            current.reason = reason;
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

        // Generic addon hook: the core deliberately does not parse
        // addon-specific PIDF extensions. Hooks run before admission so an
        // extension can be consumed even when the addon suppresses the SIP
        // mirror (CC does exactly that for agent presence).
        let publish_body = tx.original.body.clone();
        let mut consumed = false;
        for hook in self.manager.publish_hooks() {
            consumed |= hook.on_publish(&identity, &current, &publish_body).await;
        }
        if consumed {
            // The addon owns this publication (e.g. ring-timeout ack). It is
            // not a manual state: acknowledge SIP but do not mirror/store it.
            tx.reply(rsipstack::sip::StatusCode::OK).await.ok();
            return Ok(());
        }

        let old_state = self.manager.get_state(&identity);

        // Addon admission precedes persistence and NOTIFY, so a rejected
        // automatic client request cannot overwrite the authoritative mirror.
        if let Some(hub) = &self.server.cluster_event_hub {
            hub.emit_presence_change(&identity, Some(&old_state), &current)
                .await;
        } else {
            let publication_lock = self.manager.publication_lock(&identity);
            let _publication = publication_lock.lock().await;
            self.manager
                .update_state(&identity, current.clone(), &EventSource::Local)
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
        initial: bool,
    ) -> Result<()> {
        // Only the initial self-subscription can race an in-flight login and
        // expose an old failure state as a forced logout. Other watchers and
        // explicit logout updates must still receive the real Offline state.
        if initial
            && sub.aor.user() == Some(identity)
            && state.status == PresenceStatus::Offline
            && matches!(
                state.offline_reason.as_deref(),
                None | Some("expired" | "offline")
            )
        {
            debug!(
                identity,
                aor = %sub.aor,
                "Suppressing Offline presence NOTIFY (never pushed to clients)"
            );
            return Ok(());
        }
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
            Ok(Ok(Some(response)))
                if response.status_code.kind() == rsipstack::sip::StatusCodeKind::Successful =>
            {
                self.manager
                    .record_notify(identity, &state.status.to_string());
                Ok(())
            }
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

        // Same granted-expiry echo as the presence package: cap and advertise
        // the effective value in the 200 OK (see handle_subscribe).
        let max_expires = self
            .server
            .proxy_config
            .load()
            .clamped_max_subscription_expires();
        let expires = tx
            .original
            .expires_header()
            .and_then(|h| h.value().parse::<u32>().ok())
            .unwrap_or(3600)
            .min(max_expires);

        let (state_tx, state_rx) = tokio::sync::mpsc::unbounded_channel();
        let dialog = self
            .server
            .dialog_layer
            .get_or_create_server_subscription(tx, state_tx, None, None)
            .map_err(|e| anyhow!("{:?}", e))?;

        let dialog_id = dialog.id().clone();
        if matches!(dialog.state(), DialogState::Calling(_)) {
            let expires_header = rsipstack::sip::Header::Expires(expires.into());
            if let Err(e) = dialog.accept(Some(vec![expires_header]), None) {
                warn!(error = %e, "Failed to accept MWI SUBSCRIBE; falling back to tx.reply");
                tx.reply_with(
                    rsipstack::sip::StatusCode::OK,
                    vec![rsipstack::sip::Header::Expires(expires.into())],
                    None,
                )
                .await
                .ok();
            } else {
                // Drain the accept response before the server transaction drops.
                tx.receive().await;
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
            tx.reply_with(
                rsipstack::sip::StatusCode::OK,
                vec![rsipstack::sip::Header::Expires(expires.into())],
                None,
            )
            .await
            .ok();
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
                // vocabulary regardless of what the publisher sent. Writers
                // that only fill `Away(detail)` without a note (CC bridge on
                // REST status changes, cluster sync) must not degrade to bare
                // "away" — the break detail is otherwise stripped from every
                // NOTIFY and soft-phone reconcile flips the typed break to the
                // generic one seconds after it is applied.
                PresenceStatus::Away(detail) => match state.note.as_deref() {
                    Some(n) if !n.trim().is_empty() => state.note.clone(),
                    _ if detail.is_empty() => Some("away".to_string()),
                    _ => Some(format!("away:{detail}")),
                },
                PresenceStatus::Offline => state
                    .note
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| Some("offline".to_string())),
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

    /// 纯函数：REGISTER 从 Offline 恢复时的状态判定 —— 优先业务平面非 offline
    /// 状态，无业务语境才回退 Idle。
    #[test]
    fn register_recovery_status_prefers_business_over_ghost_idle() {
        assert_eq!(register_recovery_status(None), PresenceStatus::Idle);
        let off = PresenceState {
            status: PresenceStatus::Offline,
            ..Default::default()
        };
        assert_eq!(register_recovery_status(Some(&off)), PresenceStatus::Idle);
        let away = PresenceState {
            status: PresenceStatus::Away("2".into()),
            ..Default::default()
        };
        assert_eq!(
            register_recovery_status(Some(&away)),
            PresenceStatus::Away("2".into())
        );
    }

    /// 反向业务状态来源的桩：返回预置状态。
    struct StubBiz(Option<PresenceState>);
    #[async_trait]
    impl BusinessStatusProvider for StubBiz {
        async fn business_status(&self, _extension: &str) -> Option<PresenceState> {
            self.0.clone()
        }
    }

    /// 回归（e2e S18 C6 定罪）：节点重启后 presence store 为空，客户端重连
    /// REGISTER 若盲目兜底 Idle，会与 CC registry（持久化 away:2）不一致 ——
    /// 软电话跟随 SIP presence 把小休切成空闲。注册业务状态来源后，恢复必须
    /// 保留业务小休状态 + 明细（away:2），不得 ghost idle。
    #[tokio::test]
    async fn register_recovery_prefers_business_break_over_ghost_idle() {
        let ext = "1001";
        let loc = Location {
            aor: Uri::try_from("sip:1001@localhost").unwrap(),
            ..Default::default()
        };

        // 无业务来源（普通 SIP 分机）：历史行为 → Idle。
        let manager = PresenceManager::new(None);
        manager
            .handle_locator_event(LocatorEvent::Registered(loc.clone()), &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Idle);

        // 有业务来源（CC 小休 away:2）：恢复为小休并带明细，绝不 ghost Idle。
        let manager = PresenceManager::new(None);
        let biz = PresenceState {
            status: PresenceStatus::Away("2".into()),
            note: Some("away:2".into()),
            ..Default::default()
        };
        manager.set_business_status_provider(Arc::new(StubBiz(Some(biz))));
        manager
            .handle_locator_event(LocatorEvent::Registered(loc.clone()), &EventSource::Local)
            .await;
        let st = manager.get_state(ext);
        assert_eq!(
            st.status,
            PresenceStatus::Away("2".into()),
            "业务小休不得被兜底成 Idle"
        );
        assert_eq!(st.note.as_deref(), Some("away:2"), "明细随行");

        // 业务来源为 offline：回退 Idle（不投影 offline 到恢复路径）。
        let manager = PresenceManager::new(None);
        manager.set_business_status_provider(Arc::new(StubBiz(Some(PresenceState::default()))));
        manager
            .handle_locator_event(LocatorEvent::Registered(loc), &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Idle);
    }

    /// Regression guard for the parking_lot migration: a panic while holding
    /// a presence lock must NOT poison it. Under `std::sync::RwLock` every
    /// later `.read().unwrap()` on the hot path would panic forever (crash
    /// loop); parking_lot has no poisoning, so the manager keeps working.
    #[tokio::test]
    async fn test_lock_not_poisoned_by_panic() {
        let manager = PresenceManager::new(None);
        let states = manager.states.clone();

        // Panic inside a thread while the write guard is held.
        let panic_joined = std::thread::spawn(move || {
            let mut guard = states.write();
            guard.insert("poison-ext".to_string(), PresenceState::default());
            panic!("simulated panic while holding the presence states lock");
        });
        let res = panic_joined.join();
        assert!(res.is_err(), "the spawned thread must have panicked");

        // The lock must still be fully usable afterwards.
        assert_eq!(
            manager.get_state("poison-ext").status,
            PresenceStatus::Offline
        );
        let ext = "1001";
        let mut state = manager.get_state(ext);
        state.status = PresenceStatus::Idle;
        manager.update_state(ext, state, &EventSource::Local).await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Idle);
    }

    /// PUBLISH-TTL sweeper: an identity that published with an `Expires` but
    /// stopped refreshing is flipped offline with `offline_reason = "expired"`
    /// so the CC bridge stamps `reason_code=expired` on the agent events.
    /// Identities that never published (no `expires_at`) — ordinary phones —
    /// must be untouched, and already-offline states are not re-broadcast.
    #[tokio::test]
    async fn expire_stale_publishes_flips_only_expired_publishers() {
        let manager = PresenceManager::new(None);
        let now = chrono::Utc::now().timestamp();

        // 1) Published, TTL lapsed → must flip offline with the reason stamp.
        let mut publisher = PresenceState {
            status: PresenceStatus::Away("lunch".into()),
            note: Some("away:lunch".into()),
            last_updated: now - 600,
            expires_at: Some(now - 60),
            ..Default::default()
        };
        manager
            .update_state("agent-1001", publisher.clone(), &EventSource::Local)
            .await;

        // 2) Published, TTL still in the future → untouched.
        publisher.status = PresenceStatus::Idle;
        publisher.expires_at = Some(now + 3600);
        manager
            .update_state("agent-1002", publisher, &EventSource::Local)
            .await;

        // 3) Never published (REGISTER-only phone) → untouched even though
        //    its last_updated is ancient.
        let mut register_only = PresenceState {
            status: PresenceStatus::Idle,
            last_updated: now - 86_400,
            ..Default::default()
        };
        register_only.expires_at = None;
        manager
            .update_state("ata-2001", register_only, &EventSource::Local)
            .await;

        // 4) Already offline with an expired TTL → not re-broadcast.
        let mut gone = PresenceState {
            status: PresenceStatus::Offline,
            last_updated: now - 600,
            expires_at: Some(now - 60),
            ..Default::default()
        };
        gone.offline_reason = Some("expired".to_string());
        manager
            .update_state("agent-1003", gone, &EventSource::Local)
            .await;

        let expired = manager.expire_stale_publishes().await;
        assert_eq!(
            expired,
            vec!["agent-1001".to_string()],
            "exactly the lapsed publisher is reported"
        );

        let flipped = manager.get_state("agent-1001");
        assert_eq!(flipped.status, PresenceStatus::Offline);
        assert_eq!(flipped.offline_reason.as_deref(), Some("expired"));
        assert_eq!(flipped.expires_at, None, "TTL consumed");
        assert_eq!(flipped.note, None, "break note cleared");

        assert_eq!(manager.get_state("agent-1002").status, PresenceStatus::Idle);
        assert_eq!(manager.get_state("ata-2001").status, PresenceStatus::Idle);
        assert_eq!(
            manager.get_state("agent-1003").status,
            PresenceStatus::Offline
        );
    }

    /// P1.0 guard: a lapsed PUBLISH with a LIVE registration must NOT flip
    /// the identity offline — restsend-call publishes once at login and
    /// never re-publishes, so its soft state goes stale while the REGISTER
    /// keeps refreshing. The published status stands, the TTL is dropped,
    /// and the identity is not reported as expired. A lapsed PUBLISH with
    /// NO binding still flips offline with `offline_reason = "expired"`.
    #[tokio::test]
    async fn expire_stale_publishes_keeps_registered_clients_online() {
        use crate::proxy::locator::{Locator, MemoryLocator};

        let manager = PresenceManager::new(None);
        let locator = MemoryLocator::new();
        let now = chrono::Utc::now().timestamp();

        // Live registration for agent-live (the client that never
        // re-publishes but keeps its REGISTER fresh).
        locator
            .register(
                "agent-live",
                Some("localhost"),
                Location {
                    aor: "sip:agent-live@192.168.1.10".try_into().unwrap(),
                    expires: 50,
                    last_modified: Some(std::time::Instant::now()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Lapsed PUBLISH for the live client → must be kept as-is.
        let mut live = PresenceState {
            status: PresenceStatus::Away("break".into()),
            note: Some("away:break".into()),
            last_updated: now - 600,
            expires_at: Some(now - 60),
            ..Default::default()
        };
        manager
            .update_state("agent-live", live.clone(), &EventSource::Local)
            .await;

        // Lapsed PUBLISH for a vanished client → flipped offline.
        let mut gone = PresenceState {
            status: PresenceStatus::Idle,
            last_updated: now - 600,
            expires_at: Some(now - 60),
            ..Default::default()
        };
        manager
            .update_state("agent-gone", gone.clone(), &EventSource::Local)
            .await;

        let expired = manager
            .expire_stale_publishes_via_hub(None, Some(&locator))
            .await;
        assert_eq!(expired, vec!["agent-gone".to_string()]);

        let kept = manager.get_state("agent-live");
        assert_eq!(kept.status, PresenceStatus::Away("break".into()));
        assert_eq!(kept.note.as_deref(), Some("away:break"));
        assert_eq!(kept.expires_at, None, "dead soft-state timer retired");
        assert_eq!(kept.offline_reason, None, "no offline stamp");

        let flipped = manager.get_state("agent-gone");
        assert_eq!(flipped.status, PresenceStatus::Offline);
        assert_eq!(flipped.offline_reason.as_deref(), Some("expired"));

        // A no-locator sweep (legacy callers/tests) still flips the live
        // client — the guard is only as good as the locator it is given.
        live.expires_at = Some(now - 60);
        live.status = PresenceStatus::Idle;
        manager
            .update_state("agent-live", live, &EventSource::Local)
            .await;
        let expired = manager.expire_stale_publishes().await;
        assert_eq!(expired, vec!["agent-live".to_string()]);
        assert_eq!(
            manager.get_state("agent-live").status,
            PresenceStatus::Offline
        );

        // The registration guard must preserve both renewed leases and business projections.
        for renew_lease in [true, false] {
            let mut publication = manager.get_state("agent-live");
            publication.status = PresenceStatus::Idle;
            publication.expires_at = Some(now - 60);
            manager
                .update_state("agent-live", publication.clone(), &EventSource::Local)
                .await;
            let publication_lock = manager.publication_lock("agent-live");
            let renewal = publication_lock.lock().await;
            let sweep = manager.expire_stale_publishes_via_hub(None, Some(&locator));
            tokio::pin!(sweep);
            assert!(futures::poll!(sweep.as_mut()).is_pending());
            publication.status = PresenceStatus::Away("break".into());
            if renew_lease {
                publication.expires_at = Some(now + 300);
                manager
                    .update_state("agent-live", publication, &EventSource::Local)
                    .await;
            } else {
                manager
                    .update_projected_state("agent-live", publication, &EventSource::Local)
                    .await;
            }
            drop(renewal);
            assert!(sweep.await.is_empty());
            let renewed = manager.get_state("agent-live");
            assert_eq!(renewed.status, PresenceStatus::Away("break".into()));
            assert_eq!(
                renewed.expires_at,
                if renew_lease { Some(now + 300) } else { None }
            );
        }
    }

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

    /// PL2: a keepalive REGISTER refresh must lift a stale Offline back to
    /// Idle. Previously `Refreshed` returned early and left it Offline, so a
    /// reconnecting client that only re-registered stayed "offline" while its
    /// binding was live.
    #[tokio::test]
    async fn refresh_register_lifts_offline_to_idle() {
        let manager = PresenceManager::new(None);
        let ext = "1004";
        let loc = Location {
            aor: Uri::try_from("sip:1004@localhost").unwrap(),
            ..Default::default()
        };

        // A failed transport may be healed by a keepalive; explicit logout may not.
        let mut offline = manager.get_state(ext);
        offline.status = PresenceStatus::Offline;
        offline.offline_reason = Some("expired".into());
        manager
            .update_state(ext, offline, &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Offline);

        manager
            .handle_locator_event(LocatorEvent::Refreshed(loc.clone()), &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Idle);

        // A subsequent refresh while Idle is a no-op (business state kept).
        manager
            .handle_locator_event(LocatorEvent::Refreshed(loc), &EventSource::Local)
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Idle);
    }

    #[tokio::test]
    async fn refresh_register_preserves_explicit_offline() {
        for reason in [None, Some("manual"), Some("closed"), Some("unregistered")] {
            for source in [
                EventSource::Local,
                EventSource::Remote("127.0.0.1:5060".parse().unwrap()),
            ] {
                let manager = PresenceManager::new(None);
                manager
                    .update_state(
                        "1004",
                        PresenceState {
                            status: PresenceStatus::Offline,
                            offline_reason: reason.map(str::to_string),
                            ..Default::default()
                        },
                        &source,
                    )
                    .await;
                for _ in 0..2 {
                    manager
                        .handle_locator_event(
                            LocatorEvent::Refreshed(Location {
                                aor: Uri::try_from("sip:1004@localhost").unwrap(),
                                ..Default::default()
                            }),
                            &source,
                        )
                        .await;
                    assert_eq!(
                        manager.get_state("1004").status,
                        PresenceStatus::Offline,
                        "keepalive must preserve explicit logout: {reason:?}"
                    );
                    assert_eq!(manager.get_state("1004").offline_reason.as_deref(), reason);
                }
            }
        }
    }

    /// PL1/PL2: a REGISTER (fresh or refresh) must never downgrade a live
    /// business state.
    #[tokio::test]
    async fn register_preserves_live_business_state() {
        let loc = Location {
            aor: Uri::try_from("sip:1005@localhost").unwrap(),
            ..Default::default()
        };
        for state in [
            PresenceStatus::Idle,
            PresenceStatus::Away("lunch".into()),
            PresenceStatus::Dnd,
            PresenceStatus::Ringing,
            PresenceStatus::Busy,
            PresenceStatus::Wrapup,
        ] {
            let manager = PresenceManager::new(None);
            let mut current = manager.get_state("1005");
            current.status = state.clone();
            manager
                .update_state("1005", current, &EventSource::Local)
                .await;

            manager
                .handle_locator_event(LocatorEvent::Registered(loc.clone()), &EventSource::Local)
                .await;
            manager
                .handle_locator_event(LocatorEvent::Refreshed(loc.clone()), &EventSource::Local)
                .await;
            assert_eq!(
                manager.get_state("1005").status,
                state,
                "REGISTER must preserve {state:?}"
            );
        }
    }

    /// A REGISTER carrying `X-CC-Presence` still applies that status verbatim,
    /// even over an Offline state.
    #[tokio::test]
    async fn register_x_cc_presence_header_applies() {
        let manager = PresenceManager::new(None);
        let loc = Location {
            aor: Uri::try_from("sip:1006@localhost").unwrap(),
            headers: Some(vec![rsipstack::sip::Header::Other(
                "X-CC-Presence".to_string(),
                "away:lunch".to_string(),
            )]),
            ..Default::default()
        };
        manager
            .handle_locator_event(LocatorEvent::Registered(loc), &EventSource::Local)
            .await;
        assert_eq!(
            manager.get_state("1006").status,
            PresenceStatus::Away("lunch".into())
        );
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
            registered_username: None,
            registered_realm: None,
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
            registered_username: None,
            registered_realm: None,
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
            .handle_locator_event(
                LocatorEvent::Offline(
                    vec![loc],
                    crate::proxy::locator::LocatorOfflineReason::TransportClosed,
                ),
                &EventSource::Local,
            )
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
            ..Default::default()
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

    /// Break detail must survive a state that carries `Away(detail)` without a
    /// note (CC bridge / cluster-sync writers). The PIDF note is the only
    /// field SIP subscribers parse — falling back to bare "away" strips the
    /// break type and soft-phone reconcile flips the agent to the generic
    /// break label 3-5s after applying a typed break.
    #[test]
    fn test_build_pidf_body_away_detail_without_note_stays_canonical() {
        // Detail present + no note → canonical "away:<detail>".
        let state = make_state(PresenceStatus::Away("1".to_string()));
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<note>away:1</note>"), "body: {body}");
        assert!(!body.contains("<note>away</note>"), "body: {body}");
        // Blank note behaves like an absent one.
        let mut state = make_state(PresenceStatus::Away("meeting".to_string()));
        state.note = Some("   ".to_string());
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<note>away:meeting</note>"), "body: {body}");
        // Bare away (no detail) keeps the legacy fallback.
        let state = make_state(PresenceStatus::Away(String::new()));
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<note>away</note>"), "body: {body}");
        // An explicit note always wins verbatim.
        let mut state = make_state(PresenceStatus::Away("1".to_string()));
        state.note = Some("away:2".to_string());
        let body = build_pidf_body("1001", "pbx.example.com", &state);
        assert!(body.contains("<note>away:2</note>"), "body: {body}");
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

    /// The PIDF note vocabulary is a CLOSED contract with soft-phone
    /// reconcilers. `build_pidf_body` still emits `<note>offline</note>` for a
    /// literal Offline state (and a stamped `offline_reason` must ride
    /// `reason_code` / webhooks / the status history only — never suffixed
    /// into the note, or `offline:expired` would fall through the client's
    /// match arm and desync it). Initial self-subscriptions may suppress a
    /// stale failure state, while ordinary watchers still receive Offline.
    #[test]
    fn test_build_pidf_body_offline_note_stays_in_closed_vocabulary() {
        for reason in ["expired", "unregistered", "offline"] {
            let mut state = make_state(PresenceStatus::Offline);
            state.offline_reason = Some(reason.to_string());
            let body = build_pidf_body("1001", "pbx.example.com", &state);
            assert!(
                body.contains("<note>offline</note>"),
                "reason {reason:?} must not leak into the note: {body}"
            );
            assert!(!body.contains("offline:"), "body: {body}");
        }
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

    #[test]
    fn test_parse_publish_reason_from_standard_rpid_person() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<presence xmlns="urn:ietf:params:xml:ns:pidf"
          xmlns:dm="urn:ietf:params:xml:ns:pidf:data-model"
          xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid">
  <tuple id="presence"><status><basic>open</basic></status>
    <rpid:activities><rpid:away/></rpid:activities>
    <note>away:break</note></tuple>
  <dm:person id="p1"><rpid:activities>
    <rpid:note>客户回访 &lt;30 分钟&gt;</rpid:note>
  </rpid:activities></dm:person>
</presence>"#;
        let pidf = quick_xml::de::from_str::<IncomingPresence>(body).unwrap();
        assert_eq!(
            incoming_reason(&pidf).as_deref(),
            Some("客户回访 <30 分钟>")
        );
        // The tuple note is still the machine state, not the reason.
        assert_eq!(pidf.tuples[0].note.as_deref(), Some("away:break"));
    }

    #[test]
    fn test_parse_publish_without_reason_stays_none() {
        let body = r#"<presence><tuple><status><basic>open</basic></status><note>away:break</note></tuple></presence>"#;
        let pidf = quick_xml::de::from_str::<IncomingPresence>(body).unwrap();
        assert_eq!(incoming_reason(&pidf), None);
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
