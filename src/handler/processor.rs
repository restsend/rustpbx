use crate::{
    event::EventSender, media::processor::Processor, transcription::TranscriptionClient,
    AudioFrame, Samples,
};
use anyhow::Result;
pub struct AsrProcessor<T: TranscriptionClient> {
    track_id: String,
    asr: T,
    event_sender: EventSender,
}

impl<T: TranscriptionClient> AsrProcessor<T> {
    pub fn new(track_id: String, asr: T, event_sender: EventSender) -> Self {
        Self {
            track_id,
            asr,
            event_sender,
        }
    }
    pub async fn process_events(&self) -> Result<()> {
        while let Some(frame) = self.asr.next().await {
            self.event_sender.send(frame.into())?;
        }
        Ok(())
    }
}

impl<T: TranscriptionClient> Processor for AsrProcessor<T> {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        match &frame.samples {
            Samples::PCM(samples) => {
                self.asr.send_audio(&samples)?;
            }
            _ => {}
        }
        Ok(())
    }
}
