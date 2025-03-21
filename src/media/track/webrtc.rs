use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::media::{
    codecs::{CodecType, Decoder, DecoderFactory},
    processor::{AudioFrame, Processor},
    stream::EventSender,
    track::{Track, TrackId, TrackPacket, TrackPacketSender, TrackPayload},
};

pub struct WebrtcTrack {
    id: TrackId,
    processors: Vec<Box<dyn Processor>>,
    sample_rate: u32,
    decoders: HashMap<u8, Box<dyn Decoder>>,
    receiver: Option<mpsc::UnboundedReceiver<TrackPacket>>,
    packet_sender: Option<TrackPacketSender>,
}

impl WebrtcTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            processors: Vec::new(),
            sample_rate: 48000, // WebRTC typically uses 48kHz for audio
            decoders: HashMap::new(),
            receiver: None,
            packet_sender: None,
        }
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    pub fn with_codecs(mut self, codec_types: Vec<CodecType>) -> Self {
        // Create decoders for each codec type
        let decoder_factory = DecoderFactory::new();

        for codec_type in codec_types {
            if let Ok(decoder) = decoder_factory.create_decoder(codec_type) {
                let payload_type = match codec_type {
                    CodecType::PCMU => 0, // PCMU is payload type 0
                    CodecType::PCMA => 8, // PCMA is payload type 8
                    CodecType::G722 => 9, // G722 is payload type 9
                };

                self.decoders.insert(payload_type, decoder);
            }
        }

        self
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
        // Save the packet sender for later use
        if let Some(this) = unsafe { (self as *const Self as *mut Self).as_mut() } {
            this.packet_sender = Some(packet_sender.clone());
        }

        // Create a channel for receiving packets
        let (receiver_sender, receiver) = mpsc::unbounded_channel();

        // Store the receiver in self
        if let Some(this) = unsafe { (self as *const Self as *mut Self).as_mut() } {
            this.receiver = Some(receiver);
        }

        // Signal track start
        let track_id = self.id.clone();
        let event_sender_clone = event_sender.clone();
        let _ = event_sender.send(crate::media::stream::MediaStreamEvent::TrackStart(
            track_id.clone(),
        ));

        // Wait for the token to be cancelled
        tokio::spawn(async move {
            token.cancelled().await;
            // Signal track stop
            let _ = event_sender_clone
                .send(crate::media::stream::MediaStreamEvent::TrackStop(track_id));
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Nothing special to do here
        Ok(())
    }

    async fn send_packet(&self, packet: &TrackPacket) -> Result<()> {
        match &packet.payload {
            TrackPayload::PCM(samples) => {
                // Process with attached processors
                let mut frame = AudioFrame {
                    track_id: packet.track_id.clone(),
                    samples: samples.clone(),
                    timestamp: packet.timestamp as u32,
                    sample_rate: self.sample_rate as u16,
                };

                for processor in &self.processors {
                    let _ = processor.process_frame(&mut frame);
                }
            }
            TrackPayload::RTP(payload_type, payload) => {
                // Forward RTP packet
                if let Some(this) = unsafe { (self as *const Self as *mut Self).as_mut() } {
                    if let Some(sender) = &this.packet_sender {
                        // Check if we have a decoder for this payload type
                        if let Some(decoder) = this.decoders.get(payload_type) {
                            // Decode to PCM
                            if let Ok(pcm_samples) = decoder.decode(payload) {
                                // Create PCM packet
                                let pcm_packet = TrackPacket {
                                    track_id: packet.track_id.clone(),
                                    timestamp: packet.timestamp,
                                    payload: TrackPayload::PCM(pcm_samples),
                                };

                                // Send PCM packet
                                let _ = sender.send(pcm_packet);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn recv_packet(&self) -> Option<TrackPacket> {
        if let Some(this) = unsafe { (self as *const Self as *mut Self).as_mut() } {
            if let Some(receiver) = &mut this.receiver {
                return receiver.recv().await;
            }
        }
        None
    }
}
