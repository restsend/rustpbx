use super::{processor::AsrProcessor, Command, ReferOption, StreamOption};
use crate::{
    app::AppState,
    event::{EventReceiver, EventSender, SessionEvent},
    media::{
        denoiser::NoiseReducer,
        negotiate::strip_ipv6_candidates,
        processor::Processor,
        stream::{MediaStream, MediaStreamBuilder},
        track::{
            file::FileTrack,
            tts::{TtsCommand, TtsCommandSender, TtsTrack},
            webrtc::WebrtcTrack,
            Track, TrackConfig,
        },
        vad::VadProcessor,
    },
    synthesis::{
        create_synthesis_client, SynthesisOption, SynthesisType, TencentCloudTtsClient,
        VoiceApiTtsClient,
    },
    transcription::{TencentCloudAsrClientBuilder, TranscriptionType, VoiceApiAsrClientBuilder},
    TrackId,
};
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use chrono::{DateTime, Utc};
use futures::{stream::SplitSink, SinkExt, StreamExt};
use rsipstack::dialog::{
    dialog::{Dialog, DialogState, DialogStateReceiver},
    DialogId,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

pub type ActiveCallRef = Arc<ActiveCall>;
#[derive(Deserialize)]
pub struct CallParams {
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
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
    pub tts_command_tx: Mutex<Option<TtsCommandSender>>,
    pub tts_option: Option<SynthesisOption>,
    pub auto_hangup: Arc<Mutex<Option<bool>>>,
    pub event_sender: EventSender,
    pub dialog_id: Mutex<Option<DialogId>>,
}

impl ActiveCall {
    pub async fn create_stream(
        mut caller_track: Box<dyn Track>,
        state: AppState,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        track_id: TrackId,
        session_id: &String,
        option: StreamOption,
    ) -> Result<MediaStream> {
        let mut media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .cancel_token(cancel_token.clone());

        if let Some(_) = option.recorder {
            let recorder_file = state.get_recorder_file(session_id);
            info!("recorder: created recording file: {}", recorder_file);
            media_stream_builder = media_stream_builder.recorder(recorder_file);
        }

        let media_stream = media_stream_builder.build();

        let offer = match option.enable_ipv6 {
            Some(false) | None => strip_ipv6_candidates(
                option
                    .offer
                    .as_ref()
                    .ok_or(anyhow::anyhow!("SDP is required"))?,
            ),
            _ => option
                .offer
                .clone()
                .ok_or(anyhow::anyhow!("SDP is required"))?,
        };
        let mut processors = vec![];
        match option.denoise {
            Some(true) => {
                let noise_reducer = NoiseReducer::new(16000)?;
                processors.push(Box::new(noise_reducer) as Box<dyn Processor>);
            }
            _ => {}
        }
        match option.vad {
            Some(ref vad_option) => {
                let vad_option = vad_option.to_owned();
                info!("Vad processor added {:?}", vad_option);
                let vad_processor = VadProcessor::new(
                    vad_option.r#type,
                    media_stream.get_event_sender(),
                    vad_option,
                )?;
                processors.push(Box::new(vad_processor) as Box<dyn Processor>);
            }
            None => {}
        }
        match option.asr {
            Some(ref asr_option) => match asr_option.provider {
                Some(TranscriptionType::TencentCloud) => {
                    let asr_option = asr_option.clone().check_default();
                    let event_sender = media_stream.get_event_sender();
                    let asr_client = TencentCloudAsrClientBuilder::new(asr_option, event_sender)
                        .with_track_id(track_id.clone())
                        .with_cancellation_token(cancel_token.child_token())
                        .build()
                        .await?;
                    let asr_processor = AsrProcessor::new(asr_client);
                    processors.push(Box::new(asr_processor) as Box<dyn Processor>);
                    debug!("TencentCloud Asr processor added");
                }
                Some(TranscriptionType::VoiceApi) => {
                    let asr_option = asr_option.clone().check_default();
                    let event_sender = media_stream.get_event_sender();
                    let asr_client = VoiceApiAsrClientBuilder::new(asr_option, event_sender)
                        .with_track_id(track_id.clone())
                        .with_cancellation_token(cancel_token.child_token())
                        .build()
                        .await?;
                    let asr_processor = AsrProcessor::new(asr_client);
                    processors.push(Box::new(asr_processor) as Box<dyn Processor>);
                    debug!("VoiceApi Asr processor added");
                }
                None => {}
            },
            None => {}
        }

        for processor in processors {
            caller_track.append_processor(processor);
        }

        let timeout = option
            .handshake_timeout
            .as_ref()
            .map(|d| {
                d.clone()
                    .parse::<u64>()
                    .map(|d| Duration::from_secs(d))
                    .ok()
            })
            .flatten();

        match caller_track.handshake(offer, timeout).await {
            Ok(answer) => {
                let sdp = strip_ipv6_candidates(&answer);
                info!("track setup complete answer: {}", answer);
                event_sender
                    .send(SessionEvent::Answer {
                        track_id: track_id.clone(),
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
        tts_option: Option<SynthesisOption>,
        dialog_id: Option<DialogId>,
    ) -> Result<Self> {
        let tts_option = tts_option.and_then(|cfg| Some(cfg.check_default()));
        let active_call = ActiveCall {
            cancel_token,
            call_type,
            session_id,
            created_at: Utc::now(),
            media_stream,
            track_config: TrackConfig::default(),
            tts_command_tx: Mutex::new(None),
            tts_option,
            auto_hangup: Arc::new(Mutex::new(None)),
            event_sender,
            dialog_id: Mutex::new(dialog_id),
        };
        Ok(active_call)
    }

    pub async fn serve(&self) -> Result<()> {
        let mut event_receiver = self.media_stream.subscribe();
        let auto_hangup = self.auto_hangup.clone();
        let event_hook_loop = async move {
            loop {
                match event_receiver.recv().await {
                    Ok(event) => match event {
                        SessionEvent::TrackEnd { track_id, .. } => {
                            if let Some(auto_hangup) = auto_hangup.lock().await.take() {
                                if auto_hangup {
                                    info!(
                                        "Auto hangup when track end track_id:{} session_id:{}",
                                        track_id, self.session_id
                                    );
                                    self.do_hangup(Some("autohangup".to_string()), Some(track_id))
                                        .await
                                        .ok();
                                }
                            }
                        }
                        _ => {}
                    },
                    Err(e) => {
                        warn!("Failed to receive event: {}", e);
                        break;
                    }
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
            } => self.do_tts(text, speaker, play_id, auto_hangup).await,
            Command::Play { url, auto_hangup } => self.do_play(url, auto_hangup).await,
            Command::Hangup { reason, initiator } => self.do_hangup(reason, initiator).await,
            Command::Refer { target, options } => self.do_refer(target, options).await,
            Command::Mute { track_id } => self.do_mute(track_id).await,
            Command::Unmute { track_id } => self.do_unmute(track_id).await,
            Command::Pause {} => self.do_pause().await,
            Command::Resume {} => self.do_resume().await,
            Command::Interrupt {} => self.do_interrupt().await,
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
    ) -> Result<()> {
        let tts_option = match self.tts_option.as_ref() {
            Some(option) => option,
            None => return Ok(()),
        };
        let speaker = match speaker {
            Some(s) => Some(s),
            None => tts_option.speaker.clone(),
        };
        let mut play_command = TtsCommand {
            text,
            speaker,
            play_id,
        };
        info!(
            "active_call: new tts command, text: {} speaker: {:?} auto_hangup: {:?}",
            play_command.text, play_command.speaker, auto_hangup
        );
        if let Some(auto_hangup) = auto_hangup {
            *self.auto_hangup.lock().await = Some(auto_hangup);
        }
        let mut tts_command_tx = self.tts_command_tx.lock().await;
        if let Some(tx) = tts_command_tx.as_ref() {
            match tx.send(play_command) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    tts_command_tx.take();
                    play_command = e.0;
                }
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(play_command)?;
        tts_command_tx.replace(tx);

        match tts_option.provider {
            Some(SynthesisType::VoiceApi) => {
                let tts_client = VoiceApiTtsClient::new(tts_option.clone());
                let tts_track = TtsTrack::new(
                    self.track_config.server_side_track_id.clone(),
                    rx,
                    "voiceapi".to_string(),
                    tts_client,
                )
                .with_cancel_token(self.cancel_token.child_token());
                self.media_stream.update_track(Box::new(tts_track)).await;
            }
            _ => {
                let tts_client = TencentCloudTtsClient::new(tts_option.clone());
                let tts_track = TtsTrack::new(
                    self.track_config.server_side_track_id.clone(),
                    rx,
                    "tencent".to_string(),
                    tts_client,
                )
                .with_cancel_token(self.cancel_token.child_token());
                self.media_stream.update_track(Box::new(tts_track)).await;
            }
        };
        Ok(())
    }

    async fn do_play(&self, url: String, auto_hangup: Option<bool>) -> Result<()> {
        self.tts_command_tx.lock().await.take();
        let file_track = FileTrack::new(self.track_config.server_side_track_id.clone())
            .with_path(url)
            .with_cancel_token(self.cancel_token.child_token());

        if let Some(auto_hangup) = auto_hangup {
            *self.auto_hangup.lock().await = Some(auto_hangup);
        }
        self.media_stream.update_track(Box::new(file_track)).await;
        Ok(())
    }
    async fn do_interrupt(&self) -> Result<()> {
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
        self.cancel_token.cancel();
        info!("Call {} do_hangup", self.session_id);
        self.media_stream.stop(reason, initiator);
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
}

async fn sip_event_loop(
    track_id: TrackId,
    event_sender: &mut EventSender,
    dlg_state_receiver: &mut DialogStateReceiver,
) -> Result<()> {
    while let Some(event) = dlg_state_receiver.recv().await {
        match event {
            DialogState::Trying(dialog_id) => {
                info!("sip_call: dialog trying: {}", dialog_id);
            }
            DialogState::Early(dialog_id, _) => {
                info!("sip_call: dialog early: {}", dialog_id);
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: true,
                })?;
            }
            DialogState::Calling(dialog_id) => {
                info!("sip_call: dialog calling: {}", dialog_id);
                event_sender.send(crate::event::SessionEvent::Ringing {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                    early_media: false,
                })?;
            }
            DialogState::Confirmed(dialog_id) => {
                info!("sip_call: dialog confirmed: {}", dialog_id);
            }
            DialogState::Terminated(dialog_id, _) => {
                info!("sip_call: dialog terminated: {}", dialog_id);
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
        let data = match serde_json::to_string(&event) {
            Ok(data) => data,
            Err(e) => {
                error!("call: error serializing event: {} {:?}", e, event);
                continue;
            }
        };
        if let Err(e) = ws_sender.send(data.into()).await {
            error!("call: error sending event to WebSocket: {}", e);
        }
    }
    Ok(())
}

pub async fn handle_call(
    call_type: ActiveCallType,
    session_id: String,
    socket: axum::extract::ws::WebSocket,
    state: AppState,
) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let cancel_token = CancellationToken::new();
    let mut event_sender = crate::event::create_event_sender();
    let mut event_receiver = event_sender.subscribe();

    let track_id = session_id.clone();
    let cancel_token_ref = cancel_token.clone();
    let event_sender_ref = event_sender.clone();
    let state_clone = state.clone();
    let (dlg_state_sender, mut dlg_state_receiver) = mpsc::unbounded_channel();
    let track_id_clone = track_id.clone();
    let call_type_ref = call_type.clone();

    let prepare_call = async move {
        let mut options = match ws_receiver.next().await {
            Some(Ok(Message::Text(text))) => {
                let command = serde_json::from_str::<Command>(&text)?;
                match command {
                    Command::Invite { options } => options,
                    _ => {
                        info!("call: the first message must be an invite {:?}", command);
                        return Err(anyhow::anyhow!("the first message must be an invite"));
                    }
                }
            }
            _ => {
                return Err(anyhow::anyhow!("Invalid message type"));
            }
        };
        info!("call: prepare call options: {:?}", options);
        let track_config = TrackConfig::default();
        let mut dialog_id = None;
        let caller_track: Box<dyn Track> = match call_type {
            ActiveCallType::Webrtc | ActiveCallType::WebSocket => {
                let webrtc_track = WebrtcTrack::new(track_id.clone(), track_config);
                Box::new(webrtc_track)
            }
            ActiveCallType::Sip => {
                let (dlg_id, rtp_track) = super::sip::new_rtp_track_with_sip(
                    state_clone.clone(),
                    cancel_token.child_token(),
                    track_id.clone(),
                    track_config,
                    &options,
                    dlg_state_sender,
                )
                .await?;
                dialog_id.replace(dlg_id);
                options.offer = rtp_track.local_description().ok();
                Box::new(rtp_track)
            }
        };

        let tts_option = options.tts.clone();
        let media_stream = ActiveCall::create_stream(
            caller_track,
            state_clone,
            cancel_token.child_token(),
            event_sender_ref.clone(),
            track_id,
            &session_id,
            options,
        )
        .await?;

        let active_call = ActiveCall::new(
            call_type,
            cancel_token.child_token(),
            event_sender_ref,
            session_id,
            media_stream,
            tts_option,
            dialog_id,
        )
        .await?;
        Ok((Arc::new(active_call), ws_receiver))
    };

    let (active_call, mut ws_receiver) = select! {
        _ = cancel_token_ref.cancelled() => {
            info!("call: prepare call cancelled");
            return Err(anyhow::anyhow!("Cancelled"));
        },
        r = prepare_call => {
            match r {
                Ok((active_call, ws_receiver)) => (active_call, ws_receiver),
                Err(e) => {
                    error!("call: prepare call failed: {}", e);
                    return Err(e);
                }
            }
        }
        _ = send_to_ws_loop(&mut ws_sender, &mut event_receiver) => {
            info!("call: prepare call send to ws");
            return Err(anyhow::anyhow!("WebSocket closed"));
        }
        _ = async {
            if matches!(call_type_ref, ActiveCallType::Sip) {
                sip_event_loop(track_id_clone.clone(), &mut event_sender, &mut dlg_state_receiver).await
            } else {
                cancel_token_ref.cancelled().await;
                Ok(())
            }
        } => {
            info!("call: sip event loop");
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
        "call: new call: {} -> {:?}, {} active calls",
        active_call.session_id, active_call.call_type, active_calls_len
    );

    let recv_from_ws = async move {
        while let Some(msg) = ws_receiver.next().await {
            let command = match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<Command>(&text) {
                    Ok(command) => Some(command),
                    Err(e) => {
                        error!("call: error deserializing command: {} {}", e, text);
                        None
                    }
                },
                _ => None,
            };

            match command {
                Some(command) => match active_call.dispatch(command).await {
                    Ok(_) => (),
                    Err(e) => {
                        error!("call: Error dispatching command: {}", e);
                    }
                },
                None => {}
            }
        }
    };
    select! {
        _ = cancel_token_ref.cancelled() => {
            info!("call: cancelled");
        },
        _ = send_to_ws_loop(&mut ws_sender, &mut event_receiver) => {
            info!("call: send_to_ws websocket disconnected");
        },
        _ = recv_from_ws => {
            info!("call: recv_from_ws websocket disconnected");
        },
        r = active_call_clone.serve() => {
            info!("call: call loop disconnected {:?}", r);
        },
        _ = async {
            if matches!(call_type_ref, ActiveCallType::Sip) {
                sip_event_loop(track_id_clone.clone(), &mut event_sender, &mut dlg_state_receiver).await
            } else {
                cancel_token_ref.cancelled().await;
                Ok(())
            }
        } => {
            info!("call: sip event loop");
            return Err(anyhow::anyhow!("Sip event loop failed"));
        }
    }

    if let Some(dialog_id) = active_call_clone.dialog_id.lock().await.take() {
        let r = match state.useragent.dialog_layer.get_dialog(&dialog_id) {
            Some(dialog) => match dialog {
                Dialog::ClientInvite(dialog) => dialog.bye().await,
                Dialog::ServerInvite(dialog) => dialog.bye().await,
            },
            None => {
                error!("call: dialog not found");
                return Err(anyhow::anyhow!("dialog not found"));
            }
        };
        match r {
            Ok(_) => (),
            Err(e) => {
                error!("call: error closing dialog: {}", e);
            }
        }
    }
    Ok(())
}
