use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::Samples,
    media::TrackId,
    media::{
        codecs::{bytes_to_samples, resample::resample_mono, samples_to_bytes},
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

type WsConn = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsConn, Message>;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MediaPassOption {
    url: String,              // websocket url, e.g. ws://localhost:8080/
    input_sample_rate: u32,   // sample rate of audio receiving from websocket
    output_sample_rate: u32,  // sample rate of audio sending to websocket server
    packet_size: Option<u32>, // packet size send to websocket server, default is 3200
    ptime: Option<u32>, // packet time in milliseconds, if set, buffer and emit at fixed intervals
}

impl MediaPassOption {
    pub fn new(
        url: String,
        input_sample_rate: u32,
        output_sample_rate: u32,
        packet_size: Option<u32>,
        ptime: Option<u32>,
    ) -> Self {
        Self {
            url,
            input_sample_rate,
            output_sample_rate,
            packet_size,
            ptime,
        }
    }
}

pub struct MediaPassTrack {
    session_id: String,
    track_id: TrackId,
    cancel_token: CancellationToken,
    config: TrackConfig, // input sample rate is here, it is 0 if the ptime is None
    url: String,
    output_sample_rate: u32, // output sample rate
    packet_size: u32,
    // buffer the rtp/webrtc packets, send to websocket server with packet size
    buffer: Mutex<BytesMut>,
    ws_sink: Arc<Mutex<Option<WsSink>>>,
    bytes_sent: Arc<AtomicU64>, // bytes sent to websocket
    ssrc: u32,
    processor_chain: ProcessorChain,
}

impl MediaPassTrack {
    pub fn new(
        session_id: String,
        ssrc: u32,
        track_id: TrackId,
        cancel_token: CancellationToken,
        option: MediaPassOption,
    ) -> Self {
        let sample_rate = option.output_sample_rate;
        let mut config = TrackConfig::default();
        config = config.with_sample_rate(option.input_sample_rate);
        config = config.with_ptime(Duration::from_millis(option.ptime.unwrap_or(0) as u64));
        // for 16000Hz, 20ms ptime, 3200 is 5 packets
        let packet_size = option.packet_size.unwrap_or(3200);
        let buffer: Mutex<BytesMut> = Mutex::new(BytesMut::with_capacity(packet_size as usize * 2));
        Self {
            session_id,
            track_id,
            cancel_token,
            config,
            url: option.url,
            output_sample_rate: sample_rate,
            packet_size,
            buffer,
            ssrc,
            ws_sink: Arc::new(Mutex::new(None)),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            // dummy processor chain, will ignore processor
            processor_chain: ProcessorChain::new(sample_rate),
        }
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
        warn!(track_id = %self.track_id, "ignore processor for media pass track");
        &mut self.processor_chain
    }
    fn insert_processor(&mut self, _: Box<dyn Processor>) {
        warn!(track_id = %self.track_id, "ignore processor for media pass track");
    }
    fn append_processor(&mut self, _: Box<dyn Processor>) {
        warn!(track_id = %self.track_id, "ignore processor for media pass track");
    }
    async fn handshake(&mut self, _: String, _: Option<Duration>) -> Result<String> {
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
        let mut url = url::Url::parse(&self.url)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("sample_rate", self.output_sample_rate.to_string().as_str());
            query.append_pair("packet_size", self.packet_size.to_string().as_str());
        }
        info!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            input_sample_rate = self.config.samplerate,
            output_sample_rate = self.output_sample_rate,
            packet_size = self.packet_size,
            ptime_ms = self.config.ptime.as_millis(),
            "Media pass track starting"
        );
        let input_sample_rate = self.config.samplerate;
        let output_sample_rate = self.output_sample_rate;
        let (ws_stream, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
        let (ws_sink, mut ws_source) = ws_stream.split();
        *self.ws_sink.lock().await = Some(ws_sink);
        let ws_sink = self.ws_sink.clone();
        let bytes_sent = self.bytes_sent.clone();
        let session_id = self.session_id.clone();
        let ssrc = self.ssrc;
        let track_id = self.track_id.clone();
        let start_time = crate::media::get_timestamp();
        let cancel_token = self.cancel_token.clone();
        let ptime = self.config.ptime;
        let ptime_ms = ptime.as_millis() as u32;
        tokio::spawn(async move {
            let mut bytes_received = 0u64;
            let mut bytes_emitted = 0u64;

            // ptimer is polled only if ptime > 0
            let capacity = input_sample_rate as usize * ptime_ms as usize / 500;
            let (mut ptimer, mut samples, mut buffer) = if ptime_ms > 0 {
                (
                    tokio::time::interval(Duration::from_millis(ptime_ms as u64)),
                    vec![0u8; capacity],
                    BytesMut::with_capacity(8 * 1024),
                )
            } else {
                (
                    tokio::time::interval(Duration::MAX),
                    Vec::new(),
                    BytesMut::new(),
                )
            };

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        info!(session_id, "Media pass track cancelled");
                        break;
                    }
                    _ = ptimer.tick(), if ptime_ms > 0 => {
                        // Fill samples buffer from audio buffer
                        samples.fill(0);
                        let mut i = 0;

                        // Fill samples until it's full or there's no more data
                        while i < capacity && buffer.len() > 0 {
                            let remaining = capacity - i;
                            let available = buffer.len();
                            let len = usize::min(remaining, available);
                            let cut = buffer.split_to(len);
                            samples[i..i+len].copy_from_slice(&cut);
                            i += len;
                        }

                        // Create frame (will have zeros if not enough data)
                        let samples_vec = bytes_to_samples(&samples[..]);
                        let frame = AudioFrame {
                            track_id: track_id.clone(),
                            samples: Samples::PCM { samples: samples_vec.clone() },
                            timestamp: crate::media::get_timestamp(),
                            sample_rate: input_sample_rate,
                        };

                        if let Ok(_) = packet_sender.send(frame) {
                            // only count the actual bytes filled from buffer
                            bytes_emitted += i as u64;
                        } else {
                            warn!(
                                track_id,
                                "packet sender closed, stopping emit loop"
                            );
                            break;
                        }
                    }
                    msg = ws_source.next() => {
                        match msg {
                            Some(Ok(Message::Binary(data))) => {
                                bytes_received += data.len() as u64;
                                if ptime_ms > 0 {
                                    // buffer if ptime was set
                                    buffer.reserve(data.len());
                                    buffer.extend_from_slice(&data);
                                } else {
                                    // send immediately if ptime was not set
                                    let samples_vec = bytes_to_samples(&data);
                                    let frame = AudioFrame {
                                        track_id: track_id.clone(),
                                        samples: Samples::PCM { samples: samples_vec.clone() },
                                        timestamp: crate::media::get_timestamp(),
                                        sample_rate: input_sample_rate,
                                    };

                                    if let Ok(_) = packet_sender.send(frame) {
                                        bytes_emitted += data.len() as u64;
                                    } else {
                                        warn!(
                                            track_id,
                                            "packet sender closed, stopping emit loop"
                                        );
                                        break;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(res))) => {
                                warn!(
                                    track_id,
                                    close_reason = ?res,
                                    bytes_received,
                                    "Media pass track closed by remote"
                                );
                                break;
                            }
                            Some(Err(e)) => {
                                error!(
                                    track_id,
                                    error = %e,
                                    bytes_received,
                                    "Media pass track WebSocket error"
                                );
                                let error = SessionEvent::Error {
                                    track_id: track_id.clone(),
                                    timestamp: crate::media::get_timestamp(),
                                    sender: format!("media_pass: {}", url),
                                    error: format!("Media pass track error: {:?}", e),
                                    code: None,
                                };
                                event_sender.send(error).ok();
                                break;
                            }
                            None => {
                                info!(
                                    track_id,
                                    bytes_received,
                                    "Media pass track WebSocket stream ended"
                                );
                                break;
                            }
                            _ => {}
                        }
                    }
                }

                // Break if packet sender is closed
                if packet_sender.is_closed() {
                    break;
                }
            }

            if let Some(mut ws_sink) = ws_sink.lock().await.take() {
                ws_sink.close().await.ok();
            };

            let duration = crate::media::get_timestamp() - start_time;
            let bytes_sent_to_ws = bytes_sent.load(Ordering::Relaxed);
            info!(
                session_id,
                duration,
                input_sample_rate,
                output_sample_rate,
                bytes_received,
                bytes_emitted,
                bytes_sent_to_ws,
                "Media pass track ended"
            );

            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::media::get_timestamp(),
                    duration,
                    ssrc,
                    play_id: None,
                })
                .ok();
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(mut ws_sink) = self.ws_sink.lock().await.take() {
            ws_sink.close().await.ok();
        }
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        if let Some(ws_sink) = self.ws_sink.lock().await.as_mut() {
            if let Samples::PCM { samples } = &packet.samples {
                let mut buffer = self.buffer.lock().await;
                let bytes = samples_to_bytes(samples.as_slice());
                buffer.reserve(bytes.len());
                buffer.extend_from_slice(bytes.as_slice());
                while buffer.len() >= self.packet_size as usize {
                    let bytes = buffer.split_to(self.packet_size as usize).freeze();
                    let bytes = if packet.sample_rate == self.output_sample_rate {
                        bytes
                    } else {
                        let samples = bytes_to_samples(&bytes);
                        let resample =
                            resample_mono(&samples, packet.sample_rate, self.output_sample_rate);
                        let bytes = samples_to_bytes(resample.as_slice());
                        Bytes::copy_from_slice(bytes.as_slice())
                    };
                    let bytes_len = bytes.len();
                    ws_sink.send(Message::Binary(bytes)).await?;
                    self.bytes_sent
                        .fetch_add(bytes_len as u64, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }
}
