use super::{track_codec::TrackCodec, Track, TrackConfig, TrackPacketSender};
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        codecs::bytes_to_samples,
        processor::{Processor, ProcessorChain},
    },
    AudioFrame, Samples, TrackId,
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::{sync::Mutex, time::Duration};
use tokio::{select, sync::mpsc::UnboundedReceiver};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

pub struct WebsocketTrack {
    track_id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    processor_chain: ProcessorChain,
    rx: Mutex<Option<UnboundedReceiver<Bytes>>>,
    encoder: TrackCodec,
    payload_type: u8,
    event_sender: EventSender,
}

impl WebsocketTrack {
    pub fn new(
        cancel_token: CancellationToken,
        track_id: TrackId,
        config: TrackConfig,
        event_sender: EventSender,
        rx: UnboundedReceiver<Bytes>,
        codec: Option<String>,
    ) -> Self {
        let processor_chain = ProcessorChain::new(config.samplerate);
        let payload_type = match codec.unwrap_or("pcm".to_string()).to_lowercase().as_str() {
            "pcmu" => 0,
            "pcma" => 8,
            "g722" => 9,
            _ => u8::MAX, // PCM
        };
        Self {
            track_id,
            config,
            cancel_token,
            processor_chain,
            rx: Mutex::new(Some(rx)),
            encoder: TrackCodec::new(),
            payload_type,
            event_sender,
        }
    }
}

#[async_trait]
impl Track for WebsocketTrack {
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }

    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }

    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let track_id = self.track_id.clone();
        let token = self.cancel_token.clone();
        let mut audio_from_ws = match self.rx.lock().unwrap().take() {
            Some(rx) => rx,
            None => {
                error!("no audio from ws");
                return Ok(());
            }
        };
        let sample_rate = self.config.samplerate;
        let payload_type = self.payload_type;
        let start_time = crate::get_timestamp();
        tokio::spawn(async move {
            let track_id_clone = track_id.clone();
            let audio_from_ws_loop = async move {
                let mut sequence_number = 0;
                while let Some(bytes) = audio_from_ws.recv().await {
                    sequence_number += 1;

                    let samples = match payload_type {
                        u8::MAX => Samples::PCM {
                            samples: bytes_to_samples(&bytes.to_vec()),
                        },
                        _ => Samples::RTP {
                            sequence_number,
                            payload_type,
                            payload: bytes.to_vec(),
                        },
                    };

                    let packet = AudioFrame {
                        track_id: track_id_clone.clone(),
                        samples,
                        timestamp: crate::get_timestamp(),
                        sample_rate,
                    };
                    match packet_sender.send(packet) {
                        Ok(_) => (),
                        Err(e) => {
                            error!("error sending packet: {}", e);
                            break;
                        }
                    }
                }
            };

            select! {
                _ = token.cancelled() => {
                    info!("RTC process cancelled");
                },
                _ = audio_from_ws_loop => {
                    info!("audio_from_ws_loop");
                }
            };

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::get_timestamp(),
                    duration: crate::get_timestamp() - start_time,
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
        let payload = self.encoder.encode(self.payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }
        self.event_sender
            .send(SessionEvent::Binary {
                track_id: self.track_id.clone(),
                timestamp: crate::get_timestamp(),
                data: payload,
            })
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("error sending binary event"))
    }
}
