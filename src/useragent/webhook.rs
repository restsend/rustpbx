use crate::useragent::invitation::InvitationHandler;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use reqwest::Client;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::server_dialog::ServerInviteDialog;
use serde_json::json;
use std::time::Instant;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct WebhookInvitationHandler {
    url: String,
    method: Option<String>,
    headers: Option<Vec<(String, String)>>,
}

impl WebhookInvitationHandler {
    pub fn new(
        url: String,
        method: Option<String>,
        headers: Option<Vec<(String, String)>>,
    ) -> Self {
        Self {
            url,
            method,
            headers,
        }
    }
}

#[async_trait]
impl InvitationHandler for WebhookInvitationHandler {
    async fn on_invite(
        &self,
        cancel_token: CancellationToken,
        dialog: ServerInviteDialog,
    ) -> Result<()> {
        let client = Client::new();
        let dialog_id = dialog.id().to_string();
        let create_time = Utc::now().to_rfc3339();

        let invite_request = dialog.initial_request();
        let caller = invite_request.from_header()?.uri()?.to_string();
        let callee = invite_request.to_header()?.uri()?.to_string();

        let payload = json!({
            "dialog_id": dialog_id,
            "created_at": create_time,
            "caller": caller,
            "callee": callee,
            "event": "invite",
            "offer": String::from_utf8_lossy(&invite_request.body()),
        });

        let method = self.method.as_deref().unwrap_or("POST");
        let mut request =
            client.request(reqwest::Method::from_bytes(method.as_bytes())?, &self.url);

        if let Some(headers) = &self.headers {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }
        let url = self.url.clone();
        tokio::spawn(async move {
            let start_time = Instant::now();
            select! {
                    _ = cancel_token.cancelled() => {
                            info!(
                                dialog_id,
                                url,
                                caller,
                                callee,
                                "invite to webhook cancelled");
                            return Ok(());
                    }
                    _ = async {
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
                            info!(
                                dialog_id,
                                url,
                                caller,
                                callee,
                                "failed to send invite to webhook: {}",
                                e
                            );
                            return Err(anyhow::anyhow!("failed to send invite to webhook: {}", e));
                        }
                    };
                    Ok::<(), anyhow::Error>(())
                } => {}
            }
            Ok::<(), anyhow::Error>(())
        });
        Ok(())
    }
}
