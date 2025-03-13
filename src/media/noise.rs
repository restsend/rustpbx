use super::processor::Processor;

pub struct NoiseSupressionProcessor {}

impl NoiseSupressionProcessor {
    pub fn new() -> Self {
        Self {}
    }
}

impl Processor for NoiseSupressionProcessor {
    fn process<'a>(&self, ts: u32, pcm: &'a Vec<f32>) -> Option<&'a Vec<f32>> {
        return Some(pcm);
    }
}
