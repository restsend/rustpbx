use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub mod upload;

pub use upload::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_CONTENT_TYPE, DEFAULT_FILE_FIELD,
    DEFAULT_REQUEST_TIMEOUT_MS, HttpUploadConfig, HttpUploader, SuccessRule, UploadRequest,
    UploadedObject, render,
};

const DEFAULT_HTTP_TCP_KEEPALIVE: Duration = Duration::from_secs(60);
const DEFAULT_HTTP_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST: usize = 8;

pub fn build_keepalive_client(
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
) -> Result<reqwest::Client> {
    build_client(keepalive_client_builder(timeout, connect_timeout))
}

/// Builds a keepalive client that returns redirects to its caller unchanged.
pub fn build_keepalive_client_without_redirect(
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
) -> Result<reqwest::Client> {
    build_client(
        keepalive_client_builder(timeout, connect_timeout)
            .redirect(reqwest::redirect::Policy::none()),
    )
}

fn keepalive_client_builder(
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder()
        .tcp_keepalive(DEFAULT_HTTP_TCP_KEEPALIVE)
        .pool_idle_timeout(DEFAULT_HTTP_POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(DEFAULT_HTTP_POOL_MAX_IDLE_PER_HOST);

    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }

    if let Some(connect_timeout) = connect_timeout {
        builder = builder.connect_timeout(connect_timeout);
    }

    builder
}

fn build_client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
    builder
        .build()
        .map_err(|e| anyhow!("Failed to build HTTP client: {}", e))
}

pub fn shared_keepalive_client() -> &'static reqwest::Client {
    static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP_CLIENT.get_or_init(|| {
        build_keepalive_client(None, None).unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Default timeout applied when a caller passes `None`. Call-path HTTP
/// requests (routing webhooks, provider hooks, …) must never hang forever on
/// a dead peer.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default)]
pub struct HttpFetchOptions {
    pub headers: HashMap<String, String>,
    /// Per-request timeout. `None` → [`DEFAULT_FETCH_TIMEOUT`].
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

/// Effective timeout: the caller's explicit value, or the default.
pub fn effective_timeout(timeout: Option<Duration>) -> Duration {
    timeout.unwrap_or(DEFAULT_FETCH_TIMEOUT)
}

/// Send a request and return the response once the headers arrive.
///
/// The timeout bounds the request through the response **headers only**;
/// reading the body afterwards is unbounded. Use the helpers below
/// ([`fetch_json`], [`fetch_text`], …) when the body read must be bounded
/// too.
pub async fn execute_request(
    mut request: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
    timeout: Option<Duration>,
) -> Result<reqwest::Response> {
    for (key, value) in headers {
        request = request.header(key, value);
    }

    let t = effective_timeout(timeout);
    let send_fut = request.send();
    let resp = tokio::time::timeout(t, send_fut)
        .await
        .map_err(|_| anyhow!("HTTP request timed out after {:?}", t))?
        .map_err(|e| anyhow!("HTTP request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(anyhow!("HTTP returned {}", resp.status()));
    }
    Ok(resp)
}

/// Read the response body within `timeout`. The body read (and the response
/// it consumes) must be passed as the `read` future.
pub async fn read_body_with_timeout<T, F>(timeout: Duration, read: F) -> Result<T>
where
    F: std::future::Future<Output = reqwest::Result<T>>,
{
    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| anyhow!("HTTP response body timed out after {:?}", timeout))?
        .map_err(|e| anyhow!("Failed to read response body: {}", e))
}

pub async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    options: &HttpFetchOptions,
) -> Result<serde_json::Value> {
    let t = effective_timeout(options.timeout);
    let req = client.get(url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    read_body_with_timeout(t, resp.json())
        .await
        .map_err(|e| anyhow!("Failed to parse JSON response: {}", e))
}

pub async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    options: &HttpFetchOptions,
) -> Result<serde_json::Value> {
    let t = effective_timeout(options.timeout);
    let req = client.post(url).json(body);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    read_body_with_timeout(t, resp.json())
        .await
        .map_err(|e| anyhow!("Failed to parse JSON response: {}", e))
}

pub async fn fetch_bytes(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    options: &HttpFetchOptions,
) -> Result<bytes::Bytes> {
    let t = effective_timeout(options.timeout);
    let req = client.request(method, url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    read_body_with_timeout(t, resp.bytes()).await
}

/// Download a body with a hard size cap so oversized responses can never
/// exhaust memory. The response headers are returned alongside the bytes so
/// callers can validate `Content-Type`.
///
/// The advertised `Content-Length` is checked up-front; when it is missing or
/// larger than the cap, the body is still streamed and aborted as soon as the
/// accumulated size exceeds `max_bytes`. Each chunk read is bounded by the
/// effective timeout, so a stalled peer aborts the transfer instead of
/// hanging it forever.
pub async fn fetch_audio_bytes(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    options: &HttpFetchOptions,
    max_bytes: u64,
) -> Result<(bytes::Bytes, reqwest::header::HeaderMap)> {
    let t = effective_timeout(options.timeout);
    let req = client.request(method, url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;

    if let Some(len) = resp.content_length()
        && len > max_bytes
    {
        return Err(anyhow!(
            "Remote file is too large (Content-Length: {} bytes, limit: {} bytes)",
            len,
            max_bytes
        ));
    }

    let headers = resp.headers().clone();
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = tokio::time::timeout(t, stream.next())
        .await
        .map_err(|_| anyhow!("HTTP response body timed out after {:?}", t))?
    {
        let chunk = chunk.map_err(|e| anyhow!("Failed to read response chunk: {}", e))?;
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(anyhow!(
                "Remote file is too large (limit: {} bytes)",
                max_bytes
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok((bytes::Bytes::from(body), headers))
}

pub async fn fetch_to_writer<W: tokio::io::AsyncWrite + Unpin>(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    options: &HttpFetchOptions,
    writer: &mut W,
) -> Result<u64> {
    let t = effective_timeout(options.timeout);
    let req = client.request(method, url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    let mut total = 0u64;
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = tokio::time::timeout(t, stream.next())
        .await
        .map_err(|_| anyhow!("HTTP response body timed out after {:?}", t))?
    {
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

pub async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    options: &HttpFetchOptions,
) -> Result<String> {
    let t = effective_timeout(options.timeout);
    let req = client.get(url);
    let resp = execute_request(req, &options.headers, options.timeout).await?;
    read_body_with_timeout(t, resp.text()).await
}
