use crate::call::cookie::SpamResult;
use crate::call::{
    DialDirection, DialStrategy, Dialplan, Location, RouteInvite, SipUser, TransactionCookie,
    TrunkContext,
};
use crate::config::{HttpRouterConfig, MediaProxyMode, RtpConfig};
use crate::proxy::call::{CallRouter, RouteError};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::sip::prelude::*;
use rsipstack::transport::SipConnection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub struct HttpCallRouter {
    pub config: HttpRouterConfig,
    pub rtp_config: RtpConfig,
    pub default_media_proxy_mode: MediaProxyMode,
    pub client: reqwest::Client,
}

impl HttpCallRouter {
    pub fn new(
        config: HttpRouterConfig,
        rtp_config: RtpConfig,
        default_media_proxy_mode: MediaProxyMode,
    ) -> Self {
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = config.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout));
        } else {
            builder = builder.timeout(Duration::from_secs(5));
        }
        Self {
            config,
            rtp_config,
            default_media_proxy_mode,
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
    Spam,
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
    /// Maximum call duration (in seconds)
    pub timeout: Option<u32>,
    /// Max ring time for call setup/ringback phase (in seconds)
    pub max_ring_time: Option<u32>,
    pub media_proxy: Option<MediaProxyMode>,
    pub headers: Option<HashMap<String, String>>,
    pub with_original_headers: Option<bool>,
    pub extensions: Option<HashMap<String, String>>,
}

#[async_trait]
impl CallRouter for HttpCallRouter {
    async fn resolve(
        &self,
        original: &rsipstack::sip::Request,
        _route_invite: Box<dyn RouteInvite>,
        caller: &SipUser,
        cookie: &TransactionCookie,
    ) -> Result<Dialplan, RouteError> {
        let direction = if cookie.get_extension::<TrunkContext>().is_some() {
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
        let source_addr = if let Ok(via) = original.via_header() {
            if let Ok((_, target)) = SipConnection::parse_target_from_via(via) {
                Some(target.to_string())
            } else {
                None
            }
        } else {
            None
        };

        let payload = HttpRequestPayload {
            call_id: call_id.clone(),
            from,
            to,
            source_addr,
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
            RouteError {
                error: anyhow!("HTTP router request failed: {}", e),
                status: Some(rsipstack::sip::StatusCode::ServiceUnavailable),
                extensions: None,
            }
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
                return Err(RouteError {
                    error: anyhow!("HTTP router returned error"),
                    status: None,
                    extensions: None,
                });
            }
            return Err(RouteError {
                error: anyhow!("HTTP router returned error: {}", response.status()),
                status: Some(rsipstack::sip::StatusCode::ServiceUnavailable),
                extensions: None,
            });
        }

        let result: HttpResponsePayload = response.json().await.map_err(|e| RouteError {
            error: anyhow!("Failed to parse HTTP router response: {}", e),
            status: Some(rsipstack::sip::StatusCode::ServerInternalError),
            extensions: None,
        })?;

        info!(
            %call_id,
            from = %payload.from,
            to = %payload.to,
            elapsed_ms = elapsed.as_millis(),
            action = ?result.action,
            "HTTP router resolved route"
        );

        match result.action {
            HttpRouteAction::Spam => {
                cookie.mark_as_spam(SpamResult::Spam);
                return Err(RouteError {
                    error: anyhow!(
                        result
                            .reason
                            .unwrap_or_else(|| "marked as spam by HTTP router".to_string())
                    ),
                    status: Some(rsipstack::sip::StatusCode::Forbidden),
                    extensions: result.extensions,
                });
            }
            HttpRouteAction::Reject | HttpRouteAction::Abort => {
                let status = result
                    .status
                    .map(rsipstack::sip::StatusCode::from)
                    .unwrap_or(rsipstack::sip::StatusCode::Forbidden);
                return Err(RouteError {
                    error: anyhow!(
                        result
                            .reason
                            .unwrap_or_else(|| "rejected by HTTP router".to_string())
                    ),
                    status: Some(status),
                    extensions: result.extensions,
                });
            }
            HttpRouteAction::NotHandled => {
                return Err(RouteError {
                    error: anyhow!("not handled by HTTP router"),
                    status: None,
                    extensions: result.extensions,
                });
            }
            HttpRouteAction::Forward => {
                let mut locs = Vec::new();
                let custom_headers = result.headers.map(|h| {
                    h.iter()
                        .map(|(k, v)| rsipstack::sip::Header::Other(k.clone(), v.clone()))
                        .collect::<Vec<_>>()
                });

                if let Some(targets) = result.targets {
                    for target in targets {
                        if let Ok(uri) = rsipstack::sip::Uri::try_from(target.clone()) {
                            locs.push(Location {
                                aor: uri,
                                headers: custom_headers.clone(),
                                ..Default::default()
                            });
                        }
                    }
                }

                let strategy = match result.strategy {
                    Some(HttpRouteStrategy::Parallel) => DialStrategy::Parallel(locs),
                    _ => DialStrategy::Sequential(locs),
                };

                let mut dialplan = Dialplan::new(call_id, original.clone(), direction);

                // Start from server defaults, then let HTTP router override individual fields.
                dialplan.media.proxy_mode = self.default_media_proxy_mode;
                dialplan.media.external_ip = self.rtp_config.external_ip.clone();
                dialplan.media.rtp_start_port = self.rtp_config.start_port;
                dialplan.media.rtp_end_port = self.rtp_config.end_port;
                dialplan.media.webrtc_port_start = self.rtp_config.webrtc_start_port;
                dialplan.media.webrtc_port_end = self.rtp_config.webrtc_end_port;
                dialplan.media.ice_servers = self.rtp_config.ice_servers.clone();

                if let Some(from) = caller.from.as_ref() {
                    dialplan = dialplan.with_caller(from.clone());
                }

                dialplan = dialplan.with_targets(strategy);

                if let Some(record) = result.record {
                    dialplan.recording.enabled = record;
                }

                if let Some(mode) = result.media_proxy {
                    dialplan.media.proxy_mode = mode;
                }

                if let Some(with_orig) = result.with_original_headers {
                    dialplan.with_original_headers = with_orig;
                }

                if let Some(exts) = result.extensions {
                    dialplan = dialplan.with_extension(exts);
                }

                if let Some(timeout) = result.timeout {
                    dialplan.max_call_duration = Some(Duration::from_secs(timeout as u64));
                }

                if let Some(max_ring_time) = result.max_ring_time {
                    dialplan.max_ring_time = Duration::from_secs(max_ring_time as u64);
                }

                Ok(dialplan)
            }
        }
    }
}
