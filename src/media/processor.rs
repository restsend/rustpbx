use super::{
    codecs::{convert_s16_to_u8, convert_u8_to_s16, resample::resample_mono},
    track::track_codec::TrackCodec,
};
use crate::{AudioFrame, Samples};
use anyhow::Result;
use std::sync::{Arc, Mutex};
use tracing::info;

pub trait Processor: Send + Sync {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()>;
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self {
            track_id: "".to_string(),
            samples: Samples::Empty,
            timestamp: 0,
            sample_rate: 16000,
        }
    }
}

impl Samples {
    pub fn is_empty(&self) -> bool {
        match self {
            Samples::PCM(samples) => samples.is_empty(),
            Samples::RTP(_, payload) => payload.is_empty(),
            Samples::Empty => true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProcessorChain {
    processors: Arc<Mutex<Vec<Box<dyn Processor>>>>,
    codec: Arc<Mutex<TrackCodec>>,
    sample_rate: u32,
}

impl ProcessorChain {
    pub fn new() -> Self {
        Self {
            processors: Arc::new(Mutex::new(Vec::new())),
            codec: Arc::new(Mutex::new(TrackCodec::new())),
            sample_rate: 16000,
        }
    }
    pub fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processors.lock().unwrap().insert(0, processor);
    }
    pub fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processors.lock().unwrap().push(processor);
    }

    pub fn process_frame(&self, frame: &AudioFrame) -> Result<()> {
        let processors = self.processors.lock().unwrap();
        if processors.is_empty() {
            return Ok(());
        }

        let mut frame = frame.clone();
        if frame.sample_rate != self.sample_rate {
            match &mut frame.samples {
                Samples::RTP(payload_type, payload) => {
                    let samples = convert_u8_to_s16(payload);
                    let resampled_samples =
                        resample_mono(&samples, frame.sample_rate as u32, self.sample_rate as u32);
                    *payload = convert_s16_to_u8(&resampled_samples);

                    info!(
                        "Resampled frame to payload_type: {} {} => {} Hz",
                        payload_type, frame.sample_rate, self.sample_rate
                    );
                    frame.sample_rate = self.sample_rate;
                }
                _ => {}
            }
        }
        if let Samples::RTP(payload_type, payload) = &frame.samples {
            if let Ok(samples) = self.codec.lock().unwrap().decode(*payload_type, &payload) {
                frame.samples = Samples::PCM(samples);
            }
        }
        // Process the frame with all processors
        for processor in processors.iter() {
            processor.process_frame(&mut frame)?;
        }
        Ok(())
    }
}
