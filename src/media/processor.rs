use super::track::track_codec::TrackCodec;
use crate::media::{AudioFrame, Samples};
use anyhow::Result;
use std::any::Any;
use std::sync::{Arc, Mutex};

pub trait Processor: Send + Sync + Any {
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
pub struct ProcessorChain {
    processors: Arc<Mutex<Vec<Box<dyn Processor>>>>,
    codec: Arc<Mutex<TrackCodec>>,
    sample_rate: u32,
    pub force_decode: bool,
}

impl ProcessorChain {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            processors: Arc::new(Mutex::new(Vec::new())),
            codec: Arc::new(Mutex::new(TrackCodec::new())),
            sample_rate,
            force_decode: true,
        }
    }
    pub fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processors.lock().unwrap().insert(0, processor);
    }
    pub fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processors.lock().unwrap().push(processor);
    }

    pub fn has_processor<T: 'static>(&self) -> bool {
        let processors = self.processors.lock().unwrap();
        processors
            .iter()
            .any(|processor| (processor.as_ref() as &dyn Any).is::<T>())
    }

    pub fn remove_processor<T: 'static>(&self) {
        let mut processors = self.processors.lock().unwrap();
        processors.retain(|processor| !(processor.as_ref() as &dyn Any).is::<T>());
    }

    pub fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        let processors = self.processors.lock().unwrap();
        if !self.force_decode && processors.is_empty() {
            return Ok(());
        }

        if let Samples::RTP {
            payload_type,
            payload,
            ..
        } = &frame.samples
        {
            if TrackCodec::is_audio(*payload_type) {
                let samples =
                    self.codec
                        .lock()
                        .unwrap()
                        .decode(*payload_type, &payload, self.sample_rate);
                frame.samples = Samples::PCM { samples };
                frame.sample_rate = self.sample_rate;
            }
        }
        // Process the frame with all processors
        for processor in processors.iter() {
            processor.process_frame(frame)?;
        }
        Ok(())
    }
}
