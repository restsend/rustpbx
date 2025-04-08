use crate::{media::processor::Processor, transcription::TranscriptionClient, AudioFrame, Samples};
use anyhow::Result;
pub struct AsrProcessor<T: TranscriptionClient> {
    asr: T,
}

impl<T: TranscriptionClient> AsrProcessor<T> {
    pub fn new(asr: T) -> Self {
        Self { asr }
    }
}

impl<T: TranscriptionClient> Processor for AsrProcessor<T> {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        match &frame.samples {
            Samples::PCM { samples } => {
                self.asr.send_audio(&samples)?;
            }
            _ => {}
        }
        Ok(())
    }
}
