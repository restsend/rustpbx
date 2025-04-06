use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use rtp_rs::{RtpPacketBuilder, RtpReader, Seq};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, AtomicU32, Ordering},
        Arc,
    },
};
use tokio::{net::UdpSocket, select, sync::Mutex, time::Duration};
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

pub struct RtpTrack {
    track_id: TrackId,
    config: TrackConfig,
    rtp_config: RtpTrackConfig,
    processor_chain: ProcessorChain,
    rtp_socket: Option<Arc<UdpSocket>>,
    rtcp_socket: Option<Arc<UdpSocket>>,
    cancel_token: CancellationToken,
    sequence_number: AtomicU16,
    timestamp: AtomicU32,
    encoder: TrackCodec,
}

impl RtpTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(8000);
        Self {
            track_id: id,
            config: config.clone(),
            rtp_config: RtpTrackConfig::default(),
            processor_chain: ProcessorChain::new(config.sample_rate),
            rtp_socket: None,
            rtcp_socket: None,
            cancel_token: CancellationToken::new(),
            sequence_number: AtomicU16::new(0),
            timestamp: AtomicU32::new(0),
            encoder: TrackCodec::new(),
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
            "rtptrack: RTP socket created - local: {:?}, remote: {:?}",
            self.rtp_config.local_addr, self.rtp_config.remote_addr
        );

        self.rtp_socket = Some(Arc::new(socket));
        Ok(())
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
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let rtcp_socket = self.rtcp_socket.clone();
        let rtcp_token_clone = token.clone();
        let rtcp_recv_loop = async move {
            let mut buf = vec![0u8; RTP_MTU];
            match rtcp_socket {
                Some(rtcp_socket) => loop {
                    let n = rtcp_socket.recv(&mut buf).await?;
                    rtcp_socket.send(&buf[..n]).await?;
                },
                None => {}
            }
            rtcp_token_clone.cancelled().await;
            Ok::<(), anyhow::Error>(())
        };

        let rtp_socket = self
            .rtp_socket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("rtptrack: RTP socket not initialized"))?;
        let track_id = self.track_id.clone();
        let config = self.config.clone();
        let rtp_recv_loop = async move {
            let mut buf = vec![0u8; RTP_MTU];
            loop {
                let n = rtp_socket.recv(&mut buf).await?;
                if n <= 0 {
                    break;
                }
                if let Ok(reader) = RtpReader::new(&buf[0..n]) {
                    let payload_type = reader.payload_type();
                    let payload = reader.payload().to_vec();
                    let timestamp = reader.timestamp();
                    let frame = AudioFrame {
                        track_id: track_id.clone(),
                        samples: Samples::RTP(payload_type, payload),
                        timestamp: timestamp as u64,
                        sample_rate: config.sample_rate,
                    };
                    packet_sender.send(frame)?;
                }
            }
            Ok::<(), anyhow::Error>(())
        };

        let track_id = self.track_id.clone();
        tokio::spawn(async move {
            event_sender
                .send(SessionEvent::TrackStart {
                    track_id: track_id.clone(),
                    timestamp: crate::get_timestamp(),
                })
                .ok();
            select! {
                _ = token.cancelled() => {},
                r = rtp_recv_loop => {
                    info!("RTP process completed {:?}", r);
                }
                r = rtcp_recv_loop => {
                    info!("RTCP process completed {:?}", r);
                }
            };
            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id: track_id,
                    timestamp: crate::get_timestamp(),
                })
                .ok();
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        if self.rtp_socket.is_none() {
            return Err(anyhow::anyhow!("rtptrack: Socket not initialized"));
        }

        let socket = self.rtp_socket.as_ref().unwrap();

        let payload = self
            .encoder
            .encode(self.rtp_config.payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }

        // Update RTP sequence number and timestamp
        let seq = self.sequence_number.fetch_add(1, Ordering::Relaxed);
        let samples_per_packet =
            (self.config.sample_rate as f32 * self.config.ptime.as_secs_f32()) as u32;
        let ts = self
            .timestamp
            .fetch_add(samples_per_packet, Ordering::Relaxed);

        // Create RTP packet using rtp-rs
        let result = RtpPacketBuilder::new()
            .payload_type(self.rtp_config.payload_type)
            .ssrc(self.rtp_config.ssrc)
            .sequence(Seq::from(seq))
            .timestamp(ts)
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
