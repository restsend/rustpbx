use crate::{call::RoutingState, useragent::invitation::InvitationHandler};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use rsip::prelude::{HasHeaders, HeadersExt};
use rsipstack::dialog::server_dialog::ServerInviteDialog;
use serde_json::json;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct WebhookInvitationHandler {
    urls: Vec<String>,
    method: Option<String>,
    headers: Option<Vec<(String, String)>>,
}

impl WebhookInvitationHandler {
    pub fn new(
        urls: Vec<String>,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
    ) -> Self {
        Self {
            urls,
            method,
            headers,
        }
    }
}

#[async_trait]
impl InvitationHandler for WebhookInvitationHandler {
    async fn on_invite(
        &self,
        dialog_id: String,
        _cancel_token: CancellationToken,
        dialog: ServerInviteDialog,
        routing_state: Arc<RoutingState>,
    ) -> Result<()> {
        let client = Client::new();
        let create_time = Utc::now().to_rfc3339();

        let invite_request = dialog.initial_request();
        let caller = invite_request.from_header()?.uri()?.to_string();
        let callee = invite_request.to_header()?.uri()?.to_string();
        let headers = invite_request
            .headers()
            .clone()
            .into_iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>();

        let payload = json!({
            "dialogId": dialog_id,
            "createdAt": create_time,
            "caller": caller,
            "callee": callee,
            "event": "invite",
            "headers": headers,
            "offer": String::from_utf8_lossy(invite_request.body()),
        });
        // TODO: better load balancing strategy
        // just use round-robin for now
        let idx = routing_state.next_round_robin_index("useragent_webhook", self.urls.len());
        let url = match self.urls.get(idx) {
            Some(u) => u,
            None => {
                return Err(anyhow::anyhow!("no webhook URL configured"));
            }
        };

        let method = self.method.as_deref().unwrap_or("POST");
        let mut request = client.request(reqwest::Method::from_bytes(method.as_bytes())?, url);

        if let Some(headers) = &self.headers {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }

        let start_time = Instant::now();
        match request.json(&payload).send().await {
            Ok(response) => {
                info!(
                    dialog_id,
                    url,
                    caller,
                    callee,
                    elapsed = start_time.elapsed().as_millis(),
                    status = ?response.status(),
                    "invite to webhook"
                );
                if !response.status().is_success() {
                    return Err(anyhow::anyhow!("failed to send invite to webhook"));
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to send invite to webhook: {}", e));
            }
        }
        Ok(())
    }
}
