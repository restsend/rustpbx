//! Firebase Cloud Messaging provider (`pn-provider=fcm`, HTTP v1 API).
//!
//! Authenticates with a service account (OAuth2 JWT-bearer grant signed with
//! RS256) and sends data-only, high-priority messages so the app's messaging
//! service runs even when the app is not in the foreground. The data payload
//! uses the keys Flexisip sends, which Linphone understands (`call-id` in
//! particular).

use super::{PushError, PushKind, PushNotification, PushParams, PushProvider};
use crate::config::FcmProjectConfig;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};
use tracing::{debug, info};

const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const DEFAULT_API_URL: &str = "https://fcm.googleapis.com";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// FCM keeps undelivered messages for at most four weeks.
const MAX_TTL: Duration = Duration::from_secs(28 * 24 * 3600);
/// Refresh the access token this long before it expires.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_millis(300);

pub struct FcmProvider {
    projects: Vec<FcmProject>,
    http: reqwest::Client,
}

impl FcmProvider {
    pub fn new(projects: Vec<FcmProject>, http: reqwest::Client) -> Self {
        Self { projects, http }
    }

    pub fn from_config(configs: &[FcmProjectConfig], http: reqwest::Client) -> Result<Self> {
        let mut projects = Vec::new();
        for config in configs {
            let json = std::fs::read_to_string(&config.service_account).with_context(|| {
                format!(
                    "failed to read FCM service account {}",
                    config.service_account
                )
            })?;
            let project = FcmProject::from_service_account_json(
                &json,
                config.project_id.clone(),
                config.aliases.clone(),
                config.api_url.clone(),
            )
            .with_context(|| format!("invalid FCM service account {}", config.service_account))?;
            info!(
                project_id = %project.project_id,
                aliases = ?project.aliases,
                client_email = %project.account.client_email,
                "FCM project configured"
            );
            projects.push(project);
        }
        Ok(Self::new(projects, http))
    }

    /// The project holding credentials for the device's `pn-param`. With a
    /// single project configured, it serves every FCM device.
    fn project_for(&self, params: &PushParams) -> Option<&FcmProject> {
        if let Some(param) = params.param.as_deref()
            && let Some(project) = self.projects.iter().find(|p| p.matches(param))
        {
            return Some(project);
        }
        match self.projects.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

#[async_trait]
impl PushProvider for FcmProvider {
    fn name(&self) -> &str {
        "fcm"
    }

    fn accepts(&self, params: &PushParams) -> bool {
        self.project_for(params).is_some()
    }

    async fn send(
        &self,
        params: &PushParams,
        notification: &PushNotification,
    ) -> Result<(), PushError> {
        let project = self
            .project_for(params)
            .ok_or_else(|| PushError::Unsupported(format!("fcm project {:?}", params.param)))?;
        let body = message(params, notification);
        let mut retried = false;
        loop {
            let token = project.access_token(&self.http, retried).await?;
            let response = self
                .http
                .post(project.send_url())
                .bearer_auth(token)
                .json(&body)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;
            let (status, text) = match response {
                Ok(response) if response.status().is_success() => {
                    debug!(project_id = %project.project_id, "FCM message accepted");
                    return Ok(());
                }
                Ok(response) => {
                    let status = response.status();
                    (status, response.text().await.unwrap_or_default())
                }
                Err(e) if !retried => {
                    debug!(error = %e, "FCM request failed, retrying");
                    retried = true;
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                Err(e) => return Err(PushError::Failed(e.to_string())),
            };
            match classify(status, &text) {
                Outcome::InvalidToken(reason) => return Err(PushError::InvalidPrid(reason)),
                Outcome::Retry if !retried => {
                    retried = true;
                    if status == reqwest::StatusCode::UNAUTHORIZED {
                        project.invalidate_token().await;
                    } else {
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
                Outcome::Retry | Outcome::Fail => {
                    return Err(PushError::Failed(format!(
                        "FCM {}: {}",
                        status,
                        text.trim()
                    )));
                }
            }
        }
    }
}

/// FCM v1 message: data only (no `notification` block, so Android always
/// hands it to the app), high priority so it is delivered in Doze.
fn message(params: &PushParams, notification: &PushNotification) -> serde_json::Value {
    let caller_uri = notification.caller_uri.clone().unwrap_or_default();
    let caller_name = notification.caller_name.clone().unwrap_or_default();
    let from = if caller_name.is_empty() {
        caller_uri.clone()
    } else {
        caller_name.clone()
    };
    let loc_key = match notification.kind {
        PushKind::Call => "IC_MSG",
        PushKind::BindingRefresh => "",
    };
    let ttl = notification.ttl.clamp(Duration::from_secs(1), MAX_TTL);
    json!({
        "message": {
            "token": params.prid,
            "android": {
                "priority": "HIGH",
                "ttl": format!("{}s", ttl.as_secs()),
            },
            "data": {
                "call-id": notification.call_id.clone().unwrap_or_default(),
                "from-uri": caller_uri,
                "display-name": caller_name,
                "sip-from": from,
                "loc-key": loc_key,
                "loc-args": from,
                "send-time": chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                "custom-payload": "{}",
            },
        }
    })
}

enum Outcome {
    InvalidToken(String),
    Retry,
    Fail,
}

#[derive(Deserialize, Default)]
struct ErrorBody {
    #[serde(default)]
    error: ErrorStatus,
}

#[derive(Deserialize, Default)]
struct ErrorStatus {
    #[serde(default)]
    message: String,
    #[serde(default)]
    details: Vec<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(rename = "errorCode")]
    error_code: Option<String>,
}

fn classify(status: reqwest::StatusCode, body: &str) -> Outcome {
    let parsed: ErrorBody = serde_json::from_str(body).unwrap_or_default();
    let unregistered = parsed
        .error
        .details
        .iter()
        .any(|d| d.error_code.as_deref() == Some("UNREGISTERED"));
    if unregistered || status == reqwest::StatusCode::NOT_FOUND {
        return Outcome::InvalidToken(parsed.error.message);
    }
    match status.as_u16() {
        401 | 429 | 500 | 502 | 503 | 504 => Outcome::Retry,
        _ => Outcome::Fail,
    }
}

/// A Firebase project and the service account used to send through it.
pub struct FcmProject {
    project_id: String,
    aliases: Vec<String>,
    api_url: String,
    account: ServiceAccount,
    token: tokio::sync::Mutex<Option<CachedToken>>,
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct ServiceAccountFile {
    project_id: Option<String>,
    private_key_id: Option<String>,
    private_key: String,
    client_email: String,
    token_uri: Option<String>,
}

struct ServiceAccount {
    client_email: String,
    private_key_id: Option<String>,
    key: RsaKeyPair,
    token_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

impl FcmProject {
    /// `project_id` overrides the service account's; `aliases` are extra
    /// `pn-param` values selecting this project.
    pub fn from_service_account_json(
        json: &str,
        project_id: Option<String>,
        aliases: Vec<String>,
        api_url: Option<String>,
    ) -> Result<Self> {
        let file: ServiceAccountFile =
            serde_json::from_str(json).context("not a service account key")?;
        let project_id = project_id
            .or(file.project_id)
            .ok_or_else(|| anyhow!("service account has no project_id; set project_id"))?;
        let der = pem_to_der(&file.private_key)?;
        let key = RsaKeyPair::from_pkcs8(&der)
            .or_else(|_| RsaKeyPair::from_der(&der))
            .map_err(|e| anyhow!("unsupported private key: {}", e))?;
        Ok(Self {
            project_id,
            aliases,
            api_url: api_url
                .unwrap_or_else(|| DEFAULT_API_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            account: ServiceAccount {
                client_email: file.client_email,
                private_key_id: file.private_key_id,
                key,
                token_uri: file
                    .token_uri
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string()),
            },
            token: tokio::sync::Mutex::new(None),
        })
    }

    fn matches(&self, param: &str) -> bool {
        self.project_id == param || self.aliases.iter().any(|alias| alias == param)
    }

    fn send_url(&self) -> String {
        format!(
            "{}/v1/projects/{}/messages:send",
            self.api_url, self.project_id
        )
    }

    async fn invalidate_token(&self) {
        *self.token.lock().await = None;
    }

    async fn access_token(&self, http: &reqwest::Client, force: bool) -> Result<String, PushError> {
        let mut cached = self.token.lock().await;
        if !force
            && let Some(token) = cached.as_ref()
            && token.expires_at > Instant::now() + TOKEN_REFRESH_MARGIN
        {
            return Ok(token.value.clone());
        }
        let assertion = self
            .account
            .assertion(super::registry::now_epoch_secs())
            .map_err(|e| PushError::Failed(e.to_string()))?;
        let response = http
            .post(&self.account.token_uri)
            .form(&[("grant_type", JWT_BEARER_GRANT), ("assertion", &assertion)])
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| PushError::Failed(format!("OAuth token request: {}", e)))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(PushError::Failed(format!(
                "OAuth token request: {} {}",
                status,
                body.trim()
            )));
        }
        let token: TokenResponse = response
            .json()
            .await
            .map_err(|e| PushError::Failed(format!("OAuth token response: {}", e)))?;
        let lifetime = Duration::from_secs(token.expires_in.unwrap_or(3600));
        *cached = Some(CachedToken {
            value: token.access_token.clone(),
            expires_at: Instant::now() + lifetime,
        });
        Ok(token.access_token)
    }
}

impl ServiceAccount {
    /// Signed JWT for the OAuth2 JWT-bearer grant (RFC 7523), valid one hour.
    fn assertion(&self, now: i64) -> Result<String> {
        let mut header = json!({ "alg": "RS256", "typ": "JWT" });
        if let Some(kid) = &self.private_key_id {
            header["kid"] = json!(kid);
        }
        let claims = json!({
            "iss": self.client_email,
            "scope": FCM_SCOPE,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + 3600,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| anyhow!("failed to sign service account assertion"))?;
        Ok(format!(
            "{}.{}",
            signing_input,
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    STANDARD
        .decode(body)
        .map_err(|e| anyhow!("invalid PEM private key: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Form, State},
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use parking_lot::Mutex;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use std::{collections::HashMap, sync::Arc};

    // Throwaway key generated for these tests only.
    const TEST_KEY: &str = include_str!("testdata/fcm_test_key.pem");

    #[derive(Default)]
    struct Mock {
        public_key: Vec<u8>,
        assertions: Mutex<Vec<String>>,
        messages: Mutex<Vec<serde_json::Value>>,
        token_counter: Mutex<u32>,
    }

    async fn token(
        State(mock): State<Arc<Mock>>,
        Form(form): Form<HashMap<String, String>>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some(JWT_BEARER_GRANT)
        );
        let assertion = form.get("assertion").cloned().unwrap_or_default();
        let (signing_input, signature) = assertion.rsplit_once('.').unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &mock.public_key)
            .verify(
                signing_input.as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .expect("assertion signed with the service account key");
        mock.assertions.lock().push(assertion);
        let mut counter = mock.token_counter.lock();
        *counter += 1;
        (
            StatusCode::OK,
            Json(json!({
                "access_token": format!("access-{}", *counter),
                "expires_in": 3599,
                "token_type": "Bearer",
            })),
        )
    }

    async fn send(
        State(mock): State<Arc<Mock>>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if auth == "Bearer access-1" && body["message"]["token"] == "expire-me" {
            // Pretend the first access token was revoked.
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"code": 401, "status": "UNAUTHENTICATED"}})),
            );
        }
        assert!(auth.starts_with("Bearer access-"), "authorized: {auth}");
        let token = body["message"]["token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        mock.messages.lock().push(body);
        if token == "uninstalled" {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": {
                    "code": 404,
                    "message": "Requested entity was not found.",
                    "status": "NOT_FOUND",
                    "details": [{
                        "@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                        "errorCode": "UNREGISTERED"
                    }]
                }})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({"name": "projects/demo-project/messages/1"})),
        )
    }

    async fn start_mock() -> (String, Arc<Mock>) {
        let der = pem_to_der(TEST_KEY).unwrap();
        let key = RsaKeyPair::from_pkcs8(&der).unwrap();
        let mock = Arc::new(Mock {
            public_key: key.public().as_ref().to_vec(),
            ..Default::default()
        });
        let app = Router::new()
            .route("/token", post(token))
            .route("/v1/projects/demo-project/messages:send", post(send))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, mock)
    }

    fn provider(base: &str) -> FcmProvider {
        let account = json!({
            "type": "service_account",
            "project_id": "demo-project",
            "private_key_id": "key-1",
            "private_key": TEST_KEY,
            "client_email": "pbx@demo-project.iam.gserviceaccount.com",
            "token_uri": format!("{base}/token"),
        });
        let project = FcmProject::from_service_account_json(
            &account.to_string(),
            None,
            vec!["123456789012".to_string()],
            Some(base.to_string()),
        )
        .unwrap();
        FcmProvider::new(vec![project], reqwest::Client::new())
    }

    fn params(prid: &str, param: Option<&str>) -> PushParams {
        PushParams {
            provider: "fcm".into(),
            prid: prid.into(),
            param: param.map(str::to_string),
        }
    }

    fn call() -> PushNotification {
        PushNotification {
            kind: PushKind::Call,
            call_id: Some("abc@pbx".into()),
            caller_uri: None,
            caller_name: None,
            ttl: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn sends_data_message_with_cached_token() {
        let (base, mock) = start_mock().await;
        let provider = provider(&base);

        provider
            .send(&params("device-token", Some("123456789012")), &call())
            .await
            .unwrap();
        provider
            .send(&params("device-token", Some("123456789012")), &call())
            .await
            .unwrap();

        assert_eq!(mock.assertions.lock().len(), 1, "access token is reused");
        let assertion = mock.assertions.lock()[0].clone();
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(assertion.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["iss"], "pbx@demo-project.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], FCM_SCOPE);
        assert_eq!(claims["aud"], format!("{base}/token"));

        let messages = mock.messages.lock();
        let message = &messages[0]["message"];
        assert_eq!(message["token"], "device-token");
        assert_eq!(message["android"]["priority"], "HIGH");
        assert_eq!(message["android"]["ttl"], "30s");
        assert_eq!(message["data"]["call-id"], "abc@pbx");
        assert_eq!(message["data"]["loc-key"], "IC_MSG");
        assert!(message.get("notification").is_none(), "data-only message");
    }

    #[tokio::test]
    async fn unregistered_token_is_reported() {
        let (base, _mock) = start_mock().await;
        let result = provider(&base)
            .send(&params("uninstalled", Some("demo-project")), &call())
            .await;
        assert!(
            matches!(result, Err(PushError::InvalidPrid(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn revoked_access_token_is_refreshed() {
        let (base, mock) = start_mock().await;
        provider(&base)
            .send(&params("expire-me", None), &call())
            .await
            .unwrap();
        assert_eq!(mock.assertions.lock().len(), 2);
        assert_eq!(mock.messages.lock().len(), 1);
    }

    #[tokio::test]
    async fn project_selection() {
        let (base, _mock) = start_mock().await;
        let single = provider(&base);
        assert!(
            single.accepts(&params("t", Some("other-project"))),
            "single project serves all"
        );

        let second = FcmProject::from_service_account_json(
            &json!({
                "project_id": "second",
                "private_key": TEST_KEY,
                "client_email": "pbx@second.iam.gserviceaccount.com",
            })
            .to_string(),
            None,
            vec![],
            None,
        )
        .unwrap();
        let mut both = provider(&base);
        both.projects.push(second);
        assert!(both.accepts(&params("t", Some("123456789012"))));
        assert!(both.accepts(&params("t", Some("second"))));
        assert!(!both.accepts(&params("t", Some("other-project"))));
        assert!(!both.accepts(&params("t", None)));
    }
}
