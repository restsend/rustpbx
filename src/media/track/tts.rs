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
use futures::{StreamExt, future};
use std::collections::{HashMap, VecDeque};
use tokio::{
    select,
    sync::{Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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

struct TtsTask {
    play_id: Option<String>,
    track_id: TrackId,
    session_id: String,
    client: Box<dyn SynthesisClient>,
    command_rx: SynthesisCommandReceiver,
    event_sender: EventSender,
    packet_sender: TrackPacketSender,
    cancel_token: CancellationToken,
    processor_chain: ProcessorChain,
    cache_enabled: bool,
    sample_rate: u32,
    ptime: Duration,
    cache_buffer: BytesMut,
    emit_q: VecDeque<EmitEntry>,
    chunks_map: HashMap<usize, ChunkEntry>,
    cur_seq: usize,
}

impl TtsTask {
    async fn run(mut self) -> Result<()> {
        // chunks for each seq, in streaming mode, all the chunks are stored in the first vec
        // item: (chunks, last_update, finished)
        let mut stream = self.client.start().await?;
        // seqence number of next tts command
        let mut cmd_seq = 0;
        let mut cmd_finished = false;
        let mut tts_finished = false;
        let sample_rate = self.sample_rate;
        let packet_duration_ms = self.ptime.as_millis();
        let capacity = sample_rate as usize * packet_duration_ms as usize / 500;
        let mut ptimer = tokio::time::interval(self.ptime);
        let mut samples = vec![0u8; capacity];
        while !cmd_finished && !tts_finished || !self.emit_q.is_empty() {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    warn!(self.session_id, self.track_id, "tts task canceled");
                    break;
                }
                _ = ptimer.tick() => {
                    samples.fill(0);
                    let mut i = 0;
                    while i < capacity && !self.emit_q.is_empty() {
                        let first_entry = &mut self.emit_q[0];

                        while i < capacity && !first_entry.chunks.is_empty() {
                            let first_chunk = &mut first_entry.chunks[0];
                            let len = first_chunk.len().min(capacity - i);
                            samples[i..i+len].copy_from_slice(&first_chunk.split_to(len));
                            i += len;
                            if first_chunk.is_empty() {
                                first_entry.chunks.pop_front();
                            }
                        }

                        if first_entry.chunks.is_empty(){
                            if first_entry.finished || first_entry.last_update.elapsed() > self.ptime {
                                self.emit_q.pop_front();
                                self.cur_seq += 1;
                            }else{
                                break;
                            }
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
                        self.handle_cmd(cmd, cmd_seq).await;
                        cmd_seq += 1;
                    }

                    if cmd.is_none() || cmd.unwrap().end_of_stream {
                        cmd_finished = true;
                        self.client.stop().await?;
                    }
                }
                res = stream.next(), if !tts_finished => {
                    if let Some(res) = res {
                        match res {
                            Ok((cmd_seq, event)) => {
                                if self.handle_event(cmd_seq.unwrap_or(0), event).await{
                                    tts_finished = true;
                                }
                            }
                            Err(e) => {
                                warn!(self.session_id, "error receiving event: {}", e);
                            }
                        }
                    }else{
                        tts_finished = true;
                    }
                }
            }
        }
        tracing::debug!(
            "tts task finished, cur_seq: {}, cmd_finished: {}, tts_finished: {}, emit_q lenth: {}",
            self.cur_seq,
            cmd_finished,
            tts_finished,
            self.emit_q.len()
        );
        Ok(())
    }

    // return true if cmd is finished
    async fn handle_cmd(&mut self, cmd: &SynthesisCommand, cmd_seq: usize) {
        let text = &cmd.text;
        if text.is_empty() {
            if let Some(entry) = self.get_emit_entry_mut(cmd_seq) {
                entry.finished = true;
            }
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

    // set cache key for each cmd, try to retrieve cached audio, return true if cached and retrieve succeed
    async fn handle_cache(&mut self, cmd: &SynthesisCommand, cmd_seq: usize) -> bool {
        let cache_key = cache::generate_cache_key(
            &format!("tts:{}{}", self.client.provider(), cmd.text),
            self.sample_rate,
            cmd.option.speaker.as_ref(),
            cmd.option.speed,
        );

        // initial chunks map at cmd_seq for tts to save chunks and get cache key
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
                    debug!(
                        self.session_id,
                        cmd.text, "using cached audio for {}", cache_key
                    );
                    let bytes = self.cache_buffer.split().freeze();
                    let len = bytes.len();

                    // entry should always be some
                    if let Some(emit_entry) = self.get_emit_entry_mut(cmd_seq) {
                        emit_entry.chunks.push_back(bytes);
                        emit_entry.finished = true;
                    }

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

    // return true if cmd is finished, only when on streaming mode and receive finished event
    async fn handle_event(&mut self, cmd_seq: usize, event: SynthesisEvent) -> bool {
        match event {
            SynthesisEvent::AudioChunk(mut chunk) => {
                // entry is some only on streaming mode
                let entry = self.chunks_map.entry(cmd_seq).or_default();

                if entry.first_chunk {
                    // first chunk
                    if chunk.len() > 44 && chunk[..4] == [0x52, 0x49, 0x46, 0x46] {
                        let _ = chunk.split_to(44);
                    }
                }

                if self.cache_enabled {
                    entry.chunks.push(chunk.clone());
                }

                // if entry is none, cmd_seq is skipped because of timeout
                if let Some(emit_entry) = self.get_emit_entry_mut(cmd_seq) {
                    emit_entry.chunks.push_back(chunk.clone());
                }
            }
            SynthesisEvent::Subtitles(..) => {}
            SynthesisEvent::Finished { .. } => {
                if let Some(emit_entry) = self.get_emit_entry_mut(cmd_seq) {
                    emit_entry.finished = true;
                }

                if let Some(entry) = self.chunks_map.remove(&cmd_seq) {
                    // if cache is enabled, cache key setted in handle_cache
                    if self.cache_enabled
                        && !cache::is_cached(&entry.cache_key).await.unwrap_or_default()
                    {
                        if let Err(e) =
                            cache::store_in_cache_v(&entry.cache_key, &entry.chunks).await
                        {
                            warn!(self.session_id, "error storing cached audio: {}", e);
                        }
                    }
                }
                return true;
            }
        }
        false
    }

    // get mutable reference of result at cmd_seq, resize if needed, update the last_update
    fn get_emit_entry_mut(&mut self, cmd_seq: usize) -> Option<&mut EmitEntry> {
        // cmd_seq is skipped because of timeout
        if cmd_seq < self.cur_seq {
            return None;
        }

        let i = cmd_seq - self.cur_seq;
        if i >= self.emit_q.len() {
            self.emit_q.resize_with(i + 1, || EmitEntry {
                chunks: VecDeque::new(),
                finished: false,
                last_update: Instant::now(),
            });
        }
        self.emit_q[i].last_update = Instant::now();
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
        };
        tokio::spawn(task.run());
        Ok(())
        // // command sender is held by tts handle, it will dropped when there is a tts command with new play_id
        // let mut command_rx = self
        //     .command_rx
        //     .lock()
        //     .await
        //     .take()
        //     .ok_or(anyhow!("Command receiver not found"))?;

        // let play_id = self.play_id.clone();
        // let track_id = self.track_id.clone();
        // let session_id = self.session_id.clone();
        // let sample_rate = self.config.samplerate;
        // let streaming = self.streaming;
        // let use_cache = self.use_cache;
        // let cache_enabled = !streaming && use_cache;

        // let mut client = self
        //     .client
        //     .lock()
        //     .await
        //     .take()
        //     .ok_or(anyhow!("Synthesis client not found"))?;

        // let mut stream = client.start().await?;
        // let event_sender_clone = event_sender.clone();

        // // channel for cache,  (seq, chunk)
        // let (cache_tx, mut cache_rx) = mpsc::channel(5);
        // // channel for tts result, (seq, Option<chunk>), None indicates seq is finished
        // let (tts_tx, mut tts_rx) = mpsc::channel(10);

        // let provider_clone = client.provider();
        // let command_loop = async move {
        //     let mut cmd_seq = 0;
        //     let mut end_of_stream = false;
        //     let mut cache_buffer = BytesMut::new();

        //     // TODO: change to if let chain
        //     // !end_of_stream && let Some(command) = command_rx.recv().await
        //     while !end_of_stream {
        //         let res = command_rx.recv().await;
        //         if res.is_none() {
        //             break;
        //         }
        //         let mut command = res.unwrap();

        //         // if end_of_stream, quit in next iteration
        //         end_of_stream = command.end_of_stream;
        //         let text = command.text;

        //         // skip empty text
        //         if text.is_empty() {
        //             continue;
        //         }

        //         if command.option.speaker.is_none() {
        //             command.option.speaker = command.speaker;
        //         }

        //         let cache_key = if cache_enabled {
        //             Some(cache::generate_cache_key(
        //                 &format!("tts:{}{}", provider_clone, text),
        //                 sample_rate,
        //                 command.option.speaker.as_ref(),
        //                 command.option.speed,
        //             ))
        //         } else {
        //             None
        //         };

        //         command.option.cache_key = cache_key.clone();

        //         if let Some(cache_key) = &cache_key {
        //             if let Ok(true) = cache::is_cached(cache_key).await {
        //                 cache_buffer.clear();
        //                 match cache::retrieve_from_cache_with_buffer(cache_key, &mut cache_buffer)
        //                     .await
        //                 {
        //                     Ok(()) => {
        //                         debug!(session_id, text, "using cached audio for {}", cache_key);
        //                         let bytes = cache_buffer.split().freeze();
        //                         let len = bytes.len();
        //                         cache_tx.send((cmd_seq, bytes)).await.ok();
        //                         event_sender_clone
        //                             .send(SessionEvent::Metrics {
        //                                 timestamp: crate::get_timestamp(),
        //                                 key: format!("completed.tts.{}", provider_clone),
        //                                 data: serde_json::json!({
        //                                         "speaker": command.option.speaker,
        //                                         "playId": play_id,
        //                                         "length": len,
        //                                         "cached": true,
        //                                 }),
        //                                 duration: 0,
        //                             })
        //                             .ok();
        //                         cmd_seq += 1;
        //                         continue;
        //                     }
        //                     Err(e) => {
        //                         warn!(session_id, "error retrieving cached audio: {}", e);
        //                     }
        //                 }
        //             }
        //         }
        //         info!(
        //             %provider_clone,
        //             session_id,
        //             text,
        //             play_id,
        //             eos = command.end_of_stream,
        //             streaming = command.streaming,
        //             "synthesizing",
        //         );
        //         if let Err(e) = client
        //             .synthesize(&text, cmd_seq, Some(command.option))
        //             .await
        //         {
        //             warn!(session_id, "error synthesizing text: {}", e);
        //             event_sender_clone
        //                 .send(SessionEvent::Error {
        //                     timestamp: crate::get_timestamp(),
        //                     track_id: track_id.clone(),
        //                     sender: format!("tts.{}", provider_clone),
        //                     error: e.to_string(),
        //                     code: None,
        //                 })
        //                 .ok();
        //             break;
        //         }
        //         cmd_seq += 1;
        //     }

        //     // clean up and waiting emit loop quit
        //     drop(cache_tx);
        //     drop(cache_buffer);
        //     client.stop().await.ok();
        //     future::pending().await
        // };

        // let session_id = self.session_id.clone();
        // let recv_loop = async move {
        //     // chunks for each seq, save for cache if `cache_enabled`
        //     // TO-DO: merge `complete_audio` and `tts_rx`
        //     let mut complete_audio = Vec::<Vec<Bytes>>::new();

        //     // flag for streaming mode, check if it is the first chunk
        //     let mut first_chunk = true;
        //     while let Some(result) = stream.next().await {
        //         match result {
        //             Ok((cmd_seq, event)) => {
        //                 match event {
        //                     SynthesisEvent::AudioChunk(mut audio_chunk) => {
        //                         // for streaming mode seq is None
        //                         let seq = cmd_seq.unwrap_or(0);
        //                         // first chunk
        //                         debug!(cmd_seq, "audio chunk: {}", audio_chunk.len());
        //                         if first_chunk && streaming
        //                             || seq >= complete_audio.len()
        //                             || complete_audio[seq].is_empty()
        //                         {
        //                             if audio_chunk.len() > 44
        //                                 && audio_chunk[..4] == [0x52, 0x49, 0x46, 0x46]
        //                             {
        //                                 let _ = audio_chunk.split_to(44);
        //                             };
        //                             first_chunk = false;
        //                         }

        //                         if cache_enabled {
        //                             if seq >= complete_audio.len() {
        //                                 complete_audio.resize_with(seq + 1, || Vec::new());
        //                             }

        //                             complete_audio[seq].push(audio_chunk.clone());
        //                         }

        //                         if let Err(e) = tts_tx.send((cmd_seq, Some(audio_chunk))).await {
        //                             warn!(session_id, "error sending cached audio: {}", e);
        //                             break;
        //                         }
        //                     }
        //                     SynthesisEvent::Subtitles(..) => {}
        //                     SynthesisEvent::Finished { cache_key } => {
        //                         tts_tx.send((cmd_seq, None)).await.ok();
        //                         if let Some(seq) = cmd_seq {
        //                             if let Some(cache_key) = cache_key {
        //                                 if cache::is_cached(&cache_key).await.unwrap_or(false) {
        //                                     let chunks = complete_audio[seq].as_slice();
        //                                     debug!(
        //                                         session_id,
        //                                         "storing cached audio: key: {}, len: {}",
        //                                         cache_key,
        //                                         chunks.len()
        //                                     );
        //                                     if let Err(e) =
        //                                         cache::store_in_cache_v(&cache_key, chunks).await
        //                                     {
        //                                         warn!(
        //                                             session_id,
        //                                             "error storing cached audio: {}", e
        //                                         );
        //                                     }
        //                                 }
        //                             }
        //                             complete_audio[seq].clear();
        //                         }
        //                     }
        //                 }
        //             }
        //             Err(e) => {
        //                 warn!("error receiving tts result: {}", e);
        //                 break;
        //             }
        //         }
        //     }

        //     drop(tts_tx);
        //     drop(complete_audio);
        //     // waiting emit loop quit
        //     future::pending().await
        // };

        // let sample_rate = self.config.samplerate;
        // let track_id = self.track_id.clone();
        // let packet_duration_ms = self.config.ptime.as_millis();
        // // audio frame in bytes size
        // let capacity = sample_rate as usize * packet_duration_ms as usize / 500;
        // let processor_chain = self.processor_chain.clone();
        // let session_id = self.session_id.clone();
        // let emit_loop = async move {
        //     let mut ptimer = tokio::time::interval_at(
        //         Instant::now() + Duration::from_millis(200),
        //         Duration::from_millis(packet_duration_ms as u64),
        //     );
        //     let mut cmd_finished = !cache_enabled;
        //     let mut tts_finished = false;
        //     // current processing seq
        //     let mut cur_seq = 0;
        //     // in streaming mode, all the chunks stored in the first deque
        //     // (deque, finished), deque save all the chunks for the seq, finished is true if all the chunks are received
        //     let mut chunks = VecDeque::<(VecDeque<Bytes>, bool)>::new();

        //     // buffer for sample, if data is not enough, rest of the capacity is filled with 0
        //     let mut samples = vec![0u8; capacity];
        //     while !cmd_finished || !tts_finished || !chunks.is_empty() {
        //         tokio::select! {
        //             biased;
        //             _ = ptimer.tick() => {
        //                 let mut i = 0;
        //                 samples.fill(0);
        //                 while i < capacity && !chunks.is_empty() {
        //                     let (dq, finished) = &mut chunks[0];
        //                     if dq.is_empty() && !*finished {
        //                         break;
        //                     }
        //                     while i < capacity && !dq.is_empty() {
        //                         let first = &mut dq[0];
        //                         let len = first.len().min(capacity - i);
        //                         samples[i..i+len].copy_from_slice(&first[..len]);
        //                         i += len;
        //                         let _ = first.split_to(len);
        //                         if first.is_empty() {
        //                             dq.pop_front();
        //                         }
        //                     }

        //                     if *finished && dq.is_empty(){
        //                         debug!("TTS track, cmd seq: {} finished", cur_seq);
        //                         chunks.pop_front();
        //                         cur_seq += 1;
        //                     }
        //                 }

        //                 let samples = bytes_to_samples(&samples[..]);
        //                 let samples = Samples::PCM{
        //                     samples,
        //                 };
        //                 let mut frame = AudioFrame {
        //                     track_id: track_id.clone(),
        //                     samples,
        //                     timestamp: crate::get_timestamp(),
        //                     sample_rate,
        //                 };
        //                 if let Err(e) = processor_chain.process_frame(&mut frame) {
        //                     warn!(track_id, session_id, "error processing frame: {}", e);
        //                     break;
        //                 }

        //                 if let Err(_) = packet_sender.send(frame) {
        //                     warn!(track_id, session_id, "track already closed");
        //                     break;
        //                 }
        //             },
        //             res = cache_rx.recv() ,if !cmd_finished => {
        //                 if let Some((seq, chunk)) = res {
        //                     if seq >= cur_seq + chunks.len(){
        //                         chunks.resize(seq + 1 - cur_seq, (VecDeque::new(), false));
        //                     }
        //                     let (deq, finished) = &mut chunks[seq];
        //                     debug!(seq, len = chunk.len());
        //                     deq.push_back(chunk);
        //                     *finished = true;
        //                 }else{
        //                     cmd_finished = true;
        //                 }
        //             },
        //             res = tts_rx.recv() , if !tts_finished => {
        //                 if let Some((seq, chunk)) = res {
        //                     tracing::debug!(seq, chunk = chunk.as_ref().map(|c| c.len()).unwrap_or(0));
        //                     // for streaming mode all the chunks stored in the 0 index
        //                     let seq = seq.unwrap_or(0);
        //                     if seq >= cur_seq + chunks.len(){
        //                         chunks.resize(seq + 1 - cur_seq, (VecDeque::new(), false));
        //                     }

        //                     let (deq, finished) = &mut chunks[seq];
        //                     if let Some(chunk) = chunk {
        //                         deq.push_back(chunk);
        //                     }else{
        //                         *finished = true;
        //                     }

        //                 }else{
        //                     tts_finished = true;
        //                 }
        //             }
        //         }
        //     }
        // };

        // let track_id = self.track_id.clone();
        // let token = self.cancel_token.clone();
        // let session_id = self.session_id.clone();
        // let ssrc = self.ssrc;
        // let event_sender_clone = event_sender.clone();
        // tokio::spawn(async move {
        //     let start_time = crate::get_timestamp();
        //     select! {
        //         biased;
        //         _ = token.cancelled() => {
        //            info!(session_id, "tts track canceled");
        //         }
        //         _ = emit_loop => {
        //             info!(session_id, "emit loop completed");
        //         }
        //         _ = recv_loop => {}
        //         _ = command_loop => {}
        //     }

        //     let duration = crate::get_timestamp() - start_time;
        //     info!(session_id, track_id, duration, "tts track ended");
        //     let play_id = Some("play".to_string());
        //     event_sender_clone
        //         .send(SessionEvent::TrackEnd {
        //             track_id,
        //             timestamp: crate::get_timestamp(),
        //             duration,
        //             ssrc,
        //             play_id,
        //         })
        //         .ok();
        // });
        // Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}
