use crate::{
    event::{EventSender, SessionEvent},
    media::{
        jitter::JitterBuffer,
        processor::Processor,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    AudioFrame,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use super::{track_codec::TrackCodec, TrackPacketReceiver};

// Configuration for integrating a webrtc crate track with our WebrtcTrack
#[derive(Clone)]
pub struct WebrtcTrackConfig {
    pub track: Arc<TrackLocalStaticSample>,
    pub payload_type: u8,
}

pub struct WebrtcTrack {
    id: TrackId,
    config: TrackConfig,
    processors: Vec<Box<dyn Processor>>,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    receiver: Mutex<Option<TrackPacketReceiver>>,
    packet_sender: Mutex<Option<TrackPacketSender>>,
    cancel_token: CancellationToken,
    webrtc_track: Option<WebrtcTrackConfig>,
}

impl WebrtcTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(48000); // WebRTC typically uses 48kHz

        Self {
            id,
            config: config.clone(),
            processors: Vec::new(),
            jitter_buffer: Arc::new(Mutex::new(JitterBuffer::new(config.sample_rate))),
            receiver: Mutex::new(None),
            packet_sender: Mutex::new(None),
            cancel_token: CancellationToken::new(),
            webrtc_track: None,
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(config.sample_rate);
        }

        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(sample_rate);
        }

        self
    }

    pub fn with_webrtc_track(mut self, webrtc_track_config: WebrtcTrackConfig) -> Self {
        self.webrtc_track = Some(webrtc_track_config);
        self
    }

    async fn start_jitter_processing(&self, token: CancellationToken) -> Result<()> {
        // Create a task to process frames from the jitter buffer
        let jitter_buffer = self.jitter_buffer.clone();
        let sample_rate = self.config.sample_rate;
        let webrtc_track_config = self.webrtc_track.clone();
        let ptime_ms = self.config.ptime.as_millis() as u32;

        // Use ptime from config for the processing interval
        let interval = tokio::time::interval(self.config.ptime);
        let mut interval_stream = IntervalStream::new(interval);

        tokio::spawn(async move {
            debug!(
                "Started jitter buffer processing with ptime: {}ms",
                ptime_ms
            );

            // Create a new codec instance for this task
            let codecs = TrackCodec::new();
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        debug!("Jitter processing task cancelled");
                        break;
                    }
                    _ = interval_stream.next() => {
                        // Get frames from jitter buffer
                        let frames = {
                            let mut jitter = jitter_buffer.lock().unwrap();
                            jitter.pull_frames(ptime_ms, sample_rate)
                        };
                        let webrtc_track_config = match &webrtc_track_config {
                            Some(config) => config,
                            None => {
                                continue;
                            }
                        };
                        for frame in frames {
                            // If we have a webrtc track, encode and send the samples
                            let payload_type = webrtc_track_config.payload_type;
                            let packet_timestamp = frame.timestamp;
                            // Encode PCM to codec format
                            if let Ok(encoded_data) = codecs.encode(payload_type, frame) {
                                // Create a Sample that the WebRTC track can use
                                let sample = Sample {
                                    data: encoded_data,
                                    duration: Duration::from_millis(ptime_ms as u64),
                                    timestamp: std::time::SystemTime::now(),
                                    packet_timestamp,
                                    prev_dropped_packets: 0,
                                    prev_padding_packets: 0,
                                };

                                debug!(
                                    "Sending jitter buffer samples to WebRTC track, payload type: {}, packet size: {}, timestamp: {}",
                                    payload_type,
                                    sample.data.len(),
                                    sample.packet_timestamp
                                );

                                // Try to write the sample to the track
                                if let Err(e) = webrtc_track_config.track.write_sample(&sample).await {
                                    error!("Failed to write jitter buffer sample to WebRTC track: {}", e);
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Track for WebrtcTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
    }

    fn processors(&self) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .map(|p| p.as_ref() as &dyn Processor)
            .collect()
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Save packet sender for later use
        {
            let mut sender_guard = self.packet_sender.lock().unwrap();
            *sender_guard = Some(packet_sender.clone());
        }

        // Create a channel for receiving packets
        let (_receiver_sender, receiver) = mpsc::unbounded_channel();

        // Store the receiver in self
        {
            let mut receiver_guard = self.receiver.lock().unwrap();
            *receiver_guard = Some(receiver);
        }

        let timestamp = Instant::now().elapsed().as_millis() as u32;
        let _ = event_sender.send(SessionEvent::TrackStart {
            track_id: self.id.clone(),
            timestamp,
        });

        // Start jitter buffer processing
        self.start_jitter_processing(token.clone()).await?;

        // Clone token for the task
        let token_clone = token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.id.clone();

        // Start a task to watch for cancellation
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let timestamp = Instant::now().elapsed().as_millis() as u32;
            let _ = event_sender_clone.send(SessionEvent::TrackEnd {
                track_id,
                timestamp,
            });
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Cancel all processing
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        let mut frame = packet.clone();
        // Process the frame with all processors
        for processor in &self.processors {
            let _ = processor.process_frame(&mut frame);
        }
        let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
        jitter_buffer.push(frame);
        Ok(())
    }
}
