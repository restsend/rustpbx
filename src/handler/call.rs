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
    synthesis::{SynthesisConfig, TencentCloudTtsClient},
    transcription::{TencentCloudAsrClientBuilder, TranscriptionType},
    useragent::UserAgent,
    TrackId,
};
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use chrono::{DateTime, Utc};
use futures::{stream::SplitSink, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

pub type ActiveCallRef = Arc<ActiveCall>;

// Session state for active calls
#[derive(Clone)]
pub struct CallHandlerState {
    pub user_agent: Arc<UserAgent>,
}
#[derive(Deserialize)]
pub struct CallParams {
    pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
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
    pub tts_config: Option<SynthesisConfig>,
    pub auto_hangup: Arc<Mutex<Option<bool>>>,
    pub event_sender: EventSender,
}

impl ActiveCall {
    pub async fn create_stream(
        call_type: &ActiveCallType,
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
            Some(ref vad_config) => {
                let vad_config = vad_config.to_owned();
                info!("Vad processor added {:?}", vad_config);
                let vad_processor = VadProcessor::new(
                    vad_config.r#type,
                    media_stream.get_event_sender(),
                    vad_config,
                )?;
                processors.push(Box::new(vad_processor) as Box<dyn Processor>);
            }
            None => {}
        }
        match option.asr {
            Some(ref asr_config) => match asr_config.provider {
                Some(TranscriptionType::TencentCloud) => {
                    let asr_config = asr_config.clone().check_default();
                    let event_sender = media_stream.get_event_sender();
                    let asr_client = TencentCloudAsrClientBuilder::new(asr_config, event_sender)
                        .with_track_id(track_id.clone())
                        .with_cancellation_token(cancel_token.child_token())
                        .build()
                        .await?;
                    let asr_processor = AsrProcessor::new(asr_client);
                    processors.push(Box::new(asr_processor) as Box<dyn Processor>);
                    debug!("TencentCloud Asr processor added");
                }
                None => {}
            },
            None => {}
        }

        let track_config = TrackConfig::default();
        let mut caller_track: Box<dyn Track> = match call_type {
            ActiveCallType::Webrtc | ActiveCallType::WebSocket => {
                let webrtc_track = WebrtcTrack::new(track_id.clone(), track_config);
                Box::new(webrtc_track)
            }
            ActiveCallType::Sip => {
                let rtp_track = super::sip::new_rtp_track_with_sip(
                    state.clone(),
                    cancel_token.child_token(),
                    track_id.clone(),
                    track_config,
                    &option,
                )
                .await?;
                Box::new(rtp_track)
            }
        };

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
                info!("Webrtc track setup complete answer: {}", sdp);
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
                warn!("Failed to setup webrtc track: {}", e);
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
        tts_config: Option<SynthesisConfig>,
    ) -> Result<Self> {
        let tts_config = tts_config.and_then(|cfg| Some(cfg.check_default()));
        let active_call = ActiveCall {
            cancel_token,
            call_type,
            session_id,
            created_at: Utc::now(),
            media_stream,
            track_config: TrackConfig::default(),
            tts_command_tx: Mutex::new(None),
            tts_config,
            auto_hangup: Arc::new(Mutex::new(None)),
            event_sender,
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
        let tts_config = match self.tts_config.as_ref() {
            Some(config) => config,
            None => return Ok(()),
        };
        let speaker = match speaker {
            Some(s) => Some(s),
            None => tts_config.speaker.clone(),
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
        let tts_client = TencentCloudTtsClient::new(tts_config.clone());
        let tts_track = TtsTrack::new(
            self.track_config.server_side_track_id.clone(),
            rx,
            tts_client,
        )
        .with_cancel_token(self.cancel_token.child_token());

        match tts_track
            .start(
                self.cancel_token.clone(),
                self.event_sender.clone(),
                self.media_stream.packet_sender.clone(),
            )
            .await
        {
            Ok(_) => {
                tx.send(play_command)?;
                tts_command_tx.replace(tx);
                Ok(())
            }
            Err(e) => {
                warn!("Failed to start tts track: {}", e);
                Err(e)
            }
        }
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

#[instrument(name = "ws_call", skip(socket, state))]
pub async fn handle_call(
    call_type: ActiveCallType,
    session_id: String,
    socket: axum::extract::ws::WebSocket,
    state: AppState,
) -> Result<()> {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let cancel_token = CancellationToken::new();
    let event_sender = crate::event::create_event_sender();
    let mut event_receiver = event_sender.subscribe();

    let track_id = session_id.clone();
    let cancel_token_ref = cancel_token.clone();
    let event_sender_ref = event_sender.clone();
    let state_clone = state.clone();
    let send_to_ws_loop = async |ws_sender: &mut SplitSink<WebSocket, Message>,
                                 event_receiver: &mut EventReceiver| {
        while let Ok(event) = event_receiver.recv().await {
            let data = match serde_json::to_string(&event) {
                Ok(data) => data,
                Err(e) => {
                    error!("webrtc call: error serializing event: {} {:?}", e, event);
                    continue;
                }
            };
            if let Err(e) = ws_sender.send(data.into()).await {
                error!("webrtc call: error sending event to WebSocket: {}", e);
            }
        }
    };

    let prepare_call = async move {
        let options = match ws_receiver.next().await {
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
        let tts_config = options.tts.clone();
        let media_stream = ActiveCall::create_stream(
            &call_type,
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
            tts_config,
        )
        .await?;
        Ok((Arc::new(active_call), ws_receiver))
    };

    let (active_call, mut ws_receiver) = select! {
        _ = cancel_token_ref.cancelled() => {
            info!("webrtc call: prepare call cancelled");
            return Err(anyhow::anyhow!("Cancelled"));
        },
        r = prepare_call => {
            match r {
                Ok((active_call, ws_receiver)) => (active_call, ws_receiver),
                Err(e) => {
                    error!("webrtc call: prepare call failed: {}", e);
                    return Err(e);
                }
            }
        }
        _ = send_to_ws_loop(&mut ws_sender, &mut event_receiver) => {
            info!("webrtc call: prepare call send to ws");
            return Err(anyhow::anyhow!("WebSocket closed"));
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
                        error!("webrtc call: error deserializing command: {} {}", e, text);
                        None
                    }
                },
                _ => None,
            };

            match command {
                Some(command) => match active_call.dispatch(command).await {
                    Ok(_) => (),
                    Err(e) => {
                        error!("webrtc call: Error dispatching command: {}", e);
                    }
                },
                None => {}
            }
        }
    };
    select! {
        _ = cancel_token_ref.cancelled() => {
            info!("webrtc call: cancelled");
        },
        _ = send_to_ws_loop(&mut ws_sender, &mut event_receiver) => {
            info!("send_to_ws: websocket disconnected");
        },
        _ = recv_from_ws => {
            info!("recv_from_ws: websocket disconnected");
        },
        r = active_call_clone.serve() => {
            info!("webrtc call: call loop disconnected {:?}", r);
        },
    }
    Ok(())
}
