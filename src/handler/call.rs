use super::{CallOption, Command, ReferOption};
use crate::{
    app::AppState,
    callrecord::{CallRecord, CallRecordHangupReason},
    event::{EventReceiver, EventSender, SessionEvent},
    media::{
        engine::StreamEngine,
        negotiate::strip_ipv6_candidates,
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder},
        track::{
            file::FileTrack,
            tts::{TtsCommand, TtsHandle},
            webrtc::WebrtcTrack,
            websocket::WebsocketTrack,
            Track, TrackConfig,
        },
    },
    TrackId,
};
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use rsipstack::dialog::{
    dialog::{DialogState, DialogStateReceiver},
    DialogId,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

pub type ActiveCallRef = Arc<ActiveCall>;
#[derive(Deserialize)]
pub struct CallParams {
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ActiveCallType {
    Webrtc,
    Sip,
    WebSocket,
}
pub struct ActiveCall {
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub media_stream: MediaStream,
    pub track_config: TrackConfig,
    pub tts_handle: Mutex<Option<TtsHandle>>,
    pub auto_hangup: Arc<Mutex<Option<bool>>>,
    pub event_sender: EventSender,
    pub option: CallOption,
    pub dialog_id: Mutex<Option<DialogId>>,
    pub state: AppState,
    pub hangup_reason: Mutex<Option<CallRecordHangupReason>>,
}

impl ActiveCall {
    pub async fn create_stream(
        mut caller_track: Box<dyn Track>,
        state: AppState,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        track_id: TrackId,
        option: &CallOption,
    ) -> Result<MediaStream> {
        let mut media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(track_id.clone())
            .with_cancel_token(cancel_token.clone());

        if let Some(recorder_option) = &option.recorder {
            let recorder_file = state.get_recorder_file(&track_id);
            info!("created recording file: {}", recorder_file);

            let track_samplerate = caller_track.config().samplerate;
            let recorder_samplerate = if track_samplerate > 0 {
                track_samplerate
            } else {
                recorder_option.samplerate
            };
            let recorder_ptime = recorder_option.ptime;
            let recorder_config = RecorderOption {
                recorder_file,
                samplerate: recorder_samplerate,
                ptime: recorder_ptime,
            };

            info!(
                "recorder config: samplerate={}, ptime={}ms (track_samplerate={}, option_samplerate={})",
                recorder_samplerate,
                recorder_ptime.as_millis(),
                track_samplerate,
                recorder_option.samplerate
            );

            media_stream_builder = media_stream_builder.with_recorder_config(recorder_config);
        }

        let media_stream = media_stream_builder.build();
        // Use the prepare_stream_hook to set up processors
        let processors = match StreamEngine::create_processors(
            state.stream_engine.clone(),
            caller_track.as_ref(),
            cancel_token,
            event_sender.clone(),
            &option,
        )
        .await
        {
            Ok(processors) => processors,
            Err(e) => {
                warn!("Failed to prepare stream processors: {}", e);
                vec![]
            }
        };

        // Add all processors from the hook
        for processor in processors {
            caller_track.append_processor(processor);
        }

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

        match caller_track.handshake(offer, timeout).await {
            Ok(answer) => {
                let sdp = match option.enable_ipv6 {
                    Some(false) | None => strip_ipv6_candidates(&answer),
                    Some(true) => answer,
                };
                info!("track setup complete answer: {}", sdp);
                event_sender
                    .send(SessionEvent::Answer {
                        track_id,
                        timestamp: crate::get_timestamp(),
                        sdp,
                    })
                    .ok();
                media_stream.update_track(caller_track).await;
                Ok(media_stream)
            }
            Err(e) => {
                warn!("Failed to setup track: {}", e);
                Err(e)
            }
        }
    }

    pub async fn new(
        call_type: ActiveCallType,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        session_id: String,
        media_stream: MediaStream,
        option: CallOption,
        dialog_id: Option<DialogId>,
        state: AppState,
    ) -> Result<Self> {
        let active_call = ActiveCall {
            cancel_token,
            call_type,
            session_id,
            created_at: Utc::now(),
            media_stream,
            track_config: TrackConfig::default(),
            auto_hangup: Arc::new(Mutex::new(None)),
            event_sender,
            option,
            tts_handle: Mutex::new(None),
            dialog_id: Mutex::new(dialog_id),
            state,
            hangup_reason: Mutex::new(None),
        };
        Ok(active_call)
    }

    pub async fn serve(&self) -> Result<()> {
        let mut event_receiver = self.media_stream.subscribe();
        let auto_hangup = self.auto_hangup.clone();
        let event_hook_loop = async move {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    SessionEvent::TrackEnd { track_id, .. } => {
                        if let Some(true) = auto_hangup.lock().await.take() {
                            info!(
                                "auto hangup when track end track_id:{} session_id:{}",
                                track_id, self.session_id
                            );
                            self.do_hangup(Some("autohangup".to_string()), Some(track_id))
                                .await
                                .ok();
                        }
                    }
                    _ => {}
                }
            }
        };

        select! {
            _ = event_hook_loop => {
                info!("Event loop done, id:{}", self.session_id);
            }
            _ = self.media_stream.serve() => {
                info!("Media stream serve done, id:{}", self.session_id);
            }
            _ = self.cancel_token.cancelled() => {
                info!("Event loop cancelled, id:{}", self.session_id);
            }
        }
        Ok(())
    }
    #[instrument(skip(self, command), fields(session_id = self.session_id))]
    pub async fn dispatch(&self, command: Command) -> Result<()> {
        match command {
            Command::Candidate { candidates } => self.do_candidate(candidates).await,
            Command::Tts {
                text,
                speaker,
                play_id,
                auto_hangup,
                streaming,
                end_of_stream,
            } => {
                self.do_tts(
                    text,
                    speaker,
                    play_id,
                    auto_hangup,
                    streaming,
                    end_of_stream,
                )
                .await
            }
            Command::Play { url, auto_hangup } => self.do_play(url, auto_hangup).await,
            Command::Hangup { reason, initiator } => self.do_hangup(reason, initiator).await,
            Command::Refer { target, options } => self.do_refer(target, options).await,
            Command::Mute { track_id } => self.do_mute(track_id).await,
            Command::Unmute { track_id } => self.do_unmute(track_id).await,
            Command::Pause {} => self.do_pause().await,
            Command::Resume {} => self.do_resume().await,
            Command::Interrupt {} => self.do_interrupt().await,
            Command::History { speaker, text } => self.do_history(speaker, text).await,
            _ => {
                info!("Invalid command: {:?}", command);
                Ok(())
            }
        }
    }
    async fn do_candidate(&self, _candidates: Vec<String>) -> Result<()> {
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
    ) -> Result<()> {
        let tts_option = match self.option.tts {
            Some(ref option) => option,
            None => return Ok(()),
        };
        let speaker = match speaker {
            Some(s) => Some(s),
            None => tts_option.speaker.clone(),
        };
        let mut play_command = TtsCommand {
            text,
            speaker,
            play_id: play_id.clone(),
            streaming,
            end_of_stream,
        };

        info!(
            "new tts command, text: {} speaker: {:?} auto_hangup: {:?}",
            play_command.text, play_command.speaker, auto_hangup
        );

        if let Some(auto_hangup) = auto_hangup {
            *self.auto_hangup.lock().await = Some(auto_hangup);
        }

        if let Some(tts_handle) = self.tts_handle.lock().await.as_ref() {
            match tts_handle.try_send(play_command) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    error!("error sending tts command: {}", e);
                    play_command = e.0;
                }
            }
        }

        let (new_handle, tts_track) = StreamEngine::create_tts_track(
            self.state.stream_engine.clone(),
            self.cancel_token.child_token(),
            self.track_config.server_side_track_id.clone(),
            play_id,
            &tts_option,
        )
        .await?;

        new_handle.try_send(play_command)?;
        *self.tts_handle.lock().await = Some(new_handle);
        self.media_stream.update_track(tts_track).await;
        Ok(())
    }

    async fn do_play(&self, url: String, auto_hangup: Option<bool>) -> Result<()> {
        self.tts_handle.lock().await.take();
        let file_track = FileTrack::new(self.track_config.server_side_track_id.clone())
            .with_path(url)
            .with_cancel_token(self.cancel_token.child_token());

        if let Some(auto_hangup) = auto_hangup {
            *self.auto_hangup.lock().await = Some(auto_hangup);
        }
        self.media_stream.update_track(Box::new(file_track)).await;
        Ok(())
    }

    async fn do_history(&self, speaker: String, text: String) -> Result<()> {
        self.media_stream
            .get_event_sender()
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
            .remove_track(&self.track_config.server_side_track_id)
            .await;
        Ok(())
    }
    async fn do_pause(&self) -> Result<()> {
        Ok(())
    }
    async fn do_resume(&self) -> Result<()> {
        Ok(())
    }
    async fn do_hangup(&self, reason: Option<String>, initiator: Option<String>) -> Result<()> {
        info!("Call {} do_hangup", self.session_id);

        // Set hangup reason based on initiator and reason
        let hangup_reason = match initiator.as_deref() {
            Some("caller") => CallRecordHangupReason::ByCaller,
            Some("callee") => CallRecordHangupReason::ByCallee,
            Some("system") => CallRecordHangupReason::Autohangup,
            _ => match reason.as_deref() {
                Some("autohangup") => CallRecordHangupReason::Autohangup,
                Some("canceled") => CallRecordHangupReason::Canceled,
                Some("rejected") => CallRecordHangupReason::Rejected,
                Some("no_answer") => CallRecordHangupReason::NoAnswer,
                _ => CallRecordHangupReason::BySystem,
            },
        };

        *self.hangup_reason.lock().await = Some(hangup_reason);

        self.tts_handle.lock().await.take();
        self.cancel_token.cancel();
        self.media_stream.stop(reason, initiator);

        if let Some(dialog_id) = self.dialog_id.lock().await.take() {
            self.state
                .useragent
                .hangup(dialog_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to hangup: {}", e))?;
        }
        Ok(())
    }

    async fn do_refer(&self, _target: String, _options: Option<ReferOption>) -> Result<()> {
        Ok(())
    }

    async fn do_mute(&self, _track_id: Option<String>) -> Result<()> {
        Ok(())
    }

    async fn do_unmute(&self, _track_id: Option<String>) -> Result<()> {
        Ok(())
    }

    pub async fn cleanup(&self) -> Result<()> {
        info!("call cleanup: {}", self.session_id);

        // Set default hangup reason if not already set
        if self.hangup_reason.lock().await.is_none() {
            *self.hangup_reason.lock().await = Some(CallRecordHangupReason::Autohangup);
        }

        self.tts_handle.lock().await.take();
        self.media_stream.stop(None, None);
        if let Some(dialog_id) = self.dialog_id.lock().await.take() {
            self.state.useragent.hangup(dialog_id).await.ok();
        }
        self.cancel_token.cancel();
        Ok(())
    }
    pub async fn get_callrecord(&self) -> CallRecord {
        let recorder_files = if self.option.recorder.is_some() {
            let recorder_file = self.state.get_recorder_file(&self.session_id);
            if std::path::Path::new(&recorder_file).exists() {
                let file_size = std::fs::metadata(&recorder_file)
                    .map(|m| m.len())
                    .unwrap_or(0);
                vec![crate::callrecord::CallRecordMedia {
                    track_id: self.session_id.clone(),
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

        CallRecord {
            option: self.option.clone(),
            call_id: self.session_id.clone(),
            call_type: self.call_type.clone(),
            start_time: self.created_at,
            end_time: Utc::now(),
            duration: Utc::now()
                .signed_duration_since(self.created_at)
                .num_milliseconds() as u64,
            caller: self
                .option
                .caller
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            callee: self
                .option
                .callee
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            hangup_reason: self
                .hangup_reason
                .lock()
                .await
                .clone()
                .unwrap_or(CallRecordHangupReason::Autohangup),
            recorder: recorder_files,
            status_code: 200,
            extras: None,
        }
    }
}

async fn sip_event_loop(
    track_id: TrackId,
    event_sender: &mut EventSender,
    dlg_state_receiver: &mut DialogStateReceiver,
) -> Result<()> {
    while let Some(event) = dlg_state_receiver.recv().await {
        match event {
            DialogState::Trying(dialog_id) => {
                info!("dialog trying: {}", dialog_id);
            }
            DialogState::Early(dialog_id, _) => {
                info!("dialog early: {}", dialog_id);
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: true,
                })?;
            }
            DialogState::Calling(dialog_id) => {
                info!("dialog calling: {}", dialog_id);
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: false,
                })?;
            }
            DialogState::Confirmed(dialog_id) => {
                info!("dialog confirmed: {}", dialog_id);
            }
            DialogState::Terminated(dialog_id, _) => {
                info!("dialog terminated: {}", dialog_id);
                break;
            }
            _ => (),
        }
    }
    Ok(())
}

async fn send_to_ws_loop(
    ws_sender: &mut SplitSink<WebSocket, Message>,
    event_receiver: &mut EventReceiver,
) -> Result<()> {
    while let Ok(event) = event_receiver.recv().await {
        match event {
            SessionEvent::Binary { data, .. } => {
                if let Err(e) = ws_sender.send(data.into()).await {
                    error!("error sending event to WebSocket: {}", e);
                }
            }
            _ => {
                let data = match serde_json::to_string(&event) {
                    Ok(data) => data,
                    Err(e) => {
                        error!("error serializing event: {} {:?}", e, event);
                        continue;
                    }
                };
                if let Err(e) = ws_sender.send(data.into()).await {
                    error!("error sending event to WebSocket: {}", e);
                }
            }
        };
    }
    Ok(())
}

pub async fn handle_call(
    call_type: ActiveCallType,
    session_id: String,
    socket: axum::extract::ws::WebSocket,
    state: AppState,
) -> Result<()> {
    let cancel_token = CancellationToken::new();
    let (mut ws_sender, ws_receiver) = socket.split();
    let event_sender = crate::event::create_event_sender();
    let mut event_receiver = event_sender.subscribe();

    select! {
        _ = send_to_ws_loop(&mut ws_sender, &mut event_receiver) => {
            info!("prepare call send to ws");
            return Err(anyhow::anyhow!("WebSocket closed"));
        }
        r = process_call(cancel_token.clone(), call_type, session_id.clone(),ws_receiver, event_sender, state) => {
            match r {
                Ok(_) => {}
                Err(e) => {
                    info!("call error: {}", e);
                    let error_event = SessionEvent::Error {
                        track_id:session_id,
                        timestamp:crate::get_timestamp(),
                        error:e.to_string(),
                        sender: "handle_call".to_string(),
                        code: None };
                    match serde_json::to_string(&error_event) {
                        Ok(data) => {ws_sender.send(data.into()).await.ok();},
                        Err(_) => {
                            error!("error serializing error event: {}", e);
                        }
                    }
                }
            }
        }
        _ = cancel_token.cancelled() => {
            info!("cancelled");
        }
    };
    ws_sender.flush().await.ok();
    Ok(())
}

async fn process_call(
    cancel_token: CancellationToken,
    call_type: ActiveCallType,
    session_id: String,
    mut ws_receiver: SplitStream<WebSocket>,
    mut event_sender: EventSender,
    state: AppState,
) -> Result<()> {
    let track_id = session_id.clone();
    let cancel_token_ref = cancel_token.clone();
    let state_clone = state.clone();
    let (dlg_state_sender, mut dlg_state_receiver) = mpsc::unbounded_channel();
    let track_id_clone = track_id.clone();
    let call_type_ref = call_type.clone();
    let event_sender_ref = event_sender.clone();

    let audio_from_ws = Arc::new(Mutex::new(None));
    let audio_from_ws_ref = audio_from_ws.clone();

    let prepare_call = async move {
        let mut option = match ws_receiver.next().await {
            Some(Ok(Message::Text(text))) => {
                let command = serde_json::from_str::<Command>(&text)?;
                match command {
                    Command::Invite { option: options } => options,
                    _ => {
                        info!("the first message must be an invite {:?}", command);
                        return Err(anyhow::anyhow!("the first message must be an invite"));
                    }
                }
            }
            _ => {
                return Err(anyhow::anyhow!("Invalid message type"));
            }
        };
        option.check_default(); // check default
        info!(
            "prepare {:?} call {} option: {:?}",
            call_type, session_id, option
        );
        let track_config = TrackConfig::default();
        let mut dialog_id = None;
        let caller_track: Box<dyn Track> = match call_type {
            ActiveCallType::WebSocket => {
                let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
                audio_from_ws_ref.lock().await.replace(tx);
                let ws_track = WebsocketTrack::new(
                    cancel_token.child_token(),
                    track_id.clone(),
                    track_config,
                    event_sender_ref.clone(),
                    rx,
                    option.codec.clone(),
                );
                Box::new(ws_track)
            }
            ActiveCallType::Webrtc => {
                let webrtc_track =
                    WebrtcTrack::new(cancel_token.child_token(), track_id.clone(), track_config);
                Box::new(webrtc_track)
            }
            ActiveCallType::Sip => {
                let (dlg_id, rtp_track) = super::sip::new_rtp_track_with_sip(
                    state_clone.clone(),
                    cancel_token.child_token(),
                    track_id.clone(),
                    track_config,
                    &option,
                    dlg_state_sender,
                )
                .await?;
                dialog_id.replace(dlg_id);
                option.offer = rtp_track.local_description().ok();
                Box::new(rtp_track)
            }
        };

        let media_stream = ActiveCall::create_stream(
            caller_track,
            state_clone.clone(),
            cancel_token.child_token(),
            event_sender_ref.clone(),
            track_id,
            &option,
        )
        .await?;

        let active_call = ActiveCall::new(
            call_type,
            cancel_token,
            event_sender_ref,
            session_id,
            media_stream,
            option,
            dialog_id,
            state_clone,
        )
        .await?;
        Ok((Arc::new(active_call), ws_receiver))
    };

    let (active_call, mut ws_receiver) = select! {
        _ = cancel_token_ref.cancelled() => {
            info!("prepare call cancelled");
            return Err(anyhow::anyhow!("Cancelled"));
        },
        r = prepare_call => {
            match r {
                Ok(call_result) => call_result,
                Err(e) => {
                    error!("prepare call failed: {}", e);
                    return Err(e);
                }
            }
        }
        _ = async {
            if matches!(call_type_ref, ActiveCallType::Sip) {
                sip_event_loop(track_id_clone.clone(), &mut event_sender, &mut dlg_state_receiver).await
            } else {
                cancel_token_ref.cancelled().await;
                Ok(())
            }
        } => {
            info!("sip event loop");
            return Err(anyhow::anyhow!("Sip event loop failed"));
        }
    };

    let active_call_clone = active_call.clone();
    let active_calls_len = {
        let mut active_calls = state.active_calls.lock().await;
        active_calls.insert(active_call.session_id.clone(), active_call.clone());
        active_calls.len()
    };

    info!(
        "new call: {} -> {:?}, {} active calls",
        active_call.session_id, active_call.call_type, active_calls_len
    );

    let audio_from_ws = audio_from_ws.lock().await.take();
    let recv_from_ws = async move {
        while let Some(msg) = ws_receiver.next().await {
            let command = match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<Command>(&text) {
                    Ok(command) => command,
                    Err(e) => {
                        error!("error deserializing command: {} {}", e, text);
                        continue;
                    }
                },
                Ok(Message::Binary(data)) => {
                    if let Some(sender) = audio_from_ws.as_ref() {
                        if let Err(e) = sender.send(data) {
                            error!("error sending audio data: {}", e);
                            break;
                        }
                    }
                    continue;
                }
                _ => continue,
            };

            match active_call.dispatch(command).await {
                Ok(_) => (),
                Err(e) => {
                    error!("Error dispatching command: {}", e);
                }
            }
        }
    };
    select! {
        _ = recv_from_ws => {
            info!("recv_from_ws websocket disconnected");
        },
        r = active_call_clone.serve() => {
            info!("call loop disconnected {:?}", r);
        },
        _ = async {
            if matches!(call_type_ref, ActiveCallType::Sip) {
                sip_event_loop(track_id_clone.clone(), &mut event_sender, &mut dlg_state_receiver).await
            } else {
                cancel_token_ref.cancelled().await;
                Ok(())
            }
        } => {
            info!("sip event loop");
        }
    }
    active_call_clone.cleanup().await?;
    Ok(())
}
