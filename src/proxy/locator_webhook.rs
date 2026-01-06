use crate::call::Location;
use crate::config::LocatorWebhookConfig;
use crate::proxy::locator::{LocatorEvent, LocatorEventReceiver};
use serde::Serialize;
use tracing::{debug, error, warn};

#[derive(Serialize)]
pub struct LocationDto {
    pub aor: String,
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(
            config.timeout_ms.unwrap_or(5000),
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    debug!("locator webhook handler started for {}", config.url);

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

        if !config.events.is_empty() && !config.events.contains(&event_name.to_string()) {
            continue;
        }

        let mut request = client.post(&config.url);
        if let Some(headers) = &config.headers {
            for (k, v) in headers {
                request = request.header(k, v);
            }
        }

        match request.json(&dto).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!(
                        "locator webhook returned error status: {} for {}",
                        resp.status(),
                        config.url
                    );
                }
            }
            Err(e) => {
                error!("failed to send locator webhook to {}: {}", config.url, e);
            }
        }
    }
}
