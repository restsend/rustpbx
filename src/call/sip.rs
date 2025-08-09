use super::CallOption;
use crate::TrackId;
use crate::app::AppState;
use crate::call::active_call::{ActiveCallStateRef, setup_track_with_stream};
use crate::callrecord::CallRecordHangupReason;
use crate::event::{EventSender, SessionEvent};
use crate::media::stream::MediaStream;
use crate::media::track::TrackConfig;
use crate::media::track::rtp::{RtpTrack, RtpTrackBuilder};
use crate::useragent::invitation::PendingDialog;
use anyhow::Result;
use chrono::Utc;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{
    DialogState, DialogStateReceiver, DialogStateSender, TerminatedReason,
};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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

pub async fn create_rtp_track(
    cancel_token: CancellationToken,
    app_state: AppState,
    track_id: TrackId,
    track_config: TrackConfig,
    ssrc: u32,
) -> Result<RtpTrack> {
    let mut rtp_track = RtpTrackBuilder::new(track_id.clone(), track_config)
        .with_ssrc(ssrc)
        .with_cancel_token(cancel_token);

    if let Some(ref sip) = app_state.config.ua {
        if let Some(rtp_start_port) = sip.rtp_start_port {
            rtp_track = rtp_track.with_rtp_start_port(rtp_start_port);
        }
        if let Some(rtp_end_port) = sip.rtp_end_port {
            rtp_track = rtp_track.with_rtp_end_port(rtp_end_port);
        }

        if let Some(ref external_ip) = sip.external_ip {
            rtp_track = rtp_track.with_external_addr(external_ip.parse()?);
        }
    }
    rtp_track.build().await
}

pub async fn new_rtp_track_with_pending_call(
    state: AppState,
    token: CancellationToken,
    track_id: TrackId,
    ssrc: u32,
    track_config: TrackConfig,
    _option: &CallOption,
    dlg_state_sender: DialogStateSender,
    pending_dialog: PendingDialog,
) -> Result<(DialogId, RtpTrack)> {
    let mut rtp_track = create_rtp_track(
        token.clone(),
        state.clone(),
        track_id.clone(),
        track_config,
        ssrc,
    )
    .await?;
    let initial_request = pending_dialog.dialog.initial_request();
    let offer = String::from_utf8_lossy(&initial_request.body);
    match rtp_track.set_remote_description(&offer) {
        Ok(_) => (),
        Err(e) => {
            error!(track_id, "failed to set remote description: {}", e);
            return Err(anyhow::anyhow!("failed to set remote description"));
        }
    }

    let answer = match rtp_track.local_description() {
        Ok(answer) => answer,
        Err(e) => {
            error!(track_id, "failed to get local description: {}", e);
            return Err(anyhow::anyhow!("failed to get local description"));
        }
    };
    let headers = vec![rsip::Header::ContentType(
        "application/sdp".to_string().into(),
    )];

    match pending_dialog
        .dialog
        .accept(Some(headers), Some(answer.as_bytes().to_vec()))
    {
        Ok(_) => (),
        Err(e) => {
            error!(track_id, "failed to accept call: {}", e);
            return Err(anyhow::anyhow!("failed to accept call"));
        }
    }

    let dialog_id = pending_dialog.dialog.id();

    let mut state_receiver = pending_dialog.state_receiver;
    let pending_token_clone = pending_dialog.token;
    tokio::spawn(async move {
        tokio::select! {
            _ = token.cancelled() => {}
            _ = pending_token_clone.cancelled() => {}
            _ = async {
                while let Some(state) = state_receiver.recv().await {
                    if let Err(_) = dlg_state_sender.send(state) {
                        break;
                    }
                }
            } => {}
        }
    });

    Ok((dialog_id, rtp_track))
}

pub async fn new_rtp_track_with_sip(
    app_state: AppState,
    token: CancellationToken,
    track_id: TrackId,
    ssrc: u32,
    track_config: TrackConfig,
    option: &CallOption,
    dlg_state_sender: DialogStateSender,
    invitation: &Invitation,
) -> Result<(DialogId, RtpTrack), rsipstack::Error> {
    let caller = match option.caller {
        Some(ref caller) => caller.clone(),
        None => return Err(rsipstack::Error::Error("caller is required".to_string())),
    };
    let callee = match option.callee {
        Some(ref callee) => callee.clone(),
        None => return Err(rsipstack::Error::Error("callee is required".to_string())),
    };

    let mut rtp_track = create_rtp_track(
        token,
        app_state.clone(),
        track_id.clone(),
        track_config,
        ssrc,
    )
    .await
    .map_err(|e| {
        rsipstack::Error::Error(format!("failed to create rtp track: {}", e.to_string()))
    })?;

    let offer = rtp_track.local_description().ok();

    let headers = option
        .sip
        .as_ref()
        .map(|sip_opt| {
            sip_opt.headers.as_ref().map(|h| {
                h.iter()
                    .map(|(k, v)| rsip::Header::Other(k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            })
        })
        .flatten();

    let invite_option = InviteOption {
        caller: caller.clone().try_into()?,
        callee: callee.try_into()?,
        destination: None,
        content_type: Some("application/sdp".to_string()),
        offer: offer.as_ref().map(|s| s.as_bytes().to_vec()),
        contact: caller.try_into()?,
        credential: option.sip.as_ref().map(|opt| Credential {
            username: opt.username.clone(),
            password: opt.password.clone(),
            realm: Some(opt.realm.clone()),
        }),
        headers,
    };
    info!(
        track_id,
        "invite {} -> {} offer: \n{}",
        invite_option.caller,
        invite_option.callee,
        offer.as_ref().map(|s| s.as_str()).unwrap_or("<NO OFFER>")
    );
    let (dialog_id, answer) = invitation.invite(invite_option, dlg_state_sender).await?;
    match answer {
        Some(answer) => {
            let answer = String::from_utf8_lossy(&answer);
            match rtp_track.set_remote_description(&answer) {
                Ok(_) => (),
                Err(e) => {
                    warn!(track_id, "sip_call:failed to set remote description: {}", e);
                    return Err(rsipstack::Error::DialogError(
                        "failed to set remote description".to_string(),
                        dialog_id.clone(),
                    ));
                }
            }
        }
        None => {
            return Err(rsipstack::Error::DialogError(
                "no answer received".to_string(),
                dialog_id.clone(),
            ));
        }
    }
    Ok((dialog_id, rtp_track))
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
                info!(session_id, track_id, %dialog_id,  ", answer: \n{}", answer);

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

                let mut rtp_track = match create_rtp_track(
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
                    .unwrap_or(CallOption::default());

                //TODO: remove these options from the call option
                option.asr = None;
                option.vad = None;
                option.denoise = None;

                setup_track_with_stream(
                    cancel_token.child_token(),
                    app_state.stream_engine.clone(),
                    &session_id,
                    event_sender.clone(),
                    media_stream.clone(),
                    caller_track,
                    &option,
                )
                .await;
                info!(
                    session_id,
                    track_id, "RTP track for early media setup completed"
                );
            }
            DialogState::Calling(dialog_id) => {
                info!(session_id, track_id, %dialog_id, "dialog calling");
                call_state
                    .write()
                    .as_mut()
                    .and_then(|cs| Ok(cs.ring_time.replace(Utc::now())))
                    .ok();
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: false,
                })?;
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

pub(crate) async fn make_sip_invite_with_stream(
    token: CancellationToken,
    app_state: AppState,
    session_id: String,
    track_id: TrackId,
    track_config: TrackConfig,
    event_sender: EventSender,
    media_stream: Arc<MediaStream>,
    refer_call_state: ActiveCallStateRef,
    invitation: Invitation,
) -> Result<()> {
    let (dlg_state_sender, dlg_state_receiver) = mpsc::unbounded_channel();
    let dialog_layer = app_state.useragent.dialog_layer.clone();
    let ssrc = refer_call_state.read().map(|cs| cs.ssrc).unwrap_or(0);

    let call_option = refer_call_state
        .read()
        .map(|cs| cs.option.clone())
        .unwrap_or(CallOption::default());

    let refer_rtp_track_loop = async {
        match new_rtp_track_with_sip(
            app_state.clone(),
            token.clone(),
            track_id.clone(),
            ssrc,
            track_config.clone(),
            &call_option,
            dlg_state_sender,
            &invitation,
        )
        .await
        {
            Ok((dialog_id, rtp_track)) => {
                refer_call_state
                    .write()
                    .as_mut()
                    .map(|cs| cs.dialog_id = Some(dialog_id.clone()))
                    .ok();

                let option = refer_call_state
                    .read()
                    .map(|cs| cs.option.clone())
                    .unwrap_or(CallOption::default());

                let callee_track = Box::new(rtp_track);

                setup_track_with_stream(
                    token.clone(),
                    app_state.stream_engine.clone(),
                    &session_id,
                    event_sender.clone(),
                    media_stream.clone(),
                    callee_track,
                    &option,
                )
                .await;

                token.cancelled().await;
                info!(session_id, %dialog_id, "RTP track for refer call stopped");
                match dialog_layer.get_dialog(&dialog_id) {
                    Some(dialog) => {
                        dialog.hangup().await.ok();
                    }
                    _ => {}
                };
            }
            Err(e) => {
                warn!(session_id, "Failed to create RTP track: {}", e);
                let event = SessionEvent::Error {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    error: e.to_string(),
                    sender: "make_sip_invite_with_stream".to_string(),
                    code: None,
                };
                event_sender.send(event).ok();
                // The rtp track creation failed, we need to send a track end event
                let track_end_event = SessionEvent::TrackEnd {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    duration: 0,
                    ssrc,
                };
                event_sender.send(track_end_event).ok();
                return Err(e);
            }
        };
        Ok(())
    };
    let start_time = crate::get_timestamp();
    let (_, r) = tokio::join!(
        sip_dialog_event_loop(
            token.clone(),
            app_state.clone(),
            session_id.clone(),
            track_id.clone(),
            track_config.clone(),
            event_sender.clone(),
            dlg_state_receiver,
            refer_call_state.clone(),
            media_stream.clone(),
        ),
        refer_rtp_track_loop
    );
    info!(
        session_id,
        "refer call ended after {} ms, reason: {:?}",
        crate::get_timestamp() - start_time,
        r
    );
    Ok(())
}
