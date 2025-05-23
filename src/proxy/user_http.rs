use super::user::{SipUser, UserBackend};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{Client, Method};
use std::{collections::HashMap, time::Instant};
use tracing::info;
use urlencoding;

pub struct HttpUserBackend {
    url: String,
    method: Method,
    username_field: String,
    realm_field: String,
    headers: HashMap<String, String>,
    client: Client,
}

impl HttpUserBackend {
    pub fn new(
        url: &str,
        method: &Option<String>,
        username_field: &Option<String>,
        realm_field: &Option<String>,
        headers: &Option<HashMap<String, String>>,
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

        HttpUserBackend {
            url: url.to_string(),
            method,
            username_field,
            realm_field,
            headers,
            client: Client::new(),
        }
    }
}

#[async_trait]
impl UserBackend for HttpUserBackend {
    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let start_time = Instant::now();
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

                self.client.get(url)
            }
            Method::POST => {
                let mut form = HashMap::new();
                form.insert(self.username_field.clone(), username);
                form.insert(self.realm_field.clone(), realm.unwrap_or(""));

                self.client.post(&self.url).form(&form)
            }
            _ => return Err(anyhow!("Unsupported HTTP method")),
        };

        for (key, value) in &self.headers {
            request_builder = request_builder.header(key, value);
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| anyhow!("HTTP request error: {}", e))?;

        info!(
            username,
            realm,
            "auth request took {:?} status {}",
            start_time.elapsed(),
            response.status()
        );

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "HTTP response error: {}",
                response.status()
            ));
        }

        response
            .json::<SipUser>()
            .await
            .map_err(|e| anyhow!("HTTP response error: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_http_backend_creation() {
        let backend = HttpUserBackend::new(
            "http://example.com/auth",
            &Some("POST".to_string()),
            &Some("username".to_string()),
            &None,
            &None,
        );

        assert_eq!(backend.url, "http://example.com/auth");
        assert_eq!(backend.username_field, "username");
        assert!(backend.headers.is_empty());

        // Test with custom headers
        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "RustPBX-Test".to_string());

        let backend = HttpUserBackend::new(
            "http://example.com/auth",
            &None,
            &None,
            &None,
            &Some(headers.clone()),
        );

        assert_eq!(backend.url, "http://example.com/auth");
        assert_eq!(backend.username_field, "username");
        assert_eq!(backend.headers, headers);
    }
}
