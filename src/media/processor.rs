pub trait Processor {
    fn process<'a>(&self, ts: u32, pcm: &'a Vec<f32>) -> Option<&'a Vec<f32>>;
}
