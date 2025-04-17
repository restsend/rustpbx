use super::{processor::AsrProcessor, Command, ReferOption, StreamOption};
use crate::{
    event::{EventSender, SessionEvent},
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
    synthesis::{SynthesisConfig, SynthesisType, TencentCloudTtsClient},
    transcription::{TencentCloudAsrClientBuilder, TranscriptionType},
    TrackId,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

pub type ActiveCallRef = Arc<ActiveCall>;
// Session state for active calls
#[derive(Clone)]
pub struct CallHandlerState {
    pub active_calls: Arc<Mutex<HashMap<String, ActiveCallRef>>>,
    pub recorder_root: String,
    pub llm_proxy: Option<String>,
}
#[derive(Deserialize)]
pub struct CallParams {
    pub id: Option<String>,
}

impl CallHandlerState {
    pub fn new() -> Self {
        let recorder_root =
            std::env::var("RECORDER_ROOT").unwrap_or_else(|_| "/tmp/recorder".to_string());
        let llm_proxy = std::env::var("LLM_PROXY").ok();
        Self {
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            recorder_root,
            llm_proxy,
        }
    }

    pub fn get_recorder_file(&self, session_id: &String) -> String {
        let root = Path::new(&self.recorder_root);
        if !root.exists() {
            match std::fs::create_dir_all(root) {
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        "Failed to create recorder root: {} {}",
                        e,
                        root.to_string_lossy()
                    );
                }
            }
        }
        root.join(session_id)
            .with_extension("wav")
            .to_string_lossy()
            .to_string()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ActiveCallType {
    #[serde(rename = "webrtc")]
    Webrtc,
    #[serde(rename = "sip")]
    Sip,
}

pub struct ActiveCall {
    pub cancel_token: CancellationToken,
    pub call_type: ActiveCallType,
    pub session_id: String,
    pub options: StreamOption,
    pub created_at: DateTime<Utc>,
    pub media_stream: Arc<MediaStream>,
    pub track_config: TrackConfig,
    pub tts_command_tx: Mutex<Option<TtsCommandSender>>,
    pub tts_config: Option<SynthesisConfig>,
    pub auto_hangup: Arc<Mutex<Option<bool>>>,
}

impl ActiveCall {
    pub async fn create_stream(
        state: &CallHandlerState,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        track_id: TrackId,
        session_id: &String,
        options: &StreamOption,
    ) -> Result<MediaStream> {
        let mut media_stream_builder = MediaStreamBuilder::new(event_sender.clone())
            .with_id(session_id.clone())
            .cancel_token(cancel_token.clone());

        if let Some(_) = options.recorder {
            let recorder_file = state.get_recorder_file(session_id);
            media_stream_builder = media_stream_builder.recorder(recorder_file);
        }

        let media_stream = media_stream_builder.build();
        let offer = match options.enable_ipv6 {
            Some(false) | None => strip_ipv6_candidates(
                options
                    .offer
                    .as_ref()
                    .ok_or(anyhow::anyhow!("SDP is required"))?,
            ),
            _ => options
                .offer
                .clone()
                .ok_or(anyhow::anyhow!("SDP is required"))?,
        };

        let mut webrtc_track = WebrtcTrack::new(track_id.clone());
        let timeout = options
            .handshake_timeout
            .as_ref()
            .map(|d| {
                d.clone()
                    .parse::<u64>()
                    .map(|d| Duration::from_secs(d))
                    .ok()
            })
            .flatten();

        let mut processors = vec![];
        match options.denoise {
            Some(true) => {
                let noise_reducer = NoiseReducer::new(16000)?;
                processors.push(Box::new(noise_reducer) as Box<dyn Processor>);
            }
            _ => {}
        }
        match options.vad {
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
        match options.asr {
            Some(ref asr_config) => match asr_config.provider {
                Some(TranscriptionType::TencentCloud) => {
                    let mut asr_config = asr_config.clone();
                    if asr_config.app_id.is_none() {
                        asr_config.app_id = std::env::var("TENCENT_APPID").ok();
                    }
                    if asr_config.secret_id.is_none() {
                        asr_config.secret_id = std::env::var("TENCENT_SECRET_ID").ok();
                    }
                    if asr_config.secret_key.is_none() {
                        asr_config.secret_key = std::env::var("TENCENT_SECRET_KEY").ok();
                    }

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

        for processor in processors {
            webrtc_track.append_processor(processor);
        }
        match webrtc_track.setup_webrtc_track(offer, timeout).await {
            Ok(answer) => {
                let sdp = strip_ipv6_candidates(&answer.sdp);
                info!("Webrtc track setup complete answer: {}", sdp);
                event_sender
                    .send(SessionEvent::Answer {
                        track_id: track_id.clone(),
                        timestamp: crate::get_timestamp(),
                        sdp,
                    })
                    .ok();
                media_stream.update_track(Box::new(webrtc_track)).await;
                Ok(media_stream)
            }
            Err(e) => {
                warn!("Failed to setup webrtc track: {}", e);
                Err(e)
            }
        }
    }

    pub async fn new_webrtc(
        state: &CallHandlerState,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        track_id: TrackId,
        session_id: String,
        options: StreamOption,
    ) -> Result<Self> {
        let media_stream = Self::create_stream(
            state,
            cancel_token.child_token(),
            event_sender,
            track_id,
            &session_id,
            &options,
        )
        .await?;
        let track_config = TrackConfig::default();
        let mut tts_config = options.tts.clone();
        match tts_config {
            Some(ref mut tts_config) => match tts_config.provider {
                Some(SynthesisType::TencentCloud) => {
                    if tts_config.app_id.is_none() {
                        tts_config.app_id = std::env::var("TENCENT_APPID").ok();
                    }
                    if tts_config.secret_id.is_none() {
                        tts_config.secret_id = std::env::var("TENCENT_SECRET_ID").ok();
                    }
                    if tts_config.secret_key.is_none() {
                        tts_config.secret_key = std::env::var("TENCENT_SECRET_KEY").ok();
                    }
                }
                _ => {}
            },
            None => {}
        }
        let active_call = ActiveCall {
            cancel_token,
            call_type: ActiveCallType::Webrtc,
            session_id,
            options,
            created_at: Utc::now(),
            media_stream: Arc::new(media_stream),
            track_config,
            tts_command_tx: Mutex::new(None),
            tts_config,
            auto_hangup: Arc::new(Mutex::new(None)),
        };
        Ok(active_call)
    }

    pub async fn process_stream(&self) -> Result<()> {
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
        let tts_config = match self.tts_config {
            Some(ref config) => config,
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
                self.media_stream.get_event_sender(),
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
