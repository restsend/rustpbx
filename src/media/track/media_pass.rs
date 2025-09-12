use crate::{
    AudioFrame, Samples, TrackId,
    event::{EventSender, SessionEvent},
    media::{
        codecs::{bytes_to_samples, resample::resample_mono, samples_to_bytes},
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::BytesMut;
use futures::{SinkExt, StreamExt, future, stream::SplitSink};
use serde::{Deserialize, Serialize};
use std::{cmp::min, time::Duration};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

type WsConn = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsConn, Message>;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MediaPassOption {
    url: String,              // websocket url, e.g. ws://localhost:8080/
    input_sample_rate: u32, // sample rate of audio receiving from websocket, and also the sample rate of the track
    output_sample_rate: u32, // sample rate of audio sending to websocket server
    packet_size: Option<u32>, // packet size send to websocket server, default is 2560
}

impl MediaPassOption {
    pub fn new(
        url: String,
        input_sample_rate: u32,
        output_sample_rate: u32,
        packet_size: Option<u32>,
    ) -> Self {
        Self {
            url,
            input_sample_rate,
            output_sample_rate,
            packet_size,
        }
    }
}

pub struct MediaPassTrack {
    track_id: TrackId,
    cancel_token: CancellationToken,
    config: TrackConfig, // input sample rate is here
    url: String,
    sample_rate: u32, // sample rate of audio sending to url
    packet_size: u32,
    buffer: Mutex<BytesMut>,
    ws_sink: Mutex<Option<WsSink>>,
    ssrc: u32,
    processor_chain: ProcessorChain,
}

impl MediaPassTrack {
    pub fn new(
        ssrc: u32,
        track_id: TrackId,
        cancel_token: CancellationToken,
        option: MediaPassOption,
    ) -> Self {
        let sample_rate = option.output_sample_rate;
        let track_sample_rate = option.input_sample_rate;
        let config = TrackConfig {
            samplerate: track_sample_rate,
            ..Default::default()
        };
        let processor_chain = ProcessorChain::new(track_sample_rate);
        let packet_size = option.packet_size.unwrap_or(2560);
        let buffer = Mutex::new(BytesMut::with_capacity(packet_size as usize * 2));
        Self {
            track_id,
            cancel_token,
            config,
            url: option.url,
            sample_rate,
            packet_size,
            buffer,
            processor_chain,
            ssrc,
            ws_sink: Mutex::new(None),
        }
    }

    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.config = self.config.with_ptime(ptime);
        self
    }
}

#[async_trait]
impl Track for MediaPassTrack {
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
    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain().insert_processor(processor);
    }
    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain().append_processor(processor);
    }
    async fn handshake(&mut self, _: String, _: Option<Duration>) -> Result<String> {
        Ok("".to_string())
    }

    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let target = format!(
            "{}?sample_rate={}&packet_size={}",
            self.url.clone(),
            self.sample_rate,
            self.packet_size
        );
        debug!("Media pass connecting url: {target}");
        let sample_rate = self.config.samplerate;
        let (ws_stream, _) = tokio_tungstenite::connect_async(target).await?;
        let (ws_sink, mut ws_source) = ws_stream.split();
        *self.ws_sink.lock().await = Some(ws_sink);

        let track_id = self.track_id.clone();
        let start_time = crate::get_timestamp();
        let ssrc = self.ssrc;
        let url = self.url.clone();
        let cancel_token = self.cancel_token.clone();
        let ptime = self.config.ptime;
        let packet_bytes_size = 2 * sample_rate * ptime.as_millis() as u32 / 1000;
        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let track_id_clone = track_id.clone();
            let event_sender_clone = event_sender.clone();
            let recv_loop = async {
                while let Some(msg) = ws_source.next().await {
                    match msg {
                        Ok(Message::Binary(data)) => {
                            tx.send(data)?;
                        }
                        Ok(Message::Close(res)) => {
                            warn!(
                                track_id_clone,
                                "Media pass track closed by remote: {:?}", res
                            );
                            break;
                        }
                        Err(e) => {
                            let error = SessionEvent::Error {
                                track_id: track_id_clone.clone(),
                                timestamp: crate::get_timestamp(),
                                sender: format!("media_pass: {}", url),
                                error: format!("Media pass track error: {:?}", e),
                                code: None,
                            };
                            event_sender_clone.send(error.clone()).ok();
                            break;
                        }
                        _ => {}
                    }
                }
                drop(tx);
                future::pending::<Result<()>>().await
            };

            let track_id_clone = track_id.clone();
            let emit_loop = async {
                let mut buffer = BytesMut::with_capacity(4 * 1024);
                let mut ticker = tokio::time::interval(ptime);
                loop {
                    if buffer.len() >= packet_bytes_size as usize
                        || rx.is_closed() && buffer.len() > 0
                    {
                        let size = min(buffer.len(), packet_bytes_size as usize);
                        let samples = bytes_to_samples(&buffer.split_to(size));
                        let frame = AudioFrame {
                            track_id: track_id_clone.clone(),
                            samples: Samples::PCM { samples },
                            timestamp: crate::get_timestamp(),
                            sample_rate,
                        };
                        ticker.tick().await;
                        packet_sender.send(frame)?;
                        continue;
                    }

                    if rx.is_closed() {
                        break;
                    }

                    if let Some(data) = rx.recv().await {
                        buffer.reserve(data.len());
                        buffer.extend_from_slice(&data);
                    }
                }
                Ok::<(), anyhow::Error>(())
            };

            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    debug!(track_id, "Media pass track is cancelled");
                }
                _ = emit_loop => {
                    debug!(track_id, "Media pass track emit complete");
                }
                _ = recv_loop => {}
            }

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::get_timestamp(),
                    duration: crate::get_timestamp() - start_time,
                    ssrc,
                    play_id: None,
                })
                .ok();
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(ws_sink) = self.ws_sink.lock().await.as_mut() {
            ws_sink.close().await.ok();
        }
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        if let Some(ws_sink) = self.ws_sink.lock().await.as_mut() {
            if let Samples::PCM { samples } = &packet.samples {
                let mut buffer = self.buffer.lock().await;
                buffer.reserve(samples.len());
                buffer.extend_from_slice(samples_to_bytes(samples.as_slice()).as_slice());
                let max_buffer_size = self.packet_size * packet.sample_rate / self.sample_rate;
                while buffer.len() >= max_buffer_size as usize {
                    let bytes = buffer.split_to(max_buffer_size as usize).freeze();
                    if packet.sample_rate == self.sample_rate {
                        ws_sink.send(Message::Binary(bytes)).await?;
                    } else {
                        let sample = bytes_to_samples(&bytes);
                        let resample = resample_mono(&sample, packet.sample_rate, self.sample_rate);
                        let bytes = samples_to_bytes(resample.as_slice());
                        ws_sink.send(Message::Binary(bytes.into())).await?;
                    }
                }
            }
        }
        Ok(())
    }
}
