use crate::call::{
    DialDirection, DialStrategy, Dialplan, Location, RouteInvite, SipUser, TransactionCookie,
};
use crate::config::HttpRouterConfig;
use crate::proxy::call::CallRouter;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsip::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub struct HttpCallRouter {
    pub config: HttpRouterConfig,
    pub client: reqwest::Client,
}

impl HttpCallRouter {
    pub fn new(config: HttpRouterConfig) -> Self {
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = config.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout));
        } else {
            builder = builder.timeout(Duration::from_secs(5));
        }
        Self {
            config,
            client: builder.build().unwrap_or_default(),
        }
    }
}

#[derive(Serialize)]
struct HttpRequestPayload {
    pub call_id: String,
    pub from: String,
    pub to: String,
    pub source_addr: Option<String>,
    pub direction: String,
    pub method: String,
    pub uri: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HttpRouteAction {
    Forward,
    Reject,
    Abort,
    NotHandled,
}

#[derive(Deserialize, Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HttpRouteStrategy {
    Sequential,
    Parallel,
}

#[derive(Deserialize)]
struct HttpResponsePayload {
    pub action: HttpRouteAction,
    pub targets: Option<Vec<String>>,
    pub strategy: Option<HttpRouteStrategy>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub record: Option<bool>,
    pub timeout: Option<u32>,
}

#[async_trait]
impl CallRouter for HttpCallRouter {
    async fn resolve(
        &self,
        original: &rsip::Request,
        _route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, (anyhow::Error, Option<rsip::StatusCode>)> {
        let direction = if cookie.is_from_trunk() {
            DialDirection::Inbound
        } else {
            DialDirection::Internal
        };

        let call_id = original
            .call_id_header()
            .map(|h| h.value().to_string())
            .unwrap_or_default();
        let from = original
            .from_header()
            .map(|h| h.value().to_string())
            .unwrap_or_default();
        let to = original
            .to_header()
            .map(|h| h.value().to_string())
            .unwrap_or_default();

        let mut headers = HashMap::new();
        for header in original.headers.iter() {
            let h_str = header.to_string();
            if let Some((name, value)) = h_str.split_once(':') {
                headers.insert(name.trim().to_string(), value.trim().to_string());
            }
        }

        let body = String::from_utf8_lossy(&original.body).to_string();

        let payload = HttpRequestPayload {
            call_id: call_id.clone(),
            from,
            to,
            source_addr: caller.destination.as_ref().map(|s| s.to_string()),
            direction: direction.to_string(),
            method: original.method.to_string(),
            uri: original.uri.to_string(),
            headers,
            body,
        };

        let mut request = self.client.post(&self.config.url).json(&payload);

        if let Some(config_headers) = &self.config.headers {
            for (k, v) in config_headers {
                request = request.header(k, v);
            }
        }

        let start = Instant::now();
        let response = request.send().await.map_err(|e| {
            let elapsed = start.elapsed();
            warn!(
                %call_id,
                from = %payload.from,
                to = %payload.to,
                elapsed_ms = elapsed.as_millis(),
                "HTTP router request failed: {}",
                e
            );
            (
                anyhow!("HTTP router request failed: {}", e),
                Some(rsip::StatusCode::ServiceUnavailable),
            )
        })?;

        let elapsed = start.elapsed();
        if !response.status().is_success() {
            if self.config.fallback_to_static {
                warn!(
                    %call_id,
                    from = %payload.from,
                    to = %payload.to,
                    elapsed_ms = elapsed.as_millis(),
                    status = %response.status(),
                    "HTTP router returned error, falling back to static"
                );
                return Err((anyhow!("HTTP router returned error"), None));
            }
            return Err((
                anyhow!("HTTP router returned error: {}", response.status()),
                Some(rsip::StatusCode::ServiceUnavailable),
            ));
        }

        let result: HttpResponsePayload = response.json().await.map_err(|e| {
            (
                anyhow!("Failed to parse HTTP router response: {}", e),
                Some(rsip::StatusCode::ServerInternalError),
            )
        })?;

        info!(
            %call_id,
            from = %payload.from,
            to = %payload.to,
            elapsed_ms = elapsed.as_millis(),
            action = ?result.action,
            "HTTP router resolved route"
        );

        let session_id = format!(
            "{}",
            rsipstack::dialog::DialogId::try_from(original)
                .map(|d| d.to_string())
                .unwrap_or_else(|_| call_id)
        );

        match result.action {
            HttpRouteAction::Reject | HttpRouteAction::Abort => {
                let status = result
                    .status
                    .and_then(|s| rsip::StatusCode::try_from(s).ok())
                    .unwrap_or(rsip::StatusCode::Forbidden);
                return Err((
                    anyhow!(
                        result
                            .reason
                            .unwrap_or_else(|| "rejected by HTTP router".to_string())
                    ),
                    Some(status),
                ));
            }
            HttpRouteAction::NotHandled => {
                return Err((anyhow!("not handled by HTTP router"), None));
            }
            HttpRouteAction::Forward => {
                let mut locs = Vec::new();
                if let Some(targets) = result.targets {
                    for target in targets {
                        if let Ok(uri) = rsip::Uri::try_from(target.clone()) {
                            locs.push(Location {
                                aor: uri,
                                ..Default::default()
                            });
                        }
                    }
                }

                let strategy = match result.strategy {
                    Some(HttpRouteStrategy::Parallel) => DialStrategy::Parallel(locs),
                    _ => DialStrategy::Sequential(locs),
                };

                let mut dialplan = Dialplan::new(session_id, original.clone(), direction);

                if let Some(from) = caller.from.as_ref() {
                    dialplan = dialplan.with_caller(from.clone());
                }

                dialplan = dialplan.with_targets(strategy);

                if let Some(record) = result.record {
                    dialplan.recording.enabled = record;
                }

                if let Some(timeout) = result.timeout {
                    dialplan.call_timeout = Duration::from_secs(timeout as u64);
                }

                Ok(dialplan)
            }
        }
    }
}
