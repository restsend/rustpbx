//! Shared HTTP utility for common request patterns (headers, timeout, status check).
//!
//! Consolidates the duplicated `build → headers → timeout → send → status → parse` skeleton
//! used across IVR API fetches, TTS drivers, webhooks, etc.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Options for an HTTP fetch request.
#[derive(Debug, Clone, Default)]
pub struct HttpFetchOptions {
    /// Extra headers to add to the request.
    pub headers: HashMap<String, String>,
    /// Request timeout. `None` = use client default (no explicit timeout).
    pub timeout: Option<Duration>,
}

impl HttpFetchOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }
}

/// Apply headers, send a `RequestBuilder` with optional timeout, and check status.
///
/// This is the shared inner call used by all convenience wrappers below.
pub async fn execute_request(
    mut request: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
    timeout: Option<Duration>,
) -> Result<reqwest::Response> {
    for (key, value) in headers {
        request = request.header(key, value);
    }

    let send_fut = request.send();
    let resp = if let Some(t) = timeout {
        tokio::time::timeout(t, send_fut)
            .await
            .map_err(|_| anyhow!("HTTP request timed out after {:?}", t))?
    } else {
        send_fut.await
    }
    .map_err(|e| anyhow!("HTTP request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(anyhow!("HTTP returned {}", resp.status()));
    }
    Ok(resp)
}

/// Fetch JSON from a URL via GET.
pub async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    options: &HttpFetchOptions,
) -> Result<serde_json::Value> {
    let req = client.get(url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    resp.json()
        .await
        .map_err(|e| anyhow!("Failed to parse JSON response: {}", e))
}

/// Fetch JSON from a URL via POST with a JSON body.
pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    options: &HttpFetchOptions,
) -> Result<serde_json::Value> {
    let req = client.post(url).json(body);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    resp.json()
        .await
        .map_err(|e| anyhow!("Failed to parse JSON response: {}", e))
}

/// Fetch raw bytes from a URL with configurable method.
pub async fn fetch_bytes(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    options: &HttpFetchOptions,
) -> Result<bytes::Bytes> {
    let req = client.request(method, url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    resp.bytes()
        .await
        .map_err(|e| anyhow!("Failed to read response body: {}", e))
}

/// Fetch raw bytes from a URL and stream the response body into a writer.
pub async fn fetch_to_writer<W: tokio::io::AsyncWrite + Unpin>(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    options: &HttpFetchOptions,
    writer: &mut W,
) -> Result<u64> {
    let req = client.request(method, url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    let mut total = 0u64;
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("Failed to read response chunk: {}", e))?;
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| anyhow!("Failed to write chunk: {}", e))?;
        total += chunk.len() as u64;
    }
    writer
        .flush()
        .await
        .map_err(|e| anyhow!("Failed to flush writer: {}", e))?;
    Ok(total)
}

/// Fetch text from a URL via GET.
pub async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    options: &HttpFetchOptions,
) -> Result<String> {
    let req = client.get(url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    resp.text()
        .await
        .map_err(|e| anyhow!("Failed to read response text: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_fetch_options_builder() {
        let opts = HttpFetchOptions::new()
            .with_timeout(Duration::from_secs(10))
            .with_header("Authorization", "Bearer token");
        assert_eq!(opts.timeout, Some(Duration::from_secs(10)));
        assert_eq!(
            opts.headers.get("Authorization"),
            Some(&"Bearer token".to_string())
        );
    }

    #[test]
    fn test_http_fetch_options_default() {
        let opts = HttpFetchOptions::new();
        assert!(opts.timeout.is_none());
        assert!(opts.headers.is_empty());
    }
}
