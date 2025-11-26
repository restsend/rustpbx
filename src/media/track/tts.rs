use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::Samples,
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
use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct SynthesisHandle {
    pub play_id: Option<String>,
    pub command_tx: SynthesisCommandSender,
}

struct EmitEntry {
    chunks: VecDeque<Bytes>,
    finished: bool,
    finish_at: Instant,
}

struct Metadata {
    cache_key: String,
    text: String,
    first_chunk: bool,
    chunks: Vec<Bytes>,
    subtitles: Vec<Subtitle>,
    total_bytes: usize,
    emitted_bytes: usize,
    recv_time: u64,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            cache_key: String::new(),
            text: String::new(),
            chunks: Vec::new(),
            first_chunk: true,
            subtitles: Vec::new(),
            total_bytes: 0,
            emitted_bytes: 0,
            recv_time: 0,
        }
    }
}

// Synthesis task for TTS track, handle tts command and synthesis event emit audio chunk to media stream
struct TtsTask {
    ssrc: u32,
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
    // metadatas for each tts command
    metadatas: HashMap<usize, Metadata>,
    // seq of current progressing tts command, ignore result from cmd_seq less than cur_seq
    cur_seq: usize,
    streaming: bool,
    graceful: Arc<AtomicBool>,
}

impl TtsTask {
    async fn run(mut self) -> Result<()> {
        let mut stream;
        match self.client.start().await {
            Ok(s) => stream = s,
            Err(e) => {
                error!(
                    session_id = %self.session_id,
                    track_id = %self.track_id,
                    play_id = ?self.play_id,
                    provider = %self.client.provider(),
                    error = %e,
                    "failed to start tts task"
                );
                return Err(e);
            }
        };

        info!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            play_id = ?self.play_id,
            streaming = self.streaming,
            provider = %self.client.provider(),
            "tts task started"
        );
        let start_time = crate::media::get_timestamp();
        // seqence number of next tts command in stream, used for non streaming mode
        let mut cmd_seq = if self.streaming { None } else { Some(0) };
        let mut cmd_finished = false;
        let mut tts_finished = false;
        let mut cancel_received = false;
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
                _ = self.cancel_token.cancelled(), if !cancel_received => {
                    cancel_received = true;
                    let graceful = self.graceful.load(Ordering::Relaxed);
                    let emitted_bytes = self.metadatas.get(&self.cur_seq).map(|entry| entry.emitted_bytes).unwrap_or(0);
                    let total_bytes = self.metadatas.get(&self.cur_seq).map(|entry| entry.total_bytes).unwrap_or(0);
                    debug!(
                        session_id = %self.session_id,
                        track_id = %self.track_id,
                        play_id = ?self.play_id,
                        cur_seq = self.cur_seq,
                        emitted_bytes,
                        total_bytes,
                        graceful,
                        streaming = self.streaming,
                        "tts task cancelled"
                    );
                    self.handle_interrupt();
                    // quit if on streaming mode (graceful only work for non streaming mode)
                    //         or graceful not set, this is ordinary cancel
                    //         or cur seq not started
                    if self.streaming || !graceful || emitted_bytes == 0 {
                        break;
                    }

                    // else, stop receiving command
                    cmd_finished = true;
                    self.client.stop().await?;
                }
                _ = ptimer.tick() => {
                    samples.fill(0);
                    let mut i = 0;
                    // fill samples until it's full or there are no more chunks to emit or current seq is not finished
                    while i < capacity && !self.emit_q.is_empty(){
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
                            self.metadatas.get_mut(&self.cur_seq).map(|entry| {
                                entry.emitted_bytes += len;
                            });
                            if first_chunk.is_empty() {
                                first_entry.chunks.pop_front();
                            }
                        }

                        if first_entry.chunks.is_empty(){
                            let elapsed = first_entry.finish_at.elapsed();
                            if self.streaming && cmd_finished && (tts_finished || elapsed > Duration::from_secs(10)) {
                                debug!(
                                    session_id = %self.session_id,
                                    track_id = %self.track_id,
                                    play_id = ?self.play_id,
                                    tts_finished,
                                    elapsed_ms = elapsed.as_millis(),
                                    "tts streaming finished"
                                );
                                tts_finished = true;
                                self.emit_q.clear();
                                continue;
                            }

                            if !self.streaming && (first_entry.finished || elapsed > Duration::from_secs(3))
                            {
                                debug!(
                                    session_id = %self.session_id,
                                    track_id = %self.track_id,
                                    play_id = ?self.play_id,
                                    cur_seq = self.cur_seq,
                                    entry_finished = first_entry.finished,
                                    elapsed_ms = elapsed.as_millis(),
                                    "tts entry finished"
                                );

                                self.emit_q.pop_front();
                                self.cur_seq += 1;

                                // if passage is set, clearn emit_q, task will quit at next iteration
                                if self.graceful.load(Ordering::Relaxed) {
                                    self.emit_q.clear();
                                }

                                // else, continue process next seq
                                continue;
                            }

                            // current seq not finished, but have no more data to emit
                            break;
                        }
                    }

                    // waiting for first chunk
                    if i == 0 && self.cur_seq == 0 && self.metadatas.get(&self.cur_seq).map(|entry| entry.emitted_bytes).unwrap_or(0) == 0 {
                        continue;
                    }

                    let samples = Samples::PCM{
                        samples: bytes_to_samples(&samples[..]),
                    };

                    let mut frame = AudioFrame {
                        track_id: self.track_id.clone(),
                        samples,
                        timestamp: crate::media::get_timestamp(),
                        sample_rate,
                    };

                    if let Err(e) = self.processor_chain.process_frame(&mut frame) {
                        warn!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            error = %e,
                            "error processing audio frame"
                        );
                        break;
                    }

                    if let Err(_) = self.packet_sender.send(frame) {
                        warn!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            "track packet sender closed, stopping task"
                        );
                        break;
                    }
                }
                cmd = self.command_rx.recv(), if !cmd_finished => {
                    if let Some(cmd) = cmd.as_ref() {
                        self.handle_cmd(cmd, cmd_seq).await;
                        cmd_seq.as_mut().map(|seq| *seq += 1);
                    }

                    // set finished if command sender is exhausted or end_of_stream is true
                    if cmd.is_none() || cmd.as_ref().map(|c| c.end_of_stream).unwrap_or(false) {
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            cmd_seq = ?cmd_seq,
                            end_of_stream = cmd.as_ref().map(|c| c.end_of_stream).unwrap_or(true),
                            "tts command finished"
                        );
                        cmd_finished = true;
                        self.client.stop().await?;
                    }
                }
                item = stream.next(), if !tts_finished => {
                    if let Some((cmd_seq, res)) = item {
                        self.handle_event(cmd_seq, res).await
                    }else{
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            "tts event stream finished"
                        );
                        tts_finished = true;
                    }
                }
            }
        }

        let (emitted_bytes, total_bytes) = self.metadatas.values().fold((0, 0), |(a, b), entry| {
            (a + entry.emitted_bytes, b + entry.total_bytes)
        });

        let duration_ms = (crate::media::get_timestamp() - start_time) as f64 / 1000.0;
        info!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            play_id = ?self.play_id,
            cur_seq = self.cur_seq,
            cmd_seq = ?cmd_seq,
            cmd_finished,
            tts_finished,
            streaming = self.streaming,
            emitted_bytes,
            total_bytes,
            duration_ms,
            provider = %self.client.provider(),
            "tts task finished"
        );

        self.event_sender
            .send(SessionEvent::TrackEnd {
                track_id: self.track_id.clone(),
                timestamp: crate::media::get_timestamp(),
                duration: crate::media::get_timestamp() - start_time,
                ssrc: self.ssrc,
                play_id: self.play_id.clone(),
            })
            .ok();
        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: &SynthesisCommand, cmd_seq: Option<usize>) {
        let session_id = self.session_id.clone();
        let track_id = self.track_id.clone();
        let play_id = self.play_id.clone();
        let streaming = self.streaming;
        debug!(
            session_id = %session_id,
            track_id = %self.track_id,
            play_id = ?play_id,
            cmd_seq = ?cmd_seq,
            text_preview = %cmd.text.chars().take(20).collect::<String>(),
            text_length = cmd.text.len(),
            base64 = cmd.base64,
            end_of_stream = cmd.end_of_stream,
            "tts track: received command"
        );
        let text = &cmd.text;

        // if cmd_seq is None(streaming mode), all metadata save into index 0
        let assume_seq = cmd_seq.unwrap_or(0);
        let meta_entry = self.metadatas.entry(assume_seq).or_default();
        meta_entry.text = text.clone();
        meta_entry.recv_time = crate::media::get_timestamp();

        let emit_entry = self.get_emit_entry_mut(assume_seq);

        // if text is empty:
        // in streaming mode, skip it
        // in non streaming mode, set entry[seq] finished to true
        if text.is_empty() {
            if !streaming {
                emit_entry.map(|entry| entry.finished = true);
            }
            return;
        }

        if cmd.base64 {
            match BASE64_STANDARD.decode(text) {
                Ok(bytes) => {
                    emit_entry.map(|entry| {
                        entry.chunks.push_back(Bytes::from(bytes));
                        entry.finished = true;
                    });
                }
                Err(e) => {
                    warn!(
                        session_id = %session_id,
                        track_id = %track_id,
                        play_id = ?play_id,
                        cmd_seq = ?cmd_seq,
                        error = %e,
                        "failed to decode base64 text"
                    );
                    emit_entry.map(|entry| entry.finished = true);
                }
            }
            return;
        }

        if self.cache_enabled && self.handle_cache(&cmd, assume_seq).await {
            return;
        }

        if let Err(e) = self
            .client
            .synthesize(&text, cmd_seq, Some(cmd.option.clone()))
            .await
        {
            warn!(
                session_id = %session_id,
                track_id = %track_id,
                play_id = ?play_id,
                cmd_seq = ?cmd_seq,
                text_length = text.len(),
                provider = %self.client.provider(),
                error = %e,
                "failed to synthesize text"
            );
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
        self.metadatas.get_mut(&cmd_seq).map(|entry| {
            entry.cache_key = cache_key.clone();
        });

        if cache::is_cached(&cache_key).await.unwrap_or_default() {
            match cache::retrieve_from_cache_with_buffer(&cache_key, &mut self.cache_buffer).await {
                Ok(()) => {
                    debug!(
                        session_id = %self.session_id,
                        track_id = %self.track_id,
                        play_id = ?self.play_id,
                        cmd_seq,
                        cache_key = %cache_key,
                        text_preview = %cmd.text.chars().take(20).collect::<String>(),
                        "using cached audio"
                    );
                    let bytes = self.cache_buffer.split().freeze();
                    let len = bytes.len();

                    self.get_emit_entry_mut(cmd_seq).map(|entry| {
                        entry.chunks.push_back(bytes);
                        entry.finished = true;
                    });

                    self.event_sender
                        .send(SessionEvent::Metrics {
                            timestamp: crate::media::get_timestamp(),
                            key: format!("completed.tts.{}", self.client.provider()),
                            data: serde_json::json!({
                                    "speaker": cmd.option.speaker,
                                    "playId": self.play_id,
                                    "cmdSeq": cmd_seq,
                                    "length": len,
                                    "cached": true,
                            }),
                            duration: 0,
                        })
                        .ok();
                    return true;
                }
                Err(e) => {
                    warn!(
                        session_id = %self.session_id,
                        track_id = %self.track_id,
                        play_id = ?self.play_id,
                        cmd_seq,
                        cache_key = %cache_key,
                        error = %e,
                        "error retrieving cached audio"
                    );
                }
            }
        }
        false
    }

    async fn handle_event(&mut self, cmd_seq: Option<usize>, event: Result<SynthesisEvent>) {
        let assume_seq = cmd_seq.unwrap_or(0);
        match event {
            Ok(SynthesisEvent::AudioChunk(mut chunk)) => {
                let entry = self.metadatas.entry(assume_seq).or_default();

                if entry.first_chunk {
                    // first chunk
                    if chunk.len() > 44 && chunk[..4] == [0x52, 0x49, 0x46, 0x46] {
                        let _ = chunk.split_to(44);
                    }
                    entry.first_chunk = false;
                }

                entry.total_bytes += chunk.len();

                // if cache is enabled, save complete chunks for caching
                if self.cache_enabled {
                    entry.chunks.push(chunk.clone());
                }

                let duration = Duration::from_millis(bytes_size_to_duration(
                    chunk.len(),
                    self.sample_rate,
                ) as u64);
                self.get_emit_entry_mut(assume_seq).map(|entry| {
                    entry.chunks.push_back(chunk.clone());
                    entry.finish_at += duration;
                });
            }
            Ok(SynthesisEvent::Subtitles(subtitles)) => {
                self.metadatas.get_mut(&assume_seq).map(|entry| {
                    entry.subtitles.extend(subtitles);
                });
            }
            Ok(SynthesisEvent::Finished) => {
                let entry = self.metadatas.entry(assume_seq).or_default();
                debug!(
                    session_id = %self.session_id,
                    track_id = %self.track_id,
                    play_id = ?self.play_id,
                    cmd_seq = ?cmd_seq,
                    streaming = self.streaming,
                    total_bytes = entry.total_bytes,
                    "tts synthesis completed for command sequence"
                );
                self.event_sender
                    .send(SessionEvent::Metrics {
                        timestamp: crate::media::get_timestamp(),
                        key: format!("completed.tts.{}", self.client.provider()),
                        data: serde_json::json!({
                                "playId": self.play_id,
                                "cmdSeq": cmd_seq,
                                "length": entry.total_bytes,
                                "cached": false,
                        }),
                        duration: (crate::media::get_timestamp() - entry.recv_time) as u32,
                    })
                    .ok();

                // streaming mode use tts_finished to indicate task finished
                if self.streaming {
                    return;
                }

                // if cache is enabled, cache key set by handle_cache
                if self.cache_enabled
                    && !cache::is_cached(&entry.cache_key).await.unwrap_or_default()
                {
                    if let Err(e) =
                        cache::store_in_cache_vectored(&entry.cache_key, &entry.chunks).await
                    {
                        warn!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            cmd_seq = ?cmd_seq,
                            cache_key = %entry.cache_key,
                            error = %e,
                            "failed to store audio in cache"
                        );
                    } else {
                        debug!(
                            session_id = %self.session_id,
                            track_id = %self.track_id,
                            play_id = ?self.play_id,
                            cmd_seq = ?cmd_seq,
                            cache_key = %entry.cache_key,
                            total_bytes = entry.total_bytes,
                            "stored audio in cache"
                        );
                    }
                    entry.chunks.clear();
                }

                self.get_emit_entry_mut(assume_seq)
                    .map(|entry| entry.finished = true);
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    track_id = %self.track_id,
                    play_id = ?self.play_id,
                    cmd_seq = ?cmd_seq,
                    error = %e,
                    "tts synthesis event error"
                );
                // set finished to true if cmd_seq failed
                self.get_emit_entry_mut(assume_seq)
                    .map(|entry| entry.finished = true);
            }
        }
    }

    // get mutable reference of result at cmd_seq, resize if needed, update the last_update
    // if cmd_seq is less than cur_seq, return none
    fn get_emit_entry_mut(&mut self, cmd_seq: usize) -> Option<&mut EmitEntry> {
        // ignore if cmd_seq is less than cur_seq
        if cmd_seq < self.cur_seq {
            debug!(
                session_id = %self.session_id,
                track_id = %self.track_id,
                play_id = ?self.play_id,
                cmd_seq,
                cur_seq = self.cur_seq,
                "ignoring timeout tts result"
            );
            return None;
        }

        // resize emit_q if needed
        let i = cmd_seq - self.cur_seq;
        if i >= self.emit_q.len() {
            self.emit_q.resize_with(i + 1, || EmitEntry {
                chunks: VecDeque::new(),
                finished: false,
                finish_at: Instant::now(),
            });
        }
        Some(&mut self.emit_q[i])
    }

    fn handle_interrupt(&self) {
        if let Some(entry) = self.metadatas.get(&self.cur_seq) {
            let current = bytes_size_to_duration(entry.emitted_bytes, self.sample_rate);
            let total_duration = bytes_size_to_duration(entry.total_bytes, self.sample_rate);
            let text = entry.text.clone();
            let mut position = None;

            for subtitle in entry.subtitles.iter().rev() {
                if subtitle.begin_time < current {
                    position = Some(subtitle.begin_index);
                    break;
                }
            }

            let interruption = SessionEvent::Interruption {
                track_id: self.track_id.clone(),
                timestamp: crate::media::get_timestamp(),
                play_id: self.play_id.clone(),
                subtitle: Some(text),
                position,
                total_duration,
                current,
            };
            self.event_sender.send(interruption).ok();
        }
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
    graceful: Arc<AtomicBool>,
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
            graceful: Arc::new(AtomicBool::new(false)),
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
    async fn update_remote_description(&mut self, _answer: &String) -> Result<()> {
        Ok(())
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
            metadatas: HashMap::new(),
            cur_seq: 0,
            streaming: self.streaming,
            graceful: self.graceful.clone(),
            ssrc: self.ssrc,
        };
        debug!(
            session_id = %self.session_id,
            track_id = %self.track_id,
            play_id = ?self.play_id,
            streaming = self.streaming,
            "spawning tts task"
        );
        tokio::spawn(async move { task.run().await });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn stop_graceful(&self) -> Result<()> {
        self.graceful.store(true, Ordering::Relaxed);
        self.stop().await
    }

    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}
