use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver};
use rsipstack::dialog::dialog_layer::DialogLayer;
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct DialogStateReceiverGuard {
    pub(super) dialog_layer: Arc<DialogLayer>,
    pub(super) receiver: Option<DialogStateReceiver>,
    pub(super) dialog_id: Option<DialogId>,
}

impl DialogStateReceiverGuard {
    pub fn new(dialog_layer: Arc<DialogLayer>, receiver: DialogStateReceiver) -> Self {
        Self {
            dialog_layer,
            receiver: Some(receiver),
            dialog_id: None,
        }
    }
    pub async fn recv(&mut self) -> Option<DialogState> {
        let receiver = self.receiver.as_mut()?;
        let state = receiver.recv().await;
        if let Some(ref s) = state {
            self.dialog_id = Some(s.id().clone());
        }
        state
    }

    pub fn take_receiver(&mut self) -> Option<DialogStateReceiver> {
        self.receiver.take()
    }

    pub fn disarm(&mut self) {
        self.dialog_id = None;
    }

    pub fn set_dialog_id(&mut self, dialog_id: DialogId) {
        self.dialog_id = Some(dialog_id);
    }

    fn take_dialog(&mut self) -> Option<Dialog> {
        let id = match self.dialog_id.take() {
            Some(id) => id,
            None => return None,
        };

        match self.dialog_layer.get_dialog(&id) {
            Some(dialog) => {
                info!(%id, "dialog removed on  drop");
                self.dialog_layer.remove_dialog(&id);
                return Some(dialog);
            }
            _ => {}
        }
        None
    }

    pub async fn drop_async(&mut self) {
        if let Some(dialog) = self.take_dialog() {
            if let Err(e) = dialog.hangup().await {
                warn!(id=%dialog.id(), "error hanging up dialog on drop: {}", e);
            }
        }
    }
}

impl Drop for DialogStateReceiverGuard {
    fn drop(&mut self) {
        if let Some(dialog) = self.take_dialog() {
            crate::utils::spawn(async move {
                if let Err(e) = dialog.hangup().await {
                    warn!(id=%dialog.id(), "error hanging up dialog on drop: {}", e);
                }
            });
        }
    }
}

pub struct ServerDialogGuard {
    pub dialog_layer: Arc<DialogLayer>,
    pub id: DialogId,
}

impl ServerDialogGuard {
    pub fn new(dialog_layer: Arc<DialogLayer>, id: DialogId) -> Self {
        Self { dialog_layer, id }
    }
}

impl Drop for ServerDialogGuard {
    fn drop(&mut self) {
        let dlg = match self.dialog_layer.get_dialog(&self.id) {
            Some(dlg) => {
                debug!(%self.id, state = %dlg.state(), "server dialog removed on drop");
                self.dialog_layer.remove_dialog(&self.id);
                match dlg {
                    Dialog::ServerInvite(dlg) => dlg,
                    _ => return,
                }
            }
            _ => return,
        };
        let state = dlg.state();
        if state.is_terminated() {
            return;
        }

        crate::utils::spawn(async move {
            if state.can_cancel() {
                dlg.reject(None, None).ok();
            } else {
                dlg.bye().await.ok();
            }
        });
    }
}
