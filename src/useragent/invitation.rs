use crate::{config::InviteHandlerConfig, useragent::webhook::WebhookInvitationHandler};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::dialog::{dialog::DialogStateReceiver, server_dialog::ServerInviteDialog};
use tokio_util::sync::CancellationToken;

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

pub fn default_create_invite_handler(
    config: Option<&InviteHandlerConfig>,
) -> Option<Box<dyn InvitationHandler>> {
    match config {
        Some(InviteHandlerConfig::Webhook {
            url,
            method,
            headers,
        }) => Some(Box::new(WebhookInvitationHandler::new(
            url.clone(),
            method.clone(),
            headers.clone(),
        ))),
        _ => None,
    }
}

pub type FnCreateInvitationHandler =
    fn(config: Option<&InviteHandlerConfig>) -> Result<Box<dyn InvitationHandler>>;
