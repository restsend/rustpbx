use super::UserAgent;
use anyhow::{anyhow, Result};
use rsipstack::dialog::{
    client_dialog::ClientInviteDialog,
    dialog::{Dialog, DialogStateSender},
    invitation::InviteOption,
    server_dialog::ServerInviteDialog,
    DialogId,
};
use tracing::info;

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
                info!("dialog not found");
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
