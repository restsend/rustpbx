use super::{CallOption, Command, ReferOption};
use crate::{
    app::AppState,
    callrecord::{CallRecord, CallRecordEvent, CallRecordEventType, CallRecordHangupReason},
    event::{EventReceiver, EventSender, SessionEvent},
    get_timestamp,
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
    synthesis::SynthesisOption,
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
    dialog::{DialogState, DialogStateReceiver, DialogStateSender, TerminatedReason},
    DialogId,
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    fs::File,
    select,
    sync::{mpsc, Mutex},
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

pub type ActiveCallRef = Arc<ActiveCall>;
#[derive(Deserialize)]
pub struct CallParams {
    pub id: Option<String>,
    #[serde(rename = "dump")]
    pub dump_events: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ActiveCallType {
    Webrtc,
    Sip,
    WebSocket,
}
pub struct ActiveCallState {
    pub created_at: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub last_status_code: u16,
    pub answer: Option<String>,
    pub option: Option<CallOption>,
}
pub type ActiveCallStateRef = Arc<RwLock<ActiveCallState>>;

pub struct ActiveCall {
    pub call_state: ActiveCallStateRef,
    pub option: CallOption,
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub media_stream: MediaStream,
    pub track_config: TrackConfig,
    pub tts_handle: Mutex<Option<TtsHandle>>,
    pub auto_hangup: Arc<Mutex<Option<bool>>>,
    pub wait_input_timeout: Arc<Mutex<Option<u32>>>,
    pub event_sender: EventSender,
    pub dialog_id: Mutex<Option<DialogId>>,
    pub app_state: AppState,
}

impl ActiveCall {
    pub async fn create_track(
        cancel_token: CancellationToken,
        call_type: &ActiveCallType,
        session_id: String,
        track_config: TrackConfig,
        event_sender: EventSender,
        option: &mut CallOption,
        audio_from_ws: Arc<Mutex<Option<mpsc::UnboundedSender<Bytes>>>>,
        state: AppState,
        dlg_state_sender: DialogStateSender,
        call_state: ActiveCallStateRef,
        dialog_id: &mut Option<DialogId>,
    ) -> Result<Box<dyn Track>> {
        let answer;
        let caller_track: Box<dyn Track> = match call_type {
            ActiveCallType::WebSocket => {
                let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
                audio_from_ws.lock().await.replace(tx);
                let ws_track = WebsocketTrack::new(
                    cancel_token.child_token(),
                    session_id.clone(),
                    track_config,
                    event_sender,
                    rx,
                    option.codec.clone(),
                );
                answer = None;
                Box::new(ws_track)
            }
            ActiveCallType::Webrtc => {
                let mut webrtc_track =
                    WebrtcTrack::new(cancel_token.child_token(), session_id.clone(), track_config);

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

                match webrtc_track.handshake(offer, timeout).await {
                    Ok(answer_sdp) => {
                        answer = match option.enable_ipv6 {
                            Some(false) | None => Some(strip_ipv6_candidates(&answer_sdp)),
                            Some(true) => Some(answer_sdp),
                        };
                        info!(session_id, "track setup complete answer: {:?}", answer);
                    }
                    Err(e) => {
                        warn!(session_id, "Failed to setup track: {}", e);
                        return Err(anyhow::anyhow!("Failed to setup track: {}", e));
                    }
                }
                Box::new(webrtc_track)
            }
            ActiveCallType::Sip => {
                let is_answer_mode = state.useragent.is_pending_call(&session_id).await;
                let r = if let Some(pending_call) =
                    state.useragent.get_pending_call(&session_id).await
                {
                    super::sip::new_rtp_track_with_pending_call(
                        state.clone(),
                        cancel_token.child_token(),
                        session_id.clone(),
                        track_config,
                        &option,
                        dlg_state_sender,
                        pending_call,
                    )
                    .await
                } else {
                    super::sip::new_rtp_track_with_sip(
                        state.clone(),
                        cancel_token.child_token(),
                        session_id.clone(),
                        track_config,
                        &option,
                        dlg_state_sender,
                    )
                    .await
                };
                let (dlg_id, rtp_track) = match r {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(session_id, "error creating rtp track: {}", e);
                        return Err(anyhow::anyhow!("error creating sip/rtp track: {}", e));
                    }
                };
                dialog_id.replace(dlg_id);

                if is_answer_mode {
                    option.offer = rtp_track.remote_description();
                    answer = rtp_track.local_description().ok();
                } else {
                    option.offer = rtp_track.local_description().ok();
                    answer = rtp_track.remote_description();
                }
                Box::new(rtp_track)
            }
        };

        match call_state.write() {
            Ok(mut call_state) => {
                if call_state.answer_time.is_none() {
                    call_state.answer_time = Some(Utc::now());
                }
                call_state.answer.replace(answer.unwrap_or("".to_string()));
                call_state
                    .option
                    .as_mut()
                    .map(|opt| opt.offer = option.offer.clone());
            }
            Err(e) => {
                warn!(session_id, "error writing call state: {}", e);
            }
        }
        Ok(caller_track)
    }

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

        media_stream.update_track(caller_track).await;
        Ok(media_stream)
    }

    pub async fn new(
        call_state: ActiveCallStateRef,
        call_type: ActiveCallType,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        session_id: String,
        media_stream: MediaStream,
        option: CallOption,
        dialog_id: Option<DialogId>,
        state: AppState,
    ) -> Result<Self> {
        call_state.write().unwrap().option.replace(option.clone());

        let active_call = ActiveCall {
            cancel_token,
            call_type,
            session_id,
            call_state,
            media_stream,
            track_config: TrackConfig::default(),
            auto_hangup: Arc::new(Mutex::new(None)),
            wait_input_timeout: Arc::new(Mutex::new(None)),
            event_sender,
            option,
            tts_handle: Mutex::new(None),
            dialog_id: Mutex::new(dialog_id),
            app_state: state,
        };
        Ok(active_call)
    }

    pub async fn serve(&self) -> Result<()> {
        let mut event_receiver = self.media_stream.subscribe();
        let auto_hangup = self.auto_hangup.clone();
        let next_input_timeout = Arc::new(Mutex::new(0));
        let next_input_timeout_ref = next_input_timeout.clone();
        let event_sender = self.media_stream.get_event_sender();
        let wait_input_timeout_loop = async {
            loop {
                let expire = { *next_input_timeout.lock().await };
                if expire > 0 && get_timestamp() >= expire {
                    info!(session_id = self.session_id, "wait input timeout reached");
                    *next_input_timeout.lock().await = 0;
                    event_sender
                        .send(SessionEvent::Silence {
                            track_id: self.track_config.server_side_track_id.clone(),
                            timestamp: crate::get_timestamp(),
                            start_time: crate::get_timestamp(),
                            duration: 0,
                            samples: None,
                        })
                        .ok();
                }
                sleep(Duration::from_millis(100)).await;
            }
        };

        let event_hook_loop = async move {
            while let Ok(event) = event_receiver.recv().await {
                match event {
                    SessionEvent::Speaking { .. }
                    | SessionEvent::Dtmf { .. }
                    | SessionEvent::AsrDelta { .. }
                    | SessionEvent::AsrFinal { .. } => {
                        *next_input_timeout_ref.lock().await = 0;
                    }
                    SessionEvent::TrackEnd { track_id, .. } => {
                        if let Some(true) = auto_hangup.lock().await.take() {
                            info!(
                                session_id = self.session_id,
                                "auto hangup when track end track_id:{}", track_id
                            );
                            self.do_hangup(Some("autohangup".to_string()), Some(track_id))
                                .await
                                .ok();
                        }
                        if let Some(wait_input_timeout) =
                            self.wait_input_timeout.lock().await.take()
                        {
                            let expire = if wait_input_timeout > 0 {
                                get_timestamp() + wait_input_timeout as u64
                            } else {
                                0
                            };
                            *next_input_timeout_ref.lock().await = expire;
                        }
                    }
                    _ => {}
                }
            }
        };

        select! {
            _ = wait_input_timeout_loop=>{
                info!(session_id = self.session_id, "Wait input timeout loop done");
            }
            _ = event_hook_loop => {
                info!(session_id = self.session_id, "Event loop done");
            }
            _ = self.media_stream.serve() => {
                info!(session_id = self.session_id, "Media stream serve done");
            }
            _ = self.cancel_token.cancelled() => {
                info!(session_id = self.session_id, "Event loop cancelled");
            }
        }
        Ok(())
    }
    #[instrument(skip(self, command), fields(session_id = self.session_id))]
    pub async fn dispatch(&self, command: Command) -> Result<()> {
        match command {
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
        let tts_option = match self.option.tts {
            Some(ref option) => option,
            None => return Ok(()),
        };
        let option = tts_option.merge_with(option);
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
            option,
        };

        info!(
            session_id = self.session_id,
            "new tts command, text: {} speaker: {:?} auto_hangup: {:?}",
            play_command.text,
            play_command.speaker,
            auto_hangup
        );

        *self.auto_hangup.lock().await = auto_hangup;
        *self.wait_input_timeout.lock().await = wait_input_timeout;

        if let Some(tts_handle) = self.tts_handle.lock().await.as_ref() {
            match tts_handle.try_send(play_command) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    warn!(
                        session_id = self.session_id,
                        "error sending tts command: {}", e
                    );
                    play_command = e.0;
                }
            }
        }

        let (new_handle, tts_track) = StreamEngine::create_tts_track(
            self.app_state.stream_engine.clone(),
            self.cancel_token.child_token(),
            self.session_id.clone(),
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

    async fn do_play(
        &self,
        url: String,
        auto_hangup: Option<bool>,
        wait_input_timeout: Option<u32>,
    ) -> Result<()> {
        self.tts_handle.lock().await.take();

        info!(
            session_id = self.session_id,
            url, auto_hangup, "play file track"
        );

        let file_track = FileTrack::new(self.track_config.server_side_track_id.clone())
            .with_path(url)
            .with_cancel_token(self.cancel_token.child_token());

        *self.auto_hangup.lock().await = auto_hangup;
        *self.wait_input_timeout.lock().await = wait_input_timeout;
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

        self.call_state
            .write()
            .unwrap()
            .hangup_reason
            .replace(hangup_reason);

        self.tts_handle.lock().await.take();
        self.cancel_token.cancel();
        self.media_stream.stop(reason, initiator);

        if let Some(dialog_id) = self.dialog_id.lock().await.take() {
            self.app_state
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
        info!(session_id = self.session_id, "call cleanup");
        if let Some(dialog_id) = self.dialog_id.lock().await.take() {
            self.app_state.useragent.hangup(dialog_id).await.ok();
        }

        self.cancel_token.cancel();
        self.tts_handle.lock().await.take();
        self.media_stream.cleanup().await.ok();

        Ok(())
    }

    pub async fn get_callrecord(&self) -> CallRecord {
        let recorder_files = if self.option.recorder.is_some() {
            let recorder_file = self.app_state.get_recorder_file(&self.session_id);
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

        let dump_events_file = self.app_state.get_dump_events_file(&self.session_id);
        let dump_events = if std::path::Path::new(&dump_events_file).exists() {
            Some(dump_events_file)
        } else {
            None
        };
        let call_state = self.call_state.read().unwrap();
        CallRecord {
            option: call_state.option.clone(),
            call_id: self.session_id.clone(),
            call_type: self.call_type.clone(),
            start_time: call_state.created_at,
            ring_time: call_state.ring_time.clone(),
            answer_time: call_state.answer_time.clone(),
            end_time: Utc::now(),
            caller: self
                .option
                .caller
                .as_ref()
                .map(|v| v.clone())
                .unwrap_or_default(),
            callee: self
                .option
                .callee
                .as_ref()
                .map(|v| v.clone())
                .unwrap_or_default(),
            hangup_reason: call_state.hangup_reason.clone(),
            recorder: recorder_files,
            status_code: call_state.last_status_code,
            answer: call_state.answer.clone(),
            offer: call_state
                .option
                .as_ref()
                .map(|o| o.offer.clone())
                .flatten(),
            extras: None,
            dump_event_file: dump_events,
        }
    }
}

async fn sip_event_loop(
    track_id: TrackId,
    event_sender: EventSender,
    mut dlg_state_receiver: DialogStateReceiver,
    call_state: ActiveCallStateRef,
) -> Result<()> {
    while let Some(event) = dlg_state_receiver.recv().await {
        let mut call_state_ref = call_state.write().unwrap();

        match event {
            DialogState::Trying(dialog_id) => {
                info!(session_id = track_id, "dialog trying: {}", dialog_id);
            }
            DialogState::Early(dialog_id, _) => {
                info!(session_id = track_id, "dialog early: {}", dialog_id);
                call_state_ref.ring_time.replace(Utc::now());
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: true,
                })?;
            }
            DialogState::Calling(dialog_id) => {
                info!(session_id = track_id, "dialog calling: {}", dialog_id);
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
                info!(session_id = track_id, "dialog confirmed: {}", dialog_id);
            }
            DialogState::Terminated(dialog_id, reason) => {
                info!(
                    session_id = track_id,
                    "dialog terminated: {} {:?}", dialog_id, reason
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
    dump_events_file: &mut Option<File>,
) -> Result<()> {
    while let Ok(event) = event_receiver.recv().await {
        match event {
            SessionEvent::Binary { data, .. } => {
                if let Err(e) = ws_sender.send(data.into()).await {
                    warn!("error sending event to WebSocket: {}", e);
                }
            }
            _ => {
                let data = match serde_json::to_string(&event) {
                    Ok(data) => data,
                    Err(e) => {
                        warn!("error serializing event: {} {:?}", e, event);
                        continue;
                    }
                };
                if let Some(dump_events_file) = dump_events_file {
                    CallRecordEvent::new(CallRecordEventType::Event, &data)
                        .write_to_file(dump_events_file)
                        .await;
                }
                if let Err(e) = ws_sender.send(data.into()).await {
                    warn!("error sending event to WebSocket: {}", e);
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
    call_state: ActiveCallStateRef,
    dump_events: bool,
) -> Result<()> {
    let cancel_token = CancellationToken::new();
    let (mut ws_sender, ws_receiver) = socket.split();
    let event_sender = crate::event::create_event_sender();
    let mut event_receiver = event_sender.subscribe();
    let (dlg_state_sender, dlg_state_receiver) = mpsc::unbounded_channel();
    let mut dump_events_file = if dump_events {
        let file_name = state.get_dump_events_file(&session_id);
        File::options()
            .create(true)
            .append(true)
            .open(file_name)
            .await
            .ok()
    } else {
        None
    };
    let dump_command_file = if dump_events {
        let file_name = state.get_dump_events_file(&session_id);
        File::options()
            .create(true)
            .append(true)
            .open(file_name)
            .await
            .ok()
    } else {
        None
    };
    select! {
        _ = send_to_ws_loop( &mut ws_sender, &mut event_receiver, &mut dump_events_file) => {
            info!(session_id, "prepare call send to ws");
            return Err(anyhow::anyhow!("WebSocket closed"));
        }
        r = sip_event_loop(session_id.clone(),  event_sender.clone(), dlg_state_receiver, call_state.clone()) => {
            match r {
                Ok(_) => {
                    info!(session_id, "sip event loop completed");
                }
                Err(e) => {
                    info!(session_id, "sip event loop error: {}", e);
                }
            }
        }
        r = process_call(cancel_token.clone(), call_type, call_state, session_id.clone(), ws_receiver, event_sender, dlg_state_sender, state, dump_command_file) => {
            match r {
                Ok(_) => {
                    info!(session_id, "process call completed");
                }
                Err(e) => {
                    info!(session_id,"call error: {}", e);
                    let error_event = SessionEvent::Error {
                        track_id:session_id.clone(),
                        timestamp:crate::get_timestamp(),
                        error:e.to_string(),
                        sender: "handle_call".to_string(),
                        code: None };
                    match serde_json::to_string(&error_event) {
                        Ok(data) => {
                            ws_sender.send(data.into()).await.ok();
                        },
                        Err(_) => {
                            warn!("error serializing error event: {}", e);
                        }
                    }
                }
            }
        }
        _ = cancel_token.cancelled() => {
            info!(session_id, "cancelled");
        }
    };

    // Ensure all remaining events are sent to the websocket before exit
    while let Ok(event) = event_receiver.try_recv() {
        match event {
            SessionEvent::Binary { data, .. } => {
                ws_sender.send(data.into()).await.ok();
            }
            _ => {
                let data = match serde_json::to_string(&event) {
                    Ok(data) => data,
                    Err(e) => {
                        warn!("error serializing event during flush: {} {:?}", e, event);
                        continue;
                    }
                };
                if let Some(dump_events_file) = &mut dump_events_file {
                    CallRecordEvent::new(CallRecordEventType::Event, &data)
                        .write_to_file(dump_events_file)
                        .await;
                }
                if let Err(_) = ws_sender.send(data.into()).await {
                    break;
                }
            }
        }
    }
    ws_sender.flush().await.ok();
    Ok(())
}

async fn process_call(
    cancel_token: CancellationToken,
    call_type: ActiveCallType,
    call_state: ActiveCallStateRef,
    session_id: String,
    mut ws_receiver: SplitStream<WebSocket>,
    event_sender: EventSender,
    dlg_state_sender: DialogStateSender,
    state: AppState,
    mut dump_command_file: Option<File>,
) -> Result<()> {
    let audio_from_ws = Arc::new(Mutex::new(None));
    let mut option = match ws_receiver.next().await {
        Some(Ok(Message::Text(text))) => {
            let command = serde_json::from_str::<Command>(&text)?;
            if let Some(dump_command_file) = &mut dump_command_file {
                CallRecordEvent::new(CallRecordEventType::Command, &text)
                    .write_to_file(dump_command_file)
                    .await;
            }
            match command {
                Command::Invite { option } => option,
                Command::Accept { option } => option,
                _ => {
                    info!(
                        session_id,
                        "the first message must be an invite {:?}", command
                    );
                    return Err(anyhow::anyhow!("the first message must be an invite"));
                }
            }
        }
        _ => {
            return Err(anyhow::anyhow!("Invalid message type"));
        }
    };
    option.check_default(); // check default
    info!(session_id, ?call_type, "prepare call option: {:?}", option);
    let track_config = TrackConfig::default();
    let mut dialog_id = None;
    let caller_track = match ActiveCall::create_track(
        cancel_token.clone(),
        &call_type,
        session_id.clone(),
        track_config,
        event_sender.clone(),
        &mut option,
        audio_from_ws.clone(),
        state.clone(),
        dlg_state_sender.clone(),
        call_state.clone(),
        &mut dialog_id,
    )
    .await
    {
        Ok(caller_track) => caller_track,
        Err(e) => {
            warn!(session_id, "error creating caller track: {}", e);
            return Err(anyhow::anyhow!("error creating caller track: {}", e));
        }
    };
    let media_stream = match ActiveCall::create_stream(
        caller_track,
        state.clone(),
        cancel_token.child_token(),
        event_sender.clone(),
        session_id.clone(),
        &option,
    )
    .await
    {
        Ok(media_stream) => media_stream,
        Err(e) => {
            warn!(session_id, "error creating media stream: {}", e);
            return Err(anyhow::anyhow!("error creating media stream: {}", e));
        }
    };

    let active_call = match ActiveCall::new(
        call_state,
        call_type,
        cancel_token,
        event_sender.clone(),
        session_id.clone(),
        media_stream,
        option,
        dialog_id,
        state.clone(),
    )
    .await
    {
        Ok(active_call) => Arc::new(active_call),
        Err(e) => {
            warn!(session_id, "error creating active call: {}", e);
            return Err(anyhow::anyhow!("error creating active call: {}", e));
        }
    };

    let active_calls_len = {
        let mut active_calls = state.active_calls.lock().await;
        active_calls.insert(session_id, active_call.clone());
        active_calls.len()
    };

    let answer = match active_call.call_state.try_read() {
        Ok(call_state) => call_state.answer.clone().unwrap_or_default(),
        Err(_) => String::new(),
    };
    event_sender
        .send(SessionEvent::Answer {
            track_id: active_call.session_id.clone(),
            timestamp: crate::get_timestamp(),
            sdp: answer,
        })
        .ok();
    info!(
        session_id = active_call.session_id,
        call_type = ?active_call.call_type,
        "new call: {} active calls", active_calls_len
    );
    //SessionEvent::Answer
    let audio_from_ws = audio_from_ws.lock().await.take();
    let active_call_clone = active_call.clone();
    let recv_from_ws = async move {
        while let Some(msg) = ws_receiver.next().await {
            let command = match msg {
                Ok(Message::Text(text)) => {
                    if let Some(dump_command_file) = &mut dump_command_file {
                        CallRecordEvent::new(CallRecordEventType::Command, &text)
                            .write_to_file(dump_command_file)
                            .await;
                    }
                    match serde_json::from_str::<Command>(&text) {
                        Ok(command) => command,
                        Err(e) => {
                            warn!("error deserializing command: {} {}", e, text);
                            continue;
                        }
                    }
                }
                Ok(Message::Binary(data)) => {
                    if let Some(sender) = audio_from_ws.as_ref() {
                        if let Err(e) = sender.send(data) {
                            warn!("error sending audio data: {}", e);
                            break;
                        }
                    }
                    continue;
                }
                _ => continue,
            };

            match active_call_clone.dispatch(command).await {
                Ok(_) => (),
                Err(e) => {
                    warn!("Error dispatching command: {}", e);
                }
            }
        }
    };
    select! {
        _ = recv_from_ws => {
            info!(session_id = active_call.session_id, "recv_from_ws websocket disconnected");
        },
        r = active_call.serve() => {
            info!(session_id = active_call.session_id, "call loop disconnected {:?}", r);
        },
    }
    active_call.cleanup().await?;
    Ok(())
}
