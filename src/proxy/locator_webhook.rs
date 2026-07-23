use crate::call::Location;
use crate::config::LocatorWebhookConfig;
use crate::proxy::locator::{LocatorEvent, LocatorEventReceiver};
use serde::Serialize;
use tracing::{debug, warn};

struct LocatorWebhookSender {
    url: String,
    headers: std::collections::HashMap<String, String>,
    allowed_events: Vec<String>,
    client: reqwest::Client,
}

impl LocatorWebhookSender {
    fn new(config: LocatorWebhookConfig) -> Self {
        let timeout = std::time::Duration::from_millis(config.timeout_ms.unwrap_or(5000));
        Self {
            url: config.url.trim().to_string(),
            headers: config.headers.unwrap_or_default(),
            allowed_events: config.events,
            client: crate::http_util::build_keepalive_client(Some(timeout), None)
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    fn accepts_event(&self, event_name: &str) -> bool {
        self.allowed_events.is_empty() || self.allowed_events.iter().any(|e| e == event_name)
    }

    async fn send_payload(&self, payload: &LocatorEventDto) -> Result<(), anyhow::Error> {
        let req = self.client.post(&self.url).json(payload);
        crate::http_util::execute_request(req, &self.headers, None).await?;
        Ok(())
    }
}

#[derive(Serialize)]
pub struct LocationDto {
    pub aor: String,
    pub home_proxy: Option<String>,
    pub expires: u32,
    pub destination: Option<String>,
    pub supports_webrtc: bool,
    pub transport: Option<String>,
    pub user_agent: Option<String>,
}

impl From<&Location> for LocationDto {
    fn from(loc: &Location) -> Self {
        Self {
            aor: loc.aor.to_string(),
            home_proxy: loc.home_proxy.as_ref().map(|proxy| proxy.to_string()),
            expires: loc.expires,
            destination: loc.destination.as_ref().map(|d| d.to_string()),
            supports_webrtc: loc.supports_webrtc,
            transport: loc.transport.map(|t| t.to_string()),
            user_agent: loc.user_agent.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct LocatorEventDto {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<LocationDto>>,
    pub timestamp: u64,
}

pub async fn handle_locator_webhook(config: LocatorWebhookConfig, mut rx: LocatorEventReceiver) {
    let sender = LocatorWebhookSender::new(config);
    debug!("locator webhook handler started for {}", sender.url);

    loop {
        let event = match rx.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("locator webhook lagged, missed {} events", n);
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        };

        let (event_name, dto) = match event {
            LocatorEvent::Registered(loc) => (
                "registered",
                LocatorEventDto {
                    event: "registered".to_string(),
                    location: Some(LocationDto::from(&loc)),
                    locations: None,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                },
            ),
            LocatorEvent::Unregistered(loc) => (
                "unregistered",
                LocatorEventDto {
                    event: "unregistered".to_string(),
                    location: Some(LocationDto::from(&loc)),
                    locations: None,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                },
            ),
            LocatorEvent::Offline(locs) => (
                "offline",
                LocatorEventDto {
                    event: "offline".to_string(),
                    location: None,
                    locations: Some(locs.iter().map(LocationDto::from).collect()),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                },
            ),
        };

        if !sender.accepts_event(event_name) {
            continue;
        }

        if let Err(e) = sender.send_payload(&dto).await {
            warn!("locator webhook send failed for {}: {}", sender.url, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_dto_includes_home_proxy() {
        let home_proxy_uri: rsipstack::sip::Uri = "sip:pbx.example.com:5060"
            .try_into()
            .expect("valid home proxy URI");
        let location = Location {
            aor: "sip:device@192.0.2.10"
                .try_into()
                .expect("valid contact URI"),
            home_proxy: Some(
                rsipstack::transport::SipAddr::try_from(home_proxy_uri)
                    .expect("valid home proxy address"),
            ),
            ..Default::default()
        };
        let expected_home_proxy = location
            .home_proxy
            .as_ref()
            .map(ToString::to_string)
            .expect("home proxy");

        let payload = serde_json::to_value(LocationDto::from(&location))
            .expect("serialize locator webhook location");

        assert_eq!(payload["home_proxy"], expected_home_proxy);
    }
}
