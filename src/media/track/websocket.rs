use super::{Track, TrackConfig, TrackPacketSender, track_codec::TrackCodec};
use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::Samples,
    media::TrackId,
    media::{codecs::bytes_to_samples, processor::ProcessorChain},
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::{sync::Mutex, time::Duration};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub type WebsocketBytesSender = tokio::sync::mpsc::UnboundedSender<Bytes>;
pub type WebsocketBytesReceiver = tokio::sync::mpsc::UnboundedReceiver<Bytes>;

pub struct WebsocketTrack {
    track_id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    processor_chain: ProcessorChain,
    rx: Mutex<Option<WebsocketBytesReceiver>>,
    encoder: TrackCodec,
    payload_type: u8,
    event_sender: EventSender,
    ssrc: u32,
}

impl WebsocketTrack {
    pub fn new(
        cancel_token: CancellationToken,
        track_id: TrackId,
        track_config: TrackConfig,
        event_sender: EventSender,
        audio_receiver: WebsocketBytesReceiver,
        codec: Option<String>,
        ssrc: u32,
    ) -> Self {
        let processor_chain = ProcessorChain::new(track_config.samplerate);
        let payload_type = match codec.unwrap_or("pcm".to_string()).to_lowercase().as_str() {
            "pcmu" => 0,
            "pcma" => 8,
            "g722" => 9,
            _ => u8::MAX, // PCM
        };
        Self {
            track_id,
            config: track_config,
            cancel_token,
            processor_chain,
            rx: Mutex::new(Some(audio_receiver)),
            encoder: TrackCodec::new(),
            payload_type,
            event_sender,
            ssrc,
        }
    }
}

#[async_trait]
impl Track for WebsocketTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }

    async fn handshake(&mut self, _offer: String, _timeout: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
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
                warn!(track_id, "no audio from ws");
                return Ok(());
            }
        };
        let sample_rate = self.config.samplerate;
        let payload_type = self.payload_type;
        let start_time = crate::media::get_timestamp();
        let ssrc = self.ssrc;
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
                        timestamp: crate::media::get_timestamp(),
                        sample_rate,
                    };
                    match packet_sender.send(packet) {
                        Ok(_) => (),
                        Err(e) => {
                            warn!("error sending packet: {}", e);
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
                    timestamp: crate::media::get_timestamp(),
                    duration: crate::media::get_timestamp() - start_time,
                    ssrc,
                    play_id: None,
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
        let (_, payload) = self.encoder.encode(self.payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }
        self.event_sender
            .send(SessionEvent::Binary {
                track_id: self.track_id.clone(),
                timestamp: crate::media::get_timestamp(),
                data: payload,
            })
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("error sending binary event"))
    }
}
