use super::{
    recorder::Recorder,
    track::{Track, TrackId, TrackPacketReceiver, TrackPacketSender},
};
use crate::media::track::TrackPacket;
use anyhow::Result;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::{
    select,
    sync::{broadcast, mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument};

#[derive(Debug, Clone)]
pub enum MediaStreamEvent {
    DTMF(TrackId, u32, String),
    StartSpeaking(TrackId, u32),
    Silence(TrackId, u32),
    Transcription(TrackId, u32, String), // word-level transcription
    TranscriptionSegment(TrackId, u32, String), // segment-level transcription
    TrackStart(TrackId),
    TrackStop(TrackId),
}

pub type EventSender = broadcast::Sender<MediaStreamEvent>;
pub type EventReceiver = broadcast::Receiver<MediaStreamEvent>;

enum StreamCommand {
    AddTrack(Box<dyn Track>),
    RemoveTrack(TrackId),
}
type StreamCommandSender = mpsc::UnboundedSender<StreamCommand>;
type StreamCommandReceiver = mpsc::UnboundedReceiver<StreamCommand>;

pub struct MediaStream {
    id: String,
    cancel_token: CancellationToken,
    recorder: Option<String>,
    tracks: Mutex<HashMap<TrackId, Box<dyn Track>>>,
    event_sender: EventSender,
    command_sender: StreamCommandSender,
    command_receiver: Mutex<Option<StreamCommandReceiver>>,
    packet_sender: TrackPacketSender,
    packet_receiver: Mutex<Option<TrackPacketReceiver>>,
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

        let (command_sender, command_receiver) = mpsc::unbounded_channel();
        let (track_packet_sender, track_packet_receiver) = mpsc::unbounded_channel();

        MediaStream {
            id: self.id.unwrap_or_default(),
            cancel_token,
            recorder: self.recorder,
            tracks,
            event_sender,
            command_sender,
            command_receiver: Mutex::new(Some(command_receiver)),
            packet_sender: track_packet_sender,
            packet_receiver: Mutex::new(Some(track_packet_receiver)),
        }
    }
}

impl MediaStream {
    pub async fn serve(&self) -> Result<()> {
        let command_receiver = self.command_receiver.lock().await.take().unwrap();
        let packet_receiver = self.packet_receiver.lock().await.take().unwrap();
        select! {
            _ = self.cancel_token.cancelled() => {}
            _ = self.handle_recorder() => {
                debug!("Recorder stopped");
            }
            _ = self.handle_command(command_receiver) => {
                debug!("Command receiver stopped");
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

    pub fn remove_track(&self, id: TrackId) {
        self.command_sender
            .send(StreamCommand::RemoveTrack(id))
            .ok();
    }

    pub fn update_track(&self, track: Box<dyn Track>) {
        self.command_sender
            .send(StreamCommand::RemoveTrack(track.id().clone()))
            .ok();
        self.command_sender
            .send(StreamCommand::AddTrack(track))
            .ok();
    }
}

impl MediaStream {
    async fn handle_recorder(&self) -> Result<()> {
        let recorder = match self.recorder {
            Some(ref file_name) => Recorder {
                cancel_token: self.cancel_token.clone(),
                file_name: file_name.clone(),
            },
            None => {
                self.cancel_token.cancelled().await;
                return Ok(());
            }
        };
        recorder.process().await;
        Ok(())
    }

    #[instrument(name = "serve_command", skip(self, command_receiver), fields(id = self.id))]
    async fn handle_command(&self, mut command_receiver: StreamCommandReceiver) {
        while let Some(command) = command_receiver.recv().await {
            match command {
                StreamCommand::AddTrack(track) => {
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
                StreamCommand::RemoveTrack(id) => {
                    if let Some(track) = self.tracks.lock().await.remove(&id) {
                        match track.stop().await {
                            Ok(_) => {
                                self.event_sender.send(MediaStreamEvent::TrackStop(id)).ok();
                            }
                            Err(e) => {
                                error!("Failed to stop track: {}", e);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn handle_forward_track(&self, mut packet_receiver: TrackPacketReceiver) {
        while let Some(track_packet) = packet_receiver.recv().await {
            let tracks = self.tracks.lock().await;
            for track in tracks.values() {
                track.send_packet(&track_packet).await.unwrap();
            }
        }
    }
}
