use super::user::UserBackend;
use crate::{call::user::SipUser, proxy::auth::AuthError};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Method};
use serde::Deserialize;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tracing::{info, warn};
use urlencoding;

#[derive(Deserialize)]
struct HttpAuthResponsePayload {
    pub reason: String,
    pub message: Option<String>,
}

pub struct HttpUserBackend {
    url: String,
    method: Method,
    username_field: String,
    realm_field: String,
    request_uri_field: String,
    headers: HashMap<String, String>,
    sip_headers: Vec<String>,
    token_header: Option<String>,
    client: Client,
    retry_count: u32,
    retry_delay: Duration,
}

impl HttpUserBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: &str,
        method: &Option<String>,
        username_field: &Option<String>,
        realm_field: &Option<String>,
        request_uri_field: &Option<String>,
        headers: &Option<HashMap<String, String>>,
        sip_headers: &Option<Vec<String>>,
        token_header: &Option<String>,
        http_timeout_ms: &Option<u64>,
        http_retry_count: &Option<u32>,
        http_retry_delay_ms: &Option<u64>,
    ) -> Self {
        let method = method
            .as_ref()
            .map(|m| match m.to_uppercase().as_str() {
                "POST" => Method::POST,
                _ => Method::GET,
            })
            .unwrap_or(Method::GET);

        let username_field = username_field
            .as_ref()
            .map_or_else(|| "username".to_string(), |s| s.clone());
        let realm_field = realm_field
            .as_ref()
            .map_or_else(|| "realm".to_string(), |s| s.clone());
        let request_uri_field = request_uri_field
            .as_ref()
            .map_or_else(|| "request_uri".to_string(), |s| s.clone());

        let headers = headers.clone().unwrap_or_default();
        let sip_headers = sip_headers.clone().unwrap_or_default();

        let timeout_ms = http_timeout_ms.unwrap_or(5000);
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .unwrap_or_else(|_| Client::new());

        let retry_count = http_retry_count.unwrap_or(1);
        let retry_delay = Duration::from_millis(http_retry_delay_ms.unwrap_or(500));

        HttpUserBackend {
            url: url.to_string(),
            method,
            username_field,
            realm_field,
            request_uri_field,
            headers,
            sip_headers,
            token_header: token_header.clone(),
            client,
            retry_count,
            retry_delay,
        }
    }

    pub fn token_header(&self) -> Option<&str> {
        self.token_header.as_deref()
    }

    /// Send the HTTP auth request with retry logic.
    async fn send_with_retry(
        &self,
        username: &str,
        realm: Option<&str>,
        request: Option<&rsipstack::sip::Request>,
    ) -> Result<reqwest::Response, AuthError> {
        let request_uri = request.map(|req| req.uri.to_string()).unwrap_or_default();
        let sip_params = if let Some(req) = request {
            let mut params: HashMap<String, String> = HashMap::new();
            for header_name in &self.sip_headers {
                for header in req.headers.iter() {
                    let s = header.to_string();
                    if let Some(pos) = s.find(':') {
                        let name = s[..pos].trim();
                        if name.eq_ignore_ascii_case(header_name) {
                            params.insert(header_name.clone(), s[pos + 1..].trim().to_string());
                            break;
                        }
                    }
                }
            }
            params
        } else {
            HashMap::new()
        };

        let mut last_err = AuthError::Other(anyhow!("no attempts made"));
        for attempt in 0..=self.retry_count {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay).await;
                info!(attempt, "retrying HTTP auth request");
            }

            let mut request_builder = match self.method {
                Method::GET => {
                    let mut url = self.url.clone();
                    if !url.contains('?') {
                        url.push('?');
                    } else if !url.ends_with('?') && !url.ends_with('&') {
                        url.push('&');
                    }

                    url.push_str(&format!(
                        "{}={}&{}={}&{}={}",
                        self.username_field,
                        urlencoding::encode(username),
                        self.realm_field,
                        urlencoding::encode(realm.unwrap_or("")),
                        self.request_uri_field,
                        urlencoding::encode(&request_uri),
                    ));

                    for (key, value) in &sip_params {
                        url.push_str(&format!(
                            "&{}={}",
                            urlencoding::encode(key),
                            urlencoding::encode(value)
                        ));
                    }

                    self.client.get(url)
                }
                Method::POST => {
                    let mut form = HashMap::new();
                    form.insert(self.username_field.clone(), username.to_string());
                    form.insert(self.realm_field.clone(), realm.unwrap_or("").to_string());
                    form.insert(self.request_uri_field.clone(), request_uri.clone());
                    for (key, value) in &sip_params {
                        form.insert(key.clone(), value.clone());
                    }

                    self.client.post(&self.url).form(&form)
                }
                _ => return Err(AuthError::Other(anyhow!("Unsupported HTTP method"))),
            };

            for (key, value) in &self.headers {
                request_builder = request_builder.header(key, value);
            }

            match request_builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_server_error() && attempt < self.retry_count {
                        warn!(attempt, %status, "HTTP auth server error, will retry");
                        continue;
                    }
                    return Ok(response);
                }
                Err(e) => {
                    warn!(attempt, error = %e, "HTTP auth request failed");
                    last_err = AuthError::Other(anyhow!("HTTP request error: {}", e));
                }
            }
        }
        Err(last_err)
    }

    /// Core method: fetch user from HTTP service. Used by both
    /// `UserBackend::get_user` (Digest path) and the token-auth wrapper.
    pub async fn fetch_user(
        &self,
        username: &str,
        realm: Option<&str>,
        request: Option<&rsipstack::sip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        let start_time = Instant::now();

        let response = self.send_with_retry(username, realm, request).await?;

        info!(
            username,
            realm,
            "auth request took {:?} status {}",
            start_time.elapsed(),
            response.status()
        );

        if !response.status().is_success() {
            let status = response.status();
            let result = response.json::<HttpAuthResponsePayload>().await;
            if let Ok(payload) = result {
                match payload.reason.as_str() {
                    "not_found" | "not_user" => return Err(AuthError::NotFound),
                    "invalid_password" | "invalid_credentials" => {
                        return Err(AuthError::InvalidCredentials);
                    }
                    "disabled" | "blocked" => return Err(AuthError::Disabled),
                    "spam" | "spam_detected" => return Err(AuthError::SpamDetected),
                    "payment_required" => return Err(AuthError::PaymentRequired),
                    _ => {
                        return Err(AuthError::Other(anyhow!(
                            "HTTP auth error: {}",
                            payload.message.unwrap_or("Unknown error".to_string())
                        )));
                    }
                }
            }
            return Err(AuthError::Other(anyhow::anyhow!(
                "HTTP response error: {}",
                status
            )));
        }

        response
            .json::<SipUser>()
            .await
            .map_err(|e| AuthError::Other(anyhow!("HTTP response error: {}", e)))
            .map(Some)
    }
}

#[async_trait]
impl UserBackend for HttpUserBackend {
    async fn is_same_realm(&self, _realm: &str) -> bool {
        false
    }
    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        request: Option<&rsipstack::sip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        self.fetch_user(username, realm, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_http_backend_creation() {
        let backend = HttpUserBackend::new(
            "http://rustpbx.com/auth",
            &Some("POST".to_string()),
            &Some("username".to_string()),
            &None,
            &None,
            &None,
            &None,
            &None,
            &None,
            &None,
            &None,
        );

        assert_eq!(backend.url, "http://rustpbx.com/auth");
        assert_eq!(backend.username_field, "username");
        assert_eq!(backend.request_uri_field, "request_uri");
        assert!(backend.headers.is_empty());

        // Test with custom headers
        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "RustPBX-Test".to_string());

        let backend = HttpUserBackend::new(
            "http://rustpbx.com/auth",
            &None,
            &None,
            &None,
            &None,
            &Some(headers.clone()),
            &None,
            &None,
            &None,
            &None,
            &None,
        );

        assert_eq!(backend.url, "http://rustpbx.com/auth");
        assert_eq!(backend.username_field, "username");
        assert_eq!(backend.headers, headers);
    }
}
