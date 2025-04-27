use crate::{
    event::EventSender,
    media::processor::Processor,
    transcription::{
        TencentCloudAsrClientBuilder, TranscriptionClient, TranscriptionOption, TranscriptionType,
        VoiceApiAsrClientBuilder,
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use tokio_util::sync::CancellationToken;
pub struct AsrProcessor {
    pub asr_client: Box<dyn TranscriptionClient>,
}

impl AsrProcessor {
    pub async fn create(
        track_id: TrackId,
        token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> Result<Self> {
        let asr_client = match option.provider {
            Some(TranscriptionType::VoiceApi) => {
                let client = VoiceApiAsrClientBuilder::new(option, event_sender)
                    .with_track_id(track_id)
                    .with_cancel_token(token)
                    .build()
                    .await?;
                Box::new(client) as Box<dyn TranscriptionClient>
            }
            _ => {
                let client = TencentCloudAsrClientBuilder::new(option, event_sender)
                    .with_track_id(track_id)
                    .with_cancel_token(token)
                    .build()
                    .await?;
                Box::new(client) as Box<dyn TranscriptionClient>
            }
        };
        Ok(Self { asr_client })
    }
}

impl Processor for AsrProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        match &frame.samples {
            Samples::PCM { samples } => {
                self.asr_client.send_audio(&samples)?;
            }
            _ => {}
        }
        Ok(())
    }
}
