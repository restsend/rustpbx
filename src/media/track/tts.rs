use crate::{
    AudioFrame, Samples,
    event::{EventSender, SessionEvent},
    media::{
        cache,
        codecs::bytes_to_samples,
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    synthesis::{SynthesisClient, SynthesisOption, TTSEvent, TTSSubtitle, bytes_size_to_duration},
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::{
    select,
    sync::{Mutex, mpsc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct TtsHandle {
    pub play_id: Option<String>,
    pub command_tx: TtsCommandSender,
}
#[derive(Clone, Default)]
pub struct TtsCommand {
    pub text: String,
    pub speaker: Option<String>,
    pub play_id: Option<String>,
    pub streaming: Option<bool>,
    pub end_of_stream: Option<bool>,
    pub option: SynthesisOption,
}
pub type TtsCommandSender = mpsc::UnboundedSender<TtsCommand>;
pub type TtsCommandReceiver = mpsc::UnboundedReceiver<TtsCommand>;

pub struct TtsTrack {
    track_id: TrackId,
    session_id: String,
    processor_chain: ProcessorChain,
    config: TrackConfig,
    cancel_token: CancellationToken,
    use_cache: bool,
    command_rx: Mutex<Option<TtsCommandReceiver>>,
    client: Mutex<Option<Box<dyn SynthesisClient>>>,
    ssrc: u32,
}

impl TtsHandle {
    pub fn new(command_tx: TtsCommandSender, play_id: Option<String>) -> Self {
        Self {
            play_id,
            command_tx,
        }
    }

    pub fn new_empty() -> Self {
        // Create an unbounded channel and discard the receiver
        let (tx, _) = mpsc::unbounded_channel();
        Self {
            play_id: None,
            command_tx: tx,
        }
    }

    pub fn try_send(&self, cmd: TtsCommand) -> Result<(), mpsc::error::SendError<TtsCommand>> {
        if self.play_id == cmd.play_id {
            self.command_tx.send(cmd)
        } else {
            Err(mpsc::error::SendError(cmd))
        }
    }
}

impl TtsTrack {
    pub fn new(
        track_id: TrackId,
        session_id: String,
        command_rx: TtsCommandReceiver,
        client: Box<dyn SynthesisClient>,
    ) -> Self {
        let config = TrackConfig::default();
        Self {
            track_id,
            session_id,
            processor_chain: ProcessorChain::new(config.samplerate),
            config,
            cancel_token: CancellationToken::new(),
            command_rx: Mutex::new(Some(command_rx)),
            use_cache: true,
            client: Mutex::new(Some(client)),
            ssrc: 0,
        }
    }
    pub fn with_ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = ssrc;
        self
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
impl Track for TtsTrack {
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

    async fn start(
        &self,
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
        let sample_rate = self.config.samplerate;
        let use_cache = self.use_cache;
        let track_id = self.track_id.clone();
        let event_sender_clone = event_sender.clone();
        let synthesize_done = Arc::new(AtomicBool::new(false));
        let synthesize_done_clone = synthesize_done.clone();
        let session_id = self.session_id.clone();
        let current_text = Arc::new(Mutex::new(None));
        let current_text_clone = current_text.clone();
        let subtitles = Arc::new(Mutex::new(Vec::<TTSSubtitle>::new()));
        let subtitles_clone = subtitles.clone();
        let total_audio_len = Arc::new(AtomicUsize::new(0));
        let total_audio_len_clone = total_audio_len.clone();
        let command_loop = async move {
            let mut last_play_id = None;
            while let Some(command) = command_rx.recv().await {
                let text = command.text;
                {
                    let mut current_text = current_text_clone.lock().await;
                    *current_text = Some(text.clone());
                }
                let play_id = command.play_id;
                let mut option = command.option;
                if option.speaker.is_none() {
                    option.speaker = command.speaker;
                }
                let speaker = option.speaker.clone();

                if play_id != last_play_id || play_id.is_none() {
                    last_play_id = play_id.clone();
                    buffer_clone.lock().await.clear();
                }
                let cache_key = cache::generate_cache_key(
                    &format!("tts:{}{}", client.provider(), text),
                    sample_rate,
                    option.speaker.as_ref(),
                    option.speed.clone(),
                );
                synthesize_done.store(false, Ordering::Relaxed);

                {
                    let mut subtitles = subtitles_clone.lock().await;
                    subtitles.clear();
                }

                if use_cache {
                    match cache::is_cached(&cache_key).await {
                        Ok(true) => match cache::retrieve_from_cache(&cache_key).await {
                            Ok(audio) => {
                                info!(session_id, "using cached audio for {}", cache_key);
                                buffer_clone.lock().await.extend(bytes_to_samples(&audio));
                                synthesize_done.store(true, Ordering::Relaxed);
                                event_sender
                                    .send(SessionEvent::Metrics {
                                        timestamp: crate::get_timestamp(),
                                        key: format!("completed.tts.{}", client.provider()),
                                        data: serde_json::json!({
                                                "speaker": speaker,
                                                "playId": play_id,
                                                "length": audio.len(),
                                                "cached": true,
                                        }),
                                        duration: 0,
                                    })
                                    .ok();
                                {
                                    let mut subtitles = subtitles_clone.lock().await;
                                    let duration = bytes_size_to_duration(audio.len(), sample_rate);
                                    total_audio_len_clone.store(audio.len(), Ordering::Relaxed);
                                    subtitles.push(TTSSubtitle::new(
                                        0,
                                        duration,
                                        0,
                                        text.chars().count() as u32,
                                    ));
                                }
                                continue;
                            }
                            Err(e) => {
                                warn!(session_id, "error retrieving cached audio: {}", e);
                            }
                        },
                        _ => {}
                    }
                }

                let start_time = Instant::now();
                match client.synthesize(&text.to_string(), Some(option)).await {
                    Ok(mut stream) => {
                        total_audio_len_clone.store(0, Ordering::Relaxed);
                        let mut audio_chunks = Vec::new();
                        let mut first_chunk = true;
                        // Process each audio chunk as it arrives
                        while let Some(chunk_result) = stream.next().await {
                            match chunk_result {
                                Ok(TTSEvent::AudioChunk(audio_chunk)) => {
                                    // Process the audio chunk
                                    total_audio_len_clone
                                        .fetch_add(audio_chunk.len(), Ordering::Relaxed);

                                    if first_chunk {
                                        first_chunk = false;
                                        // Send metrics event after the first chunk
                                        event_sender
                                            .send(SessionEvent::Metrics {
                                                timestamp: crate::get_timestamp(),
                                                key: format!("ttfb.tts.{}", client.provider()),
                                                data: serde_json::json!({
                                                        "speaker": speaker,
                                                        "playId": play_id,
                                                        "length": audio_chunk.len(),
                                                }),
                                                duration: start_time.elapsed().as_millis() as u32,
                                            })
                                            .ok();
                                    }

                                    // Strip wav header if present (only for the first chunk)
                                    let processed_chunk = if audio_chunks.is_empty()
                                        && audio_chunk.len() > 44
                                        && audio_chunk[..4] == [0x52, 0x49, 0x46, 0x46]
                                    {
                                        audio_chunk[44..].to_vec()
                                    } else {
                                        audio_chunk
                                    };

                                    // Store the processed chunk for caching later if needed
                                    audio_chunks.push(processed_chunk.clone());

                                    // Convert to s16 and add to buffer immediately for streaming playback
                                    buffer_clone
                                        .lock()
                                        .await
                                        .extend(bytes_to_samples(&processed_chunk));
                                }
                                Ok(TTSEvent::Finished) => {
                                    break;
                                }
                                Ok(TTSEvent::Subtitles(subtitles)) => {
                                    subtitles_clone.lock().await.extend(subtitles);
                                }
                                Err(e) => {
                                    warn!(session_id, "Error in audio stream chunk: {:?}", e);
                                    event_sender
                                        .send(SessionEvent::Error {
                                            timestamp: crate::get_timestamp(),
                                            track_id: track_id.clone(),
                                            sender: format!("tts.{}", client.provider()),
                                            error: e.to_string(),
                                            code: None,
                                        })
                                        .ok();
                                }
                            }
                        }
                        synthesize_done.store(true, Ordering::Relaxed);
                        // Send metrics event after all chunks are received
                        event_sender
                            .send(SessionEvent::Metrics {
                                timestamp: crate::get_timestamp(),
                                key: format!("completed.tts.{}", client.provider()),
                                data: serde_json::json!({
                                        "speaker": speaker,
                                        "playId": play_id,
                                        "length": total_audio_len_clone.load(Ordering::Relaxed),
                                }),
                                duration: start_time.elapsed().as_millis() as u32,
                            })
                            .ok();

                        info!(
                            session_id,
                            "synthesize audio {} bytes -> {}ms {} with {}",
                            total_audio_len_clone.load(Ordering::Relaxed),
                            start_time.elapsed().as_millis(),
                            text,
                            client.provider()
                        );

                        // Cache the complete audio if caching is enabled
                        if use_cache && !audio_chunks.is_empty() {
                            // Combine all chunks for caching
                            let complete_audio: Vec<u8> =
                                audio_chunks.into_iter().flatten().collect();
                            cache::store_in_cache(&cache_key, &complete_audio)
                                .await
                                .ok();
                        }
                    }
                    Err(e) => {
                        warn!(session_id, "error synthesizing text: {}", e);
                        event_sender
                            .send(SessionEvent::Error {
                                timestamp: crate::get_timestamp(),
                                track_id: track_id.clone(),
                                sender: format!("tts.{}", client.provider()),
                                error: e.to_string(),
                                code: None,
                            })
                            .ok();
                        continue;
                    }
                }
            }
        };
        let sample_rate = self.config.samplerate;
        let track_id = self.track_id.clone();
        let packet_duration_ms = self.config.ptime.as_millis() as u32;
        let max_pcm_chunk_size = sample_rate as usize * packet_duration_ms as usize / 1000;
        info!(
            session_id = self.session_id,
            "track started with sample_rate: {} packet_duration_ms: {} max_pcm_chunk_size: {}",
            sample_rate,
            packet_duration_ms,
            max_pcm_chunk_size
        );
        let mut ptimer = tokio::time::interval(Duration::from_millis(packet_duration_ms as u64));
        let processor_chain = self.processor_chain.clone();
        let session_id = self.session_id.clone();
        let buffer_clone = buffer.clone();
        let emit_loop = async move {
            let start_time = Instant::now();
            loop {
                let packet = {
                    let mut buffer = buffer_clone.lock().await;
                    if buffer.len() > max_pcm_chunk_size {
                        let s16_data = buffer.drain(..max_pcm_chunk_size).collect::<Vec<_>>();
                        Some(s16_data)
                    } else {
                        if synthesize_done_clone.load(Ordering::Relaxed) {
                            break;
                        } else {
                            None
                        }
                    }
                };
                if let Some(packet) = packet {
                    let mut packet = AudioFrame {
                        track_id: track_id.clone(),
                        samples: Samples::PCM { samples: packet },
                        timestamp: crate::get_timestamp(),
                        sample_rate,
                    };
                    // Process the frame with processor chain
                    if let Err(e) = processor_chain.process_frame(&mut packet) {
                        warn!(track_id, "error processing frame: {}", e);
                    }
                    // Send the packet
                    packet_sender.send(packet).ok();
                }
                ptimer.tick().await;
            }
            info!(
                session_id,
                "emit done {} ms",
                start_time.elapsed().as_millis()
            );
        };
        let track_id = self.track_id.clone();
        let token = self.cancel_token.clone();
        let start_time = crate::get_timestamp();
        let session_id = self.session_id.clone();
        let ssrc = self.ssrc;
        tokio::spawn(async move {
            select! {
                _ = command_loop => {
                    info!(session_id,"command loop done");
                }
                _ = emit_loop => {
                    info!(session_id, "emit loop done");
                }
                _ = token.cancelled() => {
                    let total_size = total_audio_len.load(Ordering::Relaxed);
                    let remaining_size = buffer.lock().await.len() * 2;
                    debug!(session_id, "total_size: {} remaining_size: {}", total_size, remaining_size);
                    let sended_size = total_size - remaining_size;
                    let text = current_text.lock().await.take();
                    let mut position = None;
                    let current = bytes_size_to_duration(sended_size, sample_rate);
                    let total_duration = bytes_size_to_duration(total_size, sample_rate);                    
                    let subtitles = subtitles.lock().await;
                    for subtitle in subtitles.iter().rev() {
                        if subtitle.begin_time <= current {
                            position = Some(subtitle.begin_index);
                            break;
                        }
                    }
                    let event = SessionEvent::Interruption {
                        track_id: track_id.clone(),
                        timestamp: crate::get_timestamp(),
                        subtitle: text,
                        position,
                        total_duration,
                        current,
                    };
                    let _ = event_sender_clone.send(event);
                }
            }
            event_sender_clone
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::get_timestamp(),
                    duration: crate::get_timestamp() - start_time,
                    ssrc,
                })
                .ok();
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
