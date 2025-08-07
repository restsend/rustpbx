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

pub struct UnavailableInvitationHandler;
impl InvitationHandler for UnavailableInvitationHandler {}

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
