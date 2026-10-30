use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

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
        return Err(anyhow::Error::new(HttpStatusError {
            status: resp.status(),
        }));
    }
    Ok(resp)
}

/// Structured non-2xx response error.
///
/// `Display` intentionally matches the historical plain-anyhow text
/// (`"HTTP returned <status>"`) so existing string-based consumers keep
/// working; the typed payload lets callers distinguish deterministic 4xx
/// responses (never worth retrying) from transient failures.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: reqwest::StatusCode,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP returned {}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

impl HttpStatusError {
    /// Deterministic client-side rejections: retrying cannot change the
    /// outcome and only burns the caller's time. 408 (request timeout) and
    /// 429 (too many requests) are the conventional retryable exceptions.
    pub fn is_client_error(&self) -> bool {
        let code = self.status.as_u16();
        (400..500).contains(&code) && code != 408 && code != 429
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Display text must stay byte-identical to the historical
    /// plain-anyhow message: production code matches on the
    /// `"HTTP returned"` prefix (see proxy/routing/http.rs).
    #[test]
    fn status_error_display_matches_legacy_text() {
        let e = HttpStatusError {
            status: reqwest::StatusCode::CONFLICT,
        };
        assert_eq!(e.to_string(), "HTTP returned 409 Conflict");
    }

    #[test]
    fn client_error_classification() {
        let is_client = |code: u16| HttpStatusError {
            status: reqwest::StatusCode::from_u16(code).unwrap(),
        }
        .is_client_error();
        // Deterministic rejections: fail fast, never retry.
        assert!(is_client(400));
        assert!(is_client(409));
        assert!(is_client(410));
        assert!(is_client(422));
        // Conventional retryable exceptions.
        assert!(!is_client(408));
        assert!(!is_client(429));
        // Server-side / non-4xx: handled by the generic retry path.
        assert!(!is_client(500));
        assert!(!is_client(503));
    }

    /// `execute_request` surfaces non-2xx as the typed error, downcast-able
    /// through anyhow without changing the Display contract.
    #[tokio::test]
    async fn execute_request_downcasts_status_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            // Read the request headers (body is empty for this POST).
            let _ = socket.read(&mut buf).await;
            let body = "{\"error\":\"conflict\"}";
            let resp = format!(
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(resp.as_bytes()).await.unwrap();
            socket.flush().await.ok();
        });

        let client = build_keepalive_client(Some(Duration::from_secs(2)), None).unwrap();
        let req = client.post(format!("http://{addr}/step"));
        let err = execute_request(req, &HashMap::new(), Some(Duration::from_secs(2)))
            .await
            .unwrap_err();
        let status_err = err
            .downcast_ref::<HttpStatusError>()
            .expect("error should downcast to HttpStatusError");
        assert_eq!(status_err.status, reqwest::StatusCode::CONFLICT);
        assert!(status_err.is_client_error());
    }
}
