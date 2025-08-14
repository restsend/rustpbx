use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    AudioFrame, Samples,
    media::{codecs::samples_to_bytes, processor::Processor},
};
use anyhow::Result;
use bytes::{Bytes, BytesMut};
use futures::SinkExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPassOption {
    pub url: String,
    pub packet_size: usize,
}

impl MediaPassOption {
    pub fn new(url: String, packet_size: usize) -> Self {
        Self { url, packet_size }
    }
}

/// A processor that sends audio data to `url` via websocket.
/// websocket server can get `sample_rate` from `X-Sample-Rate` header.
pub struct MediaPassProcessor {
    option: MediaPassOption,
    token: CancellationToken,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<Bytes>>>>,
}

const FLUSH_TIMEOUT: Duration = Duration::from_millis(300);

impl MediaPassProcessor {
    pub fn new(option: MediaPassOption, token: CancellationToken) -> Self {
        Self {
            option,
            token,
            sender: Arc::new(Mutex::new(None)),
        }
    }

    async fn websocket_task(
        url: String,
        packet_size: usize,
        sample_rate: u32,
        mut receiver: mpsc::UnboundedReceiver<Bytes>,
        token: CancellationToken,
    ) -> Result<()> {
        let mut request = url.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("X-Sample-Rate", sample_rate.to_string().parse()?);
        headers.insert("X-Content-Type", "audio/pcm".parse()?);

        let config = WebSocketConfig::default();
        let (mut stream, _) = connect_async_with_config(request, Some(config), false).await?;
        let mut buffer = BytesMut::new();
        let mut last_send = Instant::now();

        loop {
            let elapsed = last_send.elapsed();
            let remaining = if elapsed < FLUSH_TIMEOUT {
                FLUSH_TIMEOUT - elapsed
            } else {
                Duration::ZERO
            };

            tokio::select! {
                data = receiver.recv() => {
                    if let Some(data) = data {
                        buffer.extend_from_slice(&data);
                        if buffer.len() >= packet_size {
                            stream.send(Message::Binary(buffer.split().freeze())).await?;
                            last_send = Instant::now();
                        }
                    } else {
                        break;
                    }
                }
                _ = token.cancelled() => {
                    break;
                }
                _ = tokio::time::sleep(remaining) => {
                    if !buffer.is_empty() {
                        stream.send(Message::Binary(buffer.split().freeze())).await?;
                        last_send = Instant::now();
                    }
                }
            }
        }
        Ok(())
    }
}

impl Processor for MediaPassProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        if let Samples::PCM { samples } = &frame.samples {
            let bytes = samples_to_bytes(samples);
            let buf = Bytes::from(bytes);
            loop {
                let mut guard = self.sender.lock().unwrap();
                let sender = guard.get_or_insert_with(|| {
                    debug!("Media pass: connecting to websocket {}", self.option.url);
                    let (sd, rc) = mpsc::unbounded_channel();
                    let token = self.token.clone();
                    tokio::spawn(MediaPassProcessor::websocket_task(
                        self.option.url.clone(),
                        self.option.packet_size,
                        frame.sample_rate,
                        rc,
                        token,
                    ));
                    sd
                });

                if let Err(e) = sender.send(buf.clone()) {
                    warn!("Media pass processor send samples failed: {e}");
                    guard.take();
                    continue;
                }

                break;
            }
        }
        Ok(())
    }
}
