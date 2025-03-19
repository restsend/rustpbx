use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::{
    select,
    sync::{broadcast, mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};
use uuid;

use crate::media::{
    processor::AudioFrame,
    recorder::{Recorder, RecorderConfig},
    track::{Track, TrackId, TrackPacketReceiver, TrackPacketSender},
};

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

pub struct MediaStream {
    id: String,
    cancel_token: CancellationToken,
    recorder: Option<String>,
    tracks: Mutex<HashMap<TrackId, Box<dyn Track>>>,
    event_sender: EventSender,
    packet_sender: TrackPacketSender,
    packet_receiver: Mutex<Option<TrackPacketReceiver>>,
    audio_sender: broadcast::Sender<AudioFrame>,
}
pub struct MediaStreamBuilder {
    cancel_token: Option<CancellationToken>,
    recorder: Option<String>,
    event_buf_size: usize,
    id: Option<String>,
}

impl MediaStreamBuilder {
    pub fn new() -> Self {
        Self {
            id: Some(format!("ms:{}", uuid::Uuid::new_v4())),
            cancel_token: None,
            recorder: None,
            event_buf_size: 16,
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

    pub fn build(self) -> MediaStream {
        let cancel_token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let tracks = Mutex::new(HashMap::new());
        let (event_sender, _) = broadcast::channel(self.event_buf_size);
        let (track_packet_sender, track_packet_receiver) = mpsc::unbounded_channel();
        let (audio_sender, _) = broadcast::channel(16);

        MediaStream {
            id: self.id.unwrap_or_default(),
            cancel_token,
            recorder: self.recorder,
            tracks,
            event_sender,
            packet_sender: track_packet_sender,
            packet_receiver: Mutex::new(Some(track_packet_receiver)),
            audio_sender,
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

    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioFrame> {
        self.audio_sender.subscribe()
    }
}

impl MediaStream {
    async fn handle_recorder(&self) -> Result<()> {
        let mut recorder = match self.recorder {
            Some(ref file_name) => {
                let config = RecorderConfig {
                    sample_rate: 16000,
                    channels: 1,
                };
                Recorder::new(self.id.clone(), config)
            }
            None => {
                self.cancel_token.cancelled().await;
                return Ok(());
            }
        };
        Ok(())
    }

    async fn handle_forward_track(&self, mut packet_receiver: TrackPacketReceiver) {
        while let Some(track_packet) = packet_receiver.recv().await {
            let tracks = self.tracks.lock().await;
            for track in tracks.values() {
                if let Err(e) = track.send_packet(&track_packet).await {
                    error!("Failed to forward track packet: {}", e);
                }
            }
        }
    }
}
