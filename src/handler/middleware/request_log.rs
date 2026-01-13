use crate::handler::middleware::clientaddr::ClientAddr;
use axum::{
    body::Body,
    extract::State,
    http::{Request, header::CONTENT_LENGTH},
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
            write!(
                writer,
                "{} {} | {} | {} | {} | {} | {} | {}\n",
                metadata.level(),
                metadata.target(),
                fields.take_client_ip(),
                fields.take_method(),
                fields.take_status(),
                fields.take_body_len(),
                fields.take_cost_ms(),
                fields.take_uri()
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

/// Logs basic request metadata once the downstream handler returns.
pub async fn log_requests(
    State(skip_paths): State<Arc<Vec<String>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let started_at = Instant::now();
    let method = req.method().clone();
    let uri = req.uri().to_string();
    let request_path = req.uri().path().to_string();
    let connect_info = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0);
    let client_addr = ClientAddr::from_http_parts(req.uri(), req.headers(), connect_info);
    let client_ip = client_addr.ip().to_string();

    let response = next.run(req).await;

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
        );
    }

    response
}
