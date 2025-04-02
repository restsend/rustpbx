use super::track_codec::TrackCodec;
use crate::{
    event::EventSender,
    media::{
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use rtp_rs::{RtpPacketBuilder, RtpReader, Seq};
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};
use tokio::{net::UdpSocket, select, sync::Mutex, time::Duration};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

const RTP_MTU: usize = 1500; // Standard MTU size
const RTCP_INTERVAL: Duration = Duration::from_secs(5);

// Configuration for RTP track
#[derive(Clone)]
pub struct RtpTrackConfig {
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
    pub payload_type: u8,
    pub ssrc: u32,
}

impl Default for RtpTrackConfig {
    fn default() -> Self {
        Self {
            local_addr: "0.0.0.0:0".parse().unwrap(),
            remote_addr: "127.0.0.1:5004".parse().unwrap(),
            payload_type: 0, // PCMU by default
            ssrc: rand::random::<u32>(),
        }
    }
}

// Add this extension trait for ProcessorChain to provide process_audio functionality
#[async_trait]
trait ProcessorChainExt {
    async fn process_audio(&self, samples: &[i16]) -> Vec<i16>;
}

#[async_trait]
impl ProcessorChainExt for ProcessorChain {
    async fn process_audio(&self, samples: &[i16]) -> Vec<i16> {
        // For simplicity, we'll just pass through the samples
        // In a real implementation, we would process the audio through the chain
        samples.to_vec()
    }
}

pub struct RtpTrack {
    track_id: TrackId,
    config: TrackConfig,
    rtp_config: RtpTrackConfig,
    processor_chain: ProcessorChain,
    packet_queue: Arc<Mutex<VecDeque<AudioFrame>>>,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    socket: Option<Arc<UdpSocket>>,
    cancel_token: CancellationToken,
    sequence_number: Arc<Mutex<u16>>,
    timestamp: Arc<Mutex<u32>>,
}

impl RtpTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(8000);
        Self {
            track_id: id,
            config: config.clone(),
            rtp_config: RtpTrackConfig::default(),
            processor_chain: ProcessorChain::new(config.sample_rate),
            packet_queue: Arc::new(Mutex::new(VecDeque::new())),
            packet_sender: Arc::new(Mutex::new(None)),
            socket: None,
            cancel_token: CancellationToken::new(),
            sequence_number: Arc::new(Mutex::new(0)),
            timestamp: Arc::new(Mutex::new(0)),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();
        self
    }

    pub fn with_rtp_config(mut self, rtp_config: RtpTrackConfig) -> Self {
        self.rtp_config = rtp_config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);
        self.processor_chain = ProcessorChain::new(sample_rate);
        self
    }

    pub async fn setup_rtp_socket(&mut self) -> Result<()> {
        let socket = UdpSocket::bind(self.rtp_config.local_addr).await?;
        socket.connect(self.rtp_config.remote_addr).await?;

        info!(
            "RTP socket created - local: {:?}, remote: {:?}",
            self.rtp_config.local_addr, self.rtp_config.remote_addr
        );

        self.socket = Some(Arc::new(socket));
        Ok(())
    }

    async fn start_packet_processing(&self, token: CancellationToken) -> Result<()> {
        let mut interval = IntervalStream::new(tokio::time::interval(self.config.ptime)).fuse();
        let packet_queue = self.packet_queue.clone();
        let packet_sender = self.packet_sender.clone();
        let track_id = self.track_id.clone();
        let codec = TrackCodec::new();
        let sample_rate = self.config.sample_rate;
        let processor_chain = self.processor_chain.clone();

        tokio::spawn(async move {
            while let Some(_) = select! {
                _ = token.cancelled() => None,
                tick = interval.next() => tick,
            } {
                let packet_sender_locked = packet_sender.lock().await;
                if packet_sender_locked.is_none() {
                    continue;
                }

                let mut queue = packet_queue.lock().await;
                if let Some(packet) = queue.pop_front() {
                    if let Samples::RTP(payload_type, payload) = &packet.samples {
                        // Decode RTP payload to PCM
                        let pcm = codec.decode(*payload_type, payload);
                        let processed_pcm = processor_chain.process_audio(&pcm).await;

                        if let Some(sender) = &*packet_sender_locked {
                            // Send processed audio
                            let audio_frame = AudioFrame {
                                track_id: track_id.clone(),
                                samples: Samples::PCM(processed_pcm),
                                timestamp: packet.timestamp,
                                sample_rate,
                            };
                            if let Err(e) = sender.send(audio_frame) {
                                error!("Error sending audio frame: {}", e);
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    async fn start_rtcp_processing(&self, token: CancellationToken) -> Result<()> {
        if self.socket.is_none() {
            return Ok(());
        }

        let _socket = self.socket.as_ref().unwrap().clone();

        // RTCP sender
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(RTCP_INTERVAL);

            loop {
                select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {
                        debug!("RTCP processing tick");
                        // In a real implementation, we would create and send RTCP packets
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        });

        Ok(())
    }

    // For testing purposes, get a copy of the received packets
    #[cfg(test)]
    pub async fn get_received_packets(&self) -> Vec<AudioFrame> {
        let queue = self.packet_queue.lock().await;
        queue.iter().cloned().collect()
    }
}

#[async_trait]
impl Track for RtpTrack {
    fn id(&self) -> &TrackId {
        &self.track_id
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }

    async fn start(
        &self,
        token: CancellationToken,
        _event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Set up packet sender
        {
            let mut sender = self.packet_sender.lock().await;
            *sender = Some(packet_sender);
        }

        // Start packet processing
        self.start_packet_processing(token.clone()).await?;

        // Start RTCP processing
        self.start_rtcp_processing(token.clone()).await?;

        // Start RTP receiving if socket exists
        if let Some(socket) = &self.socket {
            let socket = socket.clone();
            let packet_queue = self.packet_queue.clone();
            let track_id = self.track_id.clone();
            let sample_rate = self.config.sample_rate;
            let token_clone = token.clone();

            tokio::spawn(async move {
                let mut buf = vec![0u8; RTP_MTU];

                loop {
                    select! {
                        _ = token_clone.cancelled() => break,
                        result = socket.recv(&mut buf) => {
                            match result {
                                Ok(n) => {
                                    if n > 0 {
                                        // Parse using rtp-rs
                                        if let Ok(reader) = RtpReader::new(&buf[0..n]) {
                                            let payload_type = reader.payload_type();
                                            let timestamp = reader.timestamp();

                                            // Get the payload
                                            let payload = reader.payload().to_vec();

                                            let frame = AudioFrame {
                                                track_id: track_id.clone(),
                                                samples: Samples::RTP(
                                                    payload_type,
                                                    payload,
                                                ),
                                                timestamp: timestamp as u64,
                                                sample_rate,
                                            };

                                            let mut queue = packet_queue.lock().await;
                                            // Limit queue size to prevent memory issues
                                            if queue.len() >= 100 {
                                                queue.pop_front();
                                            }
                                            queue.push_back(frame);
                                        }
                                    }
                                },
                                Err(e) => {
                                    error!("Error receiving RTP: {}", e);
                                }
                            }
                        }
                    }
                }
            });
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        if self.socket.is_none() {
            return Err(anyhow::anyhow!("rtptrack: Socket not initialized"));
        }

        let socket = self.socket.as_ref().unwrap();
        let codec = TrackCodec::new();

        let payload = codec.encode(self.rtp_config.payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }

        // Update RTP sequence number and timestamp
        let mut seq = self.sequence_number.lock().await;
        *seq = seq.wrapping_add(1);

        let mut ts = self.timestamp.lock().await;
        let samples_per_packet =
            (self.config.sample_rate as f32 * self.config.ptime.as_secs_f32()) as u32;
        *ts = ts.wrapping_add(samples_per_packet);

        // Create RTP packet using rtp-rs
        let result = RtpPacketBuilder::new()
            .payload_type(self.rtp_config.payload_type)
            .ssrc(self.rtp_config.ssrc)
            .sequence(Seq::from(*seq))
            .timestamp(*ts)
            .payload(&payload)
            .build();

        if let Ok(rtp_data) = result {
            socket.send(&rtp_data).await?;
        } else {
            return Err(anyhow::anyhow!("Failed to build RTP packet"));
        }

        Ok(())
    }
}
