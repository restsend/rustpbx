use std::sync::Arc;

use crate::{
    call::{RoutingState, sip::Invitation},
    config::InviteHandlerConfig,
    useragent::webhook::WebhookInvitationHandler,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::dialog::{
    dialog::{Dialog, DialogStateReceiver},
    server_dialog::ServerInviteDialog,
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct PendingDialog {
    pub token: CancellationToken,
    pub dialog: ServerInviteDialog,
    pub state_receiver: DialogStateReceiver,
}
pub(super) struct PendingDialogGuard {
    pub id: String,
    pub invitation: Invitation,
}

impl PendingDialogGuard {
    pub(super) fn new(invitation: Invitation, id: String, pending_dialog: PendingDialog) -> Self {
        invitation
            .pending_dialogs
            .lock()
            .map(|mut ps| ps.insert(id.clone(), pending_dialog))
            .ok();
        info!(%id, "added pending dialog");
        Self { id, invitation }
    }

    fn take_dialog(&self) -> Option<Dialog> {
        if let Some(pending) = self.invitation.get_pending_call(&self.id) {
            let dialog_id = pending.dialog.id();
            match self.invitation.dialog_layer.get_dialog(&dialog_id) {
                Some(dialog) => {
                    self.invitation.dialog_layer.remove_dialog(&dialog_id);
                    return Some(dialog);
                }
                None => {}
            }
        }
        None
    }
    pub(super) async fn drop_async(&self) {
        if let Some(dialog) = self.take_dialog() {
            dialog.hangup().await.ok();
        }
    }
}

impl Drop for PendingDialogGuard {
    fn drop(&mut self) {
        if let Some(dialog) = self.take_dialog() {
            info!(%self.id, "removing pending dialog on drop");

            tokio::spawn(async move {
                dialog.hangup().await.ok();
            });
        }
    }
}

#[async_trait]
pub trait InvitationHandler: Send + Sync {
    async fn on_invite(
        &self,
        _session_id: String,
        _cancel_token: CancellationToken,
        _dialog: ServerInviteDialog,
        _routing_state: Arc<RoutingState>,
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
            urls,
            method,
            headers,
        }) => {
            let all_urls = if let Some(urls) = urls {
                urls.clone()
            } else if let Some(url) = url {
                vec![url.clone()]
            } else {
                vec![]
            };
            Some(Box::new(WebhookInvitationHandler::new(
                all_urls,
                method.clone(),
                headers.clone(),
            )))
        }
        _ => None,
    }
}

pub type FnCreateInvitationHandler =
    fn(config: Option<&InviteHandlerConfig>) -> Result<Box<dyn InvitationHandler>>;
