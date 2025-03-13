use super::processor::Processor;

pub struct VADConfig {
    threshold: f32,
}

impl Default for VADConfig {
    fn default() -> Self {
        Self { threshold: 0.85 }
    }
}

pub struct VADProcessor {
    config: VADConfig,
}

impl VADProcessor {
    pub fn new(config: VADConfig) -> Self {
        Self { config }
    }
}

impl Processor for VADProcessor {
    fn process<'a>(&self, ts: u32, pcm: &'a Vec<f32>) -> Option<&'a Vec<f32>> {
        return Some(pcm);
    }
}
