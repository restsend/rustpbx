use crate::event::{EventReceiver, EventSender, SessionEvent};
use crate::media::{
    dtmf::DTMFDetector,
    processor::Processor,
    recorder::{Recorder, RecorderConfig},
    track::{Track, TrackPacketReceiver, TrackPacketSender},
};
use crate::{AudioFrame, Samples, TrackId};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::{
    select,
    sync::{broadcast, mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};
use uuid;

pub struct MediaStream {
    id: String,
    cancel_token: CancellationToken,
    recorder: Option<String>,
    tracks: Mutex<HashMap<TrackId, Box<dyn Track>>>,
    event_sender: EventSender,
    pub packet_sender: TrackPacketSender,
    packet_receiver: Mutex<Option<TrackPacketReceiver>>,
    dtmf_detector: Mutex<DTMFDetector>,
}

pub struct MediaStreamBuilder {
    cancel_token: Option<CancellationToken>,
    recorder: Option<String>,
    id: Option<String>,
    event_sender: EventSender,
}

impl MediaStreamBuilder {
    pub fn new(event_sender: EventSender) -> Self {
        Self {
            id: Some(format!("ms:{}", uuid::Uuid::new_v4())),
            cancel_token: None,
            recorder: None,
            event_sender,
        }
    }
    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
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

    pub fn build(self) -> MediaStream {
        let cancel_token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let tracks = Mutex::new(HashMap::new());
        let (track_packet_sender, track_packet_receiver) = mpsc::unbounded_channel();

        MediaStream {
            id: self.id.unwrap_or_default(),
            cancel_token,
            recorder: self.recorder,
            tracks,
            event_sender: self.event_sender,
            packet_sender: track_packet_sender,
            packet_receiver: Mutex::new(Some(track_packet_receiver)),
            dtmf_detector: Mutex::new(DTMFDetector::new()),
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

        Ok(())
    }

    pub fn stop(&self) {
        self.cancel_token.cancel()
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.event_sender.subscribe()
    }
    pub fn get_event_sender(&self) -> EventSender {
        self.event_sender.clone()
    }

    pub async fn remove_track(&self, id: &TrackId) {
        if let Some(track) = self.tracks.lock().await.remove(id) {
            match track.stop().await {
                Ok(_) => {
                    let timestamp = crate::get_timestamp();
                    self.event_sender
                        .send(SessionEvent::TrackEnd {
                            track_id: id.clone(),
                            timestamp,
                        })
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
                    .send(SessionEvent::TrackStart {
                        track_id: track_id.clone(),
                        timestamp: crate::get_timestamp(),
                    })
                    .ok();
            }
            Err(e) => {
                error!("Failed to start track: {}", e);
            }
        }
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
        // Wait for cancellation
        self.cancel_token.cancelled().await;
        Ok(())
    }

    async fn handle_forward_track(&self, mut packet_receiver: TrackPacketReceiver) {
        while let Some(packet) = packet_receiver.recv().await {
            // Process for DTMF
            self.process_dtmf(&packet).await;

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
                        .send(SessionEvent::DTMF {
                            track_id: packet.track_id.clone(),
                            timestamp: crate::get_timestamp(),
                            digit: digit.clone(),
                        })
                        .ok();
                }
            }
            _ => {}
        }
    }
}
