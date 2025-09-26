use super::{
    asr_processor::AsrProcessor,
    denoiser::NoiseReducer,
    processor::Processor,
    track::{
        Track,
        tts::{SynthesisHandle, TtsTrack},
    },
    vad::{VADOption, VadProcessor, VadType},
};
use crate::{
    TrackId,
    call::{CallOption, EouOption},
    event::EventSender,
    synthesis::{
        AliyunTtsClient, SynthesisClient, SynthesisOption, SynthesisType,
        TencentCloudTtsBasicClient, TencentCloudTtsClient, VoiceApiTtsClient,
    },
    transcription::{
        AliyunAsrClientBuilder, TencentCloudAsrClientBuilder, TranscriptionClient,
        TranscriptionOption, TranscriptionType, VoiceApiAsrClientBuilder,
    },
};
use anyhow::Result;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type FnCreateVadProcessor = fn(
    token: CancellationToken,
    event_sender: EventSender,
    option: VADOption,
) -> Result<Box<dyn Processor>>;

pub type FnCreateEouProcessor = fn(
    token: CancellationToken,
    event_sender: EventSender,
    option: EouOption,
) -> Result<Box<dyn Processor>>;

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
pub type FnCreateTtsClient =
    fn(streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>>;

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
    vad_creators: HashMap<VadType, FnCreateVadProcessor>,
    eou_creators: HashMap<String, FnCreateEouProcessor>,
    asr_creators: HashMap<TranscriptionType, FnCreateAsrClient>,
    tts_creators: HashMap<SynthesisType, FnCreateTtsClient>,
    create_processors_hook: Arc<CreateProcessorsHook>,
}

impl Default for StreamEngine {
    fn default() -> Self {
        let mut engine = Self::new();
        #[cfg(feature = "vad_silero")]
        engine.register_vad(VadType::Silero, VadProcessor::create_silero);
        #[cfg(feature = "vad_webrtc")]
        engine.register_vad(VadType::WebRTC, VadProcessor::create_webrtc);
        #[cfg(feature = "vad_ten")]
        engine.register_vad(VadType::Ten, VadProcessor::create_ten);

        engine.register_asr(
            TranscriptionType::TencentCloud,
            Box::new(TencentCloudAsrClientBuilder::create),
        );
        engine.register_asr(
            TranscriptionType::VoiceApi,
            Box::new(VoiceApiAsrClientBuilder::create),
        );
        engine.register_asr(
            TranscriptionType::Aliyun,
            Box::new(AliyunAsrClientBuilder::create),
        );
        engine.register_tts(SynthesisType::Aliyun, AliyunTtsClient::create);
        engine.register_tts(SynthesisType::TencentCloud, TencentCloudTtsClient::create);
        engine.register_tts(SynthesisType::VoiceApi, VoiceApiTtsClient::create);
        engine.register_tts(
            SynthesisType::Other("tencent_basic".to_string()),
            TencentCloudTtsBasicClient::create,
        );
        engine
    }
}

impl StreamEngine {
    pub fn new() -> Self {
        Self {
            vad_creators: HashMap::new(),
            asr_creators: HashMap::new(),
            tts_creators: HashMap::new(),
            eou_creators: HashMap::new(),
            create_processors_hook: Arc::new(Box::new(Self::default_create_procesors_hook)),
        }
    }

    pub fn register_vad(&mut self, vad_type: VadType, creator: FnCreateVadProcessor) -> &mut Self {
        self.vad_creators.insert(vad_type, creator);
        self
    }

    pub fn register_eou(&mut self, name: String, creator: FnCreateEouProcessor) -> &mut Self {
        self.eou_creators.insert(name, creator);
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

    pub fn create_vad_processor(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        option: VADOption,
    ) -> Result<Box<dyn Processor>> {
        let creator = self.vad_creators.get(&option.r#type);
        if let Some(creator) = creator {
            creator(token, event_sender, option)
        } else {
            Err(anyhow::anyhow!("VAD type not found: {}", option.r#type))
        }
    }
    pub fn create_eou_processor(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        option: EouOption,
    ) -> Result<Box<dyn Processor>> {
        let creator = self
            .eou_creators
            .get(&option.r#type.clone().unwrap_or_default());
        if let Some(creator) = creator {
            creator(token, event_sender, option)
        } else {
            Err(anyhow::anyhow!("EOU type not found: {:?}", option.r#type))
        }
    }

    pub async fn create_asr_processor(
        &self,
        track_id: TrackId,
        cancel_token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> Result<Box<dyn Processor>> {
        let asr_client = match option.provider {
            Some(ref provider) => {
                let creator = self.asr_creators.get(&provider);
                if let Some(creator) = creator {
                    creator(track_id, cancel_token, option, event_sender).await?
                } else {
                    return Err(anyhow::anyhow!("ASR type not found: {}", provider));
                }
            }
            None => return Err(anyhow::anyhow!("ASR type not found: {:?}", option.provider)),
        };
        Ok(Box::new(AsrProcessor { asr_client }))
    }

    pub async fn create_tts_client(
        &self,
        streaming: bool,
        tts_option: &SynthesisOption,
    ) -> Result<Box<dyn SynthesisClient>> {
        match tts_option.provider {
            Some(ref provider) => {
                let creator = self.tts_creators.get(&provider);
                if let Some(creator) = creator {
                    creator(streaming, tts_option)
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
        session_id: String,
        track_id: TrackId,
        ssrc: u32,
        play_id: Option<String>,
        streaming: bool,
        tts_option: &SynthesisOption,
    ) -> Result<(SynthesisHandle, Box<dyn Track>)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let new_handle = SynthesisHandle::new(tx, play_id.clone());
        let tts_client = engine.create_tts_client(streaming, tts_option).await?;
        let tts_track = TtsTrack::new(track_id, session_id, streaming, play_id, rx, tts_client)
            .with_ssrc(ssrc)
            .with_cancel_token(cancel_token);
        Ok((new_handle, Box::new(tts_track) as Box<dyn Track>))
    }

    pub fn with_processor_hook(&mut self, hook_fn: CreateProcessorsHook) -> &mut Self {
        self.create_processors_hook = Arc::new(Box::new(hook_fn));
        self
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
                Some(ref option) => {
                    let vad_processor: Box<dyn Processor + 'static> = engine.create_vad_processor(
                        cancel_token.child_token(),
                        event_sender.clone(),
                        option.to_owned(),
                    )?;
                    processors.push(vad_processor);
                }
                None => {}
            }
            match option.asr {
                Some(ref option) => {
                    let asr_processor = engine
                        .create_asr_processor(
                            track_id,
                            cancel_token.child_token(),
                            option.to_owned(),
                            event_sender.clone(),
                        )
                        .await?;
                    processors.push(asr_processor);
                }
                None => {}
            }
            match option.eou {
                Some(ref option) => {
                    let eou_processor = engine.create_eou_processor(
                        cancel_token.child_token(),
                        event_sender.clone(),
                        option.to_owned(),
                    )?;
                    processors.push(eou_processor);
                }
                None => {}
            }

            Ok(processors)
        })
    }
}
