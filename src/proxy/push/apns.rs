//! Apple Push Notification service provider (`pn-provider=apns` for
//! production, `apns.dev` for the sandbox), HTTP/2 API with token-based
//! authentication.
//!
//! Devices register the way Linphone and Flexisip do it, an extension of
//! RFC 8599 that carries two tokens:
//!
//! - `pn-param`: `<team ID>.<bundle ID>.<services>`, where the services are
//!   `voip`, `remote` or `voip&remote`. Without services, `remote`.
//! - `pn-prid`: one token per service, `<token>:<service>` joined with `&`. A
//!   single token may leave out `:<service>`.
//!
//! Calls use a VoIP push (PushKit) to the `voip` token. iOS requires the app
//! to report a CallKit call for every VoIP push, so binding refreshes use a
//! background push to the `remote` token instead. Payloads use the keys
//! Flexisip sends, which Linphone understands (`aps.call-id` in particular).

use super::{PushError, PushKind, PushNotification, PushParams, PushProvider};
use crate::config::ApnsKeyConfig;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

const PRODUCTION_API_URL: &str = "https://api.push.apple.com";
const SANDBOX_API_URL: &str = "https://api.sandbox.push.apple.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// APNs rejects provider tokens older than one hour, and new tokens more
/// often than one every 20 minutes.
const TOKEN_LIFETIME: Duration = Duration::from_secs(50 * 60);
const RETRY_DELAY: Duration = Duration::from_millis(300);

/// APNs environment. The device selects it with `pn-provider`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApnsEnvironment {
    Production,
    Sandbox,
}

impl ApnsEnvironment {
    fn provider_name(self) -> &'static str {
        match self {
            ApnsEnvironment::Production => "apns",
            ApnsEnvironment::Sandbox => "apns.dev",
        }
    }
}

/// APNs in one environment. Both environments share the same keys.
pub struct ApnsProvider {
    environment: ApnsEnvironment,
    keys: Arc<Vec<ApnsKey>>,
    http: reqwest::Client,
}

impl ApnsProvider {
    pub fn new(
        environment: ApnsEnvironment,
        keys: Arc<Vec<ApnsKey>>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            environment,
            keys,
            http,
        }
    }

    /// The providers of both environments (`apns` and `apns.dev`).
    pub fn from_config(configs: &[ApnsKeyConfig], http: reqwest::Client) -> Result<Vec<Self>> {
        let mut keys = Vec::new();
        for config in configs {
            let pem = std::fs::read_to_string(&config.key)
                .with_context(|| format!("failed to read APNs key {}", config.key))?;
            let key = ApnsKey::from_pem(&pem, config)
                .with_context(|| format!("invalid APNs key {}", config.key))?;
            info!(
                key_id = %key.key_id,
                team_id = %key.team_id,
                bundle_ids = ?key.bundle_ids,
                "APNs key configured"
            );
            keys.push(key);
        }
        let keys = Arc::new(keys);
        Ok([ApnsEnvironment::Production, ApnsEnvironment::Sandbox]
            .into_iter()
            .map(|environment| Self::new(environment, keys.clone(), http.clone()))
            .collect())
    }

    /// The key that signs pushes to `device`: the key listing its bundle ID,
    /// else the key of its team without a bundle list, else the only key when
    /// it has no bundle list.
    fn key_for(&self, device: &ApnsDevice) -> Option<&ApnsKey> {
        let keys = self.keys.as_slice();
        keys.iter()
            .find(|k| k.bundle_ids.contains(&device.bundle_id))
            .or_else(|| {
                keys.iter()
                    .find(|k| k.bundle_ids.is_empty() && k.team_id == device.team_id)
            })
            .or(match keys {
                [only] if only.bundle_ids.is_empty() => Some(only),
                _ => None,
            })
    }
}

#[async_trait]
impl PushProvider for ApnsProvider {
    fn name(&self) -> &str {
        self.environment.provider_name()
    }

    /// Only devices with a VoIP token: without one a call cannot wake them.
    fn accepts(&self, params: &PushParams) -> bool {
        ApnsDevice::parse(params)
            .is_some_and(|device| device.voip_token.is_some() && self.key_for(&device).is_some())
    }

    async fn send(
        &self,
        params: &PushParams,
        notification: &PushNotification,
    ) -> Result<(), PushError> {
        let device = ApnsDevice::parse(params).ok_or_else(|| {
            PushError::Unsupported(format!("apns push parameters {:?}", params.param))
        })?;
        let key = self.key_for(&device).ok_or_else(|| {
            PushError::Unsupported(format!(
                "apns key for team {} app {}",
                device.team_id, device.bundle_id
            ))
        })?;
        let push = ApnsPush::new(&device, notification, super::registry::now_epoch_secs())?;
        let url = format!(
            "{}/3/device/{}",
            key.api_url(self.environment),
            push.device_token
        );
        let mut retried = false;
        loop {
            let provider_token = key.provider_token()?;
            let response = self
                .http
                .post(&url)
                .bearer_auth(&provider_token)
                .header("apns-topic", &push.topic)
                .header("apns-push-type", push.push_type)
                .header("apns-priority", push.priority)
                .header("apns-expiration", push.expiration.to_string())
                .json(&push.payload)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;
            let (status, text) = match response {
                Ok(response) if response.status().is_success() => {
                    debug!(
                        environment = self.name(),
                        apns_id = ?response.headers().get("apns-id"),
                        push_type = push.push_type,
                        "APNs push accepted"
                    );
                    return Ok(());
                }
                Ok(response) => {
                    let status = response.status();
                    (status, response.text().await.unwrap_or_default())
                }
                Err(e) if !retried => {
                    debug!(error = %e, "APNs request failed, retrying");
                    retried = true;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                Err(e) => return Err(PushError::Failed(e.to_string())),
            };
            let reason = serde_json::from_str::<ErrorBody>(&text)
                .unwrap_or_default()
                .reason;
            match classify(status, &reason) {
                Outcome::InvalidToken => return Err(PushError::InvalidPrid(reason)),
                Outcome::NewProviderToken if !retried => {
                    retried = true;
                    key.invalidate_token(&provider_token);
                }
                Outcome::Retry if !retried => {
                    retried = true;
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Outcome::NewProviderToken | Outcome::Retry | Outcome::Fail => {
                    let hint = if reason == "BadDeviceToken" {
                        " (a sandbox token sent to production, or the other way round?)"
                    } else {
                        ""
                    };
                    return Err(PushError::Failed(format!(
                        "APNs {} {}: {}{}",
                        self.name(),
                        status,
                        reason,
                        hint
                    )));
                }
            }
        }
    }
}

/// What an APNs device registered in `pn-param` and `pn-prid`.
#[derive(Debug, PartialEq, Eq)]
struct ApnsDevice {
    team_id: String,
    bundle_id: String,
    voip_token: Option<String>,
    remote_token: Option<String>,
}

impl ApnsDevice {
    fn parse(params: &PushParams) -> Option<Self> {
        let (team_id, rest) = params.param.as_deref()?.split_once('.')?;
        let (bundle_id, services) = match rest.rsplit_once('.') {
            Some((bundle_id, services))
                if services
                    .split('&')
                    .all(|service| service == "voip" || service == "remote") =>
            {
                (bundle_id, services.split('&').collect::<Vec<_>>())
            }
            _ => (rest, vec!["remote"]),
        };
        if team_id.is_empty() || bundle_id.is_empty() {
            return None;
        }
        let tokens: Vec<&str> = params.prid.split('&').collect();
        if tokens.len() != services.len() {
            return None;
        }
        let mut device = Self {
            team_id: team_id.to_string(),
            bundle_id: bundle_id.to_string(),
            voip_token: None,
            remote_token: None,
        };
        for token in tokens {
            let (token, service) = match token.split_once(':') {
                Some((token, service)) => (token, service),
                None if services.len() == 1 => (token, services[0]),
                None => return None,
            };
            // The token goes into the request path.
            if !services.contains(&service)
                || token.is_empty()
                || !token.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return None;
            }
            let slot = match service {
                "voip" => &mut device.voip_token,
                _ => &mut device.remote_token,
            };
            if slot.replace(token.to_string()).is_some() {
                return None;
            }
        }
        Some(device)
    }
}

/// One APNs request.
struct ApnsPush {
    device_token: String,
    topic: String,
    push_type: &'static str,
    priority: &'static str,
    /// Unix seconds after which APNs stops trying to deliver.
    expiration: i64,
    payload: serde_json::Value,
}

impl ApnsPush {
    fn new(
        device: &ApnsDevice,
        notification: &PushNotification,
        now: i64,
    ) -> Result<Self, PushError> {
        let ttl = notification.ttl.as_secs().max(1);
        let expiration = now + ttl as i64;
        let send_time = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        match notification.kind {
            PushKind::Call => {
                let device_token = device.voip_token.clone().ok_or_else(|| {
                    PushError::Unsupported("apns device without a voip token".into())
                })?;
                let caller_uri = notification.caller_uri.clone().unwrap_or_default();
                let caller_name = notification.caller_name.clone().unwrap_or_default();
                let from = if caller_name.is_empty() {
                    caller_uri.clone()
                } else {
                    caller_name.clone()
                };
                Ok(Self {
                    device_token,
                    topic: format!("{}.voip", device.bundle_id),
                    push_type: "voip",
                    priority: "10",
                    expiration,
                    payload: json!({
                        "aps": {
                            "sound": "",
                            "loc-key": "IC_MSG",
                            "loc-args": [from],
                            "call-id": notification.call_id.clone().unwrap_or_default(),
                            "send-time": send_time,
                        },
                        "from-uri": caller_uri,
                        "display-name": caller_name,
                        "pn_ttl": ttl,
                        "customPayload": {},
                    }),
                })
            }
            // Apple requires priority 5 for background pushes, and delivers
            // only a few per hour.
            PushKind::BindingRefresh => {
                let device_token = device.remote_token.clone().ok_or_else(|| {
                    PushError::Unsupported(
                        "apns binding refresh needs a remote token (VoIP pushes must ring)".into(),
                    )
                })?;
                Ok(Self {
                    device_token,
                    topic: device.bundle_id.clone(),
                    push_type: "background",
                    priority: "5",
                    expiration,
                    payload: json!({
                        "aps": {
                            "content-available": 1,
                            "call-id": "",
                            "send-time": send_time,
                        },
                        "pn_ttl": ttl,
                        "customPayload": {},
                    }),
                })
            }
        }
    }
}

enum Outcome {
    /// The device token is no longer valid (app uninstalled).
    InvalidToken,
    /// Our provider token expired: sign a new one.
    NewProviderToken,
    Retry,
    Fail,
}

#[derive(Deserialize, Default)]
struct ErrorBody {
    #[serde(default)]
    reason: String,
}

fn classify(status: reqwest::StatusCode, reason: &str) -> Outcome {
    match status.as_u16() {
        // Unregistered, ExpiredToken. BadDeviceToken (400) is not here: it
        // also comes from a token of the other environment.
        410 => Outcome::InvalidToken,
        403 if reason == "ExpiredProviderToken" => Outcome::NewProviderToken,
        500 | 503 => Outcome::Retry,
        _ => Outcome::Fail,
    }
}

/// An APNs authentication key (`.p8`) and the provider token it signed last.
pub struct ApnsKey {
    key_id: String,
    team_id: String,
    bundle_ids: Vec<String>,
    api_url: String,
    sandbox_api_url: String,
    signing_key: EcdsaKeyPair,
    token: parking_lot::Mutex<Option<CachedToken>>,
}

struct CachedToken {
    value: String,
    issued_at: Instant,
}

impl ApnsKey {
    pub fn from_pem(pem: &str, config: &ApnsKeyConfig) -> Result<Self> {
        if config.key_id.is_empty() || config.team_id.is_empty() {
            return Err(anyhow!("key_id and team_id are required"));
        }
        let der = super::fcm::pem_to_der(pem)?;
        let signing_key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new())
                .map_err(|e| anyhow!("not a P-256 private key in PKCS#8: {}", e))?;
        let url = |url: &Option<String>, default: &str| {
            url.as_deref()
                .unwrap_or(default)
                .trim_end_matches('/')
                .to_string()
        };
        Ok(Self {
            key_id: config.key_id.clone(),
            team_id: config.team_id.clone(),
            bundle_ids: config.bundle_ids.clone(),
            api_url: url(&config.api_url, PRODUCTION_API_URL),
            sandbox_api_url: url(&config.sandbox_api_url, SANDBOX_API_URL),
            signing_key,
            token: parking_lot::Mutex::new(None),
        })
    }

    fn api_url(&self, environment: ApnsEnvironment) -> &str {
        match environment {
            ApnsEnvironment::Production => &self.api_url,
            ApnsEnvironment::Sandbox => &self.sandbox_api_url,
        }
    }

    /// The cached provider token, or a new one when it is too old.
    fn provider_token(&self) -> Result<String, PushError> {
        let mut cached = self.token.lock();
        if let Some(token) = cached.as_ref()
            && token.issued_at.elapsed() < TOKEN_LIFETIME
        {
            return Ok(token.value.clone());
        }
        let value = self
            .sign(super::registry::now_epoch_secs())
            .map_err(|e| PushError::Failed(e.to_string()))?;
        *cached = Some(CachedToken {
            value: value.clone(),
            issued_at: Instant::now(),
        });
        Ok(value)
    }

    /// Forget `rejected` if it is still the cached token. Parallel pushes
    /// rejected with the same token then cause one new token, not one each
    /// (APNs refuses new tokens more often than every 20 minutes).
    fn invalidate_token(&self, rejected: &str) {
        let mut cached = self.token.lock();
        if cached.as_ref().is_some_and(|token| token.value == rejected) {
            *cached = None;
        }
    }

    /// Provider token: a JWT signed with ES256.
    fn sign(&self, now: i64) -> Result<String> {
        let header = json!({ "alg": "ES256", "kid": self.key_id });
        let claims = json!({ "iss": self.team_id, "iat": now });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self
            .signing_key
            .sign(&SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| anyhow!("failed to sign APNs provider token"))?;
        Ok(format!(
            "{}.{}",
            signing_input,
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use parking_lot::Mutex;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, KeyPair, UnparsedPublicKey};

    // Throwaway key generated for these tests only.
    const TEST_KEY: &str = include_str!("testdata/apns_test_key.p8");

    const VOIP_TOKEN: &str = "0a0a0a0a";
    const REMOTE_TOKEN: &str = "0b0b0b0b";
    const EXPIRE_PROVIDER_TOKEN: &str = "0e0e0e0e";
    const UNINSTALLED: &str = "0f0f0f0f";

    struct Request {
        path: String,
        headers: HeaderMap,
        body: serde_json::Value,
    }

    #[derive(Default)]
    struct Mock {
        public_key: Vec<u8>,
        requests: Mutex<Vec<Request>>,
        expired_once: Mutex<bool>,
    }

    impl Mock {
        fn header(&self, index: usize, name: &str) -> String {
            self.requests.lock()[index]
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        }
    }

    fn jwt_part(jwt: &str, index: usize) -> serde_json::Value {
        let part = jwt.split('.').nth(index).unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    async fn push(
        State(mock): State<Arc<Mock>>,
        Path((environment, device_token)): Path<(String, String)>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let jwt = auth.strip_prefix("Bearer ").expect("bearer provider token");
        let (signing_input, signature) = jwt.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &mock.public_key)
            .verify(
                signing_input.as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .expect("provider token signed with the APNs key");
        mock.requests.lock().push(Request {
            path: format!("/{environment}/3/device/{device_token}"),
            headers: headers.clone(),
            body,
        });
        if device_token == EXPIRE_PROVIDER_TOKEN
            && !std::mem::replace(&mut *mock.expired_once.lock(), true)
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"reason": "ExpiredProviderToken"})),
            );
        }
        if device_token == UNINSTALLED {
            return (
                StatusCode::GONE,
                Json(json!({"reason": "Unregistered", "timestamp": 1_700_000_000_000u64})),
            );
        }
        (StatusCode::OK, Json(json!({})))
    }

    async fn start_mock() -> (String, Arc<Mock>) {
        let der = super::super::fcm::pem_to_der(TEST_KEY).unwrap();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new())
                .unwrap();
        let mock = Arc::new(Mock {
            public_key: key.public_key().as_ref().to_vec(),
            ..Default::default()
        });
        let app = Router::new()
            .route("/{environment}/3/device/{token}", post(push))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, mock)
    }

    fn key_config(base: &str, team_id: &str, bundle_ids: &[&str]) -> ApnsKeyConfig {
        ApnsKeyConfig {
            key: String::new(),
            key_id: "KEY1234567".into(),
            team_id: team_id.into(),
            bundle_ids: bundle_ids.iter().map(|b| b.to_string()).collect(),
            api_url: Some(format!("{base}/prod")),
            sandbox_api_url: Some(format!("{base}/sandbox/")),
        }
    }

    fn providers(configs: &[ApnsKeyConfig]) -> (ApnsProvider, ApnsProvider) {
        let keys = Arc::new(
            configs
                .iter()
                .map(|config| ApnsKey::from_pem(TEST_KEY, config).unwrap())
                .collect::<Vec<_>>(),
        );
        let http = reqwest::Client::new();
        (
            ApnsProvider::new(ApnsEnvironment::Production, keys.clone(), http.clone()),
            ApnsProvider::new(ApnsEnvironment::Sandbox, keys, http),
        )
    }

    fn params(provider: &str, param: &str, prid: &str) -> PushParams {
        PushParams {
            provider: provider.into(),
            prid: prid.into(),
            param: Some(param.into()),
        }
    }

    /// What Linphone registers with VoIP and remote push allowed.
    fn linphone(prid_voip: &str) -> PushParams {
        params(
            "apns",
            "TEAM123456.app.akiles.phone.voip&remote",
            &format!("{prid_voip}:voip&{REMOTE_TOKEN}:remote"),
        )
    }

    fn notification(kind: PushKind) -> PushNotification {
        PushNotification {
            kind,
            call_id: (kind == PushKind::Call).then(|| "abc@pbx".to_string()),
            caller_uri: Some("sip:alice@pbx".into()),
            caller_name: Some("Alice".into()),
            ttl: Duration::from_secs(30),
        }
    }

    #[test]
    fn parses_push_parameters() {
        // Linphone: both tokens, service after each.
        assert_eq!(
            ApnsDevice::parse(&params(
                "apns",
                "ABCD1234.org.linphone.phone.voip&remote",
                "AA11:voip&BB22:remote"
            )),
            Some(ApnsDevice {
                team_id: "ABCD1234".into(),
                bundle_id: "org.linphone.phone".into(),
                voip_token: Some("AA11".into()),
                remote_token: Some("BB22".into()),
            })
        );
        // Flexisip order.
        let device = ApnsDevice::parse(&params(
            "apns",
            "ABCD1234.org.linphone.phone.remote&voip",
            "BB22:remote&AA11:voip",
        ))
        .unwrap();
        assert_eq!(device.voip_token.as_deref(), Some("AA11"));
        assert_eq!(device.remote_token.as_deref(), Some("BB22"));
        // RFC 8599: one token, no service suffix.
        let device =
            ApnsDevice::parse(&params("apns", "DEF123GHIJ.com.example.app.voip", "AA11")).unwrap();
        assert_eq!(device.bundle_id, "com.example.app");
        assert_eq!(device.voip_token.as_deref(), Some("AA11"));
        assert_eq!(device.remote_token, None);
        // No services: remote.
        let device =
            ApnsDevice::parse(&params("apns", "DEF123GHIJ.com.example.app", "BB22")).unwrap();
        assert_eq!(device.bundle_id, "com.example.app");
        assert_eq!(device.voip_token, None);
        assert_eq!(device.remote_token.as_deref(), Some("BB22"));

        for (param, prid) in [
            ("T.app.voip&remote", "AA11:voip"),
            ("T.app.voip&remote", "AA11&BB22"),
            ("T.app.voip", "AA11:remote"),
            ("T.app.voip", "AA11:voip&BB22:voip"),
            ("T.app.voip", "AA/11"),
            ("T.app.voip", ""),
            ("T", "AA11"),
            (".app.voip", "AA11"),
        ] {
            assert_eq!(
                ApnsDevice::parse(&params("apns", param, prid)),
                None,
                "{param} / {prid}"
            );
        }
    }

    #[test]
    fn parses_linphone_contact() {
        let contact = rsipstack::sip::Uri::try_from(
            "sip:bob@192.168.1.20:41203;transport=tls;pn-prid=5A6B:voip&7C8D:remote;pn-provider=apns.dev;pn-param=ABCD1234.org.linphone.phone.voip&remote;pn-silent=1;pn-timeout=0",
        )
        .unwrap();
        let params = PushParams::from_uri(&contact).expect("push params");
        assert_eq!(params.provider, "apns.dev");
        let device = ApnsDevice::parse(&params).unwrap();
        assert_eq!(device.voip_token.as_deref(), Some("5A6B"));
        assert_eq!(device.remote_token.as_deref(), Some("7C8D"));
    }

    #[tokio::test]
    async fn call_sends_voip_push() {
        let (base, mock) = start_mock().await;
        let (production, sandbox) = providers(&[key_config(&base, "TEAM123456", &[])]);
        let now = super::super::registry::now_epoch_secs();

        production
            .send(&linphone(VOIP_TOKEN), &notification(PushKind::Call))
            .await
            .unwrap();
        production
            .send(&linphone(VOIP_TOKEN), &notification(PushKind::Call))
            .await
            .unwrap();

        {
            let requests = mock.requests.lock();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].path, format!("/prod/3/device/{VOIP_TOKEN}"));
            let body = &requests[0].body;
            assert_eq!(body["aps"]["call-id"], "abc@pbx");
            assert_eq!(body["aps"]["loc-key"], "IC_MSG");
            assert_eq!(body["aps"]["loc-args"][0], "Alice");
            assert_eq!(body["from-uri"], "sip:alice@pbx");
            assert!(body["aps"].get("alert").is_none(), "no visible alert");
        }

        assert_eq!(mock.header(0, "apns-topic"), "app.akiles.phone.voip");
        assert_eq!(mock.header(0, "apns-push-type"), "voip");
        assert_eq!(mock.header(0, "apns-priority"), "10");
        let expiration: i64 = mock.header(0, "apns-expiration").parse().unwrap();
        assert!((now + 30..=now + 32).contains(&expiration), "{expiration}");

        let auth = mock.header(0, "authorization");
        assert_eq!(auth, mock.header(1, "authorization"), "token is reused");
        let jwt = auth.strip_prefix("Bearer ").unwrap();
        assert_eq!(
            jwt_part(jwt, 0),
            json!({"alg": "ES256", "kid": "KEY1234567"})
        );
        assert_eq!(jwt_part(jwt, 1)["iss"], "TEAM123456");

        // apns.dev goes to the sandbox, with the same key and token.
        sandbox
            .send(&linphone(VOIP_TOKEN), &notification(PushKind::Call))
            .await
            .unwrap();
        assert_eq!(
            mock.requests.lock()[2].path,
            format!("/sandbox/3/device/{VOIP_TOKEN}")
        );
        assert_eq!(mock.header(2, "authorization"), auth);
    }

    #[tokio::test]
    async fn binding_refresh_sends_background_push() {
        let (base, mock) = start_mock().await;
        let (production, _) = providers(&[key_config(&base, "TEAM123456", &[])]);

        production
            .send(
                &linphone(VOIP_TOKEN),
                &notification(PushKind::BindingRefresh),
            )
            .await
            .unwrap();
        assert_eq!(
            mock.requests.lock()[0].path,
            format!("/prod/3/device/{REMOTE_TOKEN}")
        );
        assert_eq!(mock.requests.lock()[0].body["aps"]["content-available"], 1);
        assert_eq!(mock.header(0, "apns-topic"), "app.akiles.phone");
        assert_eq!(mock.header(0, "apns-push-type"), "background");
        assert_eq!(mock.header(0, "apns-priority"), "5");

        // A VoIP push that does not ring gets the app killed: never use one.
        let voip_only = params("apns", "TEAM123456.app.akiles.phone.voip", VOIP_TOKEN);
        let result = production
            .send(&voip_only, &notification(PushKind::BindingRefresh))
            .await;
        assert!(
            matches!(result, Err(PushError::Unsupported(_))),
            "{result:?}"
        );
        assert_eq!(mock.requests.lock().len(), 1);
    }

    #[tokio::test]
    async fn unregistered_token_is_reported() {
        let (base, _mock) = start_mock().await;
        let (production, _) = providers(&[key_config(&base, "TEAM123456", &[])]);
        let result = production
            .send(&linphone(UNINSTALLED), &notification(PushKind::Call))
            .await;
        assert!(
            matches!(&result, Err(PushError::InvalidPrid(reason)) if reason == "Unregistered"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn expired_provider_token_is_renewed() {
        let (base, mock) = start_mock().await;
        let (production, _) = providers(&[key_config(&base, "TEAM123456", &[])]);
        production
            .send(
                &linphone(EXPIRE_PROVIDER_TOKEN),
                &notification(PushKind::Call),
            )
            .await
            .unwrap();
        assert_eq!(mock.requests.lock().len(), 2);
        assert_ne!(
            mock.header(0, "authorization"),
            mock.header(1, "authorization")
        );
    }

    #[test]
    fn key_selection() {
        let base = "http://127.0.0.1:1";
        let any_app = params("apns", "ABCD1234.org.example.other.voip", "AA11");
        let (single, _) = providers(&[key_config(base, "TEAM123456", &[])]);
        assert!(
            single.accepts(&any_app),
            "a single key serves every app (Linphone's placeholder team ID)"
        );
        assert!(
            !single.accepts(&params(
                "apns",
                "TEAM123456.app.akiles.phone.remote",
                "BB22"
            )),
            "no VoIP token: a call cannot wake it"
        );
        assert!(!single.accepts(&PushParams {
            provider: "apns".into(),
            prid: "AA11".into(),
            param: None,
        }));

        let (restricted, _) = providers(&[key_config(base, "TEAM123456", &["app.akiles.phone"])]);
        assert!(restricted.accepts(&linphone(VOIP_TOKEN)));
        assert!(!restricted.accepts(&any_app), "not in bundle_ids");

        let (two, _) = providers(&[
            key_config(base, "TEAM123456", &["app.akiles.phone"]),
            key_config(base, "OTHER12345", &[]),
        ]);
        let device = ApnsDevice::parse(&linphone(VOIP_TOKEN)).unwrap();
        assert_eq!(two.key_for(&device).unwrap().team_id, "TEAM123456");
        let other =
            ApnsDevice::parse(&params("apns", "OTHER12345.com.other.voip", "AA11")).unwrap();
        assert_eq!(two.key_for(&other).unwrap().team_id, "OTHER12345");
        assert!(!two.accepts(&any_app), "unknown team, several keys");
    }
}
