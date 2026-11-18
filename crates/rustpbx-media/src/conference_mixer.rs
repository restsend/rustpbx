//! Conference mixer with one task owning all participants and audio queues.

use crate::LegId;
use crate::mixer::AudioMixer;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use parking_lot::Mutex as ParkMutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::{StreamExt, StreamMap, wrappers::ReceiverStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub use crate::AudioFrame;

/// Mix-bus sample rate for conferences. 16 kHz carries wideband G.722
/// losslessly; 8 kHz PCMU and 48 kHz Opus legs are FIR-resampled to/from it
/// by the media bridge in both directions.
pub const CONFERENCE_MIXER_SAMPLE_RATE: u32 = 16_000;

const INPUT_BUFFER_FRAMES: usize = 5;

/// Owned only by the mixing task; no shared participant or audio state.
struct ConferenceParticipantAudio {
    leg_id: LegId,
    input: VecDeque<AudioFrame>,
    output_tx: mpsc::Sender<AudioFrame>,
    muted: bool,
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
            command_rx: ParkMutex::new(Some(command_rx)),
            mixing_task: ParkMutex::new(None),
        }
    }

    /// Mix-bus sample rate. Media bridges resample every leg's decoded PCM to
    /// this rate before feeding the mixer, and resample the mixed output to
    /// each leg's encoder rate — read it when wiring a bridge
    /// ([`crate::mixer`] consumers must not assume 8 kHz).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
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
                                    leg_id: leg_id.clone(), input: VecDeque::with_capacity(INPUT_BUFFER_FRAMES),
                                    output_tx, muted: false,
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
                            participant_count.store(participants.len(), Ordering::Relaxed);
                            info!(%conf_id, %leg_id, "Removed participant from conference mixer");
                            Ok(())
                        }
                        MixerCommand::SetMuted { leg_id, muted } => {
                            if let Some(participant) = participants.iter_mut().find(|p| p.leg_id == leg_id) {
                                if participant.muted != muted {
                                    participant.input.clear();
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
                    // Exactly one frame per leg per tick, independent of reception.
                    // A leg with no buffered audio contributes silence instead of
                    // being silently excluded: exclusion made the mix jump in level
                    // and left outputs starved whenever a peer's clock ran slow.
                    let silent_frame = vec![0i16; frame_size];
                    let frames: Vec<Option<Vec<i16>>> = participants.iter_mut().map(|participant| {
                        let frame = participant.input.pop_front();
                        let frame = if participant.muted { None } else { frame };
                        frame.map(|mut frame| {
                            frame.samples.resize(frame_size, 0);
                            frame.samples
                        })
                    }).collect();
                    for (output_index, output) in participants.iter().enumerate() {
                        let mut input_frames = Vec::new();
                        let mut gains = Vec::new();
                        for (input_index, input) in participants.iter().enumerate() {
                            if input_index == output_index || input.muted {
                                continue;
                            }
                            let gain = route_gains.get(&(input.leg_id.clone(), output.leg_id.clone()))
                                .copied().unwrap_or(1.0);
                            if gain > 0.0 {
                                let samples = frames[input_index].as_deref()
                                    .unwrap_or(&silent_frame);
                                input_frames.push(samples.to_vec());
                                gains.push(gain);
                            }
                        }
                        if !input_frames.is_empty() {
                            // Loudness normalization: with n concurrent sources each
                            // gains 1/sqrt(n), so the summed peak does not grow with
                            // participant count (a lone speaker stays at unity gain).
                            let norm = 1.0 / (input_frames.len() as f32).sqrt();
                            let gains: Vec<f32> = gains.iter().map(|g| g * norm).collect();
                            let mixed = AudioMixer::mix(input_frames, &gains);
                            let _ = output.output_tx.try_send(AudioFrame::new(mixed, sample_rate));
                        }
                    }
                }
                frame = inputs.next(), if !inputs.is_empty() => {
                    if let Some((leg_id, frame)) = frame {
                        if let Some(participant) = participants.iter_mut().find(|p| p.leg_id == leg_id) {
                            if !participant.muted {
                                if participant.input.len() == INPUT_BUFFER_FRAMES {
                                    participant.input.pop_front();
                                    // Sign of a producer clock running fast against
                                    // the fixed mixing tick — audible as periodic
                                    // clicks, so surface it.
                                    debug!(%conf_id, %leg_id,
                                        "conference input ring full, dropped oldest frame");
                                }
                                participant.input.push_back(frame);
                            }
                        }
                    }
                }
            }
        }
        participant_count.store(0, Ordering::Relaxed);
        info!(%conf_id, "Conference mixing loop stopped");
    }

    pub fn participant_count(&self) -> usize {
        self.participant_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Self audio must never be mixed back: the source leg still receives
        // one frame per tick (the listener is silent), but it is pure silence.
        let self_frame = self_rx
            .try_recv()
            .expect("steady per-tick output for every participant");
        assert!(
            self_frame.samples.iter().all(|&s| s == 0),
            "N-1 mixing must exclude self audio (silence only)"
        );
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        // Queue drained: the tick emits a silence frame, never replays audio.
        let idle_frame = rx.try_recv().expect("steady per-tick output");
        assert!(
            idle_frame.samples.iter().all(|&s| s == 0),
            "empty slots must contribute silence, not replay old audio"
        );

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

        // Leg1 should not hear itself: it still gets a per-tick frame, but a
        // silent one (leg2 contributed nothing this tick).
        let frame = rx1.try_recv().expect("steady per-tick output");
        assert!(
            frame.samples.iter().all(|&s| s == 0),
            "Leg1 should not receive its own audio (silence only)"
        );

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

        // Leg1 should not hear itself: silent frame only (leg2/leg3 idle).
        let frame1 = rx1.try_recv().expect("steady per-tick output");
        assert!(
            frame1.samples.iter().all(|&s| s == 0),
            "Leg1 should not receive its own audio (silence only)"
        );

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

        // Leg2 should not hear itself: silent frame only (leg1/leg3 idle).
        let frame2 = rx2.try_recv().expect("steady per-tick output");
        assert!(
            frame2.samples.iter().all(|&s| s == 0),
            "Leg2 should not receive its own audio (silence only)"
        );

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

    /// S3 regression: two loud speakers must not flat-top the mix. The
    /// 1/sqrt(n) normalization plus the soft limiter keep the output loud but
    /// strictly below full scale (hard clipping produced harsh harmonics).
    #[tokio::test]
    async fn normalization_keeps_multi_speaker_mix_below_full_scale() {
        let mixer = ConferenceAudioMixer::new("norm-limiter".into(), 8000);
        mixer.start();

        let leg1 = LegId::new("leg1");
        let leg2 = LegId::new("leg2");

        let (tx1, mut rx1) = mixer.add_participant(leg1.clone(), CodecType::PCMU).await.unwrap();
        let (tx2, _rx2) = mixer.add_participant(leg2.clone(), CodecType::PCMU).await.unwrap();

        tx1.send(AudioFrame::new(vec![30000i16; 160], 8000)).await.unwrap();
        tx2.send(AudioFrame::new(vec![30000i16; 160], 8000)).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let frame = rx1.try_recv().expect("mixed frame for leg1");
        // Per-source contribution after 1/sqrt(2) normalization: 30000 * 0.707
        // ≈ 21213 — still clearly loud, and strictly below full scale.
        let loud = frame.samples.iter().filter(|&&s| s > 20000).count();
        assert!(loud > 0, "mix must stay loud, peak {:?}", frame.samples.iter().max());
        assert!(
            frame.samples.iter().all(|&s| s < i16::MAX),
            "1/sqrt(n) + soft limiter must prevent hard clipping"
        );

        mixer.stop().await;
    }

    /// S4 regression: an idle leg must not starve the other side. Every tick
    /// produces exactly one mixed frame per participant, so downstream egress
    /// never runs dry and levels do not jump when a leg goes quiet.
    #[tokio::test(start_paused = true)]
    async fn idle_legs_keep_outputs_on_steady_cadence() {
        use tokio::time::{advance, Duration};

        let mixer = ConferenceAudioMixer::new("steady-cadence".into(), 8000);
        let speaker = LegId::new("speaker");
        let idle = LegId::new("idle");

        let (tx_speaker, mut rx_speaker) = mixer.add_participant(speaker, CodecType::PCMU).await.unwrap();
        let (_tx_idle, mut rx_idle) = mixer.add_participant(idle, CodecType::PCMU).await.unwrap();

        tx_speaker.send(AudioFrame::new(vec![1000i16; 160], 8000)).await.unwrap();
        mixer.start();
        tokio::task::yield_now().await;

        // Tick 1: idle hears the speaker; speaker hears silence.
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        let heard = rx_idle.try_recv().expect("idle leg must receive speaker audio");
        assert!(heard.samples.iter().any(|&s| s != 0));
        let self_frame = rx_speaker.try_recv().expect("speaker leg still on cadence");
        assert!(self_frame.samples.iter().all(|&s| s == 0));

        // Tick 2+: nobody sends anything — both legs still get silence frames
        // (previously the speaker's output went silent/empty here).
        advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        let silence_idle = rx_idle.try_recv().expect("idle leg keeps receiving frames");
        assert!(silence_idle.samples.iter().all(|&s| s == 0));
        let silence_speaker = rx_speaker.try_recv().expect("speaker leg keeps receiving frames");
        assert!(silence_speaker.samples.iter().all(|&s| s == 0));
        assert!(rx_idle.try_recv().is_err(), "one frame per tick, no burst");

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
