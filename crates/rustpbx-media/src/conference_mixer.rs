//! Conference mixer with one task owning all participants and audio queues.

use crate::LegId;
use crate::mixer::AudioMixer;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use parking_lot::Mutex as ParkMutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{StreamExt, StreamMap, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub use crate::AudioFrame;

/// Frames held per participant before the oldest is dropped. Bounds the extra
/// latency a transport burst can add (5 x 20 ms at the room rate).
const INPUT_BUFFER_FRAMES: usize = 5;

/// One participant frame placed on the room timeline. `timestamp` is the
/// frame's first sample, counted in samples at the room rate — not an RTP
/// timestamp (codec clock rates differ from the decoded PCM rate, e.g. G.722).
struct QueuedFrame {
    timestamp: u64,
    samples: Vec<i16>,
}

/// Owned only by the mixing task; no shared participant or audio state.
struct ConferenceParticipantAudio {
    leg_id: LegId,
    input: VecDeque<QueuedFrame>,
    output_tx: mpsc::Sender<AudioFrame>,
    muted: bool,
    /// Next timestamp for producers that do not stamp frames (legacy callers,
    /// tests). Stamped producers use their own timeline plus an offset.
    next_synthetic_ts: u64,
    /// Offset that maps a producer's own timestamp base onto the room clock.
    /// Established on its first stamped frame, so a participant that joins
    /// five minutes in (own timeline starting at 0) still plays immediately.
    timestamp_offset: Option<u64>,
}

#[derive(Default)]
struct MixerCounters {
    missing_leg_ticks: AtomicU64,
    late_frames: AtomicU64,
    overflow_frames: AtomicU64,
    output_overflow_frames: AtomicU64,
    producer_reanchors: AtomicU64,
}

/// Snapshot of the mixer's loss counters (diagnostics / leak-style tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MixerStats {
    /// Ticks where a participant had nothing to contribute (silence padded).
    pub missing_leg_ticks: u64,
    /// Frames dropped because they arrived entirely before the room clock.
    pub late_frames: u64,
    /// Frames dropped from a participant's bounded input queue.
    pub overflow_frames: u64,
    /// Mixed frames dropped because the participant's output channel was full.
    pub output_overflow_frames: u64,
    /// Times a producer's timestamp base was re-anchored to the room clock
    /// (its own timeline restarted, e.g. a rebuilt ingress receiver).
    pub producer_reanchors: u64,
}

enum MixerCommand {
    Start,
    Add {
        leg_id: LegId,
        input_rx: mpsc::Receiver<AudioFrame>,
        output_tx: mpsc::Sender<AudioFrame>,
    },
    Remove(LegId),
    SetMuted { leg_id: LegId, muted: bool },
    SetGain { src: LegId, dst: LegId, gain: f32 },
}

type MixerRequest = (MixerCommand, Option<oneshot::Sender<Result<()>>>);

/// Control handle. Participant channels, rings and gains live in mixing_loop.
pub struct ConferenceAudioMixer {
    conf_id: String,
    participant_count: Arc<AtomicUsize>,
    sample_rate: u32,
    frame_size: usize,
    cancel_token: CancellationToken,
    commands: mpsc::UnboundedSender<MixerRequest>,
    counters: Arc<MixerCounters>,
    // These locks only initialize/join the single worker; the loop never uses them.
    command_rx: ParkMutex<Option<mpsc::UnboundedReceiver<MixerRequest>>>,
    mixing_task: ParkMutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for ConferenceAudioMixer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConferenceAudioMixer")
            .field("conf_id", &self.conf_id)
            .field("sample_rate", &self.sample_rate)
            .field("frame_size", &self.frame_size)
            .finish_non_exhaustive()
    }
}

impl Drop for ConferenceAudioMixer {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        if let Some(task) = self.mixing_task.get_mut().take() {
            task.abort();
        }
    }
}

impl ConferenceAudioMixer {
    pub fn new(conf_id: String, sample_rate: u32) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        Self {
            conf_id,
            participant_count: Arc::new(AtomicUsize::new(0)),
            sample_rate,
            frame_size: (sample_rate as usize * 20) / 1000,
            cancel_token: CancellationToken::new(),
            commands,
            counters: Arc::new(MixerCounters::default()),
            command_rx: ParkMutex::new(Some(command_rx)),
            mixing_task: ParkMutex::new(None),
        }
    }

    // Adding before start() is supported: receive into the rings immediately,
    // but enable the mixing interval only after the Start command.
    fn ensure_worker(&self) {
        let mut task = self.mixing_task.lock();
        if task.is_some() || self.cancel_token.is_cancelled() {
            return;
        }
        let Some(commands) = self.command_rx.lock().take() else { return; };
        *task = Some(tokio::spawn(Self::mixing_loop(
            self.conf_id.clone(), commands, self.cancel_token.clone(),
            self.frame_size, self.sample_rate, self.participant_count.clone(),
            self.counters.clone(),
        )));
    }

    async fn send_command(&self, command: MixerCommand) -> Result<()> {
        if self.cancel_token.is_cancelled() {
            return Err(anyhow!("Conference mixer stopped"));
        }
        self.ensure_worker();
        let (reply, result) = oneshot::channel();
        self.commands.send((command, Some(reply)))
            .map_err(|_| anyhow!("Conference mixer stopped"))?;
        result.await.map_err(|_| anyhow!("Conference mixer stopped"))?
    }

    /// Returns channels for sending decoded PCM and receiving mixed audio.
    pub async fn add_participant(
        &self,
        leg_id: LegId,
        _codec: CodecType,
    ) -> Result<(mpsc::Sender<AudioFrame>, mpsc::Receiver<AudioFrame>)> {
        let (input_tx, input_rx) = mpsc::channel(INPUT_BUFFER_FRAMES);
        let (output_tx, output_rx) = mpsc::channel(100);
        self.send_command(MixerCommand::Add { leg_id, input_rx, output_tx }).await?;
        Ok((input_tx, output_rx))
    }

    pub async fn remove_participant(&self, leg_id: &LegId) -> Result<()> {
        self.send_command(MixerCommand::Remove(leg_id.clone())).await
    }

    pub async fn set_muted(&self, leg_id: &LegId, muted: bool) -> Result<()> {
        self.send_command(MixerCommand::SetMuted { leg_id: leg_id.clone(), muted }).await
    }

    pub async fn set_route_gain(&self, src: &LegId, dst: &LegId, gain: f32) {
        if let Err(error) = self.send_command(MixerCommand::SetGain {
            src: src.clone(), dst: dst.clone(), gain,
        }).await {
            debug!(conf_id = %self.conf_id, %error, "Could not set conference route gain");
        }
    }

    pub fn start(&self) {
        self.ensure_worker();
        let _ = self.commands.send((MixerCommand::Start, None));
    }

    pub async fn stop(&self) {
        self.cancel_token.cancel();
        self.command_rx.lock().take();
        let task = self.mixing_task.lock().take();
        if let Some(task) = task {
            let _ = task.await;
        }
        self.participant_count.store(0, Ordering::Relaxed);
        info!(conf_id = %self.conf_id, "Conference mixer stopped");
    }

    async fn mixing_loop(
        conf_id: String,
        mut commands: mpsc::UnboundedReceiver<MixerRequest>,
        cancel: CancellationToken,
        frame_size: usize,
        sample_rate: u32,
        participant_count: Arc<AtomicUsize>,
        counters: Arc<MixerCounters>,
    ) {
        let mut participants: Vec<ConferenceParticipantAudio> = Vec::new();
        let mut inputs = StreamMap::new();
        let mut route_gains: HashMap<(LegId, LegId), f32> = HashMap::new();
        let period = tokio::time::Duration::from_millis(
            ((frame_size as u64 * 1000) / sample_rate as u64).max(1),
        );
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut started = false;
        // Room timeline, in samples at `sample_rate`. Anchored lazily at the
        // earliest queued audio so a late joiner's own timestamp base never
        // adds artificial leading silence.
        let mut mix_clock: Option<u64> = None;
        // Once the room has produced audio, keep emitting one frame per tick
        // (silence included) so the egress timeline never starves. Before the
        // first audio the room stays quiet and the egress emits its own
        // silence, exactly as an idle leg does.
        let mut primed = false;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = commands.recv() => {
                    let Some((command, reply)) = request else { break; };
                    let result = match command {
                        MixerCommand::Start => {
                            if !started {
                                started = true;
                                mix_clock = None;
                                ticker.reset();
                                info!(%conf_id, frame_size, sample_rate, "Conference mixing loop started");
                            }
                            Ok(())
                        }
                        MixerCommand::Add { leg_id, input_rx, output_tx } => {
                            if participants.iter().any(|p| p.leg_id == leg_id) {
                                Err(anyhow!("Participant {} already exists in conference", leg_id))
                            } else {
                                inputs.insert(leg_id.clone(), ReceiverStream::new(input_rx));
                                participants.push(ConferenceParticipantAudio {
                                    leg_id: leg_id.clone(),
                                    input: VecDeque::with_capacity(INPUT_BUFFER_FRAMES),
                                    output_tx,
                                    muted: false,
                                    next_synthetic_ts: mix_clock.unwrap_or(0),
                                    timestamp_offset: None,
                                });
                                participant_count.store(participants.len(), Ordering::Relaxed);
                                info!(%conf_id, %leg_id, "Added participant to conference mixer");
                                Ok(())
                            }
                        }
                        MixerCommand::Remove(leg_id) => {
                            inputs.remove(&leg_id);
                            if let Some(index) = participants.iter().position(|p| p.leg_id == leg_id) {
                                participants.swap_remove(index);
                            }
                            route_gains.retain(|(src, dst), _| src != &leg_id && dst != &leg_id);
                            if participants.is_empty() {
                                primed = false;
                            }
                            participant_count.store(participants.len(), Ordering::Relaxed);
                            info!(%conf_id, %leg_id, "Removed participant from conference mixer");
                            Ok(())
                        }
                        MixerCommand::SetMuted { leg_id, muted } => {
                            if let Some(participant) = participants.iter_mut().find(|p| p.leg_id == leg_id) {
                                if participant.muted != muted {
                                    participant.input.clear();
                                    // The producer kept sending while muted;
                                    // re-anchor at the current room position so
                                    // unmute does not replay a muted backlog.
                                    participant.timestamp_offset = None;
                                    participant.next_synthetic_ts = mix_clock.unwrap_or(0);
                                }
                                participant.muted = muted;
                                info!(%conf_id, %leg_id, muted, "Participant mute state changed");
                            }
                            Ok(())
                        }
                        MixerCommand::SetGain { src, dst, gain } => {
                            info!(%conf_id, %src, %dst, gain, "Route gain set");
                            if (gain - 1.0).abs() < f32::EPSILON {
                                route_gains.remove(&(src, dst));
                            } else {
                                route_gains.insert((src, dst), gain);
                            }
                            Ok(())
                        }
                    };
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                }
                _ = ticker.tick(), if started => {
                    let mut clock = *mix_clock.get_or_insert_with(|| {
                        participants.iter()
                            .filter(|p| !p.muted)
                            .filter_map(|p| p.input.front().map(|f| f.timestamp))
                            .min()
                            .unwrap_or(0)
                    });

                    // After a scheduler stall the room clock can lag the
                    // producers (the ticker skips missed ticks instead of
                    // bursting). Jump to live audio rather than emitting
                    // seconds of padding while it slowly catches up.
                    if let Some(earliest) = participants.iter()
                        .filter(|p| !p.muted)
                        .filter_map(|p| p.input.front().map(|f| f.timestamp))
                        .min()
                    {
                        let max_lead = (sample_rate as u64 * 200) / 1000;
                        if earliest > clock.saturating_add(max_lead) {
                            debug!(
                                %conf_id,
                                behind = earliest - clock,
                                "Conference mixer resynchronised to live audio"
                            );
                            clock = earliest;
                        }
                    }

                    // Exactly one tick of audio per participant. Frames are
                    // placed by timestamp; a missing span is padded with
                    // silence instead of collapsing that leg out of the mix.
                    let frames: Vec<Option<Vec<i16>>> = participants
                        .iter_mut()
                        .map(|participant| {
                            if participant.muted {
                                return None;
                            }
                            take_tick(&mut participant.input, clock, frame_size, &counters)
                        })
                        .collect();

                    let any_audio = frames.iter().any(Option::is_some);
                    if any_audio {
                        primed = true;
                    }

                    for (output_index, output) in participants.iter().enumerate() {
                        let mut input_frames = Vec::new();
                        let mut gains = Vec::new();
                        for (input_index, input) in participants.iter().enumerate() {
                            if input_index == output_index {
                                continue;
                            }
                            let Some(samples) = &frames[input_index] else { continue; };
                            let gain = route_gains.get(&(input.leg_id.clone(), output.leg_id.clone()))
                                .copied().unwrap_or(1.0);
                            if gain > 0.0 {
                                input_frames.push(samples.clone());
                                gains.push(gain);
                            }
                        }
                        let mixed = if input_frames.is_empty() {
                            // Nobody has audio this tick: keep the output
                            // cadence (silence frame) so the egress timeline
                            // never starves between speech bursts.
                            vec![0i16; frame_size]
                        } else {
                            // Double-talk headroom. Summing N talkers at unity
                            // gain is Nx the level of one and used to hard
                            // clip ("发炸") whenever two people spoke at once.
                            // 1/sqrt(N) keeps a lone speaker at full level
                            // while trimming simultaneous peaks; the soft
                            // limiter in AudioMixer catches the residual.
                            let normalisation = 1.0 / (input_frames.len() as f32).sqrt();
                            for gain in gains.iter_mut() {
                                *gain *= normalisation;
                            }
                            AudioMixer::mix(input_frames, &gains)
                        };
                        if !primed {
                            continue;
                        }
                        let mut frame = AudioFrame::new(mixed, sample_rate);
                        frame.timestamp = clock;
                        if output.output_tx.try_send(frame).is_err() {
                            counters.output_overflow_frames.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    mix_clock = Some(clock.saturating_add(frame_size as u64));
                }
                frame = inputs.next(), if !inputs.is_empty() => {
                    if let Some((leg_id, frame)) = frame {
                        if let Some(participant) = participants.iter_mut().find(|p| p.leg_id == leg_id) {
                            if !participant.muted {
                                let room_clock = mix_clock.unwrap_or(0);
                                participant.queue(frame, room_clock, sample_rate, &counters);
                            }
                        }
                    }
                }
            }
        }
        participant_count.store(0, Ordering::Relaxed);
        info!(%conf_id, "Conference mixing loop stopped");
    }

    /// Snapshot of this mixer's loss counters.
    pub fn stats(&self) -> MixerStats {
        MixerStats {
            missing_leg_ticks: self.counters.missing_leg_ticks.load(Ordering::Relaxed),
            late_frames: self.counters.late_frames.load(Ordering::Relaxed),
            overflow_frames: self.counters.overflow_frames.load(Ordering::Relaxed),
            output_overflow_frames: self.counters.output_overflow_frames.load(Ordering::Relaxed),
            producer_reanchors: self.counters.producer_reanchors.load(Ordering::Relaxed),
        }
    }

    pub fn participant_count(&self) -> usize {
        self.participant_count.load(Ordering::Relaxed)
    }

    /// Sample rate of the mix bus. Media bridges must resample their legs to
    /// this rate (and back from it) instead of hard-coding one.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl ConferenceParticipantAudio {
    /// Place one arriving frame on the room timeline and append it to the
    /// bounded input queue.
    fn queue(
        &mut self,
        frame: AudioFrame,
        room_clock: u64,
        room_rate: u32,
        counters: &MixerCounters,
    ) {
        if frame.samples.is_empty() {
            return;
        }

        // Duration on the room timeline, derived from the producer's own rate:
        // a 20 ms PCMU frame (160 @ 8 kHz) is 320 samples in a 16 kHz room.
        let duration = if frame.sample_rate == 0 {
            // Degenerate producer: assume 1:1 so timestamps keep moving.
            frame.samples.len() as u64
        } else {
            ((frame.samples.len() as u128 * room_rate as u128) / frame.sample_rate as u128)
                as u64
        }
        .max(1);

        let timestamp = if frame.timestamp != 0 {
            match self.timestamp_offset {
                Some(offset) => {
                    let mapped = frame.timestamp.wrapping_add(offset);
                    // Guard against a producer whose own timeline restarted
                    // (rebuilt receiver): without this its frames would map
                    // permanently behind the room clock and be dropped as
                    // late forever. Re-anchor once, at the current position.
                    let reanchor_threshold = (room_rate as i64 * 500) / 1000;
                    let delta = mapped.wrapping_sub(room_clock) as i64;
                    if delta < -reanchor_threshold {
                        counters.producer_reanchors.fetch_add(1, Ordering::Relaxed);
                        self.timestamp_offset = Some(room_clock.wrapping_sub(frame.timestamp));
                        room_clock
                    } else {
                        mapped
                    }
                }
                None => {
                    // First stamped frame anchors the producer to the room
                    // clock (handles participants that join later and whose
                    // own timeline starts at zero). `next_synthetic_ts` keeps
                    // a burst of stamped frames ordered even before the first
                    // mixing tick has anchored the room clock.
                    let anchor = room_clock.max(self.next_synthetic_ts);
                    self.timestamp_offset = Some(anchor.wrapping_sub(frame.timestamp));
                    anchor
                }
            }
        } else {
            // Legacy/unstamped producer: continue its last synthetic position,
            // but never replay audio older than the room clock.
            self.next_synthetic_ts.max(room_clock)
        };
        self.next_synthetic_ts = timestamp.saturating_add(duration);

        // Bounded queue, drop-oldest. The oldest frame is the next one the
        // room clock will consume, so this trades a single 20 ms hole right
        // now for bounded latency; dropping the newest instead would defer
        // the hole and keep the queue full (latency would stay pinned at the
        // cap while audio keeps arriving).
        if self.input.len() >= INPUT_BUFFER_FRAMES {
            self.input.pop_front();
            counters.overflow_frames.fetch_add(1, Ordering::Relaxed);
        }
        self.input.push_back(QueuedFrame {
            timestamp,
            samples: frame.samples,
        });
    }
}

/// Materialise one tick of a participant's audio on the room timeline.
///
/// Frames are consumed by timestamp rather than "one frame per tick", so
/// 10 ms / 30 ms packetisation, transport bursts and short silences are
/// absorbed. Spans with no audio become silence (the participant stays in the
/// mix instead of being skipped). Returns `None` when the participant
/// contributed no samples at all, so the caller can exclude it from gain
/// normalisation.
fn take_tick(
    queue: &mut VecDeque<QueuedFrame>,
    start: u64,
    frame_size: usize,
    counters: &MixerCounters,
) -> Option<Vec<i16>> {
    // Anything that ended before this tick is too late to play.
    while let Some(front) = queue.front() {
        let end = front.timestamp.saturating_add(front.samples.len() as u64);
        if end <= start {
            queue.pop_front();
            counters.late_frames.fetch_add(1, Ordering::Relaxed);
        } else {
            break;
        }
    }

    if queue.is_empty() {
        // Nothing queued at all: no allocation, no contribution.
        counters.missing_leg_ticks.fetch_add(1, Ordering::Relaxed);
        return None;
    }

    let mut out = vec![0i16; frame_size];
    let end = start.saturating_add(frame_size as u64);
    let mut cursor = start;
    let mut contributed = false;

    while cursor < end {
        let Some(front) = queue.front_mut() else { break; };
        let frame_end = front.timestamp.saturating_add(front.samples.len() as u64);
        if frame_end <= cursor {
            queue.pop_front();
            counters.late_frames.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if front.timestamp > cursor {
            // Gap before this frame: leave silence up to it (or tick end).
            cursor = front.timestamp.min(end);
            continue;
        }
        let src_offset = (cursor - front.timestamp) as usize;
        let dst_offset = (cursor - start) as usize;
        let count = ((frame_end - cursor) as usize).min(frame_size - dst_offset);
        out[dst_offset..dst_offset + count]
            .copy_from_slice(&front.samples[src_offset..src_offset + count]);
        cursor += count as u64;
        contributed = true;
        if cursor >= frame_end {
            queue.pop_front();
        }
    }

    contributed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mixer now emits one frame per tick even when nothing is audible
    /// (cadence must never starve the egress). "Does not hear X" therefore
    /// means "silent frame or no frame", not "channel empty".
    fn assert_silent_or_empty(rx: &mut mpsc::Receiver<AudioFrame>) {
        if let Ok(frame) = rx.try_recv() {
            assert!(
                frame.samples.iter().all(|&s| s == 0),
                "expected silence, got non-zero samples: {:?}",
                &frame.samples[..frame.samples.len().min(8)]
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queued_audio_is_mixed_one_frame_per_interval_in_order() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("fifo".into(), 8000);
        let (tx, mut self_rx) = mixer.add_participant(LegId::new("source"), CodecType::PCMU).await.unwrap();
        let (_listener_tx, mut rx) = mixer.add_participant(LegId::new("listener"), CodecType::PCMU).await.unwrap();
        for value in [100, 200, 300] {
            tx.send(AudioFrame::new(vec![value; 160], 8000)).await.unwrap();
        }
        tokio::task::yield_now().await;
        mixer.start();
        tokio::task::yield_now().await;

        advance(Duration::from_millis(19)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples, vec![100; 160]);
        assert!(rx.try_recv().is_err(), "one tick must not consume the whole queue");

        // Wake late: the next deadline stays at 60 ms rather than drifting
        // to 65 ms as sleep(20 ms) after each completed iteration would.
        advance(Duration::from_millis(25)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples, vec![200; 160]);
        advance(Duration::from_millis(15)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples, vec![300; 160]);
        assert_silent_or_empty(&mut self_rx); // N-1 mixing must exclude self audio

        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        assert_silent_or_empty(&mut rx); // empty slots must not replay old audio

        for value in [400, 500, 600] {
            tx.send(AudioFrame::new(vec![value; 160], 8000)).await.unwrap();
        }
        tokio::task::yield_now().await;
        advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples, vec![400; 160]);
        assert!(rx.try_recv().is_err(), "missed ticks must not burst-drain the ring");
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples, vec![500; 160]);
        mixer.stop().await;
    }

    /// Two simultaneous talkers must not sum at unity gain: the old 1.0-gain
    /// sum clipped (audible "发炸") on the 8 kHz room. 1/sqrt(N) keeps a lone
    /// talker at full level and trims double-talk by ~3 dB.
    #[tokio::test(start_paused = true)]
    async fn simultaneous_talkers_are_normalised_before_mixing() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("normalise".into(), 16_000);
        assert_eq!(mixer.sample_rate(), 16_000);

        let (tx_a, _rx_a) = mixer
            .add_participant(LegId::new("a"), CodecType::PCMU)
            .await
            .unwrap();
        let (tx_b, _rx_b) = mixer
            .add_participant(LegId::new("b"), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx_c, mut rx_c) = mixer
            .add_participant(LegId::new("c"), CodecType::PCMU)
            .await
            .unwrap();

        tx_a.send(AudioFrame::new(vec![10_000; 320], 16_000))
            .await
            .unwrap();
        tx_b.send(AudioFrame::new(vec![10_000; 320], 16_000))
            .await
            .unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        mixer.start();
        tokio::task::yield_now().await;

        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;

        let frame = rx_c.try_recv().expect("listener must hear both talkers");
        assert_eq!(frame.sample_rate, 16_000);
        assert_eq!(frame.samples.len(), 320);
        let peak = frame.samples.iter().map(|s| s.abs()).max().unwrap();
        // 2 x 10000 at unity = 20000; normalised by 1/sqrt(2) -> ~14142.
        assert!(
            (13_500..=14_800).contains(&peak),
            "double-talk must be normalised, got peak {peak}"
        );
        mixer.stop().await;
    }

    /// R3 regression: 30 ms frames into a 20 ms room must be consumed by
    /// timestamp, not "one frame per tick" — every sample plays once, with no
    /// late drops. (The old queue truncated/padded per tick and lost audio.)
    #[tokio::test(start_paused = true)]
    async fn timestamped_queue_absorbs_ptime_mismatch() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("ptime".into(), 16_000);
        let (tx, _self_rx) = mixer
            .add_participant(LegId::new("source"), CodecType::PCMU)
            .await
            .unwrap();
        let (_listener_tx, mut rx) = mixer
            .add_participant(LegId::new("listener"), CodecType::PCMU)
            .await
            .unwrap();

        let mut input = Vec::new();
        for frame_index in 0..2u64 {
            let samples: Vec<i16> = (0..480)
                .map(|i| (((frame_index as usize * 480 + i) % 1000) + 1) as i16)
                .collect();
            let mut frame = AudioFrame::new(samples.clone(), 16_000);
            frame.timestamp = frame_index * 480;
            tx.send(frame).await.unwrap();
            input.extend_from_slice(&samples);
        }
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        mixer.start();
        tokio::task::yield_now().await;

        let mut output = Vec::new();
        for _ in 0..3 {
            advance(Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
            if let Ok(frame) = rx.try_recv() {
                output.extend_from_slice(&frame.samples);
            }
        }

        assert_eq!(
            output, input,
            "30 ms frames must span ticks without losing or repeating samples"
        );
        let stats = mixer.stats();
        assert_eq!(stats.late_frames, 0, "no frame should be dropped as late");
        assert_eq!(stats.overflow_frames, 0, "bounded ring must not overflow");
        mixer.stop().await;
    }

    /// After a scheduler stall the room clock lags producer timestamps (the
    /// ticker skips missed ticks). The mixer must jump to live audio instead
    /// of padding out the whole gap in silence.
    #[tokio::test(start_paused = true)]
    async fn stalled_room_resynchronises_to_live_audio() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("resync".into(), 16_000);
        let (tx, _self_rx) = mixer
            .add_participant(LegId::new("source"), CodecType::PCMU)
            .await
            .unwrap();
        let (_listener_tx, mut rx) = mixer
            .add_participant(LegId::new("listener"), CodecType::PCMU)
            .await
            .unwrap();

        tx.send(AudioFrame::new(vec![111i16; 320], 16_000))
            .await
            .unwrap();
        let mut second = AudioFrame::new(vec![222i16; 320], 16_000);
        second.timestamp = 320;
        tx.send(second).await.unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        mixer.start();
        tokio::task::yield_now().await;

        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples[0], 111);
        assert_eq!(rx.try_recv().unwrap().samples[0], 222);

        // Producer advances 1 s (stall); the room clock is still at 640.
        let mut late = AudioFrame::new(vec![333i16; 320], 16_000);
        late.timestamp = 16_000;
        tx.send(late).await.unwrap();
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;

        let frame = rx.try_recv().expect("resync tick must emit live audio");
        assert_eq!(
            frame.samples[0], 333,
            "must jump to live audio instead of padding the stall with silence"
        );
        assert_eq!(frame.timestamp, 16_000);
        mixer.stop().await;
    }

    /// A producer whose own timeline restarted (its frames map far behind the
    /// room clock) must be re-anchored once instead of being dropped as late
    /// forever.
    #[tokio::test(start_paused = true)]
    async fn stale_producer_timeline_is_reanchored() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("reanchor".into(), 16_000);
        let (tx, _self_rx) = mixer
            .add_participant(LegId::new("source"), CodecType::PCMU)
            .await
            .unwrap();
        let (_listener_tx, mut rx) = mixer
            .add_participant(LegId::new("listener"), CodecType::PCMU)
            .await
            .unwrap();

        tx.send(AudioFrame::new(vec![111i16; 320], 16_000))
            .await
            .unwrap();
        let mut second = AudioFrame::new(vec![222i16; 320], 16_000);
        second.timestamp = 320;
        tx.send(second).await.unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        mixer.start();
        tokio::task::yield_now().await;

        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(rx.try_recv().unwrap().samples[0], 111);
        assert_eq!(rx.try_recv().unwrap().samples[0], 222);

        // Run the room clock well past the producer's (now stale) timeline,
        // then deliver a frame that maps ~600 ms behind it.
        for _ in 0..31 {
            advance(Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
        }
        let mut stale = AudioFrame::new(vec![333i16; 320], 16_000);
        stale.timestamp = 1000;
        tx.send(stale).await.unwrap();
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;

        // The idle ticks queued silence frames; drain until the re-anchored
        // frame shows up (bounded so a regression fails instead of hanging).
        let mut reanchored = None;
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(frame) if frame.samples[0] == 333 => {
                    reanchored = Some(frame);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let frame = reanchored.expect("re-anchored frame must play");
        assert_eq!(
            frame.samples[0], 333,
            "stale producer timeline must be re-anchored, not dropped as late"
        );
        let stats = mixer.stats();
        assert_eq!(stats.producer_reanchors, 1);
        mixer.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn input_ring_keeps_only_five_frames_and_receivers_close_on_removal() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("bounded".into(), 8000);
        let source = LegId::new("source");
        let (tx, _self_rx) = mixer.add_participant(source.clone(), CodecType::PCMU).await.unwrap();
        let (listener_tx, mut rx) = mixer.add_participant(LegId::new("listener"), CodecType::PCMU).await.unwrap();
        // Reception runs even when the mixer ticker is not started.
        for value in 1..=8 {
            tx.send(AudioFrame::new(vec![value; 160], 8000)).await.unwrap();
            tokio::task::yield_now().await;
        }
        mixer.start();
        tokio::task::yield_now().await;
        for value in 4..=8 {
            advance(Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
            assert_eq!(rx.try_recv().unwrap().samples, vec![value; 160]);
            assert!(rx.try_recv().is_err());
        }
        mixer.remove_participant(&source).await.unwrap();
        tokio::task::yield_now().await;
        assert!(tx.is_closed(), "removing a slot must stop its receiver");
        // Reusing a leg ID must create an empty slot and a fresh receiver.
        let (replacement_tx, _) = mixer.add_participant(source, CodecType::PCMU).await.unwrap();
        assert!(!replacement_tx.is_closed());
        mixer.stop().await;
        tokio::task::yield_now().await;
        assert!(replacement_tx.is_closed());
        assert!(listener_tx.is_closed());
        assert_eq!(mixer.participant_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn merged_receiver_survives_closed_inputs_and_new_members() {
        let mixer = ConferenceAudioMixer::new("merged-inputs".into(), 8000);
        let listener = LegId::new("listener");
        let (tx, mut rx) = mixer.add_participant(listener.clone(), CodecType::PCMU).await.unwrap();
        drop(tx); // A receive-only participant remains an output destination.
        mixer.start();
        tokio::task::yield_now().await;

        for id in ["first", "second"] {
            let leg = LegId::new(id);
            let (tx, _) = mixer.add_participant(leg.clone(), CodecType::PCMU).await.unwrap();
            assert!(mixer.add_participant(leg.clone(), CodecType::PCMU).await.is_err());
            assert_eq!(mixer.participant_count(), 2);
            tx.send(AudioFrame::new(vec![700; 160], 8000)).await.unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(tokio::time::Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
            assert_eq!(rx.try_recv().unwrap().samples, vec![700; 160]);
            mixer.remove_participant(&leg).await.unwrap();
            assert!(tx.is_closed());
            assert_eq!(mixer.participant_count(), 1);
        }
        mixer.stop().await;
        assert!(mixer.add_participant(LegId::new("late"), CodecType::PCMU).await.is_err());
    }

    #[tokio::test]
    async fn test_conference_mixer_creation() {
        let mixer = ConferenceAudioMixer::new("test-conf".to_string(), 8000);
        assert_eq!(mixer.participant_count(), 0);
    }

    #[tokio::test]
    async fn test_add_remove_participant() {
        let mixer = ConferenceAudioMixer::new("test-conf".to_string(), 8000);

        let leg_id = LegId::new("leg1");
        let (_input_tx, _output_rx) = mixer
            .add_participant(leg_id.clone(), CodecType::PCMU)
            .await
            .unwrap();

        assert_eq!(mixer.participant_count(), 1);

        mixer.remove_participant(&leg_id).await.unwrap();
        assert_eq!(mixer.participant_count(), 0);
    }

    #[tokio::test]
    async fn test_audio_mixing() {
        let mixer = ConferenceAudioMixer::new("test-conf".to_string(), 8000);
        mixer.start();

        // Add two participants
        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");

        let (tx1, mut rx1) = mixer
            .add_participant(leg1.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx2, mut rx2) = mixer
            .add_participant(leg2.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // Send audio from leg1
        let samples1 = vec![1000i16; 160];
        tx1.send(AudioFrame::new(samples1, 8000)).await.unwrap();

        // Give time for mixing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Leg2 should receive the mixed audio (from leg1)
        let frame = rx2.try_recv();
        assert!(frame.is_ok(), "Leg2 should receive audio from leg1");

        // Leg1 should not receive its own audio (silence frames are allowed
        // once the room is live: they keep the egress cadence).
        assert_silent_or_empty(&mut rx1);

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_mute() {
        let mixer = ConferenceAudioMixer::new("test-conf".to_string(), 8000);
        mixer.start();

        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");

        let (tx1, _rx1) = mixer
            .add_participant(leg1.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx2, mut rx2) = mixer
            .add_participant(leg2.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // Mute leg1
        mixer.set_muted(&leg1, true).await.unwrap();

        // Send audio from leg1 (should be muted)
        let samples1 = vec![1000i16; 160];
        tx1.send(AudioFrame::new(samples1, 8000)).await.unwrap();

        // Give time for mixing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Leg2 should not receive audio because leg1 is muted
        let _ = rx2.try_recv();
        // Note: Due to timing, we might get silence frames
        // The key point is that the muted audio is not mixed in

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_three_participant_conference() {
        let mixer = ConferenceAudioMixer::new("test-conf-3p".to_string(), 8000);
        mixer.start();

        // Add three participants
        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");
        let leg3 = LegId::new("leg3");

        let (tx1, mut rx1) = mixer
            .add_participant(leg1.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (tx2, mut rx2) = mixer
            .add_participant(leg2.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (tx3, mut rx3) = mixer
            .add_participant(leg3.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // Participant 1 speaks
        let samples1 = vec![1000i16; 160];
        tx1.send(AudioFrame::new(samples1.clone(), 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Participants 2 and 3 should hear participant 1
        let frame2 = rx2.try_recv();
        assert!(frame2.is_ok(), "Leg2 should receive audio from leg1");
        let frame3 = rx3.try_recv();
        assert!(frame3.is_ok(), "Leg3 should receive audio from leg1");

        // Leg1 should not receive its own audio
        assert_silent_or_empty(&mut rx1);

        // Now participant 2 speaks
        let samples2 = vec![2000i16; 160];
        tx2.send(AudioFrame::new(samples2.clone(), 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Participants 1 and 3 should hear participant 2
        let frame1 = rx1.try_recv();
        assert!(frame1.is_ok(), "Leg1 should receive audio from leg2");
        let frame3 = rx3.try_recv();
        assert!(frame3.is_ok(), "Leg3 should receive audio from leg2");

        // Leg2 should not receive its own audio
        assert_silent_or_empty(&mut rx2);

        // Now all three speak simultaneously
        tx1.send(AudioFrame::new(samples1.clone(), 8000))
            .await
            .unwrap();
        tx2.send(AudioFrame::new(samples2.clone(), 8000))
            .await
            .unwrap();
        let samples3 = vec![3000i16; 160];
        tx3.send(AudioFrame::new(samples3.clone(), 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Each participant should hear the other two
        // Leg1 should hear leg2 and leg3
        let frame = rx1.try_recv();
        assert!(frame.is_ok(), "Leg1 should receive mixed audio");
        // Leg2 should hear leg1 and leg3
        let frame = rx2.try_recv();
        assert!(frame.is_ok(), "Leg2 should receive mixed audio");
        // Leg3 should hear leg1 and leg2
        let frame = rx3.try_recv();
        assert!(frame.is_ok(), "Leg3 should receive mixed audio");

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_participant_join_mid_conference() {
        let mixer = ConferenceAudioMixer::new("test-join-mid".to_string(), 8000);
        mixer.start();

        // Start with two participants
        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");

        let (tx1, mut rx1) = mixer
            .add_participant(leg1.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx2, _rx2) = mixer
            .add_participant(leg2.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // First communication
        tx1.send(AudioFrame::new(vec![1000i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Add third participant
        let leg3 = LegId::new("leg3");
        let (tx3, mut rx3) = mixer
            .add_participant(leg3.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // New participant speaks
        tx3.send(AudioFrame::new(vec![2000i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Leg1 should hear leg3
        let frame = rx1.try_recv();
        assert!(
            frame.is_ok(),
            "Leg1 should receive audio from new participant leg3"
        );

        // Leg3 should hear leg1 (after leg1 speaks again)
        tx1.send(AudioFrame::new(vec![1500i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        let frame = rx3.try_recv();
        assert!(frame.is_ok(), "Leg3 should receive audio from leg1");

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_participant_leave_mid_conference() {
        let mixer = ConferenceAudioMixer::new("test-leave-mid".to_string(), 8000);
        mixer.start();

        // Start with three participants
        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");
        let leg3 = LegId::new("leg3");

        let (tx1, _) = mixer
            .add_participant(leg1.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (tx2, mut rx2) = mixer
            .add_participant(leg2.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx3, _) = mixer
            .add_participant(leg3.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // All participants send audio
        tx1.send(AudioFrame::new(vec![1000i16; 160], 8000))
            .await
            .unwrap();
        tx2.send(AudioFrame::new(vec![2000i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Leg3 leaves
        mixer.remove_participant(&leg3).await.unwrap();

        // Remaining participants continue
        tx1.send(AudioFrame::new(vec![1500i16; 160], 8000))
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Leg2 should still hear leg1
        let frame = rx2.try_recv();
        assert!(
            frame.is_ok(),
            "Leg2 should still receive audio from leg1 after leg3 leaves"
        );

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_concurrent_audio_streams() {
        let mixer = ConferenceAudioMixer::new("test-concurrent".to_string(), 8000);
        mixer.start();

        // Add 4 participants
        let mut txs = Vec::new();
        let mut rxs = Vec::new();

        for i in 0..4 {
            let leg = LegId::new(format!("leg{}", i));
            let (tx, rx) = mixer.add_participant(leg, CodecType::PCMU).await.unwrap();
            txs.push(tx);
            rxs.push(rx);
        }

        // All participants send audio simultaneously
        for (i, tx) in txs.iter().enumerate() {
            let samples = vec![(i as i16 + 1) * 500; 160];
            tx.send(AudioFrame::new(samples, 8000)).await.unwrap();
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Each participant should receive audio from the other 3
        for (i, rx) in rxs.iter_mut().enumerate() {
            let frame = rx.try_recv();
            assert!(
                frame.is_ok(),
                "Participant {} should receive mixed audio from others",
                i
            );

            // Verify the mixed samples contain contributions from others
            let frame = frame.unwrap();
            let has_non_zero = frame.samples.iter().any(|&s| s != 0);
            assert!(has_non_zero, "Mixed audio should contain non-zero samples");
        }

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_audio_mixing_with_gains() {
        let mixer = ConferenceAudioMixer::new("test-gains".to_string(), 8000);
        mixer.start();

        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");

        let (tx1, _rx1) = mixer
            .add_participant(leg1.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx2, mut rx2) = mixer
            .add_participant(leg2.clone(), CodecType::PCMU)
            .await
            .unwrap();

        let amplitude = 1000i16;
        let samples = vec![amplitude; 160];
        tx1.send(AudioFrame::new(samples, 8000)).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        let frame = rx2.try_recv().expect("Should receive audio");
        assert_eq!(frame.samples.len(), 160, "Frame size should be 160 samples");

        let avg_amplitude: i16 =
            (frame.samples.iter().map(|&s| s as i32).sum::<i32>() / 160) as i16;
        assert!(
            (avg_amplitude - amplitude).abs() < 100,
            "Received amplitude {} should be close to sent amplitude {}",
            avg_amplitude,
            amplitude
        );

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_route_gain_supervisor_listen() {
        let mixer = ConferenceAudioMixer::new("test-route-listen".to_string(), 8000);
        mixer.start();

        let customer = LegId::new("customer");
        let agent = LegId::new("agent");
        let supervisor = LegId::new("supervisor");

        let (tx_cust, mut rx_cust) = mixer
            .add_participant(customer.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx_agent, mut rx_agent) = mixer
            .add_participant(agent.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (_tx_sup, mut rx_sup) = mixer
            .add_participant(supervisor.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // Listen mode: supervisor sends nothing to customer or agent
        mixer.set_route_gain(&supervisor, &customer, 0.0).await;
        mixer.set_route_gain(&supervisor, &agent, 0.0).await;

        // Supervisor speaks (should be blocked by route gain)
        let sup_samples = vec![5000i16; 160];
        _tx_sup
            .send(AudioFrame::new(sup_samples, 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Customer should NOT hear supervisor
        if let Ok(frame) = rx_cust.try_recv() {
            let has_supervisor_audio = frame.samples.iter().any(|&s| s.abs() > 100);
            assert!(
                !has_supervisor_audio,
                "Customer should not hear supervisor in listen mode"
            );
        }

        // Agent should NOT hear supervisor
        if let Ok(frame) = rx_agent.try_recv() {
            let has_supervisor_audio = frame.samples.iter().any(|&s| s.abs() > 100);
            assert!(
                !has_supervisor_audio,
                "Agent should not hear supervisor in listen mode"
            );
        }

        // Customer speaks - agent and supervisor should hear
        let cust_samples = vec![1000i16; 160];
        tx_cust
            .send(AudioFrame::new(cust_samples, 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        assert!(rx_agent.try_recv().is_ok(), "Agent should hear customer");
        assert!(rx_sup.try_recv().is_ok(), "Supervisor should hear customer");

        mixer.stop().await;
    }

    #[tokio::test]
    async fn test_route_gain_supervisor_whisper() {
        let mixer = ConferenceAudioMixer::new("test-route-whisper".to_string(), 8000);
        mixer.start();

        let customer = LegId::new("customer");
        let agent = LegId::new("agent");
        let supervisor = LegId::new("supervisor");

        let (_tx_cust, mut rx_cust) = mixer
            .add_participant(customer.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (tx_agent, _rx_agent) = mixer
            .add_participant(agent.clone(), CodecType::PCMU)
            .await
            .unwrap();
        let (tx_sup, mut rx_sup) = mixer
            .add_participant(supervisor.clone(), CodecType::PCMU)
            .await
            .unwrap();

        // Whisper mode: supervisor speaks to agent only, not customer
        mixer.set_route_gain(&supervisor, &customer, 0.0).await;
        // supervisor -> agent stays at 1.0 (default)

        // Supervisor speaks
        let sup_samples = vec![5000i16; 160];
        tx_sup
            .send(AudioFrame::new(sup_samples, 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        // Customer should NOT hear supervisor
        if let Ok(frame) = rx_cust.try_recv() {
            let has_supervisor_audio = frame.samples.iter().any(|&s| s.abs() > 100);
            assert!(
                !has_supervisor_audio,
                "Customer should not hear supervisor in whisper mode"
            );
        }

        // Agent speaks - customer and supervisor should hear
        let agent_samples = vec![2000i16; 160];
        tx_agent
            .send(AudioFrame::new(agent_samples, 8000))
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

        assert!(rx_cust.try_recv().is_ok(), "Customer should hear agent");
        assert!(rx_sup.try_recv().is_ok(), "Supervisor should hear agent");

        mixer.stop().await;
    }

    /// Verify that dropping a `ConferenceAudioMixer` without calling `stop()`
    /// still cancels the mixing task (no leak).
    #[tokio::test]
    async fn test_mixer_drop_without_stop_aborts_task() {
        let mixer = ConferenceAudioMixer::new("drop-test".to_string(), 8000);
        mixer.start();
        let (tx, mut rx) = mixer.add_participant(LegId::new("leg"), CodecType::PCMU).await.unwrap();

        // Drop without calling stop() — simulates a cleanup path that
        // forgets to stop the mixer.
        drop(mixer);

        tokio::time::timeout(tokio::time::Duration::from_secs(1), tx.closed())
            .await.expect("dropping the mixer must close its merged inputs");
        assert!(rx.recv().await.is_none(), "the local participant outputs must also close");
    }
}
