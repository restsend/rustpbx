use super::CallOption;
use crate::app::AppState;
use crate::call::active_call::ActiveCallStateRef;
use crate::call::ActiveCallState;
use crate::callrecord::CallRecordHangupReason;
use crate::event::EventSender;
use crate::media::engine::StreamEngine;
use crate::media::stream::MediaStream;
use crate::media::track::rtp::{RtpTrack, RtpTrackBuilder};
use crate::media::track::{Track, TrackConfig};
use crate::useragent::invitation::PendingDialog;
use crate::TrackId;
use anyhow::Result;
use chrono::Utc;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::{
    DialogState, DialogStateReceiver, DialogStateSender, TerminatedReason,
};
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::DialogId;
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub async fn new_rtp_track_with_pending_call(
    state: AppState,
    token: CancellationToken,
    track_id: TrackId,
    ssrc: u32,
    track_config: TrackConfig,
    _option: &CallOption,
    dlg_state_sender: DialogStateSender,
    pending_call: PendingDialog,
) -> Result<(DialogId, RtpTrack)> {
    let mut rtp_track = RtpTrackBuilder::new(track_id.clone(), track_config)
        .with_ssrc(ssrc)
        .with_cancel_token(token);

    if let Some(ref sip) = state.config.ua {
        if let Some(rtp_start_port) = sip.rtp_start_port {
            rtp_track = rtp_track.with_rtp_start_port(rtp_start_port);
        }
        if let Some(rtp_end_port) = sip.rtp_end_port {
            rtp_track = rtp_track.with_rtp_end_port(rtp_end_port);
        }

        if let Some(ref external_ip) = sip.external_ip {
            rtp_track = rtp_track.with_external_addr(external_ip.parse()?);
        }

        if let Some(ref stun_server) = sip.stun_server {
            rtp_track = rtp_track.with_stun_server(stun_server.clone());
        }
    }

    let mut rtp_track = rtp_track.build().await?;
    let initial_request = pending_call.dialog.initial_request();
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

    match pending_call
        .dialog
        .accept(Some(headers), Some(answer.as_bytes().to_vec()))
    {
        Ok(_) => (),
        Err(e) => {
            error!(track_id, "failed to accept call: {}", e);
            return Err(anyhow::anyhow!("failed to accept call"));
        }
    }

    let dialog_id = pending_call.dialog.id();

    let mut state_receiver = pending_call.state_receiver;
    let token_clone = pending_call.token;
    tokio::spawn(async move {
        tokio::select! {
            _ = token_clone.cancelled() => {}
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
) -> Result<(DialogId, RtpTrack)> {
    let ua = app_state.useragent.clone();
    let caller = match option.caller {
        Some(ref caller) => caller.clone(),
        None => return Err(anyhow::anyhow!("caller is required")),
    };
    let callee = match option.callee {
        Some(ref callee) => callee.clone(),
        None => return Err(anyhow::anyhow!("callee is required")),
    };
    let mut rtp_track = RtpTrackBuilder::new(track_id.clone(), track_config)
        .with_ssrc(ssrc)
        .with_cancel_token(token);

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

        if let Some(ref stun_server) = sip.stun_server {
            rtp_track = rtp_track.with_stun_server(stun_server.clone());
        }
    }

    let mut rtp_track = rtp_track.build().await?;
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
    match ua.invite(invite_option, dlg_state_sender).await {
        Ok((dialog_id, answer)) => {
            match answer {
                Some(answer) => {
                    let answer = String::from_utf8_lossy(&answer);
                    match rtp_track.set_remote_description(&answer) {
                        Ok(_) => (),
                        Err(e) => {
                            error!(track_id, "sip_call:failed to set remote description: {}", e);
                            return Err(anyhow::anyhow!("failed to set remote description"));
                        }
                    }
                }
                None => return Err(anyhow::anyhow!("failed to get answer")),
            }
            Ok((dialog_id, rtp_track))
        }
        Err(e) => Err(e),
    }
}

pub async fn sip_event_loop(
    session_id: String,
    track_id: TrackId,
    event_sender: EventSender,
    mut dlg_state_receiver: DialogStateReceiver,
    call_state: ActiveCallStateRef,
) -> Result<DialogId> {
    while let Some(event) = dlg_state_receiver.recv().await {
        let mut call_state_ref = call_state.write().unwrap();

        match event {
            DialogState::Trying(dialog_id) => {
                info!(session_id, "dialog trying: {}", dialog_id);
            }
            DialogState::Early(dialog_id, _) => {
                info!(session_id, track_id, "dialog early: {}", dialog_id);
                call_state_ref.ring_time.replace(Utc::now());
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: true,
                })?;
            }
            DialogState::Calling(dialog_id) => {
                info!(session_id, track_id, "dialog calling: {}", dialog_id);
                call_state_ref.ring_time.replace(Utc::now());
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: false,
                })?;
            }
            DialogState::Confirmed(dialog_id) => {
                call_state_ref.answer_time.replace(Utc::now());
                call_state_ref.last_status_code = 200;
                info!(session_id, track_id, "dialog confirmed: {}", dialog_id);
            }
            DialogState::Terminated(dialog_id, reason) => {
                info!(
                    session_id,
                    track_id, "dialog terminated: {} {:?}", dialog_id, reason
                );
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
    stream: Arc<MediaStream>,
    auto_hangup: Arc<Mutex<Option<u32>>>,
    caller: String,
    callee: String,
    options: Option<super::ReferOption>,
) -> Result<()> {
    let (dlg_state_sender, dlg_state_receiver) = mpsc::unbounded_channel();
    let refer_call_state = Arc::new(RwLock::new(ActiveCallState {
        created_at: Utc::now(),
        ring_time: None,
        answer_time: None,
        hangup_reason: None,
        last_status_code: 0,
        answer: None,
        option: None,
    }));
    let dialog_layer = app_state.useragent.dialog_layer.clone();
    let ssrc = rand::random::<u32>();
    let auto_hangup_when_done = options.as_ref().and_then(|o| o.auto_hangup);
    let call_option = CallOption {
        caller: Some(caller),
        callee: Some(callee.clone()),
        sip: options.as_ref().and_then(|o| o.sip.clone()),
        asr: options.as_ref().and_then(|o| o.asr.clone()),
        denoise: options.as_ref().and_then(|o| o.denoise.clone()),
        ..Default::default()
    };
    if auto_hangup_when_done.unwrap_or(false) {
        *auto_hangup.lock().await = Some(ssrc);
    } else {
        *auto_hangup.lock().await = None;
    }

    let r = tokio::join!(
        async {
            match sip_event_loop(
                session_id.clone(),
                track_id.clone(),
                event_sender.clone(),
                dlg_state_receiver,
                refer_call_state.clone(),
            )
            .await
            {
                Ok(dialog_id) => {
                    info!(
                        session_id,
                        "Refer call dialog created with ID: {}", dialog_id
                    );
                }
                Err(e) => {
                    error!(session_id, "Failed to handle SIP event loop: {}", e);
                    return Err(e);
                }
            }
            Ok(())
        },
        async {
            match new_rtp_track_with_sip(
                app_state.clone(),
                token.clone(),
                track_id.clone(),
                ssrc,
                track_config,
                &call_option,
                dlg_state_sender,
            )
            .await
            {
                Ok((dialog_id, rtp_track)) => {
                    let mut callee_track = Box::new(rtp_track) as Box<dyn Track + Send + Sync>;
                    let processors = match StreamEngine::create_processors(
                        app_state.stream_engine.clone(),
                        callee_track.as_ref(),
                        token.clone(),
                        event_sender.clone(),
                        &call_option,
                    )
                    .await
                    {
                        Ok(processors) => processors,
                        Err(e) => {
                            warn!(session_id, %dialog_id, "Failed to prepare stream processors: {}", e);
                            vec![]
                        }
                    };

                    info!(session_id, %dialog_id, processors = processors.len(), "RTP track created with dialog ID");
                    // Add all processors from the hook
                    for processor in processors {
                        callee_track.append_processor(processor);
                    }
                    stream.update_track(callee_track).await;
                    token.cancelled().await;
                    info!(session_id, "RTP track for refer call to {} stopped", callee);
                    match dialog_layer.get_dialog(&dialog_id) {
                        Some(dialog) => {
                            dialog.hangup().await.ok();
                        }
                        _ => {}
                    };
                }
                Err(e) => {
                    error!(session_id, "Failed to create RTP track: {}", e);
                    return Err(e);
                }
            };
            Ok(())
        }
    );
    match r {
        (Ok(()), Ok(())) => {
            info!(session_id, "Refer call completed successfully");
        }
        (Err(e), _) | (_, Err(e)) => {
            error!(session_id, "Refer call failed: {}", e);
            return Err(e);
        }
    };
    Ok(())
}
