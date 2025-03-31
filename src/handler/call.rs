use super::{processor::AsrProcessor, Command, StreamOptions};
use crate::{
    media::{
        processor::Processor,
        stream::{MediaStream, MediaStreamBuilder},
        track::{webrtc::WebrtcTrack, Track},
        vad::VadProcessor,
    },
    transcription::{TencentCloudAsrClientBuilder, TranscriptionType},
    TrackId,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub type ActiveCallRef = Arc<ActiveCall>;
// Session state for active calls
#[derive(Clone)]
pub struct CallHandlerState {
    pub active_calls: Arc<Mutex<HashMap<String, ActiveCallRef>>>,
    pub recorder_root: String,
}

impl CallHandlerState {
    pub fn new() -> Self {
        let recorder_root =
            std::env::var("RECORDER_ROOT").unwrap_or_else(|_| "/tmp/recorder".to_string());
        Self {
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            recorder_root,
        }
    }

    pub fn get_recorder_file(&self, session_id: &String) -> String {
        std::path::Path::new(&self.recorder_root)
            .join(session_id)
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
    pub options: StreamOptions,
    pub created_at: DateTime<Utc>,
    pub media_stream: Arc<MediaStream>,
}

impl ActiveCall {
    pub async fn create_stream(
        state: &CallHandlerState,
        cancel_token: CancellationToken,
        track_id: TrackId,
        session_id: &String,
        options: &StreamOptions,
    ) -> Result<MediaStream> {
        let mut media_stream_builder = MediaStreamBuilder::new()
            .with_id(session_id.clone())
            .cancel_token(cancel_token.clone());

        if options.enable_recorder.unwrap_or(false) {
            media_stream_builder =
                media_stream_builder.recorder(state.get_recorder_file(session_id));
        }

        let media_stream = media_stream_builder.build();
        let offer = options
            .sdp
            .clone()
            .ok_or(anyhow::anyhow!("SDP is required"))?;

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
        if options.enable_recorder.unwrap_or(false) {
            // add recorder processor
            //let recorder_processor = RecorderProcessor::new(media_stream.get_event_sender());
            // processors.push(Box::new(recorder_processor) as Box<dyn Processor>);
        }

        match options.vad_type {
            Some(ref vad_type) => {
                let vad_processor = VadProcessor::new(
                    track_id.clone(),
                    vad_type.clone(),
                    media_stream.get_event_sender(),
                );
                processors.push(Box::new(vad_processor) as Box<dyn Processor>);
            }
            None => {}
        }
        match options.asr_type {
            Some(ref asr_type) => match asr_type {
                TranscriptionType::TencentCloud => {
                    let asr_config = options.asr_config.clone().unwrap_or_default();
                    let asr_client = TencentCloudAsrClientBuilder::new(asr_config)
                        .with_track_id(track_id.clone())
                        .with_cancellation_token(cancel_token.clone())
                        .build()
                        .await?;

                    let asr_processor = AsrProcessor::new(
                        track_id.clone(),
                        asr_client,
                        media_stream.get_event_sender(),
                    );
                    processors.push(Box::new(asr_processor) as Box<dyn Processor>);
                }
            },
            None => {}
        }
        webrtc_track.with_processors(processors);

        match webrtc_track.setup_webrtc_track(offer, timeout).await {
            Ok(_) => {
                media_stream.update_track(Box::new(webrtc_track)).await;
            }
            Err(e) => {
                warn!("Failed to setup webrtc track: {}", e);
                return Err(e);
            }
        }
        Ok(media_stream)
    }

    pub async fn new_webrtc(
        state: &CallHandlerState,
        cancel_token: CancellationToken,
        track_id: TrackId,
        session_id: String,
        options: StreamOptions,
    ) -> Result<Self> {
        let media_stream =
            Self::create_stream(state, cancel_token.clone(), track_id, &session_id, &options)
                .await?;

        let active_call = ActiveCall {
            cancel_token,
            call_type: ActiveCallType::Webrtc,
            session_id,
            options,
            created_at: Utc::now(),
            media_stream: Arc::new(media_stream),
        };
        Ok(active_call)
    }

    pub async fn process_stream(&self) -> Result<()> {
        Ok(())
    }

    pub async fn dispatch(&self, command: Command) -> Result<()> {
        match command {
            Command::Candidate { candidates } => self.do_candidate(candidates).await,
            Command::Tts {
                text,
                speaker,
                play_id,
            } => self.do_tts(text, speaker, play_id).await,
            Command::Play { url } => self.do_play(url).await,
            Command::Hangup {} => self.do_hangup().await,
            Command::Refer { target } => self.do_refer(target).await,
            Command::Mute { track_id } => self.do_mute(track_id).await,
            Command::Unmute { track_id } => self.do_unmute(track_id).await,
            _ => {
                info!("Invalid command: {:?}", command);
                Ok(())
            }
        }
    }

    async fn do_candidate(&self, candidates: Vec<String>) -> Result<()> {
        Ok(())
    }

    async fn do_tts(
        &self,
        text: String,
        speaker: Option<String>,
        play_id: Option<String>,
    ) -> Result<()> {
        Ok(())
    }
    async fn do_play(&self, url: String) -> Result<()> {
        Ok(())
    }
    async fn do_hangup(&self) -> Result<()> {
        Ok(())
    }
    async fn do_refer(&self, target: String) -> Result<()> {
        Ok(())
    }
    async fn do_mute(&self, track_id: Option<String>) -> Result<()> {
        Ok(())
    }
    async fn do_unmute(&self, track_id: Option<String>) -> Result<()> {
        Ok(())
    }
}
