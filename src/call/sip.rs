use crate::TrackId;
use crate::call::active_call::ActiveCallStateRef;
use crate::callrecord::CallRecordHangupReason;
use crate::event::EventSender;
use crate::media::stream::MediaStream;
use crate::useragent::invitation::PendingDialog;
use anyhow::Result;
use chrono::Utc;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{
    DialogState, DialogStateReceiver, DialogStateSender, TerminatedReason,
};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::rsip_ext::RsipResponseExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct DialogStateReceiverGuard {
    pub(super) dialog_layer: Arc<DialogLayer>,
    pub(super) receiver: DialogStateReceiver,
    pub(super) dialog_id: Option<DialogId>,
}

impl DialogStateReceiverGuard {
    pub fn new(dialog_layer: Arc<DialogLayer>, receiver: DialogStateReceiver) -> Self {
        Self {
            dialog_layer,
            receiver,
            dialog_id: None,
        }
    }
    pub async fn recv(&mut self) -> Option<DialogState> {
        let state = self.receiver.recv().await;
        if let Some(ref s) = state {
            self.dialog_id = Some(s.id().clone());
        }
        state
    }
}

impl Drop for DialogStateReceiverGuard {
    fn drop(&mut self) {
        let id = match self.dialog_id.take() {
            Some(id) => id,
            None => return,
        };

        match self.dialog_layer.get_dialog(&id) {
            Some(dialog) => {
                info!(%id, "dialog removed on state receiver drop");
                self.dialog_layer.remove_dialog(&id);
                tokio::spawn(async move {
                    if let Err(e) = dialog.hangup().await {
                        warn!(%id, "error hanging up dialog on drop: {}", e);
                    }
                });
            }
            None => {}
        }
    }
}

#[derive(Clone)]
pub struct Invitation {
    pub dialog_layer: Arc<DialogLayer>,
    pub pending_dialogs: Arc<Mutex<HashMap<String, PendingDialog>>>,
}

impl Invitation {
    pub fn new(dialog_layer: Arc<DialogLayer>) -> Self {
        Self {
            dialog_layer,
            pending_dialogs: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub async fn add_pending(&self, session_id: String, pending: PendingDialog) {
        let mut pending_dialogs = self.pending_dialogs.lock().await;
        pending_dialogs.insert(session_id, pending);
    }

    pub async fn get_pending_call(&self, session_id: &String) -> Option<PendingDialog> {
        let mut pending_dialogs = self.pending_dialogs.lock().await;
        pending_dialogs.remove(session_id)
    }

    pub async fn has_pending_call(&self, session_id: &str) -> Option<DialogId> {
        let pending_dialogs = self.pending_dialogs.lock().await;
        pending_dialogs.get(session_id).map(|d| d.dialog.id())
    }

    pub async fn hangup(
        &self,
        dialog_id: DialogId,
        code: Option<rsip::StatusCode>,
        reason: Option<String>,
    ) -> Result<()> {
        let dialog_id_str = dialog_id.to_string();
        if let Some(call) = self.pending_dialogs.lock().await.remove(&dialog_id_str) {
            call.dialog.reject(code, reason).ok();
            call.token.cancel();
        }
        match self.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => {
                dialog.hangup().await.ok();
                self.dialog_layer.remove_dialog(&dialog_id);
            }
            None => {}
        }
        Ok(())
    }
    pub async fn reject(&self, dialog_id: DialogId) -> Result<()> {
        let dialog_id_str = dialog_id.to_string();
        if let Some(call) = self.pending_dialogs.lock().await.remove(&dialog_id_str) {
            call.dialog.reject(None, None).ok();
            call.token.cancel();
        }
        match self.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => {
                dialog.hangup().await.ok();
                self.dialog_layer.remove_dialog(&dialog_id);
            }
            None => {}
        }
        Ok(())
    }

    pub async fn invite(
        &self,
        invite_option: InviteOption,
        state_sender: DialogStateSender,
    ) -> Result<(DialogId, Option<Vec<u8>>), rsipstack::Error> {
        let (dialog, resp) = self
            .dialog_layer
            .do_invite(invite_option, state_sender)
            .await?;

        let offer = match resp {
            Some(resp) => match resp.status_code.kind() {
                rsip::StatusCodeKind::Successful => {
                    let offer = resp.body.clone();
                    Some(offer)
                }
                _ => {
                    let reason = resp
                        .reason_phrase()
                        .unwrap_or(&resp.status_code.to_string())
                        .to_string();
                    return Err(rsipstack::Error::DialogError(
                        reason,
                        dialog.id(),
                        resp.status_code,
                    ));
                }
            },
            None => {
                return Err(rsipstack::Error::DialogError(
                    "no response received".to_string(),
                    dialog.id(),
                    rsip::StatusCode::NotAcceptableHere,
                ));
            }
        };
        Ok((dialog.id(), offer))
    }
}

pub(super) fn on_dialog_terminated(
    call_state: ActiveCallStateRef,
    track_id: TrackId,
    reason: TerminatedReason,
    event_sender: EventSender,
) {
    let mut call_state_ref = match call_state.write() {
        Ok(cs) => cs,
        Err(_) => {
            return;
        }
    };
    call_state_ref.last_status_code = match &reason {
        TerminatedReason::UacCancel => 487,
        TerminatedReason::UacBye => 200,
        TerminatedReason::UacBusy => 486,
        TerminatedReason::UasBye => 200,
        TerminatedReason::UasBusy => 486,
        TerminatedReason::UasDecline => 603,
        TerminatedReason::UacOther(code) => code.code(),
        TerminatedReason::UasOther(code) => code.code(),
        _ => 500, // Default to internal server error
    };

    if call_state_ref.hangup_reason.is_none() {
        call_state_ref.hangup_reason.replace(match reason {
            TerminatedReason::UacCancel => CallRecordHangupReason::Canceled,
            TerminatedReason::UacBye | TerminatedReason::UacBusy => {
                CallRecordHangupReason::ByCaller
            }
            TerminatedReason::UasBye | TerminatedReason::UasBusy => {
                CallRecordHangupReason::ByCallee
            }
            TerminatedReason::UasDecline => CallRecordHangupReason::ByCallee,
            TerminatedReason::UacOther(_) => CallRecordHangupReason::ByCaller,
            TerminatedReason::UasOther(_) => CallRecordHangupReason::ByCallee,
            _ => CallRecordHangupReason::BySystem,
        });
    };
    let initiator = match reason {
        TerminatedReason::UacCancel => "caller".to_string(),
        TerminatedReason::UacBye | TerminatedReason::UacBusy => "caller".to_string(),
        TerminatedReason::UasBye | TerminatedReason::UasBusy | TerminatedReason::UasDecline => {
            "callee".to_string()
        }
        _ => "system".to_string(),
    };
    event_sender
        .send(crate::event::SessionEvent::TrackEnd {
            track_id: track_id.clone(),
            timestamp: crate::get_timestamp(),
            duration: call_state_ref
                .answer_time
                .map(|t| (Utc::now() - t).num_milliseconds())
                .unwrap_or_default() as u64,
            ssrc: call_state_ref.ssrc,
            play_id: None,
        })
        .ok();
    let hangup_event = call_state_ref.build_hangup_event(track_id, Some(initiator));
    event_sender.send(hangup_event).ok();
}

pub async fn client_dialog_event_loop(
    session_id: String,
    track_id: TrackId,
    event_sender: EventSender,
    mut dlg_state_receiver: DialogStateReceiver,
    call_state: ActiveCallStateRef,
    media_stream: Arc<MediaStream>,
) -> Result<DialogId> {
    while let Some(event) = dlg_state_receiver.recv().await {
        match event {
            DialogState::Trying(dialog_id) => {
                call_state
                    .write()
                    .as_mut()
                    .map(|cs| {
                        cs.track_dialog(dialog_id.clone());
                        cs.ring_time.replace(Utc::now())
                    })
                    .ok();
            }
            DialogState::Early(dialog_id, resp) => {
                let body = resp.body();
                let answer = String::from_utf8_lossy(body);
                info!(session_id, track_id, %dialog_id,  "client dialog early answer: \n{}", answer);

                call_state
                    .write()
                    .as_mut()
                    .map(|cs| {
                        cs.track_dialog(dialog_id.clone());
                        cs.ring_time.replace(Utc::now())
                    })
                    .ok();

                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: !answer.is_empty(),
                })?;

                if answer.is_empty() {
                    continue;
                }
                media_stream
                    .update_remote_description(&track_id, &answer.to_string())
                    .await?;
            }
            DialogState::Calling(dialog_id) => {
                info!(session_id, track_id, %dialog_id, "client dialog calling");
            }
            DialogState::Confirmed(dialog_id, _) => {
                info!(session_id, track_id, %dialog_id, "client dialog confirmed");
                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| {
                        cs.track_dialog(dialog_id.clone());
                        cs.answer_time.replace(Utc::now());
                        cs.last_status_code = 200;
                        Ok(())
                    })
                    .ok();
            }
            DialogState::Info(dialog_id, req) => {
                let body_str = String::from_utf8_lossy(req.body());
                info!(session_id, track_id, %dialog_id, body=%body_str, "client dialog info received");
                if body_str.starts_with("Signal=") {
                    let digit = body_str.trim_start_matches("Signal=").chars().next();
                    if let Some(digit) = digit {
                        event_sender.send(crate::event::SessionEvent::Dtmf {
                            track_id: track_id.clone(),
                            timestamp: crate::get_timestamp(),
                            digit: digit.to_string(),
                        })?;
                    }
                }
            }
            DialogState::Terminated(dialog_id, reason) => {
                info!(
                    session_id,
                    track_id,
                    ?dialog_id,
                    ?reason,
                    "client dialog terminated"
                );
                on_dialog_terminated(
                    call_state.clone(),
                    track_id.clone(),
                    reason,
                    event_sender.clone(),
                );
                return Ok(dialog_id);
            }
            _ => (),
        }
    }
    Err(anyhow::anyhow!(
        "client_dialog_event_loop: dialog state receiver closed"
    ))
}

pub async fn server_dialog_event_loop(
    cancel_token: CancellationToken,
    session_id: String,
    track_id: TrackId,
    event_sender: EventSender,
    mut dlg_state_receiver: DialogStateReceiver,
    call_state: ActiveCallStateRef,
) -> Result<DialogId> {
    while let Some(event) = dlg_state_receiver.recv().await {
        match event {
            DialogState::Trying(dialog_id) => {
                info!(session_id, "server dialog trying: {}", dialog_id);
            }
            DialogState::Early(dialog_id, resp) => {
                let code = resp.status_code.code();
                info!(session_id, track_id, %dialog_id, "server dialog calling");
                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| {
                        cs.ring_time.replace(Utc::now());
                        cs.last_status_code = code;
                        Ok(())
                    })
                    .ok();
            }
            DialogState::Calling(dialog_id) => {
                info!(session_id, track_id, %dialog_id, "server dialog calling");
            }
            DialogState::Confirmed(dialog_id, _) => {
                info!(session_id, track_id, %dialog_id, "server dialog confirmed");
                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| {
                        cs.track_dialog(dialog_id.clone());
                        cs.answer_time.replace(Utc::now());
                        cs.last_status_code = 200;
                        Ok(())
                    })
                    .ok();
            }
            DialogState::Terminated(dialog_id, reason) => {
                info!(
                    session_id,
                    track_id,
                    ?dialog_id,
                    ?reason,
                    "server dialog terminated"
                );
                on_dialog_terminated(
                    call_state.clone(),
                    track_id.clone(),
                    reason,
                    event_sender.clone(),
                );
                cancel_token.cancel(); // Cancel the token to stop any ongoing tasks
                return Ok(dialog_id);
            }
            DialogState::Info(dialog_id, req) => {
                let body_str = String::from_utf8_lossy(req.body());
                info!(session_id, track_id, %dialog_id, body=%body_str, "server dialog info received");
                if body_str.starts_with("Signal=") {
                    let digit = body_str.trim_start_matches("Signal=").chars().next();
                    if let Some(digit) = digit {
                        event_sender.send(crate::event::SessionEvent::Dtmf {
                            track_id: track_id.clone(),
                            timestamp: crate::get_timestamp(),
                            digit: digit.to_string(),
                        })?;
                    }
                }
            }
            _ => (),
        }
    }
    Err(anyhow::anyhow!(
        "server_dialog_event_loop: dialog state receiver closed"
    ))
}
