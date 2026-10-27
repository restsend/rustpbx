use crate::handler::middleware::clientaddr::ClientAddr;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Request, header::CONTENT_LENGTH},
    middleware::Next,
    response::Response,
};
use std::{sync::Arc, time::Instant};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::info;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format};
use tracing_subscriber::registry::LookupSpan;

#[derive(Clone)]
pub struct AccessLogEventFormat<T = SystemTime> {
    timer: T,
}

impl<T> AccessLogEventFormat<T>
where
    T: FormatTime,
{
    pub fn new(timer: T) -> Self {
        Self { timer }
    }
}

impl<T> Default for AccessLogEventFormat<T>
where
    T: FormatTime + Default,
{
    fn default() -> Self {
        Self {
            timer: T::default(),
        }
    }
}

#[derive(Default)]
struct AccessLogFields {
    method: Option<String>,
    status: Option<u16>,
    body_len: Option<String>,
    cost_ms: Option<f64>,
    uri: Option<String>,
    client_ip: Option<String>,
    request_id: Option<String>,
}

impl AccessLogFields {
    fn take_method(&self) -> &str {
        self.method.as_deref().unwrap_or("-")
    }

    fn take_status(&self) -> String {
        self.status
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    }

    fn take_body_len(&self) -> &str {
        self.body_len.as_deref().unwrap_or("-")
    }

    fn take_cost_ms(&self) -> String {
        self.cost_ms
            .map(|value| format!("{value:.3}ms"))
            .unwrap_or_else(|| "-".to_string())
    }

    fn take_uri(&self) -> &str {
        self.uri.as_deref().unwrap_or("-")
    }

    fn take_client_ip(&self) -> &str {
        self.client_ip.as_deref().unwrap_or("-")
    }
}

impl Visit for AccessLogFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "method" => self.method = Some(value.to_string()),
            "body_len" => self.body_len = Some(value.to_string()),
            "uri" => self.uri = Some(value.to_string()),
            "client_ip" => self.client_ip = Some(value.to_string()),
            "request_id" => self.request_id = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        match field.name() {
            "method" => self.method = Some(rendered.trim_matches('"').to_string()),
            "body_len" => self.body_len = Some(rendered.trim_matches('"').to_string()),
            "uri" => self.uri = Some(rendered.trim_matches('"').to_string()),
            "client_ip" => self.client_ip = Some(rendered.trim_matches('"').to_string()),
            "request_id" => self.request_id = Some(rendered.trim_matches('"').to_string()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "status" {
            self.status = Some(value as u16);
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if field.name() == "status" {
            self.status = Some(value as u16);
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "cost_ms" {
            self.cost_ms = Some(value);
        }
    }
}

impl<S, N, T> FormatEvent<S, N> for AccessLogEventFormat<T>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    T: FormatTime + Clone,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let metadata = event.metadata();

        if metadata.target() == "http.access" {
            let mut fields = AccessLogFields::default();
            event.record(&mut fields);
            self.timer.format_time(&mut writer)?;
            write!(writer, " ")?;
            writeln!(
                writer,
                "{} {} | {} | {} | {} | {} | {} | {} | request_id={}",
                metadata.level(),
                metadata.target(),
                fields.take_client_ip(),
                fields.take_method(),
                fields.take_status(),
                fields.take_body_len(),
                fields.take_cost_ms(),
                fields.take_uri(),
                fields.request_id.as_deref().unwrap_or("-")
            )?;
            Ok(())
        } else {
            let fallback = format::Format::default()
                .with_timer(self.timer.clone())
                .with_target(true)
                .with_source_location(false);
            fallback.format_event(ctx, writer, event)
        }
    }
}

fn should_skip_logging(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if let Some(prefix) = pattern.strip_suffix('*') {
            path.starts_with(prefix)
        } else {
            path == pattern
        }
    })
}

/// Query parameters whose values must never reach access logs.
const REDACTED_QUERY_PARAMS: &[&str] = &[
    "token",
    "access_token",
    "refresh_token",
    "code",
    "code_verifier",
    "code_challenge",
    "ticket",
    "secret",
    "client_secret",
    "state",
];

/// Mask sensitive query values (SSO/OAuth traffic) while keeping the rest of
/// the URI intact.
fn redact_query(uri: &str) -> String {
    let Some((path, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let redacted = query
        .split('&')
        .map(|pair| {
            if let Some((key, _)) = pair.split_once('=')
                && REDACTED_QUERY_PARAMS.contains(&key)
            {
                format!("{key}=<redacted>")
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    if redacted.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{redacted}")
    }
}

/// Logs basic request metadata once the downstream handler returns.
pub async fn log_requests(
    State(skip_paths): State<Arc<Vec<String>>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let started_at = Instant::now();
    // Accept only bounded hexadecimal IDs; arbitrary caller headers must not reach logs.
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let request_header = HeaderValue::from_str(&request_id)
        .expect("Validated hexadecimal request IDs are valid HTTP header values");
    req.headers_mut()
        .insert("x-request-id", request_header.clone());
    let method = req.method().clone();
    let uri = redact_query(req.uri().to_string().as_str());
    let request_path = req.uri().path().to_string();
    let connect_info = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0);
    let client_addr = ClientAddr::from_http_parts(req.uri(), req.headers(), connect_info);
    let client_ip = client_addr.ip().to_string();

    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert("x-request-id", request_header);

    let status = response.status();
    let body_len = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string());
    let cost_ms = started_at.elapsed().as_secs_f64() * 1_000.0;

    if !should_skip_logging(&request_path, skip_paths.as_slice()) {
        info!(
            target: "http.access",
            method = method.as_str(),
            status = status.as_u16(),
            body_len = body_len.as_str(),
            cost_ms = cost_ms,
            uri = uri.as_str(),
            client_ip = client_ip.as_str(),
            request_id = request_id.as_str(),
        );
    }

    response
}


#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, middleware, routing::get};
    use std::io::{self, Write};
    use tower::ServiceExt;

    #[derive(Clone)]
    struct LogBuffer(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn completed_request_correlates_response_and_access_log_without_untrusted_headers() {
        const REQUEST_ID: &str = "0123456789abcdef0123456789abcdef";
        for (supplied, skip) in [
            (Some(REQUEST_ID), false),
            (None, false),
            (Some("private-header-value"), false),
            (Some("0123456789ABCDEF0123456789ABCDEF"), false),
            (Some(REQUEST_ID), true),
        ] {
            let output = LogBuffer(Arc::new(parking_lot::Mutex::new(Vec::new())));
            let writer = output.clone();
            let interest_guard = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new());
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .event_format(AccessLogEventFormat::<SystemTime>::default())
                .with_writer(move || writer.clone())
                .finish();
            let _capture = tracing::subscriber::set_default(subscriber);
            let app = Router::new()
                .route("/resource", get(|| async { (StatusCode::ACCEPTED, "ok") }))
                .layer(middleware::from_fn_with_state(
                    Arc::new(if skip {
                        vec!["/resource".to_string()]
                    } else {
                        vec![]
                    }),
                    log_requests,
                ));
            let mut request = Request::builder().uri("/resource?token=private-query-value");
            if let Some(value) = supplied {
                request = request.header("x-request-id", value);
            }
            let response = app
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let request_id = response
                .headers()
                .get("x-request-id")
                .expect("Completed responses must identify their access log")
                .to_str()
                .unwrap();
            assert_eq!(request_id.len(), 32);
            assert!(
                request_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            if supplied == Some(REQUEST_ID) {
                assert_eq!(request_id, REQUEST_ID);
            }
            let text = String::from_utf8(output.0.lock().clone()).unwrap();
            if skip {
                assert!(text.is_empty(), "{text}");
            } else {
                assert_eq!(text.lines().count(), 1, "{text}");
                assert!(text.contains(&format!("request_id={request_id}")), "{text}");
                assert!(text.contains("| GET | 202 |"), "{text}");
                assert!(text.contains("/resource?token=<redacted>"), "{text}");
                assert!(
                    !text.contains("private-header-value") && !text.contains("private-query-value"),
                    "{text}"
                );
            }
            drop(interest_guard);
        }
    }
}
