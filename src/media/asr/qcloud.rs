use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error};
use url::Url;

use crate::media::{
    processor::{AudioFrame, Processor},
    stream::{EventSender, MediaStreamEvent},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QCloudAsrConfig {
    pub app_id: String,
    pub secret_id: String,
    pub secret_key: String,
    pub engine_type: String,
    pub voice_format: String,
    pub speaker_diarization: bool,
    pub filter_dirty: bool,
    pub filter_modal: bool,
    pub filter_punc: bool,
    pub convert_num_mode: i32,
}

impl Default for QCloudAsrConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            secret_id: String::new(),
            secret_key: String::new(),
            engine_type: "16k_zh".to_string(),
            voice_format: "pcm".to_string(),
            speaker_diarization: false,
            filter_dirty: false,
            filter_modal: false,
            filter_punc: false,
            convert_num_mode: 1,
        }
    }
}

pub struct QCloudAsr {
    track_id: String,
    ssrc: u32,
    config: QCloudAsrConfig,
    event_sender: EventSender,
    ws_sender: Option<mpsc::UnboundedSender<Message>>,
}

impl QCloudAsr {
    pub fn new(
        track_id: String,
        ssrc: u32,
        config: QCloudAsrConfig,
        event_sender: EventSender,
    ) -> Self {
        Self {
            track_id,
            ssrc,
            config,
            event_sender,
            ws_sender: None,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        let url = format!(
            "wss://asr.cloud.tencent.com/asr/v2/{}/{}?secretid={}&engine_type={}&voice_format={}&speaker_diarization={}&filter_dirty={}&filter_modal={}&filter_punc={}&convert_num_mode={}",
            self.config.app_id,
            uuid::Uuid::new_v4(),
            self.config.secret_id,
            self.config.engine_type,
            self.config.voice_format,
            self.config.speaker_diarization as i32,
            self.config.filter_dirty as i32,
            self.config.filter_modal as i32,
            self.config.filter_punc as i32,
            self.config.convert_num_mode,
        );

        let url = Url::parse(&url)?;
        let (ws_stream, _) = connect_async(url).await?;
        let (write, mut read) = ws_stream.split();

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.ws_sender = Some(tx);

        // Handle incoming messages
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(response) = serde_json::from_str::<serde_json::Value>(&text) {
                            debug!("Received ASR response: {:?}", response);
                            // Handle ASR response and send events
                            // TODO: Implement response handling
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Forward messages to WebSocket
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = write.send(msg).await {
                    error!("Failed to send WebSocket message: {}", e);
                    break;
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Processor for QCloudAsr {
    async fn init(&mut self) -> Result<()> {
        self.connect().await
    }

    async fn process_frame(&mut self, frame: AudioFrame) -> Result<AudioFrame> {
        if let Some(sender) = &self.ws_sender {
            // Convert samples to bytes
            let mut bytes = Vec::with_capacity(frame.samples.len() * 2);
            for sample in frame.samples.iter() {
                let pcm = (*sample * 32768.0) as i16;
                bytes.extend_from_slice(&pcm.to_le_bytes());
            }

            // Send audio data
            let msg = Message::Binary(bytes);
            sender.send(msg).ok();
        }

        Ok(frame)
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(sender) = &self.ws_sender {
            sender.send(Message::Close(None)).ok();
        }
        Ok(())
    }
}
