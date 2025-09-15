use crate::{
    app::AppState,
    call::{
        ActiveCall, ActiveCallRef, ActiveCallState, ActiveCallType, CallOption, Command,
        CommandSender, DialStrategy, Dialplan, Location, RouteInvite, TransactionCookie,
        sip::{Invitation, client_dialog_event_loop},
    },
    config::RouteResult,
    media::{recorder::RecorderOption, track::TrackConfig},
    useragent::invitation::PendingDialog,
};
use anyhow::Result;
use chrono::Utc;
use rsip::prelude::HeadersExt;
use rsipstack::{dialog::dialog::DialogState, transaction::transaction::Transaction};
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct B2bua {
    pub cancel_token: CancellationToken,
    pub media_capabilities: Vec<crate::media::codecs::CodecType>,
    pub cookie: TransactionCookie,
    pub session_id: String,
    pub dump_events: bool,
    pub recorder: bool,
    pub cmd_sender: CommandSender,
}

pub struct B2buaBuilder {
    pub app_state: AppState,
    pub cancel_token: Option<CancellationToken>,
    pub media_capabilities: Option<Vec<crate::media::codecs::CodecType>>,
    pub cookie: TransactionCookie,
    pub dump_events: bool,
    pub session_id: String,
    pub recorder: bool,
}

impl B2buaBuilder {
    pub fn new(app_state: AppState, cookie: TransactionCookie, session_id: String) -> Self {
        Self {
            app_state,
            cancel_token: None,
            media_capabilities: None,
            cookie,
            dump_events: true,
            session_id,
            recorder: true,
        }
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_recorder(mut self, enable: bool) -> Self {
        self.recorder = enable;
        self
    }

    pub fn with_media_capabilities(
        mut self,
        capabilities: Vec<crate::media::codecs::CodecType>,
    ) -> Self {
        self.media_capabilities = Some(capabilities);
        self
    }

    pub async fn build(self, _tx: &Transaction) -> Result<B2bua> {
        let cancel_token = self.cancel_token.unwrap_or_else(CancellationToken::new);
        let b2bua = B2bua {
            cancel_token,
            media_capabilities: self.media_capabilities.unwrap_or_default(),
            cookie: self.cookie,
            session_id: self.session_id,
            dump_events: self.dump_events,
            recorder: self.recorder,
            cmd_sender: broadcast::Sender::<Command>::new(32),
        };
        Ok(b2bua)
    }
}

impl B2bua {
    pub async fn serve(
        &self,
        tx: &mut Transaction,
        caller_contact: rsip::typed::Contact,
        app_state: AppState,
        invitation: Invitation,
        dialplan: Dialplan,
    ) -> Result<()> {
        let (state_sender, state_receiver) = mpsc::unbounded_channel();
        let mut dialog = match invitation.dialog_layer.get_or_create_server_invite(
            &tx,
            state_sender,
            None,
            Some(caller_contact.uri.clone()),
        ) {
            Ok(d) => d,
            Err(e) => {
                info!(
                    session_id = self.session_id,
                    "failed to obtain dialog: {:?}", e
                );
                tx.reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .ok();
                return Err(anyhow::anyhow!("failed to obtain dialog: {}", e));
            }
        };

        let pending_dialog = PendingDialog {
            token: self.cancel_token.clone(),
            dialog: dialog.clone(),
            state_receiver,
        };
        invitation
            .add_pending(self.session_id.clone(), pending_dialog)
            .await;

        info!(
            session_id = self.session_id,
            capabilities = ?self.media_capabilities,
            "incoming call"
        );
        let active_call = Arc::new(ActiveCall::new(
            ActiveCallType::B2bua,
            self.cancel_token.clone(),
            self.session_id.clone(),
            invitation,
            app_state.clone(),
            TrackConfig::default(),
            None,
            self.dump_events,
            None,
            dialplan.extras.clone(),
        ));

        let active_calls = {
            let mut calls = app_state.active_calls.lock().await;
            calls.insert(self.session_id.clone(), active_call.clone());
            calls.len()
        };
        info!(session_id = self.session_id, active_calls, "b2bua started");
        let original = tx.original.clone();
        let dialog_ref = dialog.clone();
        let (_, _, _) = tokio::join!(
            dialog.handle(tx),
            async {
                match self
                    .process_callee_loop(active_call.clone(), caller_contact, dialplan, &original)
                    .await
                {
                    Ok(_) => {}
                    Err(_) => {
                        dialog_ref.reject(None, None).ok();
                    }
                }
            },
            active_call.serve()
        );

        if let Some(pending) = active_call
            .invitation
            .get_pending_call(&self.session_id)
            .await
        {
            warn!(
                session_id = self.session_id,
                "pending dialog still exists, cleaning up"
            );
            pending.dialog.reject(None, None).ok();
        }
        app_state.active_calls.lock().await.remove(&self.session_id);
        Ok(())
    }

    async fn process_callee_loop(
        &self,
        active_call: ActiveCallRef,
        caller_contact: rsip::typed::Contact,
        dialplan: Dialplan,
        original: &rsip::Request,
    ) -> Result<()> {
        if dialplan.is_empty() {
            warn!(
                session_id = self.session_id,
                "dialplan is empty, ending call"
            );
            active_call.cancel_token.cancel();
            return Err(anyhow::anyhow!("Dialplan is empty"));
        }

        let route_invite = dialplan.route_invite;
        let caller = dialplan.caller.clone().or_else(|| {
            original
                .from_header()
                .ok()
                .and_then(|f| f.uri().ok().map(|u| u))
        });
        let invite_callee_loop = async {
            match dialplan.targets {
                DialStrategy::Sequential(targets) => {
                    for target in targets {
                        match self
                            .invite_callee(
                                active_call.clone(),
                                caller,
                                &caller_contact,
                                target,
                                original,
                                &route_invite,
                            )
                            .await
                        {
                            Ok(_) => {
                                return Ok(());
                            }
                            Err(e) => {
                                return Err(e);
                            }
                        }
                    }
                    return Err(anyhow::anyhow!("All targets failed"));
                }
                DialStrategy::Parallel(_targets) => {
                    todo!("Parallel dialing not implemented yet");
                }
            }
        };
        match invite_callee_loop.await {
            Ok(_) => {
                info!(session_id = self.session_id, "Callee loop completed");
                let option = CallOption {
                    recorder: if self.recorder {
                        let recorder_file =
                            active_call.app_state.get_recorder_file(&self.session_id);
                        Some(RecorderOption::new(recorder_file))
                    } else {
                        None
                    },
                    ..CallOption::default()
                };
                let answer_command = Command::Accept { option };
                if let Err(e) = active_call.enqueue_command(answer_command).await {
                    warn!(
                        session_id = self.session_id,
                        "Failed to enqueue answer command: {}", e
                    );
                }
                return Ok(());
            }
            Err(e) => {
                self.cancel_token.cancel();
                return Err(e);
            }
        }
    }

    async fn invite_callee(
        &self,
        active_call: ActiveCallRef,
        caller: Option<rsip::Uri>,
        caller_contact: &rsip::typed::Contact,
        target: Location,
        original: &rsip::Request,
        route_invite: &Option<Box<dyn RouteInvite>>,
    ) -> Result<()> {
        let ssrc = rand::random::<u32>();
        let rtp_token = self.cancel_token.child_token();
        let rtp_track = ActiveCall::create_rtp_track(
            rtp_token.clone(),
            active_call.app_state.clone(),
            active_call.server_side_track_id.clone(),
            active_call.track_config.clone(),
            ssrc,
        )
        .await?;

        let offer = rtp_track.local_description().ok().unwrap_or_default();
        let mut call_option = CallOption::default();
        call_option.caller = caller.map(|u| u.to_string());
        call_option.callee = Some(target.aor.to_string());

        let mut invite_option = call_option.build_invite_option()?;
        invite_option.destination = Some(target.destination.clone());
        invite_option.offer = Some(offer.clone().into());
        invite_option.contact = caller_contact.uri.clone();

        ActiveCall::setup_track_with_stream(
            active_call.app_state.clone(),
            rtp_token,
            active_call.media_stream.clone(),
            active_call.event_sender.clone(),
            &active_call.session_id,
            &call_option,
            Box::new(rtp_track),
        )
        .await?;

        let invite_option = if let Some(route_invite) = &route_invite {
            let route_result = route_invite.route_invite(invite_option, original).await?;
            match route_result {
                RouteResult::Forward(option) => option,
                RouteResult::Abort(code, reason) => {
                    warn!(session_id = self.session_id, code, reason, "route abort");
                    return Err(anyhow::anyhow!("Route abort: {} {}", code, reason));
                }
            }
        } else {
            invite_option
        };
        let track_id = active_call.server_side_track_id.clone();
        info!(
            session_id = self.session_id,
            track_id,
            contact = %invite_option.contact,
            "b2bua invite callee {} -> {} offer: \n{}",
            invite_option.caller,
            invite_option.callee,
            offer,
        );

        let call_state_ref = Arc::new(RwLock::new(ActiveCallState {
            start_time: Utc::now(),
            option: Some(call_option),
            ssrc,
            ..Default::default()
        }));

        active_call
            .call_state
            .write()
            .as_mut()
            .and_then(|cs| {
                cs.refer_callstate = Some(call_state_ref.clone());
                Ok(())
            })
            .ok();

        let (dlg_state_sender, mut dlg_state_receiver) = tokio::sync::mpsc::unbounded_channel();
        let session_id = self.session_id.clone();
        let event_sender = active_call.event_sender.clone();
        let media_stream = active_call.media_stream.clone();
        let call_state = call_state_ref.clone();
        let track_id_clone = track_id.clone();
        let cancel_token = self.cancel_token.clone();
        let active_call_ref = active_call.clone();
        let recorder = self.recorder;

        tokio::spawn(async move {
            let (refer_dlg_state_sender, refer_dlg_state_receiver) = mpsc::unbounded_channel();

            let forward_dlg_state_loop = async {
                let mut dialog_id = None;
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        if let Some(dialog_id) = dialog_id {
                            info!(session_id = session_id, dialog_id = %dialog_id, "cancelling callee dialog");
                            active_call_ref.invitation.hangup(dialog_id, None, None).await.ok();
                        }
                    }
                    _ = async {
                        while let Some(state) = dlg_state_receiver.recv().await {
                            match &state {
                                DialogState::Calling(id) => {
                                    dialog_id = Some(id.clone());
                                }
                                DialogState::Confirmed(id) => {
                                    dialog_id = Some(id.clone());
                                }
                                DialogState::Early(_, resp) => {
                                    let body = String::from_utf8_lossy(&resp.body);
                                    let recorder_option = if recorder {
                                        let recorder_file = active_call_ref.app_state.get_recorder_file(&session_id);
                                        Some(RecorderOption::new(recorder_file))
                                    } else {
                                        None
                                    };
                                    let rinning_command = Command::Ringing {
                                        ringtone: None,
                                        recorder: recorder_option,
                                        early_media: Some(!body.is_empty()),
                                    };
                                    active_call_ref.enqueue_command(rinning_command).await.ok();
                                }
                                _ => {}
                            }
                            if let Err(_) = refer_dlg_state_sender.send(state) {
                                break;
                            }
                        }
                    } => {}
                }
            };

            let (_, r) = tokio::join!(
                forward_dlg_state_loop,
                client_dialog_event_loop(
                    cancel_token.clone(),
                    session_id.clone(),
                    track_id_clone,
                    event_sender,
                    refer_dlg_state_receiver,
                    call_state,
                    media_stream,
                    active_call_ref.invitation.dialog_layer.clone(),
                )
            );
            info!(session_id, "b2bua callee completed with: {:?}", r);
        });

        let caller = invite_option.caller.clone();
        let callee = invite_option.callee.clone();
        let (dialog_id, answer) = match active_call
            .invitation
            .invite(
                &active_call.event_sender,
                &track_id,
                invite_option,
                dlg_state_sender,
            )
            .await
        {
            Ok((id, ans)) => (id, ans),
            Err(e) => {
                warn!(session_id = self.session_id, %caller, %callee, "callee invite failed: {}", e);
                return Err(anyhow::anyhow!("{}", e));
            }
        };

        let answer = match answer {
            Some(answer) => String::from_utf8_lossy(&answer).to_string(),
            None => {
                warn!(session_id = self.session_id, %caller, %callee, "no answer received");
                return Err(anyhow::anyhow!(
                    "no answer received for dialog: {}",
                    dialog_id
                ));
            }
        };

        active_call
            .media_stream
            .update_remote_description(&track_id, &answer)
            .await
            .ok();

        info!(
            session_id = self.session_id,
            %caller,
            %callee,
            answer,
            "callee answered with SDP");
        Ok(())
    }
}
