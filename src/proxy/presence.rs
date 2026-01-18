use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::Location;
use crate::call::TransactionCookie;
use crate::config::ProxyConfig;
use crate::models::presence;
use crate::proxy::locator::LocatorEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsip::prelude::{HeadersExt, ToTypedHeader, UntypedHeader};
use rsipstack::dialog::DialogId;
use rsipstack::transaction::transaction::Transaction;
use sea_orm::{DatabaseConnection, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info};

// PIDF-XML (RFC 3863) and RPID (RFC 4480) support
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "presence")]
struct PidfPresence {
    #[serde(rename = "@xmlns")]
    xmlns: String,
    #[serde(rename = "@xmlns:rpid", default)]
    xmlns_rpid: Option<String>,
    #[serde(rename = "@entity")]
    entity: String,
    #[serde(rename = "tuple", default)]
    tuples: Vec<PidfTuple>,
    #[serde(rename = "note", default)]
    notes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PidfTuple {
    #[serde(rename = "@id")]
    id: String,
    status: PidfStatus,
    #[serde(rename = "note")]
    note: Option<String>,
    #[serde(rename = "contact")]
    contact: Option<String>,
    #[serde(rename = "rpid:activities", default)]
    activities: Option<RpidActivities>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PidfStatus {
    basic: String, // "open" or "closed"
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RpidActivities {
    #[serde(rename = "rpid:away", default)]
    away: Option<RpidEmpty>,
    #[serde(rename = "rpid:busy", default)]
    busy: Option<RpidEmpty>,
    #[serde(rename = "rpid:on-the-phone", default)]
    on_the_phone: Option<RpidEmpty>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RpidEmpty {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PresenceStatus {
    Available,
    Busy,
    Away,
    Offline,
}

impl std::fmt::Display for PresenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceStatus::Available => write!(f, "available"),
            PresenceStatus::Busy => write!(f, "busy"),
            PresenceStatus::Away => write!(f, "away"),
            PresenceStatus::Offline => write!(f, "offline"),
        }
    }
}

impl Default for PresenceStatus {
    fn default() -> Self {
        PresenceStatus::Offline
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

#[derive(Clone, Debug)]
pub struct Subscriber {
    pub aor: rsip::Uri,
    pub dialog_id: DialogId,
    pub expires: std::time::Instant,
}

#[derive(Clone)]
pub struct PresenceManager {
    states: Arc<RwLock<HashMap<String, PresenceState>>>,
    subscribers: Arc<RwLock<HashMap<String, Vec<Subscriber>>>>,
    database: Option<DatabaseConnection>,
    notify_tx: Arc<RwLock<Option<tokio::sync::mpsc::Sender<String>>>>,
}

impl PresenceManager {
    pub fn new(database: Option<DatabaseConnection>) -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            database,
            notify_tx: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_notify_tx(&self, tx: tokio::sync::mpsc::Sender<String>) {
        let mut lock = self.notify_tx.write().unwrap();
        *lock = Some(tx);
    }

    pub async fn load_from_db(&self) -> Result<()> {
        if let Some(db) = &self.database {
            let states = presence::Entity::find().all(db).await?;
            let mut map = self.states.write().unwrap();
            for s in states {
                let status = match s.status.as_str() {
                    "available" => PresenceStatus::Available,
                    "busy" => PresenceStatus::Busy,
                    "away" => PresenceStatus::Away,
                    _ => PresenceStatus::Offline,
                };
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

    pub fn get_state(&self, identity: &str) -> PresenceState {
        let map = self.states.read().unwrap();
        map.get(identity).cloned().unwrap_or_default()
    }

    pub async fn update_state(&self, identity: &str, state: PresenceState) {
        {
            let mut map = self.states.write().unwrap();
            map.insert(identity.to_string(), state.clone());
        }

        if let Some(db) = &self.database {
            let active: presence::ActiveModel = presence::ActiveModel {
                identity: Set(identity.to_string()),
                status: Set(state.status.to_string()),
                note: Set(state.note),
                activity: Set(state.activity),
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

        let tx = {
            let lock = self.notify_tx.read().unwrap();
            lock.clone()
        };

        if let Some(tx) = tx {
            let _ = tx.send(identity.to_string()).await;
        }
    }

    pub fn add_subscriber(&self, identity: &str, sub: Subscriber) {
        let mut map = self.subscribers.write().unwrap();
        let subs = map.entry(identity.to_string()).or_insert_with(Vec::new);
        // Remove old sub with same dialog_id or similar if needed
        subs.retain(|s| s.dialog_id != sub.dialog_id);
        subs.push(sub);
    }

    pub fn get_subscribers(&self, identity: &str) -> Vec<Subscriber> {
        let map = self.subscribers.read().unwrap();
        map.get(identity).cloned().unwrap_or_default()
    }

    pub fn cleanup_expired(&self) {
        let mut subscribers = self.subscribers.write().unwrap();
        let now = std::time::Instant::now();
        for subs in subscribers.values_mut() {
            subs.retain(|s| s.expires > now);
        }
    }

    fn get_user(loc: &Location) -> Option<String> {
        loc.aor.user().map(|u| u.to_string())
    }

    // Process locator events
    pub async fn handle_locator_event(&self, event: LocatorEvent) {
        match event {
            LocatorEvent::Registered(loc) => {
                if let Some(user) = Self::get_user(&loc) {
                    info!("Presence: Registered {}", user);
                    let current = self.get_state(&user);
                    if current.status == PresenceStatus::Offline {
                        self.update_state(
                            &user,
                            PresenceState {
                                status: PresenceStatus::Available,
                                last_updated: chrono::Utc::now().timestamp(),
                                ..current
                            },
                        )
                        .await;
                    }
                }
            }
            LocatorEvent::Unregistered(loc) => {
                if let Some(user) = Self::get_user(&loc) {
                    self.update_state(
                        &user,
                        PresenceState {
                            status: PresenceStatus::Offline,
                            last_updated: chrono::Utc::now().timestamp(),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
            LocatorEvent::Offline(locs) => {
                for loc in locs {
                    if let Some(user) = Self::get_user(&loc) {
                        self.update_state(
                            &user,
                            PresenceState {
                                status: PresenceStatus::Offline,
                                last_updated: chrono::Utc::now().timestamp(),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                }
            }
        }
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
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Subscribe,
            rsip::Method::Publish,
            rsip::Method::Notify,
        ]
    }
    async fn on_start(&mut self) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);
        self.manager.set_notify_tx(tx);

        // Spawn listener for notification requests (e.g. from UI or PUBLISH)
        let module_clone = self.clone();
        crate::utils::spawn(async move {
            while let Some(identity) = rx.recv().await {
                let state = module_clone.manager.get_state(&identity);
                let subscribers = module_clone.manager.get_subscribers(&identity);
                for sub in subscribers {
                    let _ = module_clone.send_notify(&identity, &sub, &state).await;
                }
            }
        });

        // Spawn listener for locator events
        let manager = self.manager.clone();
        if let Some(mut rx) = self.server.locator_events.as_ref().map(|tx| tx.subscribe()) {
            crate::utils::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    manager.handle_locator_event(event).await;
                }
            });
        }

        // Spawn background cleanup for expired subscriptions
        let manager_cleanup = self.manager.clone();
        crate::utils::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                manager_cleanup.cleanup_expired();
            }
        });

        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: tokio_util::sync::CancellationToken,
        tx: &mut Transaction,
        cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsip::Method::Subscribe => {
                self.handle_subscribe(tx, &cookie).await?;
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Publish => {
                self.handle_publish(tx, &cookie).await?;
                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }
}

impl PresenceModule {
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

        let (state_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let dialog = self
            .server
            .dialog_layer
            .get_or_create_server_subscription(tx, state_tx, None, None)
            .map_err(|e| anyhow!("{:?}", e))?;

        let expires = tx
            .original
            .expires_header()
            .and_then(|h| h.seconds().ok())
            .unwrap_or(3600);

        let sub = Subscriber {
            aor: from.uri.clone(),
            dialog_id: dialog.id().clone(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(expires as u64),
        };

        self.manager.add_subscriber(&identity, sub.clone());

        // Send 200 OK
        tx.reply(rsip::StatusCode::OK).await.ok();

        // Send initial NOTIFY
        let state = self.manager.get_state(&identity);
        self.send_notify(&identity, &sub, &state).await?;

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
            .and_then(|h| h.seconds().ok())
            .unwrap_or(3600);

        let mut current = self.manager.get_state(&identity);
        current.last_updated = chrono::Utc::now().timestamp();

        if expires == 0 {
            current.status = PresenceStatus::Offline;
        } else if let Ok(pidf) = quick_xml::de::from_str::<PidfPresence>(&body) {
            let mut status = PresenceStatus::Offline;
            let mut activity_note = None;

            for tuple in &pidf.tuples {
                if tuple.status.basic == "open" {
                    status = PresenceStatus::Available;

                    // Try to refine status from RPID activities
                    if let Some(activities) = &tuple.activities {
                        if activities.busy.is_some() || activities.on_the_phone.is_some() {
                            status = PresenceStatus::Busy;
                        } else if activities.away.is_some() {
                            status = PresenceStatus::Away;
                        }
                    }
                    if let Some(note) = &tuple.note {
                        activity_note = Some(note.clone());
                    }
                    break;
                }
            }

            if status == PresenceStatus::Offline && pidf.tuples.is_empty() {
                // Fallback to simple string check if XML parsed but no tuples found
                if body.contains("busy") {
                    status = PresenceStatus::Busy;
                } else if body.contains("away") {
                    status = PresenceStatus::Away;
                } else if body.contains("available") || body.contains("open") {
                    status = PresenceStatus::Available;
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
            if body.contains("busy") {
                current.status = PresenceStatus::Busy;
            } else if body.contains("away") {
                current.status = PresenceStatus::Away;
            } else if body.contains("offline") {
                current.status = PresenceStatus::Offline;
            } else {
                current.status = PresenceStatus::Available;
            }
        }

        self.manager.update_state(&identity, current).await;
        tx.reply(rsip::StatusCode::OK).await.ok();

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

        // Build PIDF-XML (RFC 3863)
        let basic_status = if matches!(
            state.status,
            PresenceStatus::Available | PresenceStatus::Busy | PresenceStatus::Away
        ) {
            "open"
        } else {
            "closed"
        };

        let domain = sub.aor.host().to_string();
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
                note: state
                    .note
                    .clone()
                    .or_else(|| Some(state.status.to_string())),
                contact: Some(format!("sip:{}@{}", identity, domain)),
                activities: match state.status {
                    PresenceStatus::Busy => Some(RpidActivities {
                        busy: Some(RpidEmpty {}),
                        ..Default::default()
                    }),
                    PresenceStatus::Away => Some(RpidActivities {
                        away: Some(RpidEmpty {}),
                        ..Default::default()
                    }),
                    _ => None,
                },
            }],
            notes: vec![],
        };

        let body = match quick_xml::se::to_string(&pidf) {
            Ok(xml) => format!(r#"<?xml version="1.0" encoding="UTF-8"?>{}"#, xml),
            Err(e) => {
                tracing::error!("failed to serialize PIDF-XML: {}", e);
                return Err(anyhow!("XML serialization failed"));
            }
        };

        let dialog = self
            .server
            .dialog_layer
            .get_dialog(&sub.dialog_id)
            .ok_or_else(|| anyhow!("Dialog not found"))?;

        let expires_left = sub
            .expires
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        let headers = vec![
            rsip::Header::Event(rsip::headers::Event::new("presence")),
            rsip::Header::SubscriptionState(rsip::headers::SubscriptionState::new(format!(
                "active;expires={}",
                expires_left
            ))),
            rsip::Header::ContentType(rsip::headers::ContentType::from("application/pidf+xml")),
        ];

        dialog
            .request(rsip::Method::Notify, Some(headers), Some(body.into_bytes()))
            .await
            .map_err(|e| anyhow!("{:?}", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::Location;
    use rsip::Uri;

    #[tokio::test]
    async fn test_presence_manager_state() {
        let manager = PresenceManager::new(None);
        let ext = "1001";

        // Initial state
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Offline);

        // Update state manually
        let mut state = manager.get_state(ext);
        state.status = PresenceStatus::Available;
        state.note = Some("On line".to_string());
        manager.update_state(ext, state).await;

        let updated = manager.get_state(ext);
        assert_eq!(updated.status, PresenceStatus::Available);
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
            .handle_locator_event(LocatorEvent::Registered(loc.clone()))
            .await;
        assert_eq!(manager.get_state(ext).status, PresenceStatus::Available);

        // Test unregistration
        manager
            .handle_locator_event(LocatorEvent::Unregistered(loc))
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
    }
}
