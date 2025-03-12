use anyhow::Result;

use super::processor::Processor;

#[derive(Debug, Clone)]
pub enum TrackDirection {
    Inbound,
    Outbound,
}

pub trait Track: Send + Sync {
    fn direction(&self) -> TrackDirection;
    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>);
    fn start(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
}
