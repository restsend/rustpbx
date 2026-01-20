use super::user::UserBackend;
use crate::{call::user::SipUser, proxy::auth::AuthError};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Method};
use serde::Deserialize;
use std::{collections::HashMap, time::Instant};
use tracing::info;
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
    headers: HashMap<String, String>,
    sip_headers: Vec<String>,
    client: Client,
}

impl HttpUserBackend {
    pub fn new(
        url: &str,
        method: &Option<String>,
        username_field: &Option<String>,
        realm_field: &Option<String>,
        headers: &Option<HashMap<String, String>>,
        sip_headers: &Option<Vec<String>>,
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

        let headers = headers.clone().unwrap_or_default();
        let sip_headers = sip_headers.clone().unwrap_or_default();

        HttpUserBackend {
            url: url.to_string(),
            method,
            username_field,
            realm_field,
            headers,
            sip_headers,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl UserBackend for HttpUserBackend {
    async fn is_same_realm(&self, realm: &str) -> bool {
        self.get_user("", Some(realm), None).await.is_ok()
    }
    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        let start_time = Instant::now();
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

        let mut request_builder = match self.method {
            Method::GET => {
                let mut url = self.url.clone();
                if !url.contains('?') {
                    url.push('?');
                } else if !url.ends_with('?') && !url.ends_with('&') {
                    url.push('&');
                }

                url.push_str(&format!(
                    "{}={}&{}={}",
                    self.username_field,
                    urlencoding::encode(username),
                    self.realm_field,
                    urlencoding::encode(realm.unwrap_or("")),
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

        let response = request_builder
            .send()
            .await
            .map_err(|e| AuthError::Other(anyhow!("HTTP request error: {}", e)))?;

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
            .map(|user| Some(user))
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
        );

        assert_eq!(backend.url, "http://rustpbx.com/auth");
        assert_eq!(backend.username_field, "username");
        assert!(backend.headers.is_empty());

        // Test with custom headers
        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "RustPBX-Test".to_string());

        let backend = HttpUserBackend::new(
            "http://rustpbx.com/auth",
            &None,
            &None,
            &None,
            &Some(headers.clone()),
            &None,
        );

        assert_eq!(backend.url, "http://rustpbx.com/auth");
        assert_eq!(backend.username_field, "username");
        assert_eq!(backend.headers, headers);
    }
}
