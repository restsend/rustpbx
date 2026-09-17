//! Generic, configuration-driven HTTP upload scheme.
//!
//! An [`HttpUploader`] performs a single multipart (or text) POST/PUT of a
//! byte payload to a configurable endpoint and resolves the resulting object
//! URL either by concatenating the request URL (`url` may contain `{key}`) or
//! by extracting it from the JSON response (`response_url_path`).
//!
//! It is intentionally generic: field names, file name, content type, extra
//! form fields, headers, timeouts and response handling are all data, so
//! arbitrary third-party upload APIs can be described without code changes.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Default multipart field carrying the binary payload.
pub const DEFAULT_FILE_FIELD: &str = "file";
/// Default MIME type for the binary payload.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
/// Default total request timeout (mirrors the historical recording upload).
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 10_000;
/// Default connect timeout (mirrors the historical recording upload).
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 3_000;

/// Configures how a payload is uploaded to an HTTP endpoint.
///
/// All fields except `url` are optional. `file_field` and `body_field` are
/// mutually exclusive: when `body_field` is set the payload is sent as a text
/// form field (typically JSON), otherwise it is sent as a binary file part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpUploadConfig {
    /// Endpoint. May contain the `{key}` placeholder, which is replaced with
    /// the object key; otherwise the URL is used verbatim and the resulting
    /// URL is taken from the response (see `response_url_path`).
    pub url: String,
    /// HTTP method. Defaults to `POST`.
    #[serde(default)]
    pub method: Option<String>,
    /// Extra request headers (values support the same `{...}` placeholders).
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// Multipart field name carrying the binary payload. Defaults to `file`.
    /// Mutually exclusive with `body_field`.
    #[serde(default)]
    pub file_field: Option<String>,
    /// Multipart field name carrying the payload as text. Mutually exclusive
    /// with `file_field`.
    #[serde(default)]
    pub body_field: Option<String>,
    /// File name reported in the multipart `Content-Disposition`. Defaults to
    /// the object key. Supports `{key}`/`{filename}`/custom placeholders.
    #[serde(default)]
    pub file_name: Option<String>,
    /// MIME type of the binary payload. Defaults to `application/octet-stream`.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Extra text form fields. Values support `{...}` placeholders.
    #[serde(default)]
    pub fields: Option<HashMap<String, String>>,
    /// Dot-separated path into the JSON response holding the object URL, e.g.
    /// `data.url`. When unset, a response body starting with `http` is used,
    /// otherwise the request URL is returned.
    #[serde(default)]
    pub response_url_path: Option<String>,
    /// Optional business-level success rule evaluated against the JSON
    /// response (e.g. `code == 0`). When unset, any 2xx response is success.
    #[serde(default)]
    pub response_success: Option<SuccessRule>,
    /// TCP connect timeout in milliseconds. Defaults to 3000.
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
    /// Total request timeout (connect + send + read) in milliseconds.
    /// Defaults to 10000.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
}

/// Business-level success predicate: the JSON value at `path` must equal
/// `equals`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuccessRule {
    /// Dot-separated path into the JSON response, e.g. `code`/`data.status`.
    pub path: String,
    /// Expected value.
    pub equals: serde_json::Value,
}

impl Default for HttpUploadConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: None,
            headers: None,
            file_field: None,
            body_field: None,
            file_name: None,
            content_type: None,
            fields: None,
            response_url_path: None,
            response_success: None,
            connect_timeout_ms: None,
            request_timeout_ms: None,
        }
    }
}

impl HttpUploadConfig {
    /// Whether this config sends the payload as a binary file part.
    pub fn uses_file_field(&self) -> bool {
        self.body_field.as_deref().unwrap_or("").is_empty()
    }

    /// Effective HTTP method (uppercased), defaulting to `POST`.
    pub fn effective_method(&self) -> &str {
        self.method.as_deref().unwrap_or("POST")
    }

    /// Render the endpoint URL for `key` (replaces `{key}`).
    pub fn build_url(&self, key: &str) -> String {
        render(&self.url, &HashMap::from([("key".to_string(), key.to_string())]))
    }

    fn effective_file_field(&self) -> String {
        self.file_field
            .clone()
            .unwrap_or_else(|| DEFAULT_FILE_FIELD.to_string())
    }

    fn effective_content_type(&self) -> String {
        self.content_type
            .clone()
            .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string())
    }

    fn effective_request_timeout(&self) -> Duration {
        Duration::from_millis(
            self.request_timeout_ms
                .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        )
    }

    fn effective_connect_timeout(&self) -> Duration {
        Duration::from_millis(
            self.connect_timeout_ms
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        )
    }
}

/// A single upload request.
#[derive(Debug, Clone, Default)]
pub struct UploadRequest {
    /// Object key used for the `{key}` URL placeholder and default file name.
    pub key: String,
    /// File name sent in the multipart part. Defaults to `key`.
    pub file_name: Option<String>,
    /// Per-request MIME override (falls back to the config default).
    pub content_type: Option<String>,
    /// Per-request binary field override (falls back to the config default).
    pub file_field: Option<String>,
    /// Per-request text field override (falls back to the config default).
    pub body_field: Option<String>,
    /// Additional template variables available as `{name}` in `url`,
    /// `file_name`, `fields` and header values.
    pub vars: HashMap<String, String>,
    /// Payload bytes.
    pub bytes: Bytes,
}

impl UploadRequest {
    pub fn new(key: impl Into<String>, bytes: Bytes) -> Self {
        Self {
            key: key.into(),
            bytes,
            ..Default::default()
        }
    }

    pub fn with_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    pub fn with_file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = Some(file_name.into());
        self
    }

    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    pub fn with_file_field(mut self, field: impl Into<String>) -> Self {
        self.file_field = Some(field.into());
        self
    }

    pub fn with_body_field(mut self, field: impl Into<String>) -> Self {
        self.body_field = Some(field.into());
        self
    }
}

/// Result of a successful upload.
#[derive(Debug, Clone, Default)]
pub struct UploadedObject {
    /// Resolved object URL: response extraction, `http`-looking body, or the
    /// final request URL.
    pub url: Option<String>,
    /// Parsed JSON response body, when available.
    pub body: Option<serde_json::Value>,
}

/// Uploads payloads according to an [`HttpUploadConfig`].
#[derive(Clone)]
pub struct HttpUploader {
    config: HttpUploadConfig,
    client: reqwest::Client,
}

impl HttpUploader {
    /// Build an uploader, validating the config and constructing a client with
    /// the configured timeouts.
    pub fn new(config: HttpUploadConfig) -> Result<Self> {
        if config.url.trim().is_empty() {
            bail!("http upload url is required");
        }
        let has_file = config
            .file_field
            .as_deref()
            .map(|f| !f.is_empty())
            .unwrap_or(false);
        let has_body = config
            .body_field
            .as_deref()
            .map(|f| !f.is_empty())
            .unwrap_or(false);
        if has_file && has_body {
            bail!("http upload: file_field and body_field are mutually exclusive");
        }
        let client = crate::build_keepalive_client(
            Some(config.effective_request_timeout()),
            Some(config.effective_connect_timeout()),
        )?;
        Ok(Self { config, client })
    }

    pub fn config(&self) -> &HttpUploadConfig {
        &self.config
    }

    /// Upload `req.bytes` and resolve the resulting object URL.
    pub async fn upload_bytes(&self, req: &UploadRequest) -> Result<UploadedObject> {
        let mut vars = req.vars.clone();
        vars.insert("key".to_string(), req.key.clone());
        vars.insert(
            "filename".to_string(),
            req.file_name.clone().unwrap_or_else(|| req.key.clone()),
        );

        let url = render(&self.config.url, &vars);
        let method = reqwest::Method::from_bytes(self.config.effective_method().as_bytes())
            .with_context(|| {
                format!(
                    "invalid http method {}",
                    self.config.effective_method()
                )
            })?;

        let mut form = Form::new();
        let body_field = req
            .body_field
            .as_deref()
            .filter(|field| !field.is_empty())
            .or_else(|| self.config.body_field.as_deref().filter(|field| !field.is_empty()));
        let file_field = req
            .file_field
            .as_deref()
            .filter(|field| !field.is_empty())
            .or_else(|| self.config.file_field.as_deref().filter(|field| !field.is_empty()));
        if body_field.is_some() && file_field.is_some() {
            bail!("http upload: file_field and body_field are mutually exclusive");
        }
        if let Some(body_field) = body_field {
            form = form.text(
                body_field.to_string(),
                String::from_utf8_lossy(&req.bytes).to_string(),
            );
        } else {
            let file_name = req
                .file_name
                .clone()
                .unwrap_or_else(|| req.key.clone());
            let content_type = req
                .content_type
                .clone()
                .unwrap_or_else(|| self.config.effective_content_type());
            let part = Part::bytes(req.bytes.to_vec())
                .file_name(render(&file_name, &vars))
                .mime_str(&content_type)
                .with_context(|| format!("invalid content type {content_type}"))?;
            let field = file_field
                .map(str::to_string)
                .unwrap_or_else(|| self.config.effective_file_field());
            form = form.part(field, part);
        }

        if let Some(fields) = self.config.fields.as_ref() {
            for (name, value) in fields {
                form = form.text(name.clone(), render(value, &vars));
            }
        }

        let mut request = self.client.request(method, &url).multipart(form);
        if let Some(headers) = self.config.headers.as_ref() {
            for (name, value) in headers {
                request = request.header(name.as_str(), render(value, &vars));
            }
        }

        let response = request.send().await.with_context(|| {
            format!("http upload request failed for {}", req.key)
        })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!("http upload failed: {} - {}", status, text.trim());
        }

        let mut body: Option<serde_json::Value> = None;
        let needs_json =
            self.config.response_url_path.is_some() || self.config.response_success.is_some();
        if needs_json {
            body = Some(
                serde_json::from_str(&text)
                    .with_context(|| "http upload: failed to parse JSON response")?,
            );
        } else if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
            body = Some(parsed);
        }

        if let Some(rule) = self.config.response_success.as_ref() {
            let actual = body
                .as_ref()
                .and_then(|value| json_path(value, &rule.path));
            if actual != Some(&rule.equals) {
                bail!(
                    "http upload reported failure: {} != {}",
                    actual
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "missing".to_string()),
                    rule.equals
                );
            }
        }

        let resolved = match self.config.response_url_path.as_deref() {
            Some(path) => body
                .as_ref()
                .and_then(|value| json_path(value, path))
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .with_context(|| {
                    format!(
                        "http upload: response_url_path '{}' missing in response",
                        path
                    )
                })?,
            None => {
                let trimmed = text.trim();
                if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                    trimmed.to_string()
                } else {
                    url.clone()
                }
            }
        };

        Ok(UploadedObject {
            url: Some(resolved),
            body,
        })
    }
}

/// Replace `{name}` placeholders using `vars`. Unknown placeholders are left
/// untouched.
pub fn render(template: &str, vars: &HashMap<String, String>) -> String {
    if !template.contains('{') {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = template[i + 1..].find('}')
        {
            let name = &template[i + 1..i + 1 + end];
            if let Some(value) = vars.get(name) {
                out.push_str(value);
                i += end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Walk a dot-separated path into a JSON value (`data.url`, `code`, ...).
fn json_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        current = match current {
            serde_json::Value::Object(map) => map.get(segment)?,
            serde_json::Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Minimal single-shot HTTP server. Returns the raw request bytes captured
    /// and a canned response string.
    async fn spawn_mock_server(response: &'static str) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/upload", addr);
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break buf.len();
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let content_length = headers
                .split("\r\n")
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let total = header_end + content_length;
            while buf.len() < total {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.ok();
            buf
        });
        (url, handle)
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn request_body(raw: &[u8]) -> String {
        let pos = find_subslice(raw, b"\r\n\r\n").expect("headers terminator");
        String::from_utf8_lossy(&raw[pos + 4..]).to_string()
    }

    fn ok_response(body: &'static str) -> &'static str {
        // Static leak keeps the helper signature simple in tests.
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        )
    }

    #[tokio::test]
    async fn uploads_multipart_with_configured_field_and_name() {
        let response = ok_response(r#"{"data":{"url":"https://cdn.example.com/a.wav"}}"#);
        let (url, handle) = spawn_mock_server(response).await;

        let cfg = HttpUploadConfig {
            url: format!("{url}/{{key}}"),
            file_field: Some("filecontent".to_string()),
            file_name: Some("{key}".to_string()),
            content_type: Some("application/octet-stream".to_string()),
            response_url_path: Some("data.url".to_string()),
            ..Default::default()
        };
        let uploader = HttpUploader::new(cfg).unwrap();
        let result = uploader
            .upload_bytes(&UploadRequest::new("gift/a.wav", Bytes::from_static(b"hello")))
            .await
            .unwrap();

        assert_eq!(
            result.url.as_deref(),
            Some("https://cdn.example.com/a.wav")
        );

        let raw = handle.await.unwrap();
        let head = String::from_utf8_lossy(&raw);
        assert!(head.starts_with("POST /upload/gift/a.wav HTTP/1.1"));
        let body = request_body(&raw);
        assert!(body.contains("name=\"filecontent\""));
        assert!(body.contains("filename=\"gift/a.wav\""));
        assert!(body.contains("hello"));
    }

    #[tokio::test]
    async fn uploads_body_field_as_text_and_applies_fields_and_headers() {
        let response = ok_response("ok");
        let (url, handle) = spawn_mock_server(response).await;

        let cfg = HttpUploadConfig {
            url: url.clone(),
            body_field: Some("calllog.json".to_string()),
            fields: Some(HashMap::from([(
                "call_id".to_string(),
                "{call_id}".to_string(),
            )])),
            headers: Some(HashMap::from([(
                "Authorization".to_string(),
                "Bearer {call_id}".to_string(),
            )])),
            ..Default::default()
        };
        let uploader = HttpUploader::new(cfg).unwrap();
        let result = uploader
            .upload_bytes(
                &UploadRequest::new("cdr/1.json", Bytes::from_static(b"{\"a\":1}"))
                    .with_var("call_id", "abc"),
            )
            .await
            .unwrap();

        assert_eq!(result.url.as_deref(), Some(url.as_str()));

        let raw = handle.await.unwrap();
        let head = String::from_utf8_lossy(&raw);
        assert!(head.contains("authorization: Bearer abc") || head.contains("Authorization: Bearer abc"));
        let body = request_body(&raw);
        assert!(body.contains("name=\"calllog.json\""));
        assert!(body.contains("{\"a\":1}"));
        assert!(body.contains("name=\"call_id\""));
        assert!(body.contains("abc"));
    }

    #[tokio::test]
    async fn uses_request_url_when_response_has_no_url() {
        let response = ok_response("done");
        let (url, _handle) = spawn_mock_server(response).await;
        let cfg = HttpUploadConfig {
            url: format!("{url}/{{key}}"),
            ..Default::default()
        };
        let uploader = HttpUploader::new(cfg).unwrap();
        let result = uploader
            .upload_bytes(&UploadRequest::new("x/y.wav", Bytes::from_static(b"z")))
            .await
            .unwrap();
        assert_eq!(result.url.as_deref(), Some(format!("{url}/x/y.wav").as_str()));
    }

    #[tokio::test]
    async fn fails_when_success_rule_not_met() {
        let response = ok_response(r#"{"code":7}"#);
        let (url, _handle) = spawn_mock_server(response).await;
        let cfg = HttpUploadConfig {
            url,
            response_success: Some(SuccessRule {
                path: "code".to_string(),
                equals: serde_json::json!(0),
            }),
            response_url_path: Some("data.url".to_string()),
            ..Default::default()
        };
        let uploader = HttpUploader::new(cfg).unwrap();
        let err = uploader
            .upload_bytes(&UploadRequest::new("k", Bytes::from_static(b"z")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reported failure"));
    }

    #[tokio::test]
    async fn fails_on_non_2xx() {
        let response: &'static str =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\n\r\nbad";
        let (url, _handle) = spawn_mock_server(response).await;
        let cfg = HttpUploadConfig {
            url,
            ..Default::default()
        };
        let uploader = HttpUploader::new(cfg).unwrap();
        let err = uploader
            .upload_bytes(&UploadRequest::new("k", Bytes::from_static(b"z")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn rejects_both_file_and_body_field() {
        let cfg = HttpUploadConfig {
            url: "http://x".to_string(),
            file_field: Some("f".to_string()),
            body_field: Some("b".to_string()),
            ..Default::default()
        };
        assert!(HttpUploader::new(cfg).is_err());
    }

    #[test]
    fn renders_placeholders_and_leaves_unknown() {
        let vars = HashMap::from([("key".to_string(), "a".to_string())]);
        assert_eq!(render("{key}/{other}", &vars), "a/{other}");
    }
}
