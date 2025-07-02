use crate::event::{EventReceiver, EventSender, SessionEvent};
use crate::media::{
    dtmf::DtmfDetector,
    processor::Processor,
    recorder::{Recorder, RecorderOption},
    track::{Track, TrackPacketReceiver, TrackPacketSender},
};
use crate::{AudioFrame, Samples, TrackId};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::{
    select,
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};
use uuid;

pub struct MediaStream {
    id: String,
    cancel_token: CancellationToken,
    recorder_config: Option<RecorderOption>,
    tracks: Mutex<HashMap<TrackId, Box<dyn Track>>>,
    event_sender: EventSender,
    pub packet_sender: TrackPacketSender,
    packet_receiver: Mutex<Option<TrackPacketReceiver>>,
    dtmf_detector: DtmfDetector,
    recorder_sender: mpsc::UnboundedSender<AudioFrame>,
    recorder_receiver: Mutex<Option<mpsc::UnboundedReceiver<AudioFrame>>>,
    recorder_handle: Mutex<Option<JoinHandle<()>>>,
}

pub struct MediaStreamBuilder {
    cancel_token: Option<CancellationToken>,
    id: Option<String>,
    event_sender: EventSender,
    recorder_config: Option<RecorderOption>,
}

impl MediaStreamBuilder {
    pub fn new(event_sender: EventSender) -> Self {
        Self {
            id: Some(format!("ms:{}", uuid::Uuid::new_v4())),
            cancel_token: None,
            event_sender,
            recorder_config: None,
        }
    }
    pub fn with_id(mut self, id: String) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_recorder_config(mut self, recorder_config: RecorderOption) -> Self {
        self.recorder_config = Some(recorder_config);
        self
    }

    pub fn build(self) -> MediaStream {
        let cancel_token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let tracks = Mutex::new(HashMap::new());
        let (track_packet_sender, track_packet_receiver) = mpsc::unbounded_channel();
        let (recorder_sender, recorder_receiver) = mpsc::unbounded_channel();
        MediaStream {
            id: self.id.unwrap_or_default(),
            cancel_token,
            recorder_config: self.recorder_config,
            tracks,
            event_sender: self.event_sender,
            packet_sender: track_packet_sender,
            packet_receiver: Mutex::new(Some(track_packet_receiver)),
            dtmf_detector: DtmfDetector::new(),
            recorder_sender,
            recorder_receiver: Mutex::new(Some(recorder_receiver)),
            recorder_handle: Mutex::new(None),
        }
    }
}

impl MediaStream {
    #[instrument(name = "MediaStream::serve", skip(self), fields(id = self.id))]
    pub async fn serve(&self) -> Result<()> {
        let packet_receiver = match self.packet_receiver.lock().await.take() {
            Some(receiver) => receiver,
            None => {
                warn!("MediaStream::serve() called multiple times, stream already serving");
                return Ok(());
            }
        };
        self.start_recorder().await.ok();
        select! {
            _ = self.cancel_token.cancelled() => {}
            r = self.handle_forward_track(packet_receiver) => {
                info!("media_stream: Track packet receiver stopped {:?}", r);
            }
        }
        Ok(())
    }

    pub fn stop(&self, reason: Option<String>, initiator: Option<String>) {
        self.event_sender
            .send(SessionEvent::Hangup {
                timestamp: crate::get_timestamp(),
                reason,
                initiator,
            })
            .ok();
        self.cancel_token.cancel()
    }

    pub async fn cleanup(&self) -> Result<()> {
        self.cancel_token.cancel();
        if let Some(recorder_handle) = self.recorder_handle.lock().await.take() {
            if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(30), recorder_handle).await
            {
                info!("media_stream: Recorder stopped");
            } else {
                error!("media_stream: Recorder timeout");
            }
        }
        Ok(())
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
                Ok(_) => {}
                Err(e) => {
                    error!("media_stream: Failed to stop track: {}", e);
                }
            }
        }
    }

    pub async fn update_track(&self, mut track: Box<dyn Track>) {
        self.remove_track(track.id()).await;
        if self.recorder_config.is_some() {
            track.insert_processor(Box::new(RecorderProcessor::new(
                self.recorder_sender.clone(),
            )));
        }
        match track
            .start(self.event_sender.clone(), self.packet_sender.clone())
            .await
        {
            Ok(_) => {
                info!("media_stream: track started {:?}", track.id());
                let track_id = track.id().clone();
                self.tracks.lock().await.insert(track_id.clone(), track);
                self.event_sender
                    .send(SessionEvent::TrackStart {
                        track_id,
                        timestamp: crate::get_timestamp(),
                    })
                    .ok();
            }
            Err(e) => {
                error!("Failed to start track: {}", e);
            }
        }
    }

    /// Set remote SDP for a specific track by track ID
    pub async fn set_track_remote_sdp(&self, track_id: &TrackId, sdp: &str) -> Result<()> {
        let mut tracks = self.tracks.lock().await;
        if let Some(track) = tracks.get_mut(track_id) {
            track.set_remote_sdp(sdp)?;
            info!("Successfully set remote SDP for track: {}", track_id);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Track not found: {}", track_id))
        }
    }

    /// Set remote SDP for all RTP tracks in the stream
    pub async fn set_rtp_tracks_remote_sdp(&self, sdp: &str) -> Result<()> {
        let mut tracks = self.tracks.lock().await;
        let mut updated_count = 0;

        for (track_id, track) in tracks.iter_mut() {
            // Try to set remote SDP for all tracks
            // Only RTP tracks will actually process it, others will ignore
            if track.set_remote_sdp(sdp).is_ok() {
                updated_count += 1;
                info!("Set remote SDP for track: {}", track_id);
            }
        }

        if updated_count > 0 {
            info!("Successfully set remote SDP for {} tracks", updated_count);
            Ok(())
        } else {
            Err(anyhow::anyhow!("No tracks were updated with remote SDP"))
        }
    }
}

#[derive(Clone)]
pub struct RecorderProcessor {
    sender: mpsc::UnboundedSender<AudioFrame>,
}

impl RecorderProcessor {
    pub fn new(sender: mpsc::UnboundedSender<AudioFrame>) -> Self {
        Self { sender }
    }
}

impl Processor for RecorderProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        let frame_clone = frame.clone();
        let _ = self.sender.send(frame_clone);
        Ok(())
    }
}

impl MediaStream {
    async fn start_recorder(&self) -> Result<()> {
        if let Some(ref recorder_config) = self.recorder_config {
            let recorder_receiver = match self.recorder_receiver.lock().await.take() {
                Some(receiver) => receiver,
                None => {
                    warn!("Recorder already started, skipping");
                    return Ok(());
                }
            };
            let cancel_token = self.cancel_token.child_token();
            let recorder_config_clone = recorder_config.clone();

            let recorder_handle = tokio::spawn(async move {
                let recorder_file = recorder_config_clone.recorder_file.clone();
                let recorder = Recorder::new(cancel_token, recorder_config_clone);
                match recorder
                    .process_recording(Path::new(&recorder_file), recorder_receiver)
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!("media_stream: Failed to process recorder: {}", e);
                    }
                }
            });
            *self.recorder_handle.lock().await = Some(recorder_handle);
        }
        Ok(())
    }

    async fn handle_forward_track(&self, mut packet_receiver: TrackPacketReceiver) {
        while let Some(packet) = packet_receiver.recv().await {
            // Process for DTMF
            self.process_dtmf(&packet).await;
            // Process the packet with each track
            for track in self.tracks.lock().await.values() {
                if &packet.track_id == track.id() {
                    continue;
                }
                if let Err(e) = track.send_packet(&packet).await {
                    error!("media_stream: Failed to send packet to track: {}", e);
                }
            }
        }
    }

    async fn process_dtmf(&self, packet: &AudioFrame) {
        match &packet.samples {
            // Check if the packet contains RTP data (our focus for DTMF detection)
            Samples::RTP {
                payload_type,
                payload,
                ..
            } => {
                if let Some(digit) = self.dtmf_detector.detect_rtp(*payload_type, payload) {
                    self.event_sender
                        .send(SessionEvent::Dtmf {
                            track_id: packet.track_id.clone(),
                            timestamp: packet.timestamp,
                            digit,
                        })
                        .ok();
                }
            }
            _ => {}
        }
    }
}
