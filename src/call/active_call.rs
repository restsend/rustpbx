use super::{CallOption, Command, ReferOption};
use crate::{
    app::AppState,
    call::CommandReceiver,
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
            tts::{TtsCommand, TtsHandle},
            webrtc::WebrtcTrack,
            websocket::{WebsocketBytesReceiver, WebsocketTrack},
        },
    },
    synthesis::SynthesisOption,
    useragent::invitation::PendingDialog,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rsipstack::dialog::{DialogId, dialog::DialogStateSender};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{fs::File, join, select, sync::Mutex, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

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
#[derive(Default)]
pub struct ActiveCallState {
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub last_status_code: u16,
    pub option: CallOption,
    pub answer: Option<String>,
    pub dialog_id: Option<DialogId>,
    pub ssrc: u32,
}

pub type ActiveCallStateRef = Arc<RwLock<ActiveCallState>>;

pub struct ActiveCall {
    pub call_state: ActiveCallStateRef,
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub track_config: TrackConfig,
    pub tts_handle: Mutex<Option<TtsHandle>>,
    pub auto_hangup: Arc<Mutex<Option<u32>>>,
    pub wait_input_timeout: Arc<Mutex<Option<u32>>>,
    pub event_sender: EventSender,
    pub app_state: AppState,
}

impl ActiveCall {
    pub async fn create_stream(
        cancel_token: CancellationToken,
        session_id: String,
        option: &CallOption,
        mut caller_track: Box<dyn Track>,
        app_state: AppState,
        event_sender: EventSender,
    ) -> Result<MediaStream> {
        let mut media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.clone());

        if let Some(recorder_option) = &option.recorder {
            let recorder_file = app_state.get_recorder_file(&session_id);
            info!(session_id, "created recording file: {}", recorder_file);

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
                session_id,
                sample_rate = recorder_samplerate,
                ptime = recorder_ptime.as_millis(),
                track_sample_rate = track_samplerate,
                "recorder config",
            );

            media_stream_builder = media_stream_builder.with_recorder_config(recorder_config);
        }

        let media_stream = media_stream_builder.build();
        // Use the prepare_stream_hook to set up processors
        let processors = match StreamEngine::create_processors(
            app_state.stream_engine.clone(),
            caller_track.as_ref(),
            cancel_token,
            event_sender.clone(),
            &option,
        )
        .await
        {
            Ok(processors) => processors,
            Err(e) => {
                warn!(session_id, "Failed to prepare stream processors: {}", e);
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

    pub fn new(
        call_state: ActiveCallStateRef,
        call_type: ActiveCallType,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        session_id: String,
        media_stream: MediaStream,
        app_state: AppState,
    ) -> Self {
        Self {
            cancel_token,
            call_type,
            session_id,
            call_state,
            media_stream: Arc::new(media_stream),
            track_config: TrackConfig::default(),
            auto_hangup: Arc::new(Mutex::new(None)),
            wait_input_timeout: Arc::new(Mutex::new(None)),
            event_sender,
            tts_handle: Mutex::new(None),
            app_state,
        }
    }

    pub async fn serve(&self) -> Result<()> {
        let mut event_receiver = self.media_stream.subscribe();
        let auto_hangup = self.auto_hangup.clone();
        let wait_input_timeout = self.wait_input_timeout.clone();

        let input_timeout_expire = Arc::new(Mutex::new((0u64, 0u32)));
        let input_timeout_expire_ref = input_timeout_expire.clone();
        let event_sender = self.media_stream.get_event_sender();
        let wait_input_timeout_loop = async {
            loop {
                let (start_time, expire) = { *input_timeout_expire.lock().await };
                if expire > 0 && crate::get_timestamp() >= start_time + expire as u64 {
                    info!(session_id = self.session_id, "wait input timeout reached");
                    *input_timeout_expire.lock().await = (0, 0);
                    event_sender
                        .send(SessionEvent::Silence {
                            track_id: self.track_config.server_side_track_id.clone(),
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
        let server_side_track_id = &self.track_config.server_side_track_id;
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
                            if *auto_hangup_ssrc == ssrc {
                                auto_hangup_ref.take();
                                info!(
                                    session_id = self.session_id,
                                    ssrc, "auto hangup when track end track_id:{}", track_id
                                );
                                self.do_hangup(Some("autohangup".to_string()), None)
                                    .await
                                    .ok();
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
        let tts_option = match self.call_state.read() {
            Ok(ref call_state) => match call_state.option.tts {
                Some(ref opt) => opt.clone(),
                None => return Ok(()),
            },
            Err(_) => return Err(anyhow::anyhow!("failed to read call state")),
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
        let ssrc = rand::random::<u32>();
        match auto_hangup {
            Some(true) => *self.auto_hangup.lock().await = Some(ssrc),
            _ => *self.auto_hangup.lock().await = None,
        }
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
            ssrc,
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
        let ssrc = rand::random::<u32>();
        info!(
            session_id = self.session_id,
            ssrc, url, auto_hangup, "play file track"
        );

        let file_track = FileTrack::new(self.track_config.server_side_track_id.clone())
            .with_ssrc(ssrc)
            .with_path(url)
            .with_cancel_token(self.cancel_token.child_token());
        match auto_hangup {
            Some(true) => *self.auto_hangup.lock().await = Some(ssrc),
            _ => *self.auto_hangup.lock().await = None,
        }
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

        self.tts_handle.lock().await.take();
        self.media_stream.stop(reason, initiator);

        let dialog_id = match self.call_state.write().as_mut() {
            Ok(call_state) => {
                if call_state.hangup_reason.is_none() {
                    call_state.hangup_reason.replace(hangup_reason);
                }
                call_state.dialog_id.take()
            }
            Err(_) => None,
        };

        if let Some(dialog_id) = dialog_id {
            self.app_state
                .useragent
                .hangup(dialog_id)
                .await
                .map_err(|e| anyhow::anyhow!("failed to hangup: {}", e))?;
        }
        self.cancel_token.cancel();
        Ok(())
    }

    async fn do_refer(
        &self,
        caller: String,
        callee: String,
        options: Option<ReferOption>,
    ) -> Result<()> {
        if let Some(moh) = options.as_ref().and_then(|o| o.moh.clone()) {
            self.do_play(moh, None, None).await?;
        }
        let token = self.cancel_token.child_token();
        let session_id = self.session_id.clone();
        let app_state = self.app_state.clone();
        let event_sender = self.media_stream.get_event_sender();
        let track_id = self.track_config.server_side_track_id.clone();
        let stream = self.media_stream.clone();
        let track_config = self.track_config.clone();
        let auto_hangup = self.auto_hangup.clone();

        tokio::spawn(async move {
            match super::sip::make_sip_invite_with_stream(
                token,
                app_state,
                session_id.clone(),
                track_id,
                track_config,
                event_sender,
                stream,
                auto_hangup,
                caller,
                callee.clone(),
                options,
            )
            .await
            {
                Ok(_) => {
                    info!(session_id, callee, "Refer call started successfully");
                }
                Err(e) => {
                    error!(session_id, callee, "Failed to start refer call: {}", e);
                }
            }
        });
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
        info!(session_id = self.session_id, "call cleanup");

        let dialog_id = match self.call_state.write().as_mut() {
            Ok(call_state) => call_state.dialog_id.take(),
            Err(_) => None,
        };

        if let Some(dialog_id) = dialog_id {
            self.app_state.useragent.hangup(dialog_id).await.ok();
        }

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
}

impl ActiveCallState {
    pub fn build_callrecord(
        &self,
        app_state: AppState,
        session_id: String,
        call_type: ActiveCallType,
    ) -> CallRecord {
        let option = &self.option;
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

        let option = &self.option;
        CallRecord {
            option: Some(option.clone()),
            call_id: session_id,
            call_type,
            start_time: self.start_time,
            ring_time: self.ring_time.clone(),
            answer_time: self.answer_time.clone(),
            end_time: Utc::now(),
            caller: option.caller.clone().unwrap_or_default(),
            callee: option.callee.clone().unwrap_or_default(),
            hangup_reason: self.hangup_reason.clone(),
            status_code: self.last_status_code,
            answer: self.answer.clone(),
            offer: self.option.offer.clone(),
            extras: None,
            dump_event_file,
            recorder,
        }
    }
}

pub async fn dump_events_to_file(
    cancel_token: CancellationToken,
    dump_file: &mut File,
    mut cmd_receiver: CommandReceiver,
    event_receiver: &mut EventReceiver,
) {
    loop {
        select! {
            _ = cancel_token.cancelled() => {
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

pub async fn handle_call(
    cancel_token: CancellationToken,
    call_type: ActiveCallType,
    session_id: String,
    dump_events: bool,
    app_state: AppState,
    event_sender: EventSender,
    audio_receiver: WebsocketBytesReceiver,
    dump_cmd_receiver: CommandReceiver,
    cmd_receiver: CommandReceiver,
) -> Result<CallRecord> {
    let dump_events_loop = async {
        if !dump_events {
            return;
        }
        let file_name = app_state.get_dump_events_file(&session_id);
        let mut dump_file = match File::options()
            .create(true)
            .append(true)
            .open(&file_name)
            .await
        {
            Ok(file) => file,
            Err(e) => {
                warn!(
                    session_id,
                    file_name, "Failed to open dump events file: {}", e
                );
                return;
            }
        };
        let mut event_receiver = event_sender.subscribe();

        dump_events_to_file(
            cancel_token.clone(),
            &mut dump_file,
            dump_cmd_receiver,
            &mut event_receiver,
        )
        .await;

        while let Ok(event) = event_receiver.try_recv() {
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
    };

    let process_call_loop = async {
        let r = process_call(
            cancel_token.clone(),
            call_type,
            session_id.clone(),
            app_state.clone(),
            event_sender.clone(),
            audio_receiver,
            cmd_receiver,
        )
        .await;

        let mut active_calls = app_state.active_calls.lock().await;
        active_calls.remove(&session_id);

        cancel_token.cancel();
        r
    };
    let (_, process_call_result) = join! {
        dump_events_loop,
        process_call_loop
    };
    debug!(session_id, "call processing completed");
    process_call_result
}

pub async fn process_call(
    cancel_token: CancellationToken,
    call_type: ActiveCallType,
    session_id: String,
    app_state: AppState,
    event_sender: EventSender,
    audio_receiver: WebsocketBytesReceiver,
    mut cmd_receiver: CommandReceiver,
) -> Result<CallRecord> {
    let mut option = match cmd_receiver.recv().await {
        Ok(command) => match command {
            Command::Invite { option } => option,
            Command::Accept { option } => option,
            _ => {
                info!(
                    session_id,
                    "the first message must be an invite {:?}", command
                );
                return Err(anyhow::anyhow!("the first message must be an invite"));
            }
        },
        _ => {
            return Err(anyhow::anyhow!("Invalid message type"));
        }
    };
    option.check_default(); // check default

    info!(session_id, ?call_type, "prepare call option: {:?}", option);
    let mut call_state = ActiveCallState {
        start_time: Utc::now(),
        option: option,
        ssrc: rand::random::<u32>(),
        ..Default::default()
    };

    let track_config = TrackConfig::default();
    let (dlg_state_sender, dlg_state_receiver) = tokio::sync::mpsc::unbounded_channel();
    let caller_track = match call_type {
        ActiveCallType::WebSocket => {
            create_websocket_track(
                cancel_token.clone(),
                track_config,
                &mut call_state,
                session_id.clone(),
                event_sender.clone(),
                audio_receiver,
            )
            .await?
        }
        ActiveCallType::Webrtc => {
            create_webrtc_track(
                cancel_token.clone(),
                track_config,
                &mut call_state,
                session_id.clone(),
            )
            .await?
        }
        ActiveCallType::Sip => {
            let r = if let Some(pending_dialog) =
                app_state.useragent.get_pending_call(&session_id).await
            {
                create_incoming_sip_track(
                    pending_dialog,
                    cancel_token.clone(),
                    track_config,
                    &mut call_state,
                    session_id.clone(),
                    app_state.clone(),
                    dlg_state_sender,
                )
                .await
            } else {
                create_outgoing_sip_track(
                    cancel_token.clone(),
                    track_config,
                    &mut call_state,
                    session_id.clone(),
                    app_state.clone(),
                    dlg_state_sender,
                )
                .await
            };
            let rtp_track = match r {
                Ok(Some(track)) => track,
                Ok(None) => {
                    warn!(session_id, "no rtp track created for sip call");
                    return Ok(call_state.build_callrecord(
                        app_state.clone(),
                        session_id.clone(),
                        call_type,
                    ));
                }
                Err(e) => {
                    warn!(session_id, "error creating sip/rtp track: {}", e);
                    return Err(anyhow::anyhow!("error creating sip/rtp track: {}", e));
                }
            };
            rtp_track
        }
    };

    let answer = call_state.answer.clone().unwrap_or_default();
    let media_stream = match ActiveCall::create_stream(
        cancel_token.clone(),
        session_id.clone(),
        &call_state.option,
        caller_track,
        app_state.clone(),
        event_sender.clone(),
    )
    .await
    {
        Ok(media_stream) => media_stream,
        Err(e) => {
            warn!(session_id, "error creating media stream: {}", e);
            return Err(anyhow::anyhow!("error creating media stream: {}", e));
        }
    };

    let call_state = Arc::new(RwLock::new(call_state));
    let active_call = Arc::new(ActiveCall::new(
        call_state,
        call_type,
        cancel_token.clone(),
        event_sender.clone(),
        session_id.clone(),
        media_stream,
        app_state.clone(),
    ));

    let active_calls_len = {
        let mut active_calls = app_state.active_calls.lock().await;
        active_calls.insert(session_id, active_call.clone());
        active_calls.len()
    };

    info!(
        active_calls = active_calls_len,
        session_id = active_call.session_id,
        call_type = ?active_call.call_type,
        answer,
        "new call"
    );

    event_sender
        .send(SessionEvent::Answer {
            track_id: active_call.session_id.clone(),
            timestamp: crate::get_timestamp(),
            sdp: answer,
        })
        .ok();

    let active_call_ref = active_call.clone();
    let process_command_loop = async move {
        while let Ok(command) = cmd_receiver.recv().await {
            match active_call_ref.dispatch(command).await {
                Ok(_) => (),
                Err(e) => {
                    warn!(
                        session_id = active_call_ref.session_id,
                        "Error dispatching command: {}", e
                    );
                }
            }
        }
    };

    let call_state_ref = active_call.call_state.clone();
    let session_id = active_call.session_id.clone();
    let process_sip_dlg_loop = async {
        super::sip::sip_event_loop(
            session_id.clone(),
            session_id,
            event_sender.clone(),
            dlg_state_receiver,
            call_state_ref,
        )
        .await
    };

    select! {
        _ = async {
            join! {
                process_sip_dlg_loop,
                cancel_token.cancelled()
            }
        } => {
            info!(session_id = active_call.session_id, "audio/call loop done");
        }
        _ = process_command_loop => {
            info!(session_id = active_call.session_id, "command loop done");
        }
        _ = active_call.serve() => {
            info!(session_id = active_call.session_id, "call serve done");
        }
        _ = cancel_token.cancelled() => {
            info!(session_id = active_call.session_id, "call cancelled");
        }
    }
    active_call.cleanup().await.ok();
    Ok(active_call.get_callrecord().await)
}

async fn create_websocket_track(
    cancel_token: CancellationToken,
    track_config: TrackConfig,
    call_state: &mut ActiveCallState,
    sesion_id: String,
    event_sender: EventSender,
    audio_receiver: WebsocketBytesReceiver,
) -> Result<Box<dyn Track>> {
    let ws_track = WebsocketTrack::new(
        cancel_token.child_token(),
        sesion_id,
        track_config,
        event_sender,
        audio_receiver,
        call_state.option.codec.clone(),
        call_state.ssrc,
    );
    call_state.answer_time = Some(Utc::now());
    call_state.answer = Some("".to_string());
    call_state.last_status_code = 200;
    Ok(Box::new(ws_track))
}

async fn create_webrtc_track(
    cancel_token: CancellationToken,
    track_config: TrackConfig,
    call_state: &mut ActiveCallState,
    session_id: String,
) -> Result<Box<dyn Track>> {
    let mut webrtc_track =
        WebrtcTrack::new(cancel_token.child_token(), session_id.clone(), track_config)
            .with_ssrc(call_state.ssrc);

    let timeout = call_state
        .option
        .handshake_timeout
        .as_ref()
        .map(|d| d.parse::<u64>().map(|d| Duration::from_secs(d)).ok())
        .flatten();

    let offer = match call_state.option.enable_ipv6 {
        Some(false) | None => {
            strip_ipv6_candidates(call_state.option.offer.as_ref().unwrap_or(&"".to_string()))
        }
        _ => call_state.option.offer.clone().unwrap_or("".to_string()),
    };
    let answer: Option<String>;
    match webrtc_track.handshake(offer, timeout).await {
        Ok(answer_sdp) => {
            answer = match call_state.option.enable_ipv6 {
                Some(false) | None => Some(strip_ipv6_candidates(&answer_sdp)),
                Some(true) => Some(answer_sdp),
            };
        }
        Err(e) => {
            warn!(session_id, "Failed to setup track: {}", e);
            return Err(anyhow::anyhow!("Failed to setup track: {}", e));
        }
    }
    call_state.answer_time = Some(Utc::now());
    call_state.answer = answer;
    call_state.last_status_code = 200;
    Ok(Box::new(webrtc_track))
}

async fn create_outgoing_sip_track(
    cancel_token: CancellationToken,
    track_config: TrackConfig,
    call_state: &mut ActiveCallState,
    session_id: String,
    app_state: AppState,
    dlg_state_sender: DialogStateSender,
) -> Result<Option<Box<dyn Track>>> {
    let r = super::sip::new_rtp_track_with_sip(
        app_state,
        cancel_token.child_token(),
        session_id.clone(),
        call_state.ssrc,
        track_config,
        &call_state.option,
        dlg_state_sender,
    )
    .await;
    match r {
        Ok((dialog_id, rtp_track)) => {
            call_state.dialog_id = Some(dialog_id);
            call_state.option.offer = rtp_track.local_description().ok();
            call_state.answer = rtp_track.remote_description();
            call_state.answer_time = Some(Utc::now());
            call_state.last_status_code = 200;
            Ok(Some(Box::new(rtp_track)))
        }
        Err(e) => {
            warn!(session_id, "error creating rtp track: {}", e);
            Err(anyhow::anyhow!("error creating sip/rtp track: {}", e))
        }
    }
}

async fn create_incoming_sip_track(
    pending_dialog: PendingDialog,
    cancel_token: CancellationToken,
    track_config: TrackConfig,
    call_state: &mut ActiveCallState,
    session_id: String,
    app_state: AppState,
    dlg_state_sender: DialogStateSender,
) -> Result<Option<Box<dyn Track>>> {
    let r = super::sip::new_rtp_track_with_pending_call(
        app_state,
        cancel_token.child_token(),
        session_id.clone(),
        call_state.ssrc,
        track_config,
        &call_state.option,
        dlg_state_sender,
        pending_dialog,
    )
    .await;
    match r {
        Ok((dialog_id, rtp_track)) => {
            call_state.dialog_id = Some(dialog_id);
            call_state.option.offer = rtp_track.remote_description();
            call_state.answer = rtp_track.local_description().ok();
            call_state.answer_time = Some(Utc::now());
            call_state.last_status_code = 200;
            Ok(Some(Box::new(rtp_track)))
        }
        Err(e) => {
            warn!(session_id, "error creating rtp track: {}", e);
            Err(anyhow::anyhow!("error creating sip/rtp track: {}", e))
        }
    }
}
