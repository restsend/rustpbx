use crate::{
    app::AppState,
    call::{
        self, ActiveCallState, CallOption, Command, CommandSender, DialStrategy, Dialplan,
        Location, RouteInvite, TransactionCookie,
        active_call::{
            ActiveCallStateRef, create_incoming_sip_track, create_stream, dump_events_loop,
            process_call,
        },
        sip::{Invitation, create_rtp_track},
    },
    callrecord::CallRecord,
    config::RouteResult,
    media::{recorder::RecorderOption, track::TrackConfig},
    useragent::invitation::PendingDialog,
};
use anyhow::{Error, Result};
use chrono::Utc;
use ort::session;
use rsip::prelude::HeadersExt;
use rsipstack::{
    dialog::{
        dialog::{DialogState, DialogStateReceiver, DialogStateSender},
        invitation::InviteOption,
    },
    transaction::transaction::Transaction,
};
use std::{
    sync::{Arc, RwLock, atomic::Ordering},
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct B2bua {
    pub cancel_token: CancellationToken,
    pub media_capabilities: Vec<crate::media::codecs::CodecType>,
    pub cookie: TransactionCookie,
    pub session_id: String,
    pub dump_events: bool,
    pub option: CallOption,
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
            recorder: false,
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
        let option = CallOption {
            recorder: if self.recorder {
                let recorder_file = self.app_state.get_recorder_file(&self.session_id);
                Some(RecorderOption::new(recorder_file))
            } else {
                None
            },
            ..CallOption::default()
        };

        let cancel_token = self.cancel_token.unwrap_or_else(CancellationToken::new);
        let b2bua = B2bua {
            cancel_token,
            media_capabilities: self.media_capabilities.unwrap_or_default(),
            cookie: self.cookie,
            session_id: self.session_id,
            dump_events: self.dump_events,
            option,
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
    ) -> Result<CallRecord, (Error, Option<CallRecord>)> {
        let event_sender = crate::event::create_event_sender();
        let dump_cmd_receiver = self.cmd_sender.subscribe();
        let cmd_receiver = self.cmd_sender.subscribe();
        let (state_sender, state_receiver) = mpsc::unbounded_channel();

        let dialog = match invitation.dialog_layer.get_or_create_server_invite(
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
                return Err((anyhow::anyhow!("failed to obtain dialog: {}", e), None));
            }
        };

        let pending_dialog = PendingDialog {
            token: self.cancel_token.clone(),
            dialog,
            state_receiver,
        };

        tx.reply(rsip::StatusCode::Ringing).await.ok(); // send 180 Ringing immediately

        info!(
            session_id = self.session_id,
            capabilities = ?self.media_capabilities,
            "incoming call"
        );

        app_state.total_calls.fetch_add(1, Ordering::Relaxed);
        let track_config = TrackConfig::default();
        let media_stream = Arc::new(
            create_stream(
                self.cancel_token.clone(),
                self.session_id.clone(),
                &self.option,
                &track_config,
                app_state.clone(),
                event_sender.clone(),
            )
            .await
            .map_err(|e| (e, None))?,
        );

        let call_state_ref = Arc::new(RwLock::new(ActiveCallState {
            start_time: Utc::now(),
            option: self.option.clone(),
            ssrc: rand::random::<u32>(),
            ..Default::default()
        }));

        let call_type = call::ActiveCallType::B2bua;
        let process_caller_loop = async {
            let r = process_call(
                self.cancel_token.clone(),
                call_type.clone(),
                track_config.clone(),
                self.session_id.clone(),
                app_state.clone(),
                media_stream.clone(),
                event_sender.clone(),
                None,
                cmd_receiver,
                None,
                call_state_ref.clone(),
                invitation.clone(),
            )
            .await;
            app_state.active_calls.lock().await.remove(&self.session_id);
            self.cancel_token.cancel();
            r
        };
        let original = &tx.original;

        let callee = original
            .to_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;

        let caller = original
            .from_header()
            .map_err(|e| (anyhow::anyhow!(e), None))?
            .uri()
            .map_err(|e| (anyhow::anyhow!(e), None))?;

        let invite_option = InviteOption {
            caller,
            callee,
            destination: None,
            content_type: Some("application/sdp".to_string()),
            offer: Some(original.body().to_vec()),
            contact: caller_contact.uri.clone(),
            credential: None,
            headers: None,
        };

        let (caller_dlg_state_sender, caller_dlg_state_receiver) =
            mpsc::unbounded_channel::<DialogState>();

        let (_, _, _, _, process_call_result) = tokio::join! {
            media_stream.serve(),
            self.process_caller_dialog(
                event_sender.clone(),
                caller_dlg_state_receiver,
                call_state_ref.clone(),
            ),
            dump_events_loop(
                app_state.clone(),
                event_sender.clone(),
                self.cancel_token.clone(),
                self.session_id.clone(),
                self.dump_events,
                dump_cmd_receiver
            ),
            self.process_callee_loop(
                app_state.clone(),
                event_sender.clone(),
                dialplan,
                invitation.clone(),
                invite_option,
                original,
                caller_dlg_state_sender,
                pending_dialog,
                call_state_ref.clone(),),
            process_caller_loop,
        };
        debug!(session_id = self.session_id, "call processing completed");

        match process_call_result {
            Ok(call_record) => Ok(call_record),
            Err(e) => {
                let call_record = call_state_ref
                    .read()
                    .as_ref()
                    .map(|cs| cs.build_callrecord(app_state, self.session_id.clone(), call_type))
                    .ok();
                Err((e, call_record))
            }
        }
    }

    async fn process_caller_dialog(
        &self,
        event_sender: crate::event::EventSender,
        mut dlg_state_receiver: DialogStateReceiver,
        call_state_ref: ActiveCallStateRef,
    ) -> Result<()> {
        let track_id = self.session_id.clone();
        let session_id = &self.session_id;
        while let Some(event) = dlg_state_receiver.recv().await {
            match event {
                DialogState::Trying(dialog_id) => {
                    info!(session_id, "dialog trying: {}", dialog_id);
                }
                DialogState::Early(dialog_id, resp) => {
                    let body = resp.body();
                    let answer = String::from_utf8_lossy(body);
                    info!(session_id, %dialog_id,  ", answer: \n{}", answer);

                    call_state_ref
                        .write()
                        .as_mut()
                        .and_then(|cs| Ok(cs.ring_time.replace(Utc::now())))
                        .ok();

                    event_sender.send(crate::event::SessionEvent::Ringing {
                        track_id: track_id.clone(),
                        timestamp: crate::get_timestamp(),
                        early_media: true,
                    })?;
                }
                DialogState::Calling(dialog_id) => {
                    info!(session_id, track_id, %dialog_id, "dialog calling");
                    call_state_ref
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
                    call_state_ref
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
                    self.cancel_token.cancel(); // Cancel the token to stop any ongoing tasks
                    return Ok(());
                }
                _ => (),
            }
        }
        Err(anyhow::anyhow!("dialog state receiver closed"))
    }

    async fn process_callee_dialog(
        &self,
        cancel_token: CancellationToken,
        event_sender: crate::event::EventSender,
        mut dlg_state_receiver: DialogStateReceiver,
        call_state_ref: ActiveCallStateRef,
    ) -> Result<()> {
        let track_id = self.session_id.clone();
        let session_id = &self.session_id;
        while let Some(event) = dlg_state_receiver.recv().await {
            match event {
                DialogState::Trying(dialog_id) => {
                    info!(session_id, track_id, %dialog_id, "dialog trying");
                }
                DialogState::Early(dialog_id, resp) => {
                    info!(session_id, track_id, %dialog_id, "dialog early");
                    let body = resp.body();
                    let answer = String::from_utf8_lossy(body);
                    info!(session_id, %dialog_id,  "early answer: \n{}", answer);

                    call_state_ref
                        .write()
                        .as_mut()
                        .and_then(|cs| Ok(cs.ring_time.replace(Utc::now())))
                        .ok();

                    event_sender.send(crate::event::SessionEvent::Ringing {
                        track_id: track_id.clone(),
                        timestamp: crate::get_timestamp(),
                        early_media: true,
                    })?;
                }
                DialogState::Calling(dialog_id) => {
                    info!(session_id, track_id, %dialog_id, "dialog calling");
                    call_state_ref
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
                    call_state_ref
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
                    cancel_token.cancel(); // Cancel the token to stop any ongoing tasks
                    return Ok(());
                }
                _ => (),
            }
        }
        Err(anyhow::anyhow!("dialog state receiver closed"))
    }

    async fn process_callee_loop(
        &self,
        app_state: AppState,
        event_sender: crate::event::EventSender,
        dialplan: Dialplan,
        invitation: Invitation,
        invite_option: InviteOption,
        original: &rsip::Request,
        caller_dlg_state_sender: DialogStateSender,
        pending_dialog: PendingDialog,
        call_state_ref: ActiveCallStateRef,
    ) -> Result<()> {
        let max_time = Duration::from_secs(dialplan.max_ring_time.max(60u32) as u64);
        tokio::select! {
            _ = self.cancel_token.cancelled() => {
                info!("B2bua process cancelled");
                return Ok(());
            },
            _ = tokio::time::sleep(max_time) => {
                warn!("Max ring time reached, ending call");
                return Err(anyhow::anyhow!("Max ring time reached"));
            },
            r = async {
                match dialplan.targets {
                    DialStrategy::Sequential(targets) => {
                        match self.process_sequential_callee(app_state, event_sender,
                                &invitation, invite_option, original, targets,
                                dialplan.route_invite,
                            ).await {
                                Ok(r) => {
                                    info!("Callee answered successfully");
                                    return Ok(r);
                                },
                                Err(e) => {
                                    warn!("Failed to process sequential callee: {}", e);
                                    return Err(e);
                                }
                            }
                    }
                    DialStrategy::Parallel(_targets) => {
                        todo!("Parallel dialing not implemented yet");
                    }
                }
            } => {
                r
            }
        }
    }

    async fn process_sequential_callee(
        &self,
        app_state: AppState,
        event_sender: crate::event::EventSender,
        invitation: &Invitation,
        invite_option: InviteOption,
        origin: &rsip::Request,
        targets: Vec<Location>,
        route_invite: Option<Box<dyn RouteInvite>>,
    ) -> Result<()> {
        let ssrc = rand::random::<u32>();
        let track_id = format!("{}-callee-{}", self.session_id, ssrc);
        let track_config = TrackConfig::default();
        let mut rtp_track = create_rtp_track(
            self.cancel_token.clone(),
            app_state.clone(),
            track_id.clone(),
            track_config.clone(),
            ssrc,
        )
        .await?;

        let offer = rtp_track.local_description().ok().unwrap_or_default();

        for target in targets {
            info!(
                session_id = self.session_id,
                calee = %target.aor,
                "dialing target"
            );

            let mut invite_option = invite_option.clone();
            invite_option.callee = target.aor.clone();
            invite_option.destination = Some(target.destination.clone());
            invite_option.credential = target.credential.clone();
            invite_option.headers = target.headers.clone();
            invite_option.offer = Some(offer.clone().into());

            let option = if let Some(route_invite) = &route_invite {
                let route_result = route_invite.route_invite(invite_option, origin).await?;
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

            let (dlg_state_sender, dlg_state_receiver) = mpsc::unbounded_channel();

            info!(
                session_id = self.session_id,
                track_id,
                "invite {} -> {} offer: \n{}",
                option.caller,
                option.callee,
                String::from_utf8_lossy(&option.offer.as_ref().unwrap_or(&vec![]))
            );

            let callee_invite_loop = async {
                let (dialog_id, answer) = invitation
                    .invite(option, dlg_state_sender)
                    .await
                    .map_err(|e| {
                        warn!(session_id = self.session_id, "Invite failed: {}", e);
                        anyhow::anyhow!("Invite failed: {}", e)
                    })?;
                match answer {
                    Some(answer) => {
                        info!(session_id = self.session_id, track_id, %dialog_id, "Received answer for invite");
                        let answer = String::from_utf8_lossy(&answer);
                        match rtp_track.set_remote_description(&answer) {
                            Ok(_) => {
                                return Ok(());
                            }
                            Err(e) => {
                                warn!(track_id, "sip_call:failed to set remote description: {}", e);
                                return Err(anyhow::anyhow!(
                                    "Failed to set remote description: {}",
                                    e
                                ));
                            }
                        }
                    }
                    None => {
                        warn!(track_id, "No answer received for invite");
                        return Ok(()); // Try the next target
                    }
                }
            };

            let calee_call_state = Arc::new(RwLock::new(ActiveCallState {
                start_time: Utc::now(),
                option: self.option.clone(),
                ssrc,
                dialog_id: None,
                ..Default::default()
            }));
            let callee_token = self.cancel_token.child_token();
            let wait_callee_answer = async {
                let (_, r) = tokio::join!(
                    callee_invite_loop,
                    self.process_callee_dialog(
                        callee_token,
                        event_sender.clone(),
                        dlg_state_receiver,
                        calee_call_state.clone()
                    )
                );
                match r {
                    Ok(_) => {
                        info!(session_id = self.session_id, track_id, "Callee answered");
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            session_id = self.session_id,
                            track_id, "Callee invite failed: {}", e
                        );
                        return Err(e);
                    }
                }
            };

            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    info!("B2bua process cancelled");
                    return Ok(());
                },
                r = wait_callee_answer => {
                    match r {
                        Ok(_) => {
                            info!(session_id = self.session_id, track_id, "Callee invite loop completed");
                            return Ok(());
                        }
                        Err(e) => {
                            warn!(session_id = self.session_id, track_id, "Callee invite failed: {}", e);
                            continue; // Try the next target
                        }
                    }
                }
            };
        }
        Ok(())
    }
}
