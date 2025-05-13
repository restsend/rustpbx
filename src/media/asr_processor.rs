use super::processor::Processor;
use crate::{transcription::TranscriptionClient, AudioFrame, Samples};
use anyhow::Result;

pub struct AsrProcessor {
    pub asr_client: Box<dyn TranscriptionClient>,
}

impl AsrProcessor {}

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
