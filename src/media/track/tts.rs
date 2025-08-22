use crate::{
    AudioFrame, Samples,
    event::{EventSender, SessionEvent},
    media::{
        cache,
        codecs::bytes_to_samples,
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    synthesis::{
        Subtitle, SynthesisClient, SynthesisCommand, SynthesisCommandReceiver,
        SynthesisCommandSender, SynthesisEvent, bytes_size_to_duration,
    },
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicIsize, Ordering},
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

#[derive(Clone)]
struct SynthesisStatus {
    play_id: Option<String>,
    speaker: Option<String>,
    total_audio_len: usize,
    full_text: String,
    subtitles: Vec<Subtitle>,
}

pub struct SynthesisHandle {
    pub play_id: Option<String>,
    pub command_tx: SynthesisCommandSender,
}

pub struct TtsTrack {
    track_id: TrackId,
    session_id: String,
    processor_chain: ProcessorChain,
    config: TrackConfig,
    cancel_token: CancellationToken,
    use_cache: bool,
    command_rx: Mutex<Option<SynthesisCommandReceiver>>,
    client: Mutex<Option<Box<dyn SynthesisClient>>>,
    ssrc: u32,
}

impl SynthesisHandle {
    pub fn new(command_tx: SynthesisCommandSender, play_id: Option<String>) -> Self {
        Self {
            play_id,
            command_tx,
        }
    }
    pub fn try_send(
        &self,
        cmd: SynthesisCommand,
    ) -> Result<(), mpsc::error::SendError<SynthesisCommand>> {
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
        command_rx: SynthesisCommandReceiver,
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

        let sample_rate = self.config.samplerate;
        let use_cache = self.use_cache;
        let track_id = self.track_id.clone();
        let session_id = self.session_id.clone();

        let client = self
            .client
            .lock()
            .await
            .take()
            .ok_or(anyhow!("Synthesis client not found"))?;
        let client = Arc::new(client);
        let client_ref = client.clone();
        let mut stream = client_ref.start(self.cancel_token.clone()).await?;
        let status = Arc::new(RwLock::new(SynthesisStatus {
            play_id: None,
            speaker: None,
            total_audio_len: 0,
            full_text: String::new(),
            subtitles: Vec::new(),
        }));
        let status_ref = status.clone();
        let provider = client.provider();
        let client = client.clone();
        let event_sender_clone = event_sender.clone();
        let client_ref = client.clone();
        let (buffer_tx, mut buffer_rx) = mpsc::unbounded_channel();
        let buffer_tx_ref = buffer_tx.clone();
        let synthesis_tasks = Arc::new(AtomicIsize::new(0));
        let synthesis_tasks_ref = synthesis_tasks.clone();

        let command_loop = async move {
            while let Some(mut command) = command_rx.recv().await {
                let text = command.text;
                let play_id = command.play_id;
                if command.option.speaker.is_none() {
                    command.option.speaker = command.speaker;
                }
                let streaming = command.streaming.unwrap_or(false);
                let cache_key = if !streaming && use_cache {
                    Some(cache::generate_cache_key(
                        &format!("tts:{}{}", provider, text),
                        sample_rate,
                        command.option.speaker.as_ref(),
                        command.option.speed.clone(),
                    ))
                } else {
                    None
                };
                command.option.cache_key = cache_key.clone();

                status_ref
                    .write()
                    .as_mut()
                    .and_then(|status| {
                        if !streaming {
                            status.full_text = text.clone();
                            status.subtitles.clear();
                            status.total_audio_len = 0;
                        } else {
                            status.full_text.push_str(&text);
                        }
                        status.play_id = play_id.clone();
                        status.speaker = command.option.speaker.clone();
                        Ok(())
                    })
                    .ok();

                if let Some(cache_key) = cache_key {
                    match cache::is_cached(&cache_key).await {
                        Ok(true) => match cache::retrieve_from_cache(&cache_key).await {
                            Ok(audio) => {
                                info!(session_id, text, "using cached audio for {}", cache_key);
                                buffer_tx_ref.send(Some(bytes_to_samples(&audio))).ok();
                                if command.end_of_stream.unwrap_or_default() {
                                    buffer_tx_ref.send(None).ok();
                                }

                                event_sender_clone
                                    .send(SessionEvent::Metrics {
                                        timestamp: crate::get_timestamp(),
                                        key: format!("completed.tts.{}", provider),
                                        data: serde_json::json!({
                                                "speaker": command.option.speaker,
                                                "playId": play_id,
                                                "length": audio.len(),
                                                "cached": true,
                                        }),
                                        duration: 0,
                                    })
                                    .ok();

                                let duration = bytes_size_to_duration(audio.len(), sample_rate);
                                status_ref
                                    .write()
                                    .as_mut()
                                    .and_then(|status| {
                                        status.total_audio_len = audio.len();
                                        status.subtitles.push(Subtitle::new(
                                            0,
                                            duration,
                                            0,
                                            text.chars().count() as u32,
                                        ));
                                        Ok(())
                                    })
                                    .ok();
                                continue;
                            }
                            Err(e) => {
                                warn!(session_id, "error retrieving cached audio: {}", e);
                            }
                        },
                        _ => {}
                    }
                }
                info!(
                    session_id,
                    text,
                    end_of_stream = command.end_of_stream,
                    play_id,
                    tasks = synthesis_tasks_ref.load(Ordering::SeqCst),
                    "synthesizing"
                );
                synthesis_tasks_ref.fetch_add(1, Ordering::SeqCst);
                match client_ref
                    .synthesize(&text, command.end_of_stream, Some(command.option))
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        synthesis_tasks_ref.fetch_sub(1, Ordering::SeqCst);
                        warn!(session_id, "error synthesizing text: {}", e);
                        event_sender_clone
                            .send(SessionEvent::Error {
                                timestamp: crate::get_timestamp(),
                                track_id: track_id.clone(),
                                sender: format!("tts.{}", provider),
                                error: e.to_string(),
                                code: None,
                            })
                            .ok();
                        continue;
                    }
                }
            }
        };
        let status_ref = status.clone();
        let provider = client.provider();
        let session_id = self.session_id.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.track_id.clone();
        let buffer_tx_ref = buffer_tx.clone();
        let synthesis_tasks_ref = synthesis_tasks.clone();

        let receive_result_loop = async move {
            let mut audio_chunks = Vec::new();
            let mut first_chunk = true;
            let start_time = Instant::now();
            // Process each audio chunk as it arrives
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(SynthesisEvent::AudioChunk(audio_chunk)) => {
                        // Process the audio chunk
                        status_ref
                            .write()
                            .as_mut()
                            .and_then(|status| {
                                status.total_audio_len += audio_chunk.len();
                                if !first_chunk {
                                    return Ok(());
                                }
                                first_chunk = false;
                                // Send metrics event after the first chunk
                                event_sender_clone
                                    .send(SessionEvent::Metrics {
                                        timestamp: crate::get_timestamp(),
                                        key: format!("ttfb.tts.{}", provider),
                                        data: serde_json::json!({
                                                "speaker": status.speaker,
                                                "playId": status.play_id,
                                                "length": audio_chunk.len(),
                                        }),
                                        duration: start_time.elapsed().as_millis() as u32,
                                    })
                                    .ok();
                                Ok(())
                            })
                            .ok();

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
                        match buffer_tx_ref.send(Some(bytes_to_samples(&processed_chunk))) {
                            Ok(_) => {}
                            Err(e) => {
                                warn!(session_id, "error sending cached audio: {}", e);
                                continue;
                            }
                        }
                    }
                    Ok(SynthesisEvent::Finished {
                        end_of_stream,
                        cache_key,
                    }) => {
                        synthesis_tasks_ref.fetch_sub(1, Ordering::SeqCst);
                        let status = match status_ref.read() {
                            Ok(status) => status.clone(),
                            Err(e) => {
                                warn!(session_id, "error reading synthesis status: {}", e);
                                return;
                            }
                        };
                        // Send metrics event after all chunks are received
                        event_sender_clone
                            .send(SessionEvent::Metrics {
                                timestamp: crate::get_timestamp(),
                                key: format!("completed.tts.{}", provider),
                                data: serde_json::json!({
                                        "speaker": status.speaker,
                                        "playId": status.play_id,
                                        "length": status.total_audio_len,
                                }),
                                duration: start_time.elapsed().as_millis() as u32,
                            })
                            .ok();

                        info!(
                            session_id,
                            %provider,
                            audio_len = status.total_audio_len,
                            elapsed = start_time.elapsed().as_millis(),
                            text = status.full_text,
                            "synthesize audio completed",
                        );
                        let complete_audio = audio_chunks.drain(..).flatten().collect::<Vec<_>>();
                        if use_cache && !complete_audio.is_empty() {
                            if let Some(ref cache_key) = cache_key {
                                cache::store_in_cache(cache_key, &complete_audio).await.ok();
                            }
                        }
                        if end_of_stream.unwrap_or_default() {
                            buffer_tx_ref.send(None).ok();
                        }
                    }
                    Ok(SynthesisEvent::Subtitles(subtitles)) => {
                        status_ref
                            .write()
                            .as_mut()
                            .and_then(|status| {
                                status.subtitles.extend(subtitles);
                                Ok(())
                            })
                            .ok();
                    }
                    Err(e) => {
                        synthesis_tasks_ref.fetch_sub(1, Ordering::SeqCst);
                        warn!(session_id, "Error in audio stream chunk: {}", e);
                        event_sender_clone
                            .send(SessionEvent::Error {
                                timestamp: crate::get_timestamp(),
                                track_id: track_id.clone(),
                                sender: format!("tts.{}", provider),
                                error: e.to_string(),
                                code: None,
                            })
                            .ok();
                        audio_chunks.drain(..);
                        buffer_tx_ref.send(None).ok();
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
            track_id, sample_rate, packet_duration_ms, max_pcm_chunk_size, "tts track started"
        );
        let processor_chain = self.processor_chain.clone();
        let session_id = self.session_id.clone();
        let remaining_size = Arc::new(Mutex::new(0usize));
        let remaining_size_ref = Arc::new(Mutex::new(0usize));
        let emit_loop = async move {
            let mut ptimer =
                tokio::time::interval(Duration::from_millis(packet_duration_ms as u64));
            let mut buffer = Vec::new();
            let mut is_recv_finished = false;
            loop {
                select! {
                        _ = ptimer.tick() => {
                                let mut packet = if buffer.len() >= max_pcm_chunk_size {
                                    let packet_samples = buffer.drain(..max_pcm_chunk_size).collect::<Vec<_>>();
                                    AudioFrame {
                                        track_id: track_id.clone(),
                                        samples: Samples::PCM { samples: packet_samples },
                                        timestamp: crate::get_timestamp(),
                                        sample_rate,
                                    }
                                } else {
                                    if is_recv_finished && synthesis_tasks.load(Ordering::SeqCst) <= 0 {
                                        break;
                                    }
                                    AudioFrame {
                                        track_id: track_id.clone(),
                                        samples: Samples::PCM { samples: Vec::new() },
                                        timestamp: crate::get_timestamp(),
                                        sample_rate,
                                    }
                                };
                                *remaining_size_ref.lock().await = buffer.len();
                                // Process the frame with processor chain
                                if let Err(e) = processor_chain.process_frame(&mut packet) {
                                    warn!(track_id, session_id, "error processing frame: {}", e);
                                }
                                // Send the packet
                                match packet_sender.send(packet) {
                                    Ok(_) => {}
                                    Err(_) => {
                                        break; /* Track has been closed */
                                    }
                                }
                        }
                        chunk = buffer_rx.recv() => {
                            match chunk {
                                Some(Some(mut samples)) => {
                                    is_recv_finished = false;
                                    buffer.append(&mut samples);
                                }
                                Some(None) => {
                                    is_recv_finished = true;
                                }
                                None => {
                                    // Channel closed
                                    break;
                                }
                            }
                        }
                }
            }
        };
        let track_id = self.track_id.clone();
        let token = self.cancel_token.clone();
        let session_id = self.session_id.clone();
        let ssrc = self.ssrc;
        let event_sender_clone = event_sender.clone();
        tokio::spawn(async move {
            let start_time = crate::get_timestamp();
            select! {
                _ = command_loop => {
                    info!(session_id, "command loop completed");
                }
                _ = receive_result_loop => {
                    info!(session_id, "receive result loop completed");
                }
                _ = emit_loop => {
                    info!(session_id, "emit loop completed");
                }
                _ = token.cancelled() => {
                    let remaining_size = {*remaining_size.lock().await * 2};
                    let status = match status.write() {
                        Ok(status) => status.clone(),
                        Err(e) => {
                            warn!(session_id, "error writing synthesis status: {}", e);
                            return;
                        }
                    };
                    let total_size = status.total_audio_len;
                    debug!(session_id, "total_size: {} remaining_size: {}", total_size, remaining_size);
                    let sended_size = total_size - remaining_size;
                    let mut position = None;
                    let current = bytes_size_to_duration(sended_size, sample_rate);
                    let total_duration = bytes_size_to_duration(total_size, sample_rate);
                    for subtitle in status.subtitles.iter().rev() {
                        if subtitle.begin_time <= current {
                            position = Some(subtitle.begin_index);
                            break;
                        }
                    }
                    let event = SessionEvent::Interruption {
                        track_id: track_id.clone(),
                        play_id: status.play_id.clone(),
                        timestamp: crate::get_timestamp(),
                        subtitle: Some(status.full_text),
                        position,
                        total_duration,
                        current,
                    };
                   event_sender_clone.send(event).ok();
                }
            }

            let duration = crate::get_timestamp() - start_time;
            info!(session_id, track_id, duration, "tts track ended");
            let play_id = match status.read() {
                Ok(status) => status.play_id.clone(),
                Err(_) => None,
            };
            event_sender_clone
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::get_timestamp(),
                    duration,
                    ssrc,
                    play_id,
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
