use crate::{
    event::{EventSender, SessionEvent},
    media::{
        cache,
        codecs::convert_u8_to_s16,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    synthesis::SynthesisClient,
    AudioFrame, Samples,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::{sync::Arc, time::Instant};
use tokio::{
    select,
    sync::{mpsc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct TtsCommand {
    pub text: String,
    pub speaker: Option<String>,
    pub play_id: Option<String>,
}
pub type TtsCommandSender = mpsc::UnboundedSender<TtsCommand>;
type TtsCommandReceiver = mpsc::UnboundedReceiver<TtsCommand>;

pub struct TtsTrack<T: SynthesisClient> {
    track_id: TrackId,
    processor_chain: ProcessorChain,
    config: TrackConfig,
    cancel_token: CancellationToken,
    use_cache: bool,
    command_rx: Mutex<Option<TtsCommandReceiver>>,
    client: Mutex<Option<T>>,
}

impl<T: SynthesisClient> TtsTrack<T> {
    pub fn new(track_id: TrackId, command_rx: TtsCommandReceiver, client: T) -> Self {
        let config = TrackConfig::default();
        Self {
            track_id,
            processor_chain: ProcessorChain::new(config.sample_rate),
            config,
            cancel_token: CancellationToken::new(),
            command_rx: Mutex::new(Some(command_rx)),
            use_cache: true,
            client: Mutex::new(Some(client)),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config;
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

    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.config = self.config.with_ptime(ptime);
        self
    }

    pub fn with_cache_enabled(mut self, use_cache: bool) -> Self {
        self.use_cache = use_cache;
        self
    }
}

#[async_trait]
impl<T: SynthesisClient + 'static> Track for TtsTrack<T> {
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
        let mut command_rx = self
            .command_rx
            .lock()
            .await
            .take()
            .ok_or(anyhow!("Command receiver not found"))?;

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let client = self.client.lock().await.take().unwrap();
        let buffer_clone = buffer.clone();
        let sample_rate = self.config.sample_rate;
        let use_cache = self.use_cache;
        let command_loop = async move {
            let mut last_play_id = None;
            while let Some(command) = command_rx.recv().await {
                let text = command.text;
                let speaker = command.speaker;
                let play_id = command.play_id;
                if play_id != last_play_id {
                    last_play_id = play_id.clone();
                    buffer_clone.lock().await.clear();
                }
                let cache_key = format!("tts:{:?}{}", speaker, text);
                let cache_key = cache::generate_cache_key(&cache_key, sample_rate);
                if use_cache {
                    match cache::is_cached(&cache_key).await {
                        Ok(true) => match cache::retrieve_from_cache(&cache_key).await {
                            Ok(audio) => {
                                info!("Using cached TTS audio for {}", cache_key);
                                buffer_clone.lock().await.extend(audio);
                                continue;
                            }
                            Err(e) => {
                                warn!("Error retrieving cached TTS audio: {}", e);
                            }
                        },
                        _ => {}
                    }
                }

                let start_time = Instant::now();
                match client.synthesize(&text).await {
                    Ok(audio) => {
                        event_sender
                            .send(SessionEvent::Metrics {
                                timestamp: crate::get_timestamp(),
                                sender: "tts.tencent_cloud".to_string(),
                                metrics: serde_json::json!({
                                        "speaker": speaker.clone(),
                                        "play_id": play_id.clone(),
                                        "audio_length": audio.len(),
                                        "duration": start_time.elapsed().as_millis() as u32,
                                }),
                            })
                            .ok();
                        debug!(
                            "TTS audio length: {} bytes -> {}ms {}",
                            audio.len(),
                            start_time.elapsed().as_millis(),
                            text
                        );

                        let audio = if audio.len() > 44 && audio[..4] == [0x52, 0x49, 0x46, 0x46] {
                            audio[44..].to_vec()
                        } else {
                            audio
                        };
                        if use_cache {
                            cache::store_in_cache(&cache_key, &audio).await.ok();
                        }
                        buffer_clone.lock().await.extend(audio);
                    }
                    Err(e) => {
                        warn!("Error synthesizing text: {}", e);
                        continue;
                    }
                }
            }
        };
        let sample_rate = self.config.sample_rate;
        let max_pcm_chunk_size = self.config.max_pcm_chunk_size;
        let track_id = self.track_id.clone();
        let packet_duration = 1000.0 / sample_rate as f64 * max_pcm_chunk_size as f64;
        let packet_duration_ms = packet_duration as u32;
        info!(
            "TTS track {} with sample_rate: {} max_pcm_chunk_size: {} packet_duration_ms: {}",
            track_id, sample_rate, max_pcm_chunk_size, packet_duration_ms
        );
        let mut ptimer = tokio::time::interval(Duration::from_millis(packet_duration_ms as u64));
        let processor_chain = self.processor_chain.clone();
        let emit_loop = async move {
            loop {
                let packet = {
                    let mut buffer = buffer.lock().await;
                    if buffer.len() > max_pcm_chunk_size {
                        let s16_data = buffer.drain(..max_pcm_chunk_size).collect::<Vec<u8>>();
                        Some(s16_data)
                    } else {
                        None
                    }
                };
                if let Some(packet) = packet {
                    let packet = AudioFrame {
                        track_id: track_id.clone(),
                        samples: Samples::PCM(convert_u8_to_s16(&packet)),
                        timestamp: crate::get_timestamp(),
                        sample_rate,
                    };

                    // Process the frame with processor chain
                    if let Err(e) = processor_chain.process_frame(&packet) {
                        warn!("Error processing frame: {}", e);
                    }

                    // Send the packet
                    packet_sender.send(packet).ok();
                }
                ptimer.tick().await;
            }
        };

        tokio::spawn(async move {
            select! {
                _ = command_loop => {}
                _ = emit_loop => {}
                _ = token.cancelled() => {}
            }
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}
