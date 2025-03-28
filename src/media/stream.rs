use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::{
    select,
    sync::{broadcast, mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};
use uuid;

use crate::event;
use crate::media::{
    dtmf::DTMFDetector,
    pipeline::PipelineManager,
    processor::{AudioFrame, Processor},
    recorder::{Recorder, RecorderConfig},
    track::{Track, TrackId, TrackPacketReceiver, TrackPacketSender},
};

use super::processor::Samples;

pub type EventSender = broadcast::Sender<MediaStreamEvent>;
pub type EventReceiver = broadcast::Receiver<MediaStreamEvent>;

#[derive(Debug, Clone)]
pub enum MediaStreamEvent {
    DTMF(TrackId, u32, String),
    StartSpeaking(TrackId, u32),
    Silence(TrackId, u32),
    Transcription(TrackId, u32, String),
    TranscriptionSegment(TrackId, u32, String),
    TrackStart(TrackId),
    TrackStop(TrackId),
}

// Convert a MediaStream EventSender to a SessionEvent EventSender
fn create_session_event_sender(media_sender: &EventSender) -> event::EventSender {
    // Create a new event::EventSender that will forward SessionEvents from the MediaStreamEvents
    let (session_sender, _) = broadcast::channel(32);

    // Create a subscription to the media events
    let mut media_receiver = media_sender.subscribe();

    // Clone the sender for the spawned task
    let session_sender_clone = session_sender.clone();

    // Spawn a task to forward events
    tokio::spawn(async move {
        while let Ok(event) = media_receiver.recv().await {
            match event {
                MediaStreamEvent::DTMF(track_id, timestamp, digit) => {
                    let _ = session_sender_clone
                        .send(event::SessionEvent::DTMF(track_id, timestamp, digit));
                }
                MediaStreamEvent::Transcription(track_id, timestamp, text) => {
                    let _ = session_sender_clone.send(event::SessionEvent::Transcription(
                        track_id, timestamp, text,
                    ));
                }
                MediaStreamEvent::TranscriptionSegment(track_id, timestamp, text) => {
                    let _ = session_sender_clone.send(event::SessionEvent::TranscriptionSegment(
                        track_id, timestamp, text,
                    ));
                }
                MediaStreamEvent::TrackStart(track_id) => {
                    let _ = session_sender_clone.send(event::SessionEvent::TrackStart(track_id));
                }
                MediaStreamEvent::TrackStop(track_id) => {
                    let _ = session_sender_clone.send(event::SessionEvent::TrackEnd(track_id));
                }
                _ => {
                    // Some events don't map directly
                }
            }
        }
    });

    session_sender
}

pub struct MediaStream {
    id: String,
    cancel_token: CancellationToken,
    recorder: Option<String>,
    tracks: Mutex<HashMap<TrackId, Box<dyn Track>>>,
    event_sender: EventSender,
    pub packet_sender: TrackPacketSender,
    packet_receiver: Mutex<Option<TrackPacketReceiver>>,
    dtmf_detector: Mutex<DTMFDetector>,
    pipeline_manager: Option<Arc<PipelineManager>>,
}
pub struct MediaStreamBuilder {
    cancel_token: Option<CancellationToken>,
    recorder: Option<String>,
    event_buf_size: usize,
    id: Option<String>,
    use_pipeline: bool,
}

impl MediaStreamBuilder {
    pub fn new() -> Self {
        Self {
            id: Some(format!("ms:{}", uuid::Uuid::new_v4())),
            cancel_token: None,
            recorder: None,
            event_buf_size: 16,
            use_pipeline: false,
        }
    }

    pub fn id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    pub fn event_buf_size(mut self, event_buf_size: usize) -> Self {
        self.event_buf_size = event_buf_size;
        self
    }

    pub fn cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn recorder(mut self, recorder: String) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn use_pipeline(mut self, use_pipeline: bool) -> Self {
        self.use_pipeline = use_pipeline;
        self
    }

    pub fn build(self) -> MediaStream {
        let cancel_token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let tracks = Mutex::new(HashMap::new());
        let (event_sender, _) = broadcast::channel(self.event_buf_size);
        let (track_packet_sender, track_packet_receiver) = mpsc::unbounded_channel();

        let pipeline_manager = if self.use_pipeline {
            let id = self.id.as_ref().unwrap_or(&String::from("unknown")).clone();
            let pipeline_id = format!("pipeline:{}", id);
            let session_event_sender = create_session_event_sender(&event_sender);
            Some(Arc::new(PipelineManager::new(
                pipeline_id,
                session_event_sender,
                cancel_token.clone(),
            )))
        } else {
            None
        };

        MediaStream {
            id: self.id.unwrap_or_default(),
            cancel_token,
            recorder: self.recorder,
            tracks,
            event_sender,
            packet_sender: track_packet_sender,
            packet_receiver: Mutex::new(Some(track_packet_receiver)),
            dtmf_detector: Mutex::new(DTMFDetector::new()),
            pipeline_manager,
        }
    }
}

impl MediaStream {
    pub async fn serve(&self) -> Result<()> {
        let packet_receiver = self.packet_receiver.lock().await.take().unwrap();
        select! {
            _ = self.cancel_token.cancelled() => {}
            _ = self.handle_recorder() => {
                debug!("Recorder stopped");
            }
            _ = self.handle_forward_track(packet_receiver) => {
                debug!("Track packet receiver stopped");
            }
        }

        // Stop the pipeline manager if it exists
        if let Some(pipeline_manager) = &self.pipeline_manager {
            if let Err(e) = pipeline_manager.stop().await {
                error!("Failed to stop pipeline manager: {}", e);
            }
        }

        Ok(())
    }

    pub fn stop(&self) {
        self.cancel_token.cancel()
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.event_sender.subscribe()
    }

    pub fn pipeline_manager(&self) -> Option<Arc<PipelineManager>> {
        self.pipeline_manager.clone()
    }

    pub async fn remove_track(&self, id: &TrackId) {
        if let Some(track) = self.tracks.lock().await.remove(id) {
            match track.stop().await {
                Ok(_) => {
                    self.event_sender
                        .send(MediaStreamEvent::TrackStop(id.clone()))
                        .ok();
                }
                Err(e) => {
                    error!("Failed to stop track: {}", e);
                }
            }
        }
    }

    pub async fn update_track(&self, track: Box<dyn Track>) {
        self.remove_track(track.id()).await;
        let token = self.cancel_token.child_token();
        match track
            .start(token, self.event_sender.clone(), self.packet_sender.clone())
            .await
        {
            Ok(_) => {
                let track_id = track.id().clone();
                self.tracks.lock().await.insert(track_id.clone(), track);
                self.event_sender
                    .send(MediaStreamEvent::TrackStart(track_id))
                    .ok();
            }
            Err(e) => {
                error!("Failed to start track: {}", e);
            }
        }
    }

    // Forward audio frames to the pipeline manager if it exists
    async fn process_audio_through_pipeline(&self, frame: &AudioFrame) -> Result<()> {
        if let Some(pipeline_manager) = &self.pipeline_manager {
            match &frame.samples {
                Samples::PCM(samples) => {
                    pipeline_manager
                        .process_audio(samples.clone(), frame.sample_rate as u32)
                        .await?;
                }
                _ => {
                    // Ignore non-PCM samples for now
                }
            }
        }
        Ok(())
    }
}

// RecorderProcessor implementation for processing audio data
#[derive(Clone)]
pub struct RecorderProcessor {
    recorder: Arc<Mutex<Recorder>>,
    sender: mpsc::Sender<AudioFrame>,
}

impl RecorderProcessor {
    pub fn new(recorder: Arc<Mutex<Recorder>>) -> (Self, mpsc::Receiver<AudioFrame>) {
        let (sender, receiver) = mpsc::channel(100); // Buffer size of 100
        (Self { recorder, sender }, receiver)
    }
}

impl Processor for RecorderProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // Clone the frame and send it to the recorder
        let frame_clone = frame.clone();
        let _ = self.sender.try_send(frame_clone);
        Ok(())
    }
}

impl MediaStream {
    async fn handle_recorder(&self) -> Result<()> {
        let recorder = match self.recorder.clone() {
            Some(_) => {
                let config = RecorderConfig {
                    sample_rate: 16000,
                    channels: 1,
                };
                let recorder = Arc::new(Mutex::new(Recorder::new(self.id.clone(), config)));

                // Create RecorderProcessor and get the receiver
                let (processor, mut frame_receiver) = RecorderProcessor::new(recorder.clone());

                // Create a task to process received frames
                tokio::spawn(async move {
                    while let Some(frame) = frame_receiver.recv().await {
                        if let Ok(mut recorder) = recorder.try_lock() {
                            let _ = recorder.process_frame(frame).await;
                        }
                    }
                });

                // Return the processor for tracks to use
                Some(processor)
            }
            None => None,
        };

        if let Some(processor) = recorder {
            // Add processor to all tracks
            let mut tracks = self.tracks.lock().await;
            for track in tracks.values_mut() {
                track.with_processors(vec![Box::new(processor.clone())]);
            }
        }

        // Wait for cancellation
        self.cancel_token.cancelled().await;
        Ok(())
    }

    async fn handle_forward_track(&self, mut packet_receiver: TrackPacketReceiver) {
        while let Some(packet) = packet_receiver.recv().await {
            // Process for DTMF
            self.process_dtmf(&packet).await;

            // Forward to pipeline manager if enabled
            if let Err(e) = self.process_audio_through_pipeline(&packet).await {
                error!("Failed to process audio through pipeline: {}", e);
            }

            // Process the packet with each track
            for track in self.tracks.lock().await.values() {
                if let Err(e) = track.send_packet(&packet).await {
                    error!("Failed to send packet to track: {}", e);
                }
            }
        }
    }

    async fn process_dtmf(&self, packet: &AudioFrame) {
        match &packet.samples {
            // Check if the packet contains RTP data (our focus for DTMF detection)
            Samples::RTP(payload_type, payload) => {
                // Check for DTMF events in RTP payload
                let mut dtmf_detector = self.dtmf_detector.lock().await;
                if let Some(digit) =
                    dtmf_detector.detect_rtp(&packet.track_id, *payload_type, payload)
                {
                    // Send DTMF event
                    self.event_sender
                        .send(MediaStreamEvent::DTMF(
                            packet.track_id.clone(),
                            packet.timestamp as u32,
                            digit.clone(),
                        ))
                        .ok();
                }
            }
            _ => {}
        }
    }
}
