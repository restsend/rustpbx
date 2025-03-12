pub trait Processor {
    fn process(&self, ts: u32, pcm: &Vec<f32>) -> Option<&Vec<f32>>;
}
