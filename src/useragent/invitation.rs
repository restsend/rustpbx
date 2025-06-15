use crate::{config::InviteHandlerConfig, useragent::webhook::WebhookInvitationHandler};

use super::UserAgent;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rsipstack::dialog::{
    dialog::{Dialog, DialogStateReceiver, DialogStateSender},
    invitation::InviteOption,
    server_dialog::ServerInviteDialog,
    DialogId,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct PendingDialog {
    pub token: CancellationToken,
    pub dialog: ServerInviteDialog,
    pub state_receiver: DialogStateReceiver,
}

#[async_trait]

pub trait InvitationHandler: Send + Sync {
    async fn on_invite(
        &self,
        _cancel_token: CancellationToken,
        _dialog: ServerInviteDialog,
    ) -> Result<()> {
        return Err(anyhow!("invite not handled"));
    }
}

pub struct UnavailableInvitationHandler;
impl InvitationHandler for UnavailableInvitationHandler {}

impl UserAgent {
    pub async fn hangup(&self, dialog_id: DialogId) -> Result<()> {
        let dialog_id_str = dialog_id.to_string();
        if let Some(call) = self.pending_dialogs.lock().await.remove(&dialog_id_str) {
            call.dialog.reject().ok();
            call.token.cancel();
        }

        let dialog = self
            .dialog_layer
            .get_dialog(&dialog_id)
            .ok_or(anyhow!("dialog not found"))?;
        match dialog {
            Dialog::ClientInvite(dialog) => {
                dialog
                    .bye()
                    .await
                    .map_err(|e| anyhow!("failed to bye: {}", e))?;
            }
            Dialog::ServerInvite(dialog) => {
                dialog
                    .bye()
                    .await
                    .map_err(|e| anyhow!("failed to bye: {}", e))?;
            }
        }
        self.dialog_layer.remove_dialog(&dialog_id);
        Ok(())
    }

    pub async fn invite(
        &self,
        invite_option: InviteOption,
        state_sender: DialogStateSender,
    ) -> Result<(DialogId, Option<Vec<u8>>)> {
        let (dialog, resp) = self
            .dialog_layer
            .do_invite(invite_option, state_sender)
            .await
            .map_err(|e| anyhow!("{}", e))?;

        let offer = match resp {
            Some(resp) => {
                info!("invite response: {}", resp);
                match resp.status_code.kind() {
                    rsip::StatusCodeKind::Successful => {
                        let offer = resp.body.clone();
                        Some(offer)
                    }
                    _ => return Err(anyhow!("{}", resp.status_code)),
                }
            }
            None => return Err(anyhow!("no response received")),
        };
        Ok((dialog.id(), offer))
    }
}

pub fn create_invite_handler(config: &InviteHandlerConfig) -> Option<Box<dyn InvitationHandler>> {
    match config {
        InviteHandlerConfig::Webhook {
            url,
            method,
            headers,
        } => Some(Box::new(WebhookInvitationHandler::new(
            url.clone(),
            method.clone(),
            headers.clone(),
        ))),
    }
}
