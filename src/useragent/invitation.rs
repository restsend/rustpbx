use super::UserAgent;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rsipstack::dialog::{
    client_dialog::ClientInviteDialog,
    dialog::{Dialog, DialogStateSender, TerminatedReason},
    invitation::InviteOption,
    server_dialog::ServerInviteDialog,
    DialogId,
};
use tracing::info;

#[async_trait]

pub trait InvitationHandler: Send + Sync {
    async fn on_invite(&self, _dialog: ServerInviteDialog) -> Result<()> {
        return Err(anyhow!("invite not handled"));
    }
    async fn on_confirmed(&self, _dialog: ServerInviteDialog) -> Result<()> {
        Ok(())
    }
    async fn on_terminate(
        &self,
        _dialog: ServerInviteDialog,
        _reason: TerminatedReason,
    ) -> Result<()> {
        Ok(())
    }
    async fn on_early_media(
        &self,
        _dialog: ServerInviteDialog,
        _resp: rsip::Response,
    ) -> Result<()> {
        Ok(())
    }
    async fn on_update(&self, _dialog: ServerInviteDialog, _req: rsip::Request) -> Result<()> {
        Ok(())
    }
    async fn on_info(&self, _dialog: ServerInviteDialog, _req: rsip::Request) -> Result<()> {
        Ok(())
    }
    async fn on_options(&self, _dialog: ServerInviteDialog, _req: rsip::Request) -> Result<()> {
        Ok(())
    }
}

pub struct UnavailableInvitationHandler;
impl InvitationHandler for UnavailableInvitationHandler {}

impl UserAgent {
    pub(super) async fn handle_server_invite(&self, dialog: ServerInviteDialog) -> Result<()> {
        self.dialogs
            .lock()
            .await
            .insert(dialog.id(), Dialog::ServerInvite(dialog));
        Ok(())
    }

    pub(super) async fn handle_client_invite(&self, dialog: ClientInviteDialog) -> Result<()> {
        self.dialogs
            .lock()
            .await
            .insert(dialog.id(), Dialog::ClientInvite(dialog));
        Ok(())
    }

    pub async fn hangup(&self, dialog_id: DialogId) -> Result<()> {
        let dialog = match self.dialogs.lock().await.remove(&dialog_id) {
            Some(dialog) => dialog,
            None => {
                return Err(anyhow!("dialog not found"));
            }
        };
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

        let dialog_id = dialog.id();
        match self.handle_client_invite(dialog).await {
            Ok(_) => Ok((dialog_id, offer)),
            Err(e) => {
                info!("error handling client invite: {:?}", e);
                Err(e)
            }
        }
    }
}
