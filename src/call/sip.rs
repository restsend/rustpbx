use crate::TrackId;
use crate::app::AppState;
use crate::call::ActiveCall;
use crate::call::active_call::ActiveCallStateRef;
use crate::callrecord::CallRecordHangupReason;
use crate::event::EventSender;
use crate::media::stream::MediaStream;
use crate::media::track::TrackConfig;
use crate::useragent::invitation::PendingDialog;
use anyhow::Result;
use chrono::Utc;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{
    DialogState, DialogStateReceiver, DialogStateSender, TerminatedReason,
};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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

    pub async fn hangup(&self, dialog_id: DialogId) -> Result<()> {
        let dialog_id_str = dialog_id.to_string();
        if let Some(call) = self.pending_dialogs.lock().await.remove(&dialog_id_str) {
            call.dialog.reject().ok();
            call.token.cancel();
        }

        let dialog = self
            .dialog_layer
            .get_dialog(&dialog_id)
            .ok_or(anyhow::anyhow!("dialog not found"))?;
        dialog.hangup().await.ok();
        self.dialog_layer.remove_dialog(&dialog_id);
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
                    return Err(rsipstack::Error::DialogError(
                        resp.status_code.to_string(),
                        dialog.id(),
                    ));
                }
            },
            None => {
                return Err(rsipstack::Error::DialogError(
                    "No response received".to_string(),
                    dialog.id(),
                ));
            }
        };
        Ok((dialog.id(), offer))
    }
}

pub async fn sip_dialog_event_loop(
    cancel_token: CancellationToken,
    app_state: AppState,
    session_id: String,
    track_id: TrackId,
    track_config: TrackConfig,
    event_sender: EventSender,
    mut dlg_state_receiver: DialogStateReceiver,
    call_state: ActiveCallStateRef,
    media_stream: Arc<MediaStream>,
) -> Result<DialogId> {
    while let Some(event) = dlg_state_receiver.recv().await {
        match event {
            DialogState::Trying(dialog_id) => {
                info!(session_id, "dialog trying: {}", dialog_id);
            }
            DialogState::Early(dialog_id, resp) => {
                let body = resp.body();
                let answer = String::from_utf8_lossy(body);
                info!(session_id, track_id, %dialog_id,  "early answer: \n{}", answer);

                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| Ok(cs.ring_time.replace(Utc::now())))
                    .ok();

                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: true,
                })?;

                if answer.is_empty() {
                    continue;
                }

                let mut rtp_track = match ActiveCall::create_rtp_track(
                    cancel_token.child_token(),
                    app_state.clone(),
                    track_id.clone(),
                    track_config.clone(),
                    0,
                )
                .await
                {
                    Ok(track) => track,
                    Err(_) => {
                        continue;
                    }
                };
                match rtp_track.set_remote_description(&answer) {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(
                            session_id,
                            track_id,
                            %dialog_id,
                            "failed to set remote description: {}", e
                        );
                        continue;
                    }
                }
                let caller_track = Box::new(rtp_track);
                let mut option = call_state
                    .read()
                    .map(|cs| cs.option.clone())
                    .unwrap_or_default()
                    .unwrap_or_default();

                //TODO: remove these options from the call option
                option.asr = None;
                option.vad = None;
                option.denoise = None;

                match ActiveCall::setup_track_with_stream(
                    app_state.clone(),
                    cancel_token.child_token(),
                    media_stream.clone(),
                    event_sender.clone(),
                    &session_id,
                    &option,
                    caller_track,
                )
                .await
                {
                    Ok(_) => {
                        info!(
                            session_id,
                            track_id, "RTP track for early media setup completed"
                        );
                    }
                    Err(e) => {
                        warn!(
                            session_id,
                            track_id,
                            %dialog_id,
                            "failed to setup track with stream: {}", e
                        );
                    }
                }
            }
            DialogState::Calling(dialog_id) => {
                info!(session_id, track_id, %dialog_id, "dialog calling");
                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| Ok(cs.ring_time.replace(Utc::now())))
                    .ok();
                match event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: false,
                }) {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(session_id, track_id, %dialog_id, "failed to send ringing event: {}", e);
                    }
                }
            }
            DialogState::Confirmed(dialog_id) => {
                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| {
                        if cs.dialog_id.is_none() {
                            cs.dialog_id = Some(dialog_id.clone());
                        }
                        cs.answer_time.replace(Utc::now());
                        cs.last_status_code = 200;
                        Ok(())
                    })
                    .ok();
                info!(session_id, track_id, %dialog_id, "dialog confirmed");
            }
            DialogState::Terminated(dialog_id, reason) => {
                info!(
                    session_id,
                    track_id,
                    ?dialog_id,
                    ?reason,
                    "dialog terminated"
                );

                let mut call_state_ref = match call_state.write() {
                    Ok(cs) => cs,
                    Err(_) => {
                        break;
                    }
                };
                call_state_ref.last_status_code = match &reason {
                    TerminatedReason::UacCancel => 487,
                    TerminatedReason::UacBye => 200,
                    TerminatedReason::UacBusy => 486,
                    TerminatedReason::UasBye => 200,
                    TerminatedReason::UasBusy => 486,
                    TerminatedReason::UasDecline => 603,
                    TerminatedReason::UacOther(code) => code
                        .clone()
                        .unwrap_or(rsip::StatusCode::ServerInternalError)
                        .code(),
                    TerminatedReason::UasOther(code) => code
                        .clone()
                        .unwrap_or(rsip::StatusCode::ServerInternalError)
                        .code(),
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
                    TerminatedReason::UasBye
                    | TerminatedReason::UasBusy
                    | TerminatedReason::UasDecline => "callee".to_string(),
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
                    })
                    .ok();
                event_sender
                    .send(crate::event::SessionEvent::Hangup {
                        timestamp: crate::get_timestamp(),
                        reason: Some(format!("{:?}", call_state_ref.hangup_reason)),
                        initiator: Some(initiator),
                    })
                    .ok();
                cancel_token.cancel(); // Cancel the token to stop any ongoing tasks
                return Ok(dialog_id);
            }
            _ => (),
        }
    }
    Err(anyhow::anyhow!(
        "sip_event_loop: dialog state receiver closed"
    ))
}
