use super::{CallOption, Command, ReferOption};
use crate::{
    TrackId,
    app::AppState,
    call::{
        CommandReceiver, CommandSender,
        sip::{DialogGuard, Invitation, client_dialog_event_loop, server_dialog_event_loop},
    },
    callrecord::{CallRecord, CallRecordEvent, CallRecordEventType, CallRecordHangupReason},
    event::{EventReceiver, EventSender, SessionEvent},
    media::{
        engine::StreamEngine,
        negotiate::strip_ipv6_candidates,
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder},
        track::{
            Track, TrackConfig,
            file::FileTrack,
            media_pass::MediaPassTrack,
            rtp::{RtpTrack, RtpTrackBuilder},
            tts::SynthesisHandle,
            webrtc::WebrtcTrack,
            websocket::{WebsocketBytesReceiver, WebsocketTrack},
        },
    },
    synthesis::{SynthesisCommand, SynthesisOption},
    useragent::invitation::PendingDialog,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rsipstack::dialog::{invitation::InviteOption, server_dialog::ServerInviteDialog};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{fs::File, select, sync::Mutex, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Deserialize)]
pub struct CallParams {
    pub id: Option<String>,
    #[serde(rename = "dump")]
    pub dump_events: Option<bool>,
    #[serde(rename = "ping")]
    pub ping_interval: Option<u32>,
    pub server_side_track: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ActiveCallType {
    Webrtc,
    B2bua,
    WebSocket,
    Sip,
}
#[derive(Default, Clone)]
pub struct ActiveCallState {
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub last_status_code: u16,
    pub option: Option<CallOption>,
    pub answer: Option<String>,
    pub dialog: Option<DialogGuard>,
    pub ssrc: u32,
    pub refer_callstate: Option<ActiveCallStateRef>,
    pub extras: Option<HashMap<String, serde_json::Value>>,
}

pub type ActiveCallRef = Arc<ActiveCall>;
pub type ActiveCallStateRef = Arc<RwLock<ActiveCallState>>;

pub struct ActiveCall {
    pub call_state: ActiveCallStateRef,
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub track_config: TrackConfig,
    pub tts_handle: Mutex<Option<SynthesisHandle>>,
    pub auto_hangup: Arc<Mutex<Option<(u32, CallRecordHangupReason)>>>,
    pub wait_input_timeout: Arc<Mutex<Option<u32>>>,
    pub event_sender: EventSender,
    pub app_state: AppState,
    pub invitation: Invitation,
    pub cmd_sender: CommandSender,
    pub audio_receiver: Mutex<Option<WebsocketBytesReceiver>>,
    pub dump_events: bool,
    pub server_side_track_id: TrackId,
    can_start_send_command: CancellationToken,
    ready_to_answer: Mutex<Option<(String, Option<Box<dyn Track>>, ServerInviteDialog)>>,
}

impl ActiveCall {
    pub fn new(
        call_type: ActiveCallType,
        cancel_token: CancellationToken,
        session_id: String,
        invitation: Invitation,
        app_state: AppState,
        track_config: TrackConfig,
        audio_receiver: Option<WebsocketBytesReceiver>,
        dump_events: bool,
        server_side_track_id: Option<TrackId>,
        extras: Option<HashMap<String, serde_json::Value>>,
    ) -> Self {
        let event_sender = crate::event::create_event_sender();
        let cmd_sender = tokio::sync::broadcast::Sender::<Command>::new(32);
        let media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.child_token());
        let media_stream = Arc::new(media_stream_builder.build());
        let call_state = Arc::new(RwLock::new(ActiveCallState {
            start_time: Utc::now(),
            ssrc: rand::random::<u32>(),
            extras,
            ..Default::default()
        }));
        Self {
            cancel_token,
            call_type,
            session_id,
            call_state,
            media_stream,
            track_config,
            auto_hangup: Arc::new(Mutex::new(None)),
            wait_input_timeout: Arc::new(Mutex::new(None)),
            event_sender,
            tts_handle: Mutex::new(None),
            app_state,
            invitation,
            cmd_sender,
            audio_receiver: Mutex::new(audio_receiver),
            dump_events,
            server_side_track_id: server_side_track_id.unwrap_or("server-side-track".to_string()),
            can_start_send_command: CancellationToken::new(),
            ready_to_answer: Mutex::new(None),
        }
    }

    pub async fn enqueue_command(&self, command: Command) -> Result<()> {
        if !self.can_start_send_command.is_cancelled() {
            self.can_start_send_command.cancelled().await
        }

        self.cmd_sender
            .send(command)
            .map_err(|e| anyhow::anyhow!("Failed to send command: {}", e))?;
        Ok(())
    }

    pub async fn serve(&self) -> Result<()> {
        let mut cmd_receiver = self.cmd_sender.subscribe();
        let dump_cmd_receiver = self.cmd_sender.subscribe();
        let dump_event_receiver = self.event_sender.subscribe();
        // can `enqueue_command` when subscribe is done
        // so we can start sending commands
        self.can_start_send_command.cancel();

        let process_command_loop = async move {
            while let Ok(command) = cmd_receiver.recv().await {
                match self.dispatch(command).await {
                    Ok(_) => (),
                    Err(e) => {
                        warn!(session_id = self.session_id, "{}", e);
                        self.event_sender
                            .send(SessionEvent::Error {
                                track_id: self.session_id.clone(),
                                timestamp: crate::get_timestamp(),
                                sender: "command".to_string(),
                                error: e.to_string(),
                                code: None,
                            })
                            .ok();
                    }
                }
            }
        };
        self.app_state
            .total_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        tokio::join!(
            self.dump_loop(self.dump_events, dump_cmd_receiver, dump_event_receiver),
            async {
                select! {
                    _ = process_command_loop => {
                        info!(session_id = self.session_id, "command loop done");
                    }
                    _ = self.process() => {
                        info!(session_id = self.session_id, "call serve done");
                    }
                    _ = self.cancel_token.cancelled() => {
                        info!(session_id = self.session_id, "call cancelled - cleaning up resources");
                    }
                }
            }
        );
        self.cleanup().await.ok();
        // Send call record if available
        if let Some(sender) = self.app_state.callrecord_sender.as_ref() {
            if let Err(e) = sender.send(self.get_callrecord().await) {
                warn!(
                    session_id = self.session_id,
                    "failed to send call record: {}", e
                );
            }
        }
        Ok(())
    }

    async fn process(&self) -> Result<()> {
        let mut event_receiver = self.event_sender.subscribe();
        let auto_hangup = self.auto_hangup.clone();
        let wait_input_timeout = self.wait_input_timeout.clone();

        let input_timeout_expire = Arc::new(Mutex::new((0u64, 0u32)));
        let input_timeout_expire_ref = input_timeout_expire.clone();
        let event_sender = self.event_sender.clone();
        let wait_input_timeout_loop = async {
            loop {
                let (start_time, expire) = { *input_timeout_expire.lock().await };
                if expire > 0 && crate::get_timestamp() >= start_time + expire as u64 {
                    info!(session_id = self.session_id, "wait input timeout reached");
                    *input_timeout_expire.lock().await = (0, 0);
                    event_sender
                        .send(SessionEvent::Silence {
                            track_id: self.server_side_track_id.clone(),
                            timestamp: crate::get_timestamp(),
                            start_time,
                            duration: expire as u64,
                            samples: None,
                        })
                        .ok();
                }
                sleep(Duration::from_millis(100)).await;
            }
        };
        let server_side_track_id = &self.server_side_track_id;
        let event_hook_loop = async move {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    SessionEvent::Speaking { .. }
                    | SessionEvent::Dtmf { .. }
                    | SessionEvent::AsrDelta { .. }
                    | SessionEvent::AsrFinal { .. }
                    | SessionEvent::TrackStart { .. } => {
                        *input_timeout_expire_ref.lock().await = (0, 0);
                    }
                    SessionEvent::TrackEnd { track_id, ssrc, .. } => {
                        if &track_id != server_side_track_id {
                            continue;
                        }
                        let mut auto_hangup_ref = auto_hangup.lock().await;
                        if let Some(ref auto_hangup_ssrc) = *auto_hangup_ref {
                            if auto_hangup_ssrc.0 == ssrc {
                                let auto_hangup_ssrc = auto_hangup_ref.take();
                                match auto_hangup_ssrc {
                                    Some((_, auto_hangup_reason)) => {
                                        info!(
                                            session_id = self.session_id,
                                            ssrc,
                                            "auto hangup when track end track_id:{}",
                                            track_id
                                        );
                                        self.do_hangup(Some(auto_hangup_reason), None).await.ok();
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if let Some(timeout) = wait_input_timeout.lock().await.take() {
                            let expire = if timeout > 0 {
                                (crate::get_timestamp(), timeout)
                            } else {
                                (0, 0)
                            };
                            *input_timeout_expire_ref.lock().await = expire;
                        }
                    }
                    _ => {}
                }
            }
        };

        select! {
            _ = wait_input_timeout_loop=>{
                info!(session_id = self.session_id, "wait input timeout loop done");
            }
            _ = self.media_stream.serve() => {
                info!(session_id = self.session_id, "media stream loop done");
            }
            _ = event_hook_loop => {
                info!(session_id = self.session_id, "event loop done");
            }
            _ = self.cancel_token.cancelled() => {
                info!(session_id = self.session_id, "event loop cancelled");
            }
        }
        Ok(())
    }

    async fn dispatch(&self, command: Command) -> Result<()> {
        match command {
            Command::Invite { option } => self.do_invite(option).await,
            Command::Accept { option } => self.do_accept(option).await,
            Command::Reject { reason, code } => self.do_reject(reason, code).await,
            Command::Ringing {
                ringtone,
                recorder,
                early_media,
            } => self.do_ringing(ringtone, recorder, early_media).await,
            Command::Tts {
                text,
                speaker,
                play_id,
                auto_hangup,
                streaming,
                end_of_stream,
                option,
                wait_input_timeout,
            } => {
                self.do_tts(
                    text,
                    speaker,
                    play_id,
                    auto_hangup,
                    streaming,
                    end_of_stream,
                    option,
                    wait_input_timeout,
                )
                .await
            }
            Command::Play {
                url,
                auto_hangup,
                wait_input_timeout,
            } => self.do_play(url, auto_hangup, wait_input_timeout).await,
            Command::Hangup { reason, initiator } => {
                let reason = reason.map(|r| {
                    r.parse::<CallRecordHangupReason>()
                        .unwrap_or(CallRecordHangupReason::BySystem)
                });
                self.do_hangup(reason, initiator).await
            }
            Command::Refer {
                caller,
                callee,
                options,
            } => self.do_refer(caller, callee, options).await,
            Command::Mute { track_id } => self.do_mute(track_id).await,
            Command::Unmute { track_id } => self.do_unmute(track_id).await,
            Command::Pause {} => self.do_pause().await,
            Command::Resume {} => self.do_resume().await,
            Command::Interrupt {} => self.do_interrupt().await,
            Command::History { speaker, text } => self.do_history(speaker, text).await,
        }
    }

    fn build_record_option(&self, option: &CallOption) -> Option<RecorderOption> {
        if let Some(recorder_option) = &option.recorder {
            let recorder_file = if recorder_option.recorder_file.is_empty() {
                self.app_state.get_recorder_file(&self.session_id)
            } else {
                let p = Path::new(&recorder_option.recorder_file);
                p.is_absolute()
                    .then(|| recorder_option.recorder_file.clone())
                    .unwrap_or_else(|| {
                        self.app_state
                            .get_recorder_file(&recorder_option.recorder_file)
                    })
            };
            info!(
                session_id = self.session_id,
                recorder_file, "created recording file"
            );

            let track_samplerate = self.track_config.samplerate;
            let recorder_samplerate = if track_samplerate > 0 {
                track_samplerate
            } else {
                recorder_option.samplerate
            };
            let recorder_ptime = if recorder_option.ptime.is_zero() {
                Duration::from_millis(200)
            } else {
                recorder_option.ptime
            };
            let recorder_config = RecorderOption {
                recorder_file,
                samplerate: recorder_samplerate,
                ptime: recorder_ptime,
            };
            Some(recorder_config)
        } else {
            None
        }
    }

    async fn invite_or_accept(&self, mut option: CallOption, sender: String) -> Result<CallOption> {
        option.check_default();
        if let Some(opt) = self.build_record_option(&option) {
            self.media_stream.update_recorder_option(opt).await;
        }

        if let Some(opt) = &option.media_pass {
            let track_id = self.server_side_track_id.clone();
            let cancel_token = self.cancel_token.child_token();
            let ssrc = rand::random::<u32>();
            let media_pass_track = MediaPassTrack::new(ssrc, track_id, cancel_token, opt.clone());
            self.media_stream
                .update_track(Box::new(media_pass_track), None)
                .await;
        }

        info!(
            session_id = self.session_id,
            call_type = ?self.call_type,
            sender,
            ?option,
            "caller with option"
        );

        match self.setup_caller_track(option.clone()).await {
            Ok(_) => return Ok(option),
            Err(e) => {
                self.app_state
                    .total_failed_calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let error_event = crate::event::SessionEvent::Error {
                    track_id: self.session_id.clone(),
                    timestamp: crate::get_timestamp(),
                    sender,
                    error: e.to_string(),
                    code: None,
                };
                self.event_sender.send(error_event).ok();
                self.do_hangup(Some(CallRecordHangupReason::BySystem), None)
                    .await
                    .ok();
                return Err(e);
            }
        }
    }

    async fn do_invite(&self, option: CallOption) -> Result<()> {
        self.invite_or_accept(option, "invite".to_string())
            .await
            .map(|_| ())
    }

    async fn do_accept(&self, mut option: CallOption) -> Result<()> {
        if self.ready_to_answer.lock().await.is_none() {
            option = self.invite_or_accept(option, "accept".to_string()).await?;
        } else {
            option.check_default();
            self.call_state
                .write()
                .as_mut()
                .map_err(|e| anyhow::anyhow!("{}", e))?
                .option = Some(option.clone());
        }

        if let Some((answer, track, dialog)) = self.ready_to_answer.lock().await.take() {
            info!(
                session_id = self.session_id,
                track_id = track.as_ref().map(|t| t.id()),
                "ready to answer with track"
            );

            let headers = vec![rsip::Header::ContentType(
                "application/sdp".to_string().into(),
            )];

            match dialog.accept(Some(headers), Some(answer.as_bytes().to_vec())) {
                Ok(_) => {
                    self.finish_caller_stack(&option, track).await?;
                }
                Err(e) => {
                    warn!(session_id = self.session_id, "failed to accept call: {}", e);
                    return Err(anyhow::anyhow!("failed to accept call"));
                }
            }
        }
        return Ok(());
    }

    async fn do_reject(&self, reason: String, code: Option<u32>) -> Result<()> {
        match self.invitation.has_pending_call(&self.session_id).await {
            Some(id) => {
                info!(session_id = self.session_id, reason, code, "rejecting call");
                self.invitation.hangup(id).await
            }
            None => Ok(()),
        }
    }

    async fn do_ringing(
        &self,
        ringtone: Option<String>,
        recorder: Option<RecorderOption>,
        early_media: Option<bool>,
    ) -> Result<()> {
        if self.ready_to_answer.lock().await.is_none() {
            let option = CallOption {
                recorder,
                ..Default::default()
            };
            let _ = self.invite_or_accept(option, "ringing".to_string()).await?;
        }

        if let Some((answer, _, dialog)) = self.ready_to_answer.lock().await.as_ref() {
            let (headers, body) = if early_media.unwrap_or_default() || ringtone.is_some() {
                let headers = vec![rsip::Header::ContentType(
                    "application/sdp".to_string().into(),
                )];
                (Some(headers), Some(answer.as_bytes().to_vec()))
            } else {
                (None, None)
            };

            dialog.ringing(headers, body).ok();
            info!(
                session_id = self.session_id,
                ringtone, early_media, "playing ringtone"
            );
            if let Some(ringtone) = ringtone {
                self.do_play(ringtone, None, None).await.ok();
            } else {
                info!(session_id = self.session_id, "no ringtone to play");
            }
        }
        Ok(())
    }

    async fn do_tts(
        &self,
        text: String,
        speaker: Option<String>,
        play_id: Option<String>,
        auto_hangup: Option<bool>,
        streaming: Option<bool>,
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
        wait_input_timeout: Option<u32>,
    ) -> Result<()> {
        let tts_option = match self.call_state.read() {
            Ok(ref call_state) => match call_state.option.clone().unwrap_or_default().tts {
                Some(opt) => opt.merge_with(option),
                None => {
                    if let Some(opt) = option {
                        opt
                    } else {
                        return Err(anyhow::anyhow!("no tts option available"));
                    }
                }
            },
            Err(_) => return Err(anyhow::anyhow!("failed to read call state")),
        };
        let speaker = match speaker {
            Some(s) => Some(s),
            None => tts_option.speaker.clone(),
        };

        let mut play_command = SynthesisCommand {
            text,
            speaker,
            play_id: play_id.clone(),
            streaming,
            end_of_stream,
            option: tts_option,
        };
        info!(
            session_id = self.session_id,
            provider = ?play_command.option.provider,
            text = %play_command.text,
            speaker = play_command.speaker.as_deref(),
            auto_hangup = auto_hangup.unwrap_or_default(),
            play_id = play_command.play_id.as_deref(),
            streaming = play_command.streaming,
            eos = play_command.end_of_stream.unwrap_or_default(),
            wit = wait_input_timeout.unwrap_or_default(),
            "new synthesis"
        );

        let ssrc = rand::random::<u32>();
        match auto_hangup {
            Some(true) => {
                *self.auto_hangup.lock().await = Some((ssrc, CallRecordHangupReason::BySystem))
            }
            _ => *self.auto_hangup.lock().await = None,
        }
        *self.wait_input_timeout.lock().await = wait_input_timeout;

        if let Some(tts_handle) = self.tts_handle.lock().await.as_ref() {
            match tts_handle.try_send(play_command) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    play_command = e.0;
                }
            }
        }

        let (new_handle, tts_track) = StreamEngine::create_tts_track(
            self.app_state.stream_engine.clone(),
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.server_side_track_id.clone(),
            ssrc,
            play_id.clone(),
            &play_command.option,
        )
        .await?;

        new_handle.try_send(play_command)?;
        *self.tts_handle.lock().await = Some(new_handle);
        self.media_stream.update_track(tts_track, play_id).await;
        Ok(())
    }

    async fn do_play(
        &self,
        url: String,
        auto_hangup: Option<bool>,
        wait_input_timeout: Option<u32>,
    ) -> Result<()> {
        self.tts_handle.lock().await.take();
        let ssrc = rand::random::<u32>();
        info!(
            session_id = self.session_id,
            ssrc, url, auto_hangup, "play file track"
        );

        let file_track = FileTrack::new(self.server_side_track_id.clone())
            .with_ssrc(ssrc)
            .with_path(url.clone())
            .with_cancel_token(self.cancel_token.child_token());
        match auto_hangup {
            Some(true) => {
                *self.auto_hangup.lock().await = Some((ssrc, CallRecordHangupReason::BySystem))
            }
            _ => *self.auto_hangup.lock().await = None,
        }
        *self.wait_input_timeout.lock().await = wait_input_timeout;
        self.media_stream
            .update_track(Box::new(file_track), Some(url))
            .await;
        Ok(())
    }

    async fn do_history(&self, speaker: String, text: String) -> Result<()> {
        self.event_sender
            .send(SessionEvent::AddHistory {
                sender: Some(self.session_id.clone()),
                timestamp: crate::get_timestamp(),
                speaker,
                text,
            })
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn do_interrupt(&self) -> Result<()> {
        self.tts_handle.lock().await.take();
        self.media_stream
            .remove_track(&self.server_side_track_id)
            .await;
        Ok(())
    }
    async fn do_pause(&self) -> Result<()> {
        Ok(())
    }
    async fn do_resume(&self) -> Result<()> {
        Ok(())
    }
    async fn do_hangup(
        &self,
        reason: Option<CallRecordHangupReason>,
        initiator: Option<String>,
    ) -> Result<()> {
        info!(
            session_id = self.session_id,
            ?reason,
            ?initiator,
            "do_hangup"
        );

        // Set hangup reason based on initiator and reason
        let hangup_reason = match initiator.as_deref() {
            Some("caller") => CallRecordHangupReason::ByCaller,
            Some("callee") => CallRecordHangupReason::ByCallee,
            Some("system") => CallRecordHangupReason::Autohangup,
            _ => reason.unwrap_or(CallRecordHangupReason::BySystem),
        };

        self.tts_handle.lock().await.take();
        self.media_stream
            .stop(Some(hangup_reason.to_string()), initiator);

        match self.call_state.write().as_mut() {
            Ok(call_state) => {
                if call_state.hangup_reason.is_none() {
                    call_state.hangup_reason.replace(hangup_reason);
                }
                call_state.dialog.take()
            }
            Err(_) => None,
        };
        self.cancel_token.cancel();
        Ok(())
    }

    async fn do_refer(
        &self,
        caller: String,
        callee: String,
        refer_option: Option<ReferOption>,
    ) -> Result<()> {
        if let Some(moh) = refer_option.as_ref().and_then(|o| o.moh.clone()) {
            self.do_play(moh, None, None).await?;
        }
        self.tts_handle.lock().await.take();
        let token = self.cancel_token.child_token();
        let session_id = self.session_id.clone();
        let track_id = self.server_side_track_id.clone();

        let call_option = CallOption {
            caller: Some(caller),
            callee: Some(callee.clone()),
            sip: refer_option.as_ref().and_then(|o| o.sip.clone()),
            asr: refer_option.as_ref().and_then(|o| o.asr.clone()),
            denoise: refer_option.as_ref().and_then(|o| o.denoise.clone()),
            recorder: self
                .call_state
                .read()
                .as_ref()
                .map(|cs| {
                    cs.option
                        .as_ref()
                        .map(|o| o.recorder.clone())
                        .unwrap_or_default()
                })
                .ok()
                .flatten(),
            ..Default::default()
        };

        let invite_option = call_option.build_invite_option()?;

        let ssrc = rand::random::<u32>();
        let refer_call_state = Arc::new(RwLock::new(ActiveCallState {
            start_time: Utc::now(),
            ssrc,
            option: Some(call_option),
            ..Default::default()
        }));

        self.call_state
            .write()
            .as_mut()
            .and_then(|cs| {
                cs.refer_callstate.replace(refer_call_state.clone());
                Ok(())
            })
            .ok();

        let auto_hangup = refer_option
            .as_ref()
            .and_then(|o| o.auto_hangup)
            .unwrap_or(true);

        if auto_hangup {
            *self.auto_hangup.lock().await = Some((ssrc, CallRecordHangupReason::ByRefer));
        } else {
            *self.auto_hangup.lock().await = None;
        }

        info!(
            session_id = self.session_id,
            ssrc, auto_hangup, callee, "do_refer"
        );

        match self
            .create_outgoing_sip_track(
                token.clone(),
                refer_call_state.clone(),
                &track_id,
                invite_option,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                warn!(
                    session_id = session_id,
                    "failed to create refer sip track: {}", e
                );
                return Err(e);
            }
        }
        Ok(())
    }

    async fn do_mute(&self, track_id: Option<String>) -> Result<()> {
        self.media_stream.mute_track(track_id).await;
        Ok(())
    }

    async fn do_unmute(&self, track_id: Option<String>) -> Result<()> {
        self.media_stream.unmute_track(track_id).await;
        Ok(())
    }

    pub async fn cleanup(&self) -> Result<()> {
        self.call_state.write().as_mut().ok().map(|cs| {
            cs.dialog.take();
            cs.refer_callstate
                .as_mut()
                .map(|rcs| rcs.write().as_mut().ok().map(|rcs| rcs.dialog.take()))
        });

        self.tts_handle.lock().await.take();
        self.media_stream.cleanup().await.ok();
        self.cancel_token.cancel();
        Ok(())
    }

    pub async fn get_callrecord(&self) -> CallRecord {
        let call_state = self.call_state.read().unwrap();
        call_state.build_callrecord(
            self.app_state.clone(),
            self.session_id.clone(),
            self.call_type.clone(),
        )
    }

    async fn dump_to_file(
        &self,
        dump_file: &mut File,
        cmd_receiver: &mut CommandReceiver,
        event_receiver: &mut EventReceiver,
    ) {
        loop {
            select! {
                _ = self.cancel_token.cancelled() => {
                    break;
                }
                Ok(cmd) = cmd_receiver.recv() => {
                    let text = match serde_json::to_string(&cmd){
                        Ok(text) => text,
                        Err(_) => {
                            continue;
                        }
                    };
                    CallRecordEvent::new(CallRecordEventType::Command, &text)
                        .write_to_file(dump_file)
                        .await;
                }
                Ok(event) = event_receiver.recv() => {
                    if matches!(event, SessionEvent::Binary{..}) {
                        continue;
                    }
                    let text = match serde_json::to_string(&event) {
                        Ok(text) => text,
                        Err(_) => {
                            continue;
                        }
                    };
                    CallRecordEvent::new(CallRecordEventType::Event, &text)
                        .write_to_file(dump_file)
                        .await;
                }
            };
        }
    }

    async fn dump_loop(
        &self,
        dump_events: bool,
        mut dump_cmd_receiver: CommandReceiver,
        mut dump_event_receiver: EventReceiver,
    ) {
        if !dump_events {
            return;
        }

        let file_name = self.app_state.get_dump_events_file(&self.session_id);
        let mut dump_file = match File::options()
            .create(true)
            .append(true)
            .open(&file_name)
            .await
        {
            Ok(file) => file,
            Err(e) => {
                warn!(
                    session_id = self.session_id,
                    file_name, "failed to open dump events file: {}", e
                );
                return;
            }
        };
        self.dump_to_file(
            &mut dump_file,
            &mut dump_cmd_receiver,
            &mut dump_event_receiver,
        )
        .await;

        while let Ok(event) = dump_event_receiver.try_recv() {
            if matches!(event, SessionEvent::Binary { .. }) {
                continue;
            }
            let text = match serde_json::to_string(&event) {
                Ok(text) => text,
                Err(_) => {
                    continue;
                }
            };
            CallRecordEvent::new(CallRecordEventType::Event, &text)
                .write_to_file(&mut dump_file)
                .await;
        }
    }
}

impl ActiveCall {
    pub async fn create_rtp_track(
        cancel_token: CancellationToken,
        app_state: AppState,
        track_id: TrackId,
        track_config: TrackConfig,
        ssrc: u32,
    ) -> Result<RtpTrack> {
        let mut rtp_track = RtpTrackBuilder::new(track_id, track_config)
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

    async fn setup_caller_track(&self, option: CallOption) -> Result<()> {
        self.call_state
            .write()
            .as_mut()
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .option = Some(option.clone());
        info!(
            session_id = self.session_id,
            call_type = ?self.call_type,
            "setup caller track"
        );

        let track = match self.call_type {
            ActiveCallType::Webrtc => Some(self.create_webrtc_track().await?),
            ActiveCallType::WebSocket => {
                let audio_receiver = self.audio_receiver.lock().await.take();
                if let Some(receiver) = audio_receiver {
                    Some(self.create_websocket_track(receiver).await?)
                } else {
                    None
                }
            }
            ActiveCallType::Sip => {
                if let Some(pending_dialog) =
                    self.invitation.get_pending_call(&self.session_id).await
                {
                    return self
                        .prepare_incoming_sip_track(
                            self.cancel_token.clone(),
                            self.call_state.clone(),
                            &self.session_id,
                            pending_dialog,
                        )
                        .await;
                }

                let invite_option =
                    match self.call_state.read().as_ref().map(|cs| cs.option.as_ref()) {
                        Ok(Some(option)) => option.build_invite_option()?,
                        _ => return Err(anyhow::anyhow!("call option not found")),
                    };

                match self
                    .create_outgoing_sip_track(
                        self.cancel_token.clone(),
                        self.call_state.clone(),
                        &self.session_id,
                        invite_option,
                    )
                    .await
                {
                    Ok(answer) => {
                        self.event_sender
                            .send(SessionEvent::Answer {
                                timestamp: crate::get_timestamp(),
                                track_id: self.session_id.clone(),
                                sdp: answer,
                            })
                            .ok();
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            session_id = self.session_id,
                            "failed to create sip track: {}", e
                        );
                        return Err(e);
                    }
                }
            }
            ActiveCallType::B2bua => {
                match self.invitation.get_pending_call(&self.session_id).await {
                    Some(pending_dialog) => {
                        return self
                            .prepare_incoming_sip_track(
                                self.cancel_token.clone(),
                                self.call_state.clone(),
                                &self.session_id,
                                pending_dialog,
                            )
                            .await;
                    }
                    None => {
                        warn!(
                            session_id = self.session_id,
                            "no pending dialog found for B2BUA call"
                        );
                        return Err(anyhow::anyhow!(
                            "no pending dialog found for session_id: {}",
                            self.session_id
                        ));
                    }
                }
            }
        };
        match track {
            Some(track) => {
                self.finish_caller_stack(&option, Some(track)).await?;
            }
            None => {
                warn!(session_id = self.session_id, "no track created for caller");
                return Err(anyhow::anyhow!("no track created for caller"));
            }
        }
        Ok(())
    }

    async fn finish_caller_stack(
        &self,
        option: &CallOption,
        track: Option<Box<dyn Track>>,
    ) -> Result<()> {
        if let Some(track) = track {
            Self::setup_track_with_stream(
                self.app_state.clone(),
                self.cancel_token.child_token(),
                self.media_stream.clone(),
                self.event_sender.clone(),
                &self.session_id,
                &option,
                track,
            )
            .await?;
        }

        match self.call_state.read() {
            Ok(call_state) => {
                if let Some(ref answer) = call_state.answer {
                    info!(session_id = self.session_id, "call answer: {}", answer,);
                    self.event_sender
                        .send(SessionEvent::Answer {
                            timestamp: crate::get_timestamp(),
                            track_id: self.session_id.clone(),
                            sdp: answer.clone(),
                        })
                        .ok();
                }
            }
            Err(e) => {
                warn!(
                    session_id = self.session_id,
                    "failed to read call state: {}", e
                );
            }
        }
        Ok(())
    }
    pub async fn setup_track_with_stream(
        app_state: AppState,
        cancel_token: CancellationToken,
        media_stream: Arc<MediaStream>,
        event_sender: EventSender,
        session_id: &String,
        option: &CallOption,
        mut track: Box<dyn Track>,
    ) -> Result<()> {
        let processors = match StreamEngine::create_processors(
            app_state.stream_engine.clone(),
            track.as_ref(),
            cancel_token.clone(),
            event_sender.clone(),
            option,
        )
        .await
        {
            Ok(processors) => processors,
            Err(e) => {
                warn!(session_id, "failed to prepare stream processors: {}", e);
                vec![]
            }
        };

        // Add all processors from the hook
        for processor in processors {
            track.append_processor(processor);
        }

        media_stream.update_track(track, None).await;
        Ok(())
    }

    pub async fn create_websocket_track(
        &self,
        audio_receiver: WebsocketBytesReceiver,
    ) -> Result<Box<dyn Track>> {
        let (ssrc, codec) = {
            let call_state = self
                .call_state
                .read()
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            (
                call_state.ssrc,
                call_state
                    .option
                    .as_ref()
                    .map(|o| o.codec.clone())
                    .unwrap_or_default(),
            )
        };

        let ws_track = WebsocketTrack::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.track_config.clone(),
            self.event_sender.clone(),
            audio_receiver,
            codec,
            ssrc,
        );

        self.call_state
            .write()
            .as_mut()
            .and_then(|call_state| {
                call_state.answer_time = Some(Utc::now());
                call_state.answer = Some("".to_string());
                call_state.last_status_code = 200;
                Ok(())
            })
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        Ok(Box::new(ws_track))
    }

    pub(super) async fn create_webrtc_track(&self) -> Result<Box<dyn Track>> {
        let (ssrc, option) = {
            let call_state = self
                .call_state
                .read()
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            (
                call_state.ssrc,
                call_state.option.clone().unwrap_or_default(),
            )
        };

        let mut webrtc_track = WebrtcTrack::new(
            self.cancel_token.child_token(),
            self.session_id.clone(),
            self.track_config.clone(),
            self.app_state.config.ice_servers.clone(),
        )
        .with_ssrc(ssrc);

        let timeout = option
            .handshake_timeout
            .as_ref()
            .map(|d| d.parse::<u64>().map(|d| Duration::from_secs(d)).ok())
            .flatten();

        let offer = match option.enable_ipv6 {
            Some(false) | None => {
                strip_ipv6_candidates(option.offer.as_ref().unwrap_or(&"".to_string()))
            }
            _ => option.offer.clone().unwrap_or("".to_string()),
        };
        let answer: Option<String>;
        match webrtc_track.handshake(offer, timeout).await {
            Ok(answer_sdp) => {
                answer = match option.enable_ipv6 {
                    Some(false) | None => Some(strip_ipv6_candidates(&answer_sdp)),
                    Some(true) => Some(answer_sdp),
                };
            }
            Err(e) => {
                warn!(session_id = self.session_id, "failed to setup track: {}", e);
                return Err(anyhow::anyhow!("Failed to setup track: {}", e));
            }
        }

        self.call_state
            .write()
            .as_mut()
            .and_then(|call_state| {
                call_state.answer_time = Some(Utc::now());
                call_state.answer = answer;
                call_state.last_status_code = 200;
                Ok(())
            })
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(Box::new(webrtc_track))
    }

    async fn create_outgoing_sip_track(
        &self,
        cancel_token: CancellationToken,
        call_state_ref: ActiveCallStateRef,
        track_id: &String,
        mut invite_option: InviteOption,
    ) -> Result<String> {
        let ssrc = call_state_ref
            .read()
            .map_err(|e| anyhow::anyhow!("{}", e))?
            .ssrc;
        let rtp_track = Self::create_rtp_track(
            cancel_token.child_token(),
            self.app_state.clone(),
            track_id.clone(),
            self.track_config.clone(),
            ssrc,
        )
        .await?;

        let offer = rtp_track.local_description().ok();
        let call_option = call_state_ref
            .write()
            .as_mut()
            .ok()
            .map(|cs| {
                cs.option.as_mut().map(|o| {
                    o.offer = offer.clone();
                });
                cs.start_time = Utc::now();
                cs.option.clone()
            })
            .flatten()
            .unwrap_or_default();

        invite_option.offer = offer.clone().map(|s| s.into());

        Self::setup_track_with_stream(
            self.app_state.clone(),
            cancel_token.clone(),
            self.media_stream.clone(),
            self.event_sender.clone(),
            &self.session_id,
            &call_option,
            Box::new(rtp_track),
        )
        .await?;

        info!(
            session_id = self.session_id,
            track_id,
            contact = %invite_option.contact,
            "invite {} -> {} offer: \n{}",
            invite_option.caller,
            invite_option.callee,
            offer.as_ref().map(|s| s.as_str()).unwrap_or("<NO OFFER>")
        );

        let (dlg_state_sender, dlg_state_receiver) = tokio::sync::mpsc::unbounded_channel();
        let session_id = self.session_id.clone();
        let event_sender = self.event_sender.clone();
        let media_stream = self.media_stream.clone();
        let call_state = call_state_ref.clone();
        let track_id_clone = track_id.clone();
        let dialog_layer = self.invitation.dialog_layer.clone();
        tokio::spawn(async move {
            client_dialog_event_loop(
                cancel_token,
                session_id,
                track_id_clone,
                event_sender,
                dlg_state_receiver,
                call_state,
                media_stream,
                dialog_layer,
            )
            .await
            .ok();
        });

        let (dialog_id, answer) = self
            .invitation
            .invite(invite_option, dlg_state_sender)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let answer = match answer {
            Some(answer) => String::from_utf8_lossy(&answer).to_string(),
            None => {
                warn!(session_id = self.session_id, "no answer received");
                return Err(anyhow::anyhow!(
                    "no answer received for dialog: {}",
                    dialog_id
                ));
            }
        };

        self.media_stream
            .update_remote_description(&track_id, &answer)
            .await
            .ok();
        Ok(answer)
    }

    /// Detect if SDP is WebRTC format
    pub fn is_webrtc_sdp(sdp: &str) -> bool {
        (sdp.contains("a=ice-ufrag:") || sdp.contains("a=ice-pwd:"))
            && sdp.contains("a=fingerprint:")
    }

    pub async fn setup_answer_track(
        &self,
        ssrc: u32,
        option: &CallOption,
        offer: String,
    ) -> Result<(String, Box<dyn Track>)> {
        let offer = match option.enable_ipv6 {
            Some(false) | None => strip_ipv6_candidates(&offer),
            _ => offer.clone(),
        };

        let timeout = option
            .handshake_timeout
            .as_ref()
            .map(|d| d.parse::<u64>().map(|d| Duration::from_secs(d)).ok())
            .flatten();

        let mut media_track = if Self::is_webrtc_sdp(&offer) {
            let webrtc_track = WebrtcTrack::new(
                self.cancel_token.clone(),
                self.session_id.clone(),
                self.track_config.clone(),
                self.app_state.config.ice_servers.clone(),
            )
            .with_ssrc(ssrc);
            Box::new(webrtc_track) as Box<dyn Track>
        } else {
            let rtp_track = Self::create_rtp_track(
                self.cancel_token.clone(),
                self.app_state.clone(),
                self.session_id.clone(),
                self.track_config.clone(),
                ssrc,
            )
            .await?;
            Box::new(rtp_track) as Box<dyn Track>
        };
        let answer = match media_track.handshake(offer.clone(), timeout).await {
            Ok(answer) => answer,
            Err(e) => {
                return Err(anyhow::anyhow!("handshake failed: {e}"));
            }
        };

        return Ok((answer, media_track));
    }

    pub async fn prepare_incoming_sip_track(
        &self,
        cancel_token: CancellationToken,
        call_state_ref: ActiveCallStateRef,
        track_id: &String,
        pending_dialog: PendingDialog,
    ) -> Result<()> {
        let (dlg_state_sender, dlg_state_receiver) = tokio::sync::mpsc::unbounded_channel();
        let session_id = self.session_id.clone();
        let event_sender = self.event_sender.clone();
        let call_state = self.call_state.clone();
        let track_id = track_id.clone();

        let mut state_receiver = pending_dialog.state_receiver;
        let pending_token_clone = pending_dialog.token;
        let token = self.cancel_token.clone();

        let initial_request = pending_dialog.dialog.initial_request();
        let offer = String::from_utf8_lossy(&initial_request.body).to_string();

        let (ssrc, option) = {
            let call_state = call_state_ref
                .read()
                .map_err(|e| anyhow::anyhow!("{}", e))?;
            (
                call_state.ssrc,
                call_state.option.clone().unwrap_or_default(),
            )
        };

        match self.setup_answer_track(ssrc, &option, offer).await {
            Ok((offer, track)) => {
                Self::setup_track_with_stream(
                    self.app_state.clone(),
                    self.cancel_token.child_token(),
                    self.media_stream.clone(),
                    self.event_sender.clone(),
                    &self.session_id,
                    &option,
                    track,
                )
                .await?;
                self.ready_to_answer
                    .lock()
                    .await
                    .replace((offer, None, pending_dialog.dialog));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("error creating track: {}", e));
            }
        }
        let dialog_layer = self.invitation.dialog_layer.clone();
        tokio::spawn(async move {
            let forward_dlg_state_loop = async {
                tokio::select! {
                    _ = token.cancelled() => {
                        info!(session_id, "call cancelled" );
                    }
                    _ = pending_token_clone.cancelled() => {
                        info!(session_id, "pending token cancelled" );
                    }
                    _ = async {
                        while let Some(state) = state_receiver.recv().await {
                            if let Err(_) = dlg_state_sender.send(state) {
                                break;
                            }
                        }
                    } => {}
                }
            };

            tokio::join!(
                forward_dlg_state_loop,
                server_dialog_event_loop(
                    cancel_token,
                    session_id.clone(),
                    track_id,
                    event_sender,
                    dlg_state_receiver,
                    call_state,
                    dialog_layer,
                )
            )
        });
        Ok(())
    }
}

impl ActiveCallState {
    pub fn build_hangup_event(
        &self,
        track_id: TrackId,
        initiator: Option<String>,
    ) -> crate::event::SessionEvent {
        let from = self.option.as_ref().and_then(|o| o.caller.as_ref());
        let to = self.option.as_ref().and_then(|o| o.callee.as_ref());
        let extra = self.extras.clone();

        crate::event::SessionEvent::Hangup {
            track_id,
            timestamp: crate::get_timestamp(),
            reason: Some(format!("{:?}", self.hangup_reason)),
            initiator,
            start_time: self.start_time.to_rfc3339(),
            answer_time: self.answer_time.map(|t| t.to_rfc3339()),
            ringing_time: self.ring_time.map(|t| t.to_rfc3339()),
            hangup_time: Utc::now().to_rfc3339(),
            extra,
            from: from.map(|f| f.into()),
            to: to.map(|f| f.into()),
        }
    }

    pub fn build_callrecord(
        &self,
        app_state: AppState,
        session_id: String,
        call_type: ActiveCallType,
    ) -> CallRecord {
        let option = self.option.clone().unwrap_or_default();
        let recorder = if option.recorder.is_some() {
            let recorder_file = app_state.get_recorder_file(&session_id);
            if std::path::Path::new(&recorder_file).exists() {
                let file_size = std::fs::metadata(&recorder_file)
                    .map(|m| m.len())
                    .unwrap_or(0);
                vec![crate::callrecord::CallRecordMedia {
                    track_id: session_id.clone(),
                    path: recorder_file,
                    size: file_size,
                    extra: None,
                }]
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let dump_event_file = app_state.get_dump_events_file(&session_id);
        let dump_event_file = if std::path::Path::new(&dump_event_file).exists() {
            Some(dump_event_file)
        } else {
            None
        };

        let refer_callrecord = self.refer_callstate.as_ref().and_then(|rc| {
            let rc = rc.read().unwrap();
            if rc.start_time != Utc::now() {
                let call_id = rc
                    .dialog
                    .as_ref()
                    .map(|d| d.id().to_string())
                    .unwrap_or_default();
                Some(Box::new(rc.build_callrecord(
                    app_state.clone(),
                    call_id,
                    ActiveCallType::B2bua,
                )))
            } else {
                None
            }
        });

        let offer = option.offer.clone();
        let caller = option.caller.clone().unwrap_or_default();
        let callee = option.callee.clone().unwrap_or_default();

        CallRecord {
            option: Some(option),
            call_id: session_id,
            call_type,
            start_time: self.start_time,
            ring_time: self.ring_time.clone(),
            answer_time: self.answer_time.clone(),
            end_time: Utc::now(),
            caller,
            callee,
            hangup_reason: self.hangup_reason.clone(),
            status_code: self.last_status_code,
            answer: self.answer.clone(),
            offer,
            extras: self.extras.clone(),
            dump_event_file,
            recorder,
            refer_callrecord,
        }
    }
}
