//! RFC 8599 SIP push notifications.
//!
//! A UA that cannot keep a flow open (typically a mobile app that the OS
//! suspends or kills) registers a Contact URI carrying `pn-provider`,
//! `pn-prid` and `pn-param`. Push support sits at two layers so that call
//! routing needs no knowledge of it:
//!
//! - Registration: the binding of a push device survives its flow closing
//!   (the device is asleep, not gone), so every locator lookup — direct calls,
//!   routes, queues, transfers — still finds it. The registrar records the
//!   device in the [`registry`], persisted so bindings survive restarts.
//! - Delivery: every outgoing INVITE goes through [`send_invite`]. An INVITE
//!   to a push device pushes it and, if the device's flow is down, is held
//!   (the RFC's "SIP Request Push Bucket") until the device sends a
//!   binding-refresh REGISTER, then sent on the fresh flow.
//!
//! Bindings are kept alive by pushing devices shortly before they expire
//! (§5.5). [`fcm`] implements Firebase Cloud Messaging (HTTP v1), [`apns`]
//! the Apple Push Notification service.

use crate::call::Location;
use crate::config::PushConfig;
use crate::proxy::locator::Locator;
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use rsipstack::sip::{Param, Transport, Uri};
use rsipstack::transport::SipAddr;
use sea_orm::DatabaseConnection;
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub mod apns;
pub mod dial;
pub mod fcm;
pub mod registry;

pub use dial::{send_invite, wakeup_failure_status};
pub use registry::{PushDevice, PushRegistry};

pub const PN_PROVIDER: &str = "pn-provider";
pub const PN_PRID: &str = "pn-prid";
pub const PN_PARAM: &str = "pn-param";

/// 555 response reason phrase (RFC 8599 §8.1).
pub const PUSH_NOT_SUPPORTED_REASON: &str = "Push Notification Service Not Supported";

/// How often expired devices are swept and binding-refresh pushes are sent.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);

/// RFC 8599 `pn-*` parameters identifying where to push a device.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PushParams {
    /// `pn-provider`, lowercased (e.g. `fcm`).
    pub provider: String,
    /// `pn-prid`: the push resource ID (FCM registration token, APNs device
    /// token, ...).
    pub prid: String,
    /// `pn-param`: provider specific (FCM project, APNs team and topic).
    pub param: Option<String>,
}

impl PushParams {
    /// Push parameters of a Contact URI that requests push notifications.
    pub fn from_uri(uri: &Uri) -> Option<Self> {
        match ContactPush::from_uri(uri) {
            ContactPush::Request(params) => Some(params),
            _ => None,
        }
    }
}

/// What a REGISTER Contact URI asks of the push mechanism.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContactPush {
    /// No `pn-provider`: an ordinary binding.
    None,
    /// `pn-provider` without `pn-prid`: a capability query (RFC 8599 §4.1.5).
    /// `None` asks which providers are supported at all.
    Query(Option<String>),
    /// Push notifications requested for this binding (§4.1.1).
    Request(PushParams),
}

impl ContactPush {
    pub fn from_uri(uri: &Uri) -> Self {
        let Some(provider) = uri_param(uri, PN_PROVIDER) else {
            return Self::None;
        };
        let provider = Some(provider.to_ascii_lowercase()).filter(|p| !p.is_empty());
        let prid = uri_param(uri, PN_PRID).filter(|p| !p.is_empty());
        match (provider, prid) {
            (Some(provider), Some(prid)) => Self::Request(PushParams {
                provider,
                prid,
                param: uri_param(uri, PN_PARAM).filter(|p| !p.is_empty()),
            }),
            (provider, _) => Self::Query(provider),
        }
    }
}

/// Value of a URI parameter, percent-decoded (RFC 8599 §8.7 values are
/// `pvalue`s, so anything outside that set arrives escaped). A parameter
/// without a value yields an empty string.
fn uri_param(uri: &Uri, name: &str) -> Option<String> {
    uri.params.iter().find_map(|param| match param {
        Param::Other(key, value) if key.value().eq_ignore_ascii_case(name) => {
            let raw = value.as_ref().map(|v| v.value()).unwrap_or("");
            Some(
                urlencoding::decode(raw)
                    .map(|v| v.into_owned())
                    .unwrap_or_else(|_| raw.to_string()),
            )
        }
        _ => None,
    })
}

/// Why a push is being sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushKind {
    /// Wake the device for an incoming call.
    Call,
    /// Wake the device so it refreshes its binding before it expires.
    BindingRefresh,
}

impl PushKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PushKind::Call => "call",
            PushKind::BindingRefresh => "binding_refresh",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PushNotification {
    pub kind: PushKind,
    /// Call-ID of the INVITE the device will receive once awake, so the app
    /// can match the push to the call (Linphone's `call-id` payload key).
    pub call_id: Option<String>,
    pub caller_uri: Option<String>,
    pub caller_name: Option<String>,
    /// How long the PNS should keep trying to deliver the push.
    pub ttl: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// The PNS reports the push resource ID as no longer valid (app
    /// uninstalled, token rotated); the device must not be pushed again.
    #[error("push resource id is no longer valid: {0}")]
    InvalidPrid(String),
    #[error("no push provider configured for {0}")]
    Unsupported(String),
    #[error("push delivery failed: {0}")]
    Failed(String),
}

/// A push notification service (`pn-provider` value) able to wake devices.
#[async_trait]
pub trait PushProvider: Send + Sync {
    /// The `pn-provider` value handled (e.g. `fcm`).
    fn name(&self) -> &str;
    /// Whether this provider can deliver to devices registered with `params`
    /// (e.g. FCM must hold credentials for the `pn-param` project).
    fn accepts(&self, params: &PushParams) -> bool;
    async fn send(
        &self,
        params: &PushParams,
        notification: &PushNotification,
    ) -> Result<(), PushError>;
}

/// Liveness of a SIP flow towards a destination, as seen by the transport
/// layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowState {
    /// A connection to the destination is open.
    Up,
    /// Connection-oriented transport with no open connection: the device
    /// cannot be reached until it connects again.
    Down,
    /// Connectionless transport (UDP): liveness cannot be observed.
    Unknown,
}

pub struct PushService {
    config: PushConfig,
    providers: Vec<Arc<dyn PushProvider>>,
    registry: PushRegistry,
    /// Open stream connections by remote `host:port` (count of connections).
    flows: DashMap<String, u32>,
    /// Locator holding the bindings, attached by the server.
    locator: OnceLock<Arc<Box<dyn Locator>>>,
}

impl PushService {
    pub fn new(
        config: PushConfig,
        providers: Vec<Arc<dyn PushProvider>>,
        registry: PushRegistry,
    ) -> Self {
        Self {
            config,
            providers,
            registry,
            flows: DashMap::new(),
            locator: OnceLock::new(),
        }
    }

    /// Build from `[proxy.push]`, loading provider credentials and the
    /// persisted device registry.
    pub async fn from_config(
        config: &PushConfig,
        http: reqwest::Client,
        db: Option<DatabaseConnection>,
    ) -> Result<Self> {
        let mut providers: Vec<Arc<dyn PushProvider>> = Vec::new();
        if !config.fcm.is_empty() {
            providers.push(Arc::new(fcm::FcmProvider::from_config(
                &config.fcm,
                http.clone(),
            )?));
        }
        if !config.apns.is_empty() {
            for provider in apns::ApnsProvider::from_config(&config.apns, http)? {
                providers.push(Arc::new(provider));
            }
        }
        if providers.is_empty() {
            warn!("push notifications enabled but no provider is configured");
        }
        let registry = match db {
            Some(db) => PushRegistry::with_database(db).await?,
            None => {
                warn!(
                    "no database available: push device registrations will not survive a restart"
                );
                PushRegistry::new()
            }
        };
        info!(
            providers = ?providers.iter().map(|p| p.name().to_string()).collect::<Vec<_>>(),
            devices = registry.len(),
            "push notifications enabled"
        );
        Ok(Self::new(config.clone(), providers, registry))
    }

    pub fn config(&self) -> &PushConfig {
        &self.config
    }

    pub fn registry(&self) -> &PushRegistry {
        &self.registry
    }

    pub fn wakeup_timeout(&self) -> Duration {
        Duration::from_secs(self.config.wakeup_timeout_secs.max(1))
    }

    /// Attach the locator holding the bindings, and restore the bindings of
    /// devices persisted by a previous run (an in-memory locator starts
    /// empty; a sleeping device would otherwise be unreachable until it
    /// happens to register again).
    pub async fn attach_locator(&self, locator: Arc<Box<dyn Locator>>) {
        let now = registry::now_epoch_secs();
        let mut restored = 0;
        for device in self.registry.all(now) {
            if self.provider_for(&device.params).is_none() {
                continue;
            }
            let Some(binding) = device.binding(now) else {
                continue;
            };
            match locator
                .register(&device.username, device.realm.as_deref(), binding)
                .await
            {
                Ok(()) => restored += 1,
                Err(e) => {
                    warn!(identity = %device.identity, error = %e, "failed to restore push binding")
                }
            }
        }
        if restored > 0 {
            info!(count = restored, "restored push device bindings");
        }
        let _ = self.locator.set(locator);
    }

    /// A connection to `addr` was opened.
    pub fn flow_opened(&self, addr: &SipAddr) {
        *self.flows.entry(addr.addr.to_string()).or_insert(0) += 1;
    }

    /// A connection to `addr` was closed.
    pub fn flow_closed(&self, addr: &SipAddr) {
        let key = addr.addr.to_string();
        let gone = self.flows.get_mut(&key).is_some_and(|mut count| {
            *count = count.saturating_sub(1);
            *count == 0
        });
        if gone {
            self.flows.remove_if(&key, |_, count| *count == 0);
        }
    }

    pub fn flow_state(&self, destination: &SipAddr) -> FlowState {
        match destination.r#type {
            Some(Transport::Tcp | Transport::Tls | Transport::Ws | Transport::Wss) => {
                if self.flows.contains_key(&destination.addr.to_string()) {
                    FlowState::Up
                } else {
                    FlowState::Down
                }
            }
            _ => FlowState::Unknown,
        }
    }

    /// Whether `binding` must survive its flow closing: it belongs to a device
    /// registered for push, which stays reachable while asleep.
    pub fn keeps_binding(&self, binding: &Location) -> bool {
        self.config.enabled
            && PushParams::from_uri(&binding.aor)
                .and_then(|params| self.registry.device_with_prid(&params))
                .is_some_and(|device| self.provider_for(&device.params).is_some())
    }

    /// The push device an INVITE is addressed to: by the `pn-*` parameters
    /// of its Request-URI (the binding's Contact, RFC 8599 §5.6.2), or — when
    /// the Request-URI is the user's AoR, e.g. after a route rewrite — by the
    /// device's flow being the INVITE's destination.
    pub fn device_for_invite(
        &self,
        callee: &Uri,
        destination: Option<&SipAddr>,
    ) -> Option<PushDevice> {
        if !self.config.enabled {
            return None;
        }
        let device = match PushParams::from_uri(callee) {
            Some(params) => self.registry.device_with_prid(&params),
            None => self
                .registry
                .device_at(destination?, callee.user().unwrap_or_default()),
        }?;
        self.provider_for(&device.params).map(|_| device)
    }

    /// Provider able to push to devices registered with `params`.
    pub fn provider_for(&self, params: &PushParams) -> Option<&Arc<dyn PushProvider>> {
        self.providers
            .iter()
            .find(|p| p.name().eq_ignore_ascii_case(&params.provider) && p.accepts(params))
    }

    pub fn supports_provider(&self, provider: &str) -> bool {
        self.providers
            .iter()
            .any(|p| p.name().eq_ignore_ascii_case(provider))
    }

    /// Distinct provider names, in configuration order.
    pub fn provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for provider in &self.providers {
            let name = provider.name().to_ascii_lowercase();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// `Feature-Caps` value advertising support of `provider` (RFC 8599 §5.4),
    /// with the `sip.pnsreg` indicator when the UA can refresh its binding
    /// without being pushed (§5.6.1.1).
    pub fn feature_caps(&self, provider: &str, pnsreg: bool) -> String {
        let mut value = format!("*;+sip.pns=\"{}\"", provider);
        if pnsreg {
            // The indicator value must be bigger than 120.
            let secs = self.config.refresh_before_secs.max(121);
            value.push_str(&format!(";+sip.pnsreg=\"{}\"", secs));
        }
        value
    }

    /// Push `device`. A push resource ID the provider reports as invalid is
    /// dropped from the registry (RFC 8599 §5.5: never push it again).
    pub async fn send(
        &self,
        device: &PushDevice,
        notification: &PushNotification,
    ) -> Result<(), PushError> {
        let Some(provider) = self.provider_for(&device.params) else {
            return Err(PushError::Unsupported(device.params.provider.clone()));
        };
        let result = provider.send(&device.params, notification).await;
        let outcome = match &result {
            Ok(()) => {
                debug!(
                    identity = %device.identity,
                    provider = %device.params.provider,
                    kind = notification.kind.as_str(),
                    call_id = ?notification.call_id,
                    "push notification sent"
                );
                "ok"
            }
            Err(PushError::InvalidPrid(reason)) => {
                warn!(
                    identity = %device.identity,
                    provider = %device.params.provider,
                    %reason,
                    "push resource id rejected by provider, forgetting device"
                );
                let removed = self.registry.remove_prid(&device.params).await;
                self.unregister_bindings(&removed).await;
                "invalid_prid"
            }
            Err(error) => {
                warn!(
                    identity = %device.identity,
                    provider = %device.params.provider,
                    kind = notification.kind.as_str(),
                    %error,
                    "push notification failed"
                );
                "failed"
            }
        };
        crate::metrics::push::sent(&device.params.provider, notification.kind.as_str(), outcome);
        result
    }

    /// Remove the locator bindings of devices that can no longer be pushed.
    async fn unregister_bindings(&self, devices: &[PushDevice]) {
        let Some(locator) = self.locator.get() else {
            return;
        };
        for device in devices {
            if let Some(mut binding) = device.binding(registry::now_epoch_secs()) {
                binding.expires = 0;
                let _ = locator
                    .register(&device.username, device.realm.as_deref(), binding)
                    .await;
            }
        }
    }

    /// Periodically sweep expired devices and send binding-refresh pushes.
    pub fn spawn_maintenance(self: &Arc<Self>, cancel: CancellationToken) {
        let service = self.clone();
        crate::utils::spawn(async move {
            let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => service.maintain(registry::now_epoch_secs()).await,
                }
            }
        });
    }

    /// One maintenance pass at `now` (unix seconds).
    pub async fn maintain(self: &Arc<Self>, now: i64) {
        let expired = self.registry.sweep_expired(now).await;
        if !expired.is_empty() {
            info!(
                count = expired.len(),
                "swept expired push device registrations"
            );
        }
        if self.config.refresh_before_secs == 0 {
            return;
        }
        let refresh_before = self.config.refresh_before_secs as i64;
        for device in self.registry.due_for_refresh(now, refresh_before) {
            // Mark first: a slow or failing push must not be retried every pass.
            self.registry.mark_refresh_pushed(&device, now).await;
            let notification = PushNotification {
                kind: PushKind::BindingRefresh,
                call_id: None,
                caller_uri: None,
                caller_name: None,
                ttl: Duration::from_secs((device.expires_at - now).max(1) as u64),
            };
            let service = self.clone();
            crate::utils::spawn(async move {
                let _ = service.send(&device, &notification).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        Uri::try_from(s).expect("uri")
    }

    #[test]
    fn parses_linphone_contact() {
        let contact = uri(
            "sip:bob@192.168.1.20:41203;transport=tls;pn-prid=fGq3cX7rS9:APA91bH-tok_en;pn-provider=fcm;pn-param=929724111839;pn-silent=1;pn-timeout=0",
        );
        assert_eq!(
            ContactPush::from_uri(&contact),
            ContactPush::Request(PushParams {
                provider: "fcm".into(),
                prid: "fGq3cX7rS9:APA91bH-tok_en".into(),
                param: Some("929724111839".into()),
            })
        );
    }

    #[test]
    fn provider_is_case_insensitive_and_values_are_unescaped() {
        let contact = uri("sip:bob@example.com;PN-Provider=FCM;pn-prid=abc%3Adef");
        let params = PushParams::from_uri(&contact).expect("push params");
        assert_eq!(params.provider, "fcm");
        assert_eq!(params.prid, "abc:def");
        assert_eq!(params.param, None);
    }

    #[test]
    fn capability_queries() {
        assert_eq!(
            ContactPush::from_uri(&uri("sip:bob@example.com;pn-provider=fcm")),
            ContactPush::Query(Some("fcm".into()))
        );
        assert_eq!(
            ContactPush::from_uri(&uri("sip:bob@example.com;pn-provider")),
            ContactPush::Query(None)
        );
        assert_eq!(
            ContactPush::from_uri(&uri("sip:bob@example.com;transport=tcp")),
            ContactPush::None
        );
    }

    struct RecordingProvider {
        sent: parking_lot::Mutex<Vec<(PushParams, PushNotification)>>,
        invalid_prid: bool,
    }

    #[async_trait]
    impl PushProvider for RecordingProvider {
        fn name(&self) -> &str {
            "fcm"
        }

        fn accepts(&self, _params: &PushParams) -> bool {
            true
        }

        async fn send(
            &self,
            params: &PushParams,
            notification: &PushNotification,
        ) -> Result<(), PushError> {
            self.sent
                .lock()
                .push((params.clone(), notification.clone()));
            if self.invalid_prid {
                return Err(PushError::InvalidPrid("UNREGISTERED".into()));
            }
            Ok(())
        }
    }

    fn service(invalid_prid: bool) -> (Arc<PushService>, Arc<RecordingProvider>) {
        let provider = Arc::new(RecordingProvider {
            sent: parking_lot::Mutex::new(Vec::new()),
            invalid_prid,
        });
        let service = Arc::new(PushService::new(
            PushConfig::default(),
            vec![provider.clone()],
            PushRegistry::new(),
        ));
        (service, provider)
    }

    fn device(registered_at: i64, expires_at: i64) -> PushDevice {
        PushDevice {
            identity: "bob@localhost".into(),
            username: "bob".into(),
            realm: None,
            aor: "sip:bob@localhost".into(),
            contact: "sip:bob@10.0.0.2:5061;transport=tls;pn-provider=fcm;pn-prid=tok".into(),
            instance_id: Some("<urn:uuid:1>".into()),
            params: PushParams {
                provider: "fcm".into(),
                prid: "tok".into(),
                param: None,
            },
            user_agent: None,
            expires_at,
            registered_at,
            refresh_pushed_at: None,
            destination: None,
        }
    }

    async fn wait_for_sends(provider: &RecordingProvider, count: usize) {
        for _ in 0..100 {
            if provider.sent.lock().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn binding_refresh_push_before_expiry() {
        let (service, provider) = service(false);
        service.registry().upsert(device(0, 3600)).await;

        service.maintain(3000).await;
        assert!(provider.sent.lock().is_empty(), "not due yet");

        service.maintain(3301).await;
        wait_for_sends(&provider, 1).await;
        {
            let sent = provider.sent.lock();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].1.kind, PushKind::BindingRefresh);
            assert_eq!(sent[0].1.call_id, None);
        }

        service.maintain(3400).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            provider.sent.lock().len(),
            1,
            "one refresh push per registration"
        );

        service.maintain(3600).await;
        assert!(service.registry().is_empty(), "expired binding swept");
    }

    #[tokio::test]
    async fn invalid_prid_forgets_device() {
        let (service, provider) = service(true);
        let device = device(0, i64::MAX);
        service.registry().upsert(device.clone()).await;

        let notification = PushNotification {
            kind: PushKind::Call,
            call_id: Some("c".into()),
            caller_uri: None,
            caller_name: None,
            ttl: Duration::from_secs(30),
        };
        let result = service.send(&device, &notification).await;
        assert!(matches!(result, Err(PushError::InvalidPrid(_))));
        assert_eq!(provider.sent.lock().len(), 1);
        assert!(service.registry().is_empty());
    }

    fn tcp(addr: &str) -> SipAddr {
        SipAddr {
            r#type: Some(Transport::Tcp),
            addr: addr.try_into().unwrap(),
        }
    }

    #[test]
    fn flow_tracking() {
        let (service, _) = service(false);
        let flow = tcp("10.0.0.2:40001");
        assert_eq!(service.flow_state(&flow), FlowState::Down);
        service.flow_opened(&flow);
        service.flow_opened(&flow);
        assert_eq!(service.flow_state(&flow), FlowState::Up);
        service.flow_closed(&flow);
        assert_eq!(
            service.flow_state(&flow),
            FlowState::Up,
            "one connection left"
        );
        service.flow_closed(&flow);
        assert_eq!(service.flow_state(&flow), FlowState::Down);
        let udp = SipAddr {
            r#type: Some(Transport::Udp),
            addr: "10.0.0.2:5060".try_into().unwrap(),
        };
        assert_eq!(service.flow_state(&udp), FlowState::Unknown);
    }

    #[tokio::test]
    async fn push_devices_and_their_bindings() {
        let (service, _) = service(false);
        let mut registered = device(0, i64::MAX);
        registered.destination = Some(tcp("10.0.0.2:40001"));
        service.registry().upsert(registered).await;

        let binding = Location {
            aor: uri("sip:bob@10.0.0.2:5061;transport=tls;pn-provider=fcm;pn-prid=tok"),
            ..Default::default()
        };
        assert!(service.keeps_binding(&binding));
        let foreign = Location {
            aor: uri("sip:bob@10.0.0.2:5061;transport=tls;pn-provider=fcm;pn-prid=other"),
            ..Default::default()
        };
        assert!(
            !service.keeps_binding(&foreign),
            "not registered for push here"
        );
        let plain = Location {
            aor: uri("sip:bob@10.0.0.2:5061;transport=tls"),
            ..Default::default()
        };
        assert!(!service.keeps_binding(&plain));

        // Addressed by the binding's Contact (pn-* in the Request-URI).
        assert!(service.device_for_invite(&binding.aor, None).is_some());
        // Addressed by AoR (e.g. after a route rewrite), sent to its flow.
        let aor = uri("sip:bob@pbx.example.com");
        let flow = tcp("10.0.0.2:40001");
        assert!(service.device_for_invite(&aor, Some(&flow)).is_some());
        assert!(
            service
                .device_for_invite(&uri("sip:alice@pbx.example.com"), Some(&flow))
                .is_none(),
            "another user sharing the flow"
        );
        assert!(
            service
                .device_for_invite(&aor, Some(&tcp("10.0.0.3:1")))
                .is_none()
        );
    }

    #[tokio::test]
    async fn persisted_devices_are_restored_as_bindings() {
        let (service, _) = service(false);
        let now = registry::now_epoch_secs();
        service.registry().upsert(device(now, now + 3600)).await;

        let locator: Arc<Box<dyn Locator>> =
            Arc::new(Box::new(crate::proxy::locator::MemoryLocator::new()));
        service.attach_locator(locator.clone()).await;

        let bindings = locator.lookup(&uri("sip:bob@localhost")).await.unwrap();
        assert_eq!(bindings.len(), 1);
        assert!(service.keeps_binding(&bindings[0]));
        let destination = bindings[0].destination.clone().expect("destination");
        // Nothing is connected after a restart: the device must be pushed.
        assert_eq!(service.flow_state(&destination), FlowState::Down);
    }

    #[test]
    fn config_section_parses() {
        let config: crate::config::ProxyConfig = toml::from_str(
            r#"
            addr = "0.0.0.0"
            [push]
            wakeup_timeout_secs = 20
            [[push.fcm]]
            service_account = "/etc/rustpbx/sa.json"
            aliases = ["123456789012"]
            [[push.apns]]
            key = "/etc/rustpbx/AuthKey_KEY1234567.p8"
            key_id = "KEY1234567"
            team_id = "TEAM123456"
            bundle_ids = ["app.akiles.phone"]
            "#,
        )
        .expect("config");
        let push = config.push.expect("push section");
        assert!(push.enabled);
        assert!(push.push_connected);
        assert_eq!(push.wakeup_timeout_secs, 20);
        assert_eq!(push.max_expires, 604_800);
        assert_eq!(push.refresh_before_secs, 300);
        assert_eq!(push.fcm[0].service_account, "/etc/rustpbx/sa.json");
        assert_eq!(push.fcm[0].aliases, vec!["123456789012".to_string()]);
        assert_eq!(push.apns[0].key_id, "KEY1234567");
        assert_eq!(push.apns[0].team_id, "TEAM123456");
        assert_eq!(
            push.apns[0].bundle_ids,
            vec!["app.akiles.phone".to_string()]
        );
        assert_eq!(push.apns[0].api_url, None);
    }

    #[test]
    fn feature_caps_values() {
        let service = PushService::new(PushConfig::default(), Vec::new(), PushRegistry::new());
        assert_eq!(service.feature_caps("fcm", false), "*;+sip.pns=\"fcm\"");
        assert_eq!(
            service.feature_caps("fcm", true),
            "*;+sip.pns=\"fcm\";+sip.pnsreg=\"300\""
        );
    }
}
