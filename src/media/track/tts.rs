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
        SynthesisClient, SynthesisCommand, SynthesisCommandReceiver, SynthesisCommandSender,
        SynthesisEvent,
    },
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use std::collections::{HashMap, VecDeque};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub struct SynthesisHandle {
    pub play_id: Option<String>,
    pub command_tx: SynthesisCommandSender,
}

struct EmitEntry {
    chunks: VecDeque<Bytes>,
    finished: bool,
    last_update: Instant,
}

struct ChunkEntry {
    cache_key: String,
    chunks: Vec<Bytes>,
    first_chunk: bool,
}

impl Default for ChunkEntry {
    fn default() -> Self {
        Self {
            cache_key: String::new(),
            chunks: Vec::new(),
            first_chunk: true,
        }
    }
}

// Synthesis task for TTS track, handle tts command and synthesis event emit audio chunk to media stream
struct TtsTask {
    play_id: Option<String>,
    track_id: TrackId,
    session_id: String,
    client: Box<dyn SynthesisClient>,
    command_rx: SynthesisCommandReceiver,
    packet_sender: TrackPacketSender,
    event_sender: EventSender,
    cancel_token: CancellationToken,
    processor_chain: ProcessorChain,
    cache_enabled: bool,
    sample_rate: u32,
    ptime: Duration,
    cache_buffer: BytesMut,
    emit_q: VecDeque<EmitEntry>,
    chunks_map: HashMap<usize, ChunkEntry>,
    // seq of current progressing tts command, ignore result from cmd_seq less than cur_seq
    cur_seq: usize,
    streaming: bool,
    // timeout for each tts command, ignore if last update is more than timeout
    timeout: Duration,
}

impl TtsTask {
    async fn run(mut self) -> Result<()> {
        let mut stream = self.client.start().await?;
        // seqence number of next tts command in stream, used for non streaming mode
        let mut cmd_seq = 0;
        let mut cmd_finished = false;
        let mut tts_finished = false;
        let sample_rate = self.sample_rate;
        let packet_duration_ms = self.ptime.as_millis();
        // capacity of samples buffer
        let capacity = sample_rate as usize * packet_duration_ms as usize / 500;
        let mut ptimer = tokio::time::interval(self.ptime);
        // samples buffer, emit all even if it was not fully filled
        let mut samples = vec![0u8; capacity];

        // quit if cmd is finished, tts is finished and all the chunks are emitted
        while !cmd_finished || !tts_finished || !self.emit_q.is_empty() {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    warn!(self.session_id, self.play_id, "tts task canceled, {} cmd not completed", cmd_seq - self.cur_seq);
                    break;
                }
                _ = ptimer.tick() => {
                    samples.fill(0);
                    let mut i = 0;
                    // fill samples until it's full or there are no more chunks to emit or current seq is not finished
                    while i < capacity && !self.emit_q.is_empty() {
                        // first entry is cur_seq
                        let first_entry = &mut self.emit_q[0];

                        // process each chunks
                        while i < capacity && !first_entry.chunks.is_empty() {
                            let first_chunk = &mut first_entry.chunks[0];
                            let remaining = capacity - i;
                            let available = first_chunk.len();
                            let len = usize::min(remaining, available);
                            let cut = first_chunk.split_to(len);
                            samples[i..i+len].copy_from_slice(&cut);
                            i += len;
                            if first_chunk.is_empty() {
                                first_entry.chunks.pop_front();
                            }
                        }

                        if first_entry.chunks.is_empty(){
                            // if finished or timeout, skip it
                            if first_entry.finished || first_entry.last_update.elapsed() > self.timeout{
                                self.emit_q.pop_front();
                                self.cur_seq += 1;
                                continue;
                            }

                            break;
                        }
                    }

                    let samples = Samples::PCM{
                        samples: bytes_to_samples(&samples[..]),
                    };

                    let mut frame = AudioFrame {
                        track_id: self.track_id.clone(),
                        samples,
                        timestamp: crate::get_timestamp(),
                        sample_rate,
                    };

                    if let Err(e) = self.processor_chain.process_frame(&mut frame) {
                        warn!(self.track_id, self.session_id, "error processing frame: {}", e);
                        break;
                    }

                    if let Err(_) = self.packet_sender.send(frame) {
                        warn!(self.track_id, self.session_id, "track already closed");
                        break;
                    }
                }
                cmd = self.command_rx.recv(), if !cmd_finished => {
                    if let Some(cmd) = cmd.as_ref() {
                        tracing::debug!(self.session_id, self.track_id, "received cmd seq: {}, text: {}, end_of_stream: {}", cmd_seq, cmd.text, cmd.end_of_stream);
                        self.handle_cmd(cmd, cmd_seq).await;
                        cmd_seq += 1;
                    }

                    // set finished if command sender is exhausted or end_of_stream is true
                    if cmd.is_none() || cmd.unwrap().end_of_stream {
                        cmd_finished = true;
                        self.client.stop().await?;
                    }
                }
                item = stream.next(), if !tts_finished => {
                    if let Some((cmd_seq, res)) = item {
                        let cmd_seq = cmd_seq.unwrap_or(0);
                        tts_finished = self.handle_event(cmd_seq, res).await;
                    }else{
                        tts_finished = true;
                    }
                }
            }
        }

        tracing::info!(
            "tts task finished, cur_seq: {}, cmd_finished: {}, tts_finished: {}, emit_q lenth: {}",
            self.cur_seq,
            cmd_finished,
            tts_finished,
            self.emit_q.len()
        );
        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: &SynthesisCommand, cmd_seq: usize) {
        let text = &cmd.text;
        if text.is_empty() {
            self.get_emit_entry_mut(cmd_seq)
                .map(|entry| entry.finished = true);
            return;
        }

        if self.cache_enabled && self.handle_cache(&cmd, cmd_seq).await {
            return;
        }

        if let Err(e) = self
            .client
            .synthesize(&text, cmd_seq, Some(cmd.option.clone()))
            .await
        {
            warn!(self.session_id, "failed to synthesize text: {}", e);
        }
    }

    // set cache key for each cmd, return true if cached and retrieve succeed
    async fn handle_cache(&mut self, cmd: &SynthesisCommand, cmd_seq: usize) -> bool {
        let cache_key = cache::generate_cache_key(
            &format!("tts:{}{}", self.client.provider(), cmd.text),
            self.sample_rate,
            cmd.option.speaker.as_ref(),
            cmd.option.speed,
        );

        // initial chunks map at cmd_seq for tts to save chunks
        self.chunks_map.insert(
            cmd_seq,
            ChunkEntry {
                cache_key: cache_key.clone(),
                chunks: Vec::new(),
                first_chunk: true,
            },
        );

        if cache::is_cached(&cache_key).await.unwrap_or_default() {
            match cache::retrieve_from_cache_with_buffer(&cache_key, &mut self.cache_buffer).await {
                Ok(()) => {
                    tracing::debug!(
                        self.session_id,
                        self.play_id,
                        cmd_seq,
                        cmd.text,
                        "using cached audio for {}",
                        cache_key
                    );
                    let bytes = self.cache_buffer.split().freeze();
                    let len = bytes.len();

                    self.get_emit_entry_mut(cmd_seq).map(|entry| {
                        entry.chunks.push_back(bytes);
                        entry.finished = true;
                    });

                    self.event_sender
                        .send(SessionEvent::Metrics {
                            timestamp: crate::get_timestamp(),
                            key: format!("completed.tts.{}", self.client.provider()),
                            data: serde_json::json!({
                                    "speaker": cmd.option.speaker,
                                    "playId": self.play_id,
                                    "length": len,
                                    "cached": true,
                            }),
                            duration: 0,
                        })
                        .ok();
                    return true;
                }
                Err(e) => {
                    warn!(self.session_id, "error retrieving cached audio: {}", e);
                }
            }
        }
        false
    }

    // return true if on streaming moden and receive finished event
    async fn handle_event(&mut self, cmd_seq: usize, event: Result<SynthesisEvent>) -> bool {
        match event {
            Ok(SynthesisEvent::AudioChunk(mut chunk)) => {
                let entry = self.chunks_map.entry(cmd_seq).or_default();

                if entry.first_chunk {
                    // first chunk
                    if chunk.len() > 44 && chunk[..4] == [0x52, 0x49, 0x46, 0x46] {
                        let _ = chunk.split_to(44);
                    }
                }

                // if cache is enabled, save complete chunks for caching
                if self.cache_enabled {
                    entry.chunks.push(chunk.clone());
                }

                self.get_emit_entry_mut(cmd_seq).map(|entry| {
                    entry.chunks.push_back(chunk.clone());
                    entry.last_update = Instant::now();
                });
            }
            Ok(SynthesisEvent::Subtitles(..)) => {
                // to-do: handle subtitles
            }
            Ok(SynthesisEvent::Finished) => {
                tracing::debug!(
                    self.session_id,
                    self.track_id,
                    "tts result of cmd seq: {} completely received",
                    cmd_seq
                );
                self.get_emit_entry_mut(cmd_seq)
                    .map(|entry| entry.finished = true);

                if let Some(entry) = self.chunks_map.remove(&cmd_seq) {
                    // if cache is enabled, cache key set by handle_cache
                    if self.cache_enabled
                        && !cache::is_cached(&entry.cache_key).await.unwrap_or_default()
                    {
                        if let Err(e) =
                            cache::store_in_cache_vectored(&entry.cache_key, &entry.chunks).await
                        {
                            warn!(self.session_id, "failed to store cached audio: {}", e);
                        }
                    }
                }
                // return true only if on streaming mode
                return self.streaming;
            }
            Err(e) => {
                warn!(self.session_id, cmd_seq, "error receiving event: {}", e);
                // set finished to true if cmd_seq failed
                self.get_emit_entry_mut(cmd_seq)
                    .map(|entry| entry.finished = true);
            }
        }
        false
    }

    // get mutable reference of result at cmd_seq, resize if needed, update the last_update
    // if cmd_seq is less than cur_seq, return none
    fn get_emit_entry_mut(&mut self, cmd_seq: usize) -> Option<&mut EmitEntry> {
        // ignore if cmd_seq is less than cur_seq
        if cmd_seq < self.cur_seq {
            tracing::warn!(
                "TTS result is ignored, cmd_seq {}, cur_seq: {}",
                cmd_seq,
                self.cur_seq
            );
            return None;
        }

        // resize emit_q if needed
        let i = cmd_seq - self.cur_seq;
        if i >= self.emit_q.len() {
            self.emit_q.resize_with(i + 1, || EmitEntry {
                chunks: VecDeque::new(),
                finished: false,
                last_update: Instant::now(),
            });
        }
        Some(&mut self.emit_q[i])
    }
}

pub struct TtsTrack {
    track_id: TrackId,
    session_id: String,
    streaming: bool,
    play_id: Option<String>,
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
        streaming: bool,
        play_id: Option<String>,
        command_rx: SynthesisCommandReceiver,
        client: Box<dyn SynthesisClient>,
    ) -> Self {
        let config = TrackConfig::default();
        Self {
            track_id,
            session_id,
            streaming,
            play_id,
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
        let client = self
            .client
            .lock()
            .await
            .take()
            .ok_or(anyhow!("Client not found"))?;
        let command_rx = self
            .command_rx
            .lock()
            .await
            .take()
            .ok_or(anyhow!("Command receiver not found"))?;

        let task = TtsTask {
            play_id: self.play_id.clone(),
            track_id: self.track_id.clone(),
            session_id: self.session_id.clone(),
            client,
            command_rx,
            event_sender,
            packet_sender,
            cancel_token: self.cancel_token.clone(),
            processor_chain: self.processor_chain.clone(),
            cache_enabled: self.use_cache && !self.streaming,
            sample_rate: self.config.samplerate,
            ptime: self.config.ptime,
            cache_buffer: BytesMut::new(),
            emit_q: VecDeque::new(),
            chunks_map: HashMap::new(),
            cur_seq: 0,
            streaming: self.streaming,
            timeout: Duration::from_secs(2),
        };
        tracing::debug!(self.session_id, self.track_id, "tts task started");
        tokio::spawn(async move { task.run().await });
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
