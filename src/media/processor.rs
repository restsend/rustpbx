use super::track::track_codec::TrackCodec;
use crate::{AudioFrame, Samples};
use anyhow::Result;
use std::sync::{Arc, Mutex};

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
            Samples::PCM { samples } => samples.is_empty(),
            Samples::RTP { payload, .. } => payload.is_empty(),
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
    pub fn new(sample_rate: u32) -> Self {
        Self {
            processors: Arc::new(Mutex::new(Vec::new())),
            codec: Arc::new(Mutex::new(TrackCodec::new())),
            sample_rate,
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
        if let Samples::RTP {
            payload_type,
            payload,
            ..
        } = &frame.samples
        {
            let samples =
                self.codec
                    .lock()
                    .unwrap()
                    .decode(*payload_type, &payload, self.sample_rate);
            frame.samples = Samples::PCM { samples };
        }
        // Process the frame with all processors
        for processor in processors.iter() {
            processor.process_frame(&mut frame)?;
        }
        Ok(())
    }
}
