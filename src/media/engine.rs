use super::{
    asr_processor::AsrProcessor,
    denoiser::NoiseReducer,
    processor::Processor,
    track::{
        tts::{TtsHandle, TtsTrack},
        Track,
    },
    vad::{VADOption, VadEngine, VadProcessor, VadType},
};
use crate::{
    event::EventSender,
    handler::CallOption,
    synthesis::{
        SynthesisClient, SynthesisOption, SynthesisType, TencentCloudTtsClient, VoiceApiTtsClient,
    },
    transcription::{
        TencentCloudAsrClientBuilder, TranscriptionClient, TranscriptionOption, TranscriptionType,
        VoiceApiAsrClientBuilder,
    },
    TrackId,
};
use anyhow::Result;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub type FnCreateVadEngine = fn(option: &VADOption) -> Result<Box<dyn VadEngine>>;
pub type FnCreateAsrClient = Box<
    dyn Fn(
            TrackId,
            CancellationToken,
            TranscriptionOption,
            EventSender,
        ) -> Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>
        + Send
        + Sync,
>;
pub type FnCreateTtsClient = fn(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>>;

// Define hook types
pub type CreateProcessorsHook = Box<
    dyn Fn(
            Arc<StreamEngine>,
            &dyn Track,
            CancellationToken,
            EventSender,
            CallOption,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Box<dyn Processor>>>> + Send>>
        + Send
        + Sync,
>;

pub struct StreamEngine {
    vad_creators: HashMap<VadType, FnCreateVadEngine>,
    asr_creators: HashMap<TranscriptionType, FnCreateAsrClient>,
    tts_creators: HashMap<SynthesisType, FnCreateTtsClient>,
    pub create_processors_hook: Arc<CreateProcessorsHook>,
}

impl Default for StreamEngine {
    fn default() -> Self {
        let mut engine = Self::new();
        #[cfg(feature = "vad_silero")]
        engine.register_vad(VadType::Silero, VadProcessor::create_silero);
        #[cfg(feature = "vad_webrtc")]
        engine.register_vad(VadType::WebRTC, VadProcessor::create_webrtc);

        engine.register_asr(
            TranscriptionType::TencentCloud,
            Box::new(TencentCloudAsrClientBuilder::create),
        );
        engine.register_asr(
            TranscriptionType::VoiceApi,
            Box::new(VoiceApiAsrClientBuilder::create),
        );
        engine.register_tts(SynthesisType::TencentCloud, TencentCloudTtsClient::create);
        engine.register_tts(SynthesisType::VoiceApi, VoiceApiTtsClient::create);
        engine
    }
}

impl StreamEngine {
    pub fn new() -> Self {
        Self {
            vad_creators: HashMap::new(),
            asr_creators: HashMap::new(),
            tts_creators: HashMap::new(),
            create_processors_hook: Arc::new(Box::new(Self::default_create_procesors_hook)),
        }
    }

    pub fn register_vad(&mut self, vad_type: VadType, creator: FnCreateVadEngine) -> &mut Self {
        self.vad_creators.insert(vad_type, creator);
        self
    }

    pub fn register_asr(
        &mut self,
        asr_type: TranscriptionType,
        creator: FnCreateAsrClient,
    ) -> &mut Self {
        self.asr_creators.insert(asr_type, creator);
        self
    }

    pub fn register_tts(
        &mut self,
        tts_type: SynthesisType,
        creator: FnCreateTtsClient,
    ) -> &mut Self {
        self.tts_creators.insert(tts_type, creator);
        self
    }

    pub fn create_vad(&self, event_sender: EventSender, option: VADOption) -> Result<VadProcessor> {
        let creator = self.vad_creators.get(&option.r#type);
        if let Some(creator) = creator {
            let engine = creator(&option)?;
            VadProcessor::new(engine, event_sender, option)
        } else {
            Err(anyhow::anyhow!("VAD type not found: {}", option.r#type))
        }
    }

    pub async fn create_asr_client(
        &self,
        track_id: TrackId,
        cancel_token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> Result<Box<dyn TranscriptionClient>> {
        match option.provider {
            Some(ref provider) => {
                let creator = self.asr_creators.get(&provider);
                if let Some(creator) = creator {
                    let client = creator(track_id, cancel_token, option, event_sender).await?;
                    Ok(client)
                } else {
                    Err(anyhow::anyhow!("ASR type not found: {}", provider))
                }
            }
            None => Err(anyhow::anyhow!("ASR type not found: {:?}", option.provider)),
        }
    }

    pub async fn create_tts_client(
        &self,
        tts_option: &SynthesisOption,
    ) -> Result<Box<dyn SynthesisClient>> {
        match tts_option.provider {
            Some(ref provider) => {
                let creator = self.tts_creators.get(&provider);
                if let Some(creator) = creator {
                    creator(tts_option)
                } else {
                    Err(anyhow::anyhow!("TTS type not found: {}", provider))
                }
            }
            None => Err(anyhow::anyhow!(
                "TTS type not found: {:?}",
                tts_option.provider
            )),
        }
    }

    pub async fn create_processors(
        engine: Arc<StreamEngine>,
        track: &dyn Track,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        option: &CallOption,
    ) -> Result<Vec<Box<dyn Processor>>> {
        (engine.clone().create_processors_hook)(
            engine,
            track,
            cancel_token,
            event_sender,
            option.clone(),
        )
        .await
    }

    pub async fn create_tts_track(
        engine: Arc<StreamEngine>,
        cancel_token: CancellationToken,
        track_id: TrackId,
        play_id: Option<String>,
        tts_option: &SynthesisOption,
    ) -> Result<(TtsHandle, Box<dyn Track>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let new_handle = TtsHandle::new(tx, play_id);
        let tts_client = engine.create_tts_client(tts_option).await?;
        let tts_track = TtsTrack::new(track_id, rx, tts_client).with_cancel_token(cancel_token);
        Ok((new_handle, Box::new(tts_track) as Box<dyn Track>))
    }

    fn default_create_procesors_hook(
        engine: Arc<StreamEngine>,
        track: &dyn Track,
        cancel_token: CancellationToken,
        event_sender: EventSender,
        option: CallOption,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Box<dyn Processor>>>> + Send>> {
        let track_id = track.id().clone();
        let samplerate = track.config().samplerate as usize;
        Box::pin(async move {
            let mut processors = vec![];
            match option.denoise {
                Some(true) => {
                    let noise_reducer = NoiseReducer::new(samplerate)?;
                    processors.push(Box::new(noise_reducer) as Box<dyn Processor>);
                }
                _ => {}
            }
            match option.vad {
                Some(ref vad_option) => {
                    info!("Vad processor added {:?}", vad_option);
                    let vad_option = vad_option.to_owned();
                    let vad_processor = engine.create_vad(event_sender.clone(), vad_option)?;
                    processors.push(Box::new(vad_processor) as Box<dyn Processor>);
                }
                None => {}
            }
            match option.asr {
                Some(ref asr_option) => {
                    info!("Asr processor added {:?}", asr_option);
                    let asr_client = engine
                        .create_asr_client(
                            track_id,
                            cancel_token,
                            asr_option.clone(),
                            event_sender.clone(),
                        )
                        .await?;
                    processors.push(Box::new(AsrProcessor { asr_client }) as Box<dyn Processor>);
                }
                None => {}
            }

            Ok(processors)
        })
    }
}
