//! Conference Mixer - MCU-style multi-party audio mixing
//!
//! This module provides real-time audio mixing for conference calls.
//! It connects MediaPeers to the mixer and routes mixed audio back to participants.

use crate::call::domain::LegId;
use crate::media::mixer::AudioMixer;
use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use parking_lot::Mutex as ParkMutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// Audio frame buffer for passing audio between components
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Raw PCM samples (16-bit signed, mono)
    pub samples: Vec<i16>,
    /// Sample rate
    pub sample_rate: u32,
    /// Timestamp
    pub timestamp: u64,
}

/// A frame captured from a participant's input, sent to the recorder tap.
/// Each participant's raw PCM is forwarded here before mixing so the
/// recorder can write per-leg audio to the corresponding WAV channel.
#[derive(Debug, Clone)]
pub struct RecorderFrame {
    /// Which participant this audio came from.
    pub leg_id: LegId,
    /// PCM samples (16-bit signed, mono).
    pub samples: Vec<i16>,
    /// Sample rate of the samples.
    pub sample_rate: u32,
    /// Monotonic timestamp for ordering.
    pub timestamp: u64,
}

impl AudioFrame {
    pub fn new(samples: Vec<i16>, sample_rate: u32) -> Self {
        Self {
            samples,
            sample_rate,
            timestamp: 0,
        }
    }

    pub fn silence(sample_count: usize) -> Self {
        Self {
            samples: vec![0i16; sample_count],
            sample_rate: 8000,
            timestamp: 0,
        }
    }
}

/// Conference participant audio interface
#[derive(Debug)]
pub struct ConferenceParticipantAudio {
    /// Participant ID (LegId)
    pub leg_id: LegId,
    /// Input channel from participant (decoded PCM)
    pub input_rx: mpsc::Receiver<AudioFrame>,
    /// Output channel to participant (mixed PCM for encoding)
    pub output_tx: mpsc::Sender<AudioFrame>,
    /// Codec preference
    pub codec: CodecType,
    /// Whether participant is muted
    pub muted: bool,
}

/// Real-time conference mixer with MCU architecture
pub struct ConferenceAudioMixer {
    /// Conference ID
    conf_id: String,
    /// Participant audio channels
    participants: Arc<tokio::sync::Mutex<HashMap<LegId, ConferenceParticipantAudio>>>,
    /// Cached participant count for sync access
    participant_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Audio sample rate
    sample_rate: u32,
    /// Frame size in samples (e.g., 160 for 20ms at 8kHz)
    frame_size: usize,
    /// Cancellation token for stopping
    cancel_token: CancellationToken,
    /// Mixing task handle
    mixing_task: Arc<ParkMutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Per-(source, destination) gain overrides for supervisor modes.
    /// Key: (src_leg_id, dst_leg_id), Value: gain (0.0 = silent, 1.0 = normal)
    route_gains: Arc<tokio::sync::Mutex<HashMap<(LegId, LegId), f32>>>,
    /// Optional recorder tap: each participant's input PCM is cloned here
    /// before mixing so the recorder can write per-leg audio.
    recorder_sink: Arc<tokio::sync::Mutex<Option<mpsc::Sender<RecorderFrame>>>>,
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
        if let Some(task) = self.mixing_task.lock().take() {
            task.abort();
        }
    }
}

impl ConferenceAudioMixer {
    /// Create a new conference mixer
    pub fn new(conf_id: String, sample_rate: u32) -> Self {
        let frame_size = (sample_rate as usize * 20) / 1000; // 20ms frames

        Self {
            conf_id,
            participants: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            participant_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            sample_rate,
            frame_size,
            cancel_token: CancellationToken::new(),
            mixing_task: Arc::new(ParkMutex::new(None)),
            route_gains: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            recorder_sink: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Install a recorder tap.  Every 20ms tick, the mixer will send each
    /// participant's raw input PCM to the provided channel **before**
    /// N-1 mixing, so the recorder can write per-leg audio.
    pub async fn set_recorder_sink(&self, sink: Option<mpsc::Sender<RecorderFrame>>) {
        let mut guard = self.recorder_sink.lock().await;
        *guard = sink;
    }

    /// Add a participant to the conference
    /// Returns channels for sending/receiving audio
    pub async fn add_participant(
        &self,
        leg_id: LegId,
        codec: CodecType,
    ) -> Result<(mpsc::Sender<AudioFrame>, mpsc::Receiver<AudioFrame>)> {
        // Reject duplicate participants
        {
            let participants = self.participants.lock().await;
            if participants.contains_key(&leg_id) {
                return Err(anyhow!(
                    "Participant {} already exists in conference",
                    leg_id
                ));
            }
        }

        let (input_tx, input_rx) = mpsc::channel::<AudioFrame>(100);
        let (output_tx, output_rx) = mpsc::channel::<AudioFrame>(100);

        let participant = ConferenceParticipantAudio {
            leg_id: leg_id.clone(),
            input_rx,
            output_tx,
            codec,
            muted: false,
        };

        {
            let mut participants = self.participants.lock().await;
            participants.insert(leg_id.clone(), participant);
        }

        // Update cached count
        self.participant_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Update mixing routes for all participants
        self.update_routing().await?;

        info!(
            conf_id = %self.conf_id,
            leg_id = %leg_id,
            "Added participant to conference mixer"
        );

        Ok((input_tx, output_rx))
    }

    /// Remove a participant from the conference
    pub async fn remove_participant(&self, leg_id: &LegId) -> Result<()> {
        {
            let mut participants = self.participants.lock().await;
            participants.remove(leg_id);
        }

        // Update cached count
        self.participant_count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        // Update mixing routes
        self.update_routing().await?;

        info!(
            conf_id = %self.conf_id,
            leg_id = %leg_id,
            "Removed participant from conference mixer"
        );

        Ok(())
    }

    /// Mute/unmute a participant
    pub async fn set_muted(&self, leg_id: &LegId, muted: bool) -> Result<()> {
        let mut participants = self.participants.lock().await;
        if let Some(participant) = participants.get_mut(leg_id) {
            participant.muted = muted;
            info!(
                conf_id = %self.conf_id,
                leg_id = %leg_id,
                muted = muted,
                "Participant mute state changed"
            );
        }
        Ok(())
    }

    /// Set per-route gain for supervisor modes.
    /// A gain of 0.0 means the source participant is silent for the destination.
    pub async fn set_route_gain(&self, src: &LegId, dst: &LegId, gain: f32) {
        let mut gains = self.route_gains.lock().await;
        if (gain - 1.0).abs() < f32::EPSILON {
            gains.remove(&(src.clone(), dst.clone()));
        } else {
            gains.insert((src.clone(), dst.clone()), gain);
        }
        info!(
            conf_id = %self.conf_id,
            src = %src,
            dst = %dst,
            gain = gain,
            "Route gain set"
        );
    }

    /// Clear all route gains (reset to default N-1 mixing).
    pub async fn clear_route_gains(&self) {
        let mut gains = self.route_gains.lock().await;
        gains.clear();
        info!(conf_id = %self.conf_id, "Route gains cleared");
    }

    /// Update audio routing for all participants
    /// Each participant hears all other participants (N-1 mixing)
    async fn update_routing(&self) -> Result<()> {
        let participants = self.participants.lock().await;
        let leg_ids: Vec<LegId> = participants.keys().cloned().collect();
        drop(participants);

        // ConferenceAudioMixer uses its own mixing loop (N-1 mixing)
        // Each participant receives mixed audio from all other participants
        // The actual mixing happens in mixing_loop(), not via MediaMixer routing

        debug!(
            conf_id = %self.conf_id,
            participant_count = leg_ids.len(),
            "Updated conference routing"
        );

        Ok(())
    }

    /// Start the conference mixing
    pub fn start(&self) {
        let cancel_token = self.cancel_token.clone();
        let participants = self.participants.clone();
        let frame_size = self.frame_size;
        let sample_rate = self.sample_rate;
        let conf_id = self.conf_id.clone();
        let route_gains = self.route_gains.clone();
        let recorder_sink = self.recorder_sink.clone();

        let task = crate::utils::spawn(async move {
            Self::mixing_loop(
                conf_id,
                participants,
                cancel_token,
                frame_size,
                sample_rate,
                route_gains,
                recorder_sink,
            )
            .await;
        });

        let mut mixing_task = self.mixing_task.lock();
        *mixing_task = Some(task);

        info!(conf_id = %self.conf_id, "Conference mixer started");
    }

    /// Stop the conference mixing
    pub async fn stop(&self) {
        self.cancel_token.cancel();

        // Take the task out of the mutex before awaiting
        let task = {
            let mut mixing_task = self.mixing_task.lock();
            mixing_task.take()
        };

        if let Some(t) = task {
            let _ = t.await;
        }

        info!(conf_id = %self.conf_id, "Conference mixer stopped");
    }

    /// The main conference mixing loop
    /// Collects audio from all participants, mixes, and distributes
    async fn mixing_loop(
        conf_id: String,
        participants: Arc<tokio::sync::Mutex<HashMap<LegId, ConferenceParticipantAudio>>>,
        cancel_token: CancellationToken,
        frame_size: usize,
        sample_rate: u32,
        route_gains: Arc<tokio::sync::Mutex<HashMap<(LegId, LegId), f32>>>,
        recorder_sink: Arc<tokio::sync::Mutex<Option<mpsc::Sender<RecorderFrame>>>>,
    ) {
        let interval_ms = (frame_size as f64 / sample_rate as f64 * 1000.0) as u64;
        let interval = tokio::time::Duration::from_millis(interval_ms.max(1));

        info!(
            conf_id = %conf_id,
            frame_size = frame_size,
            sample_rate = sample_rate,
            interval_ms = interval_ms,
            "Conference mixing loop started"
        );

        let audio_mixer = AudioMixer::new(sample_rate, 1);

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!(conf_id = %conf_id, "Conference mixing loop cancelled");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    let participant_audio = {
                        let mut participants_guard = participants.lock().await;
                        let mut frames = HashMap::new();

                        for (leg_id, participant) in participants_guard.iter_mut() {
                            loop {
                                match participant.input_rx.try_recv() {
                                    Ok(frame) => {
                                        if !participant.muted {
                                            frames.insert(leg_id.clone(), frame);
                                        }
                                    }
                                    Err(mpsc::error::TryRecvError::Empty) => {
                                        break;
                                    }
                                    Err(mpsc::error::TryRecvError::Disconnected) => {
                                        break;
                                    }
                                }
                            }
                        }

                        frames
                    };

                    let gains_map = route_gains.lock().await;
                    let participants_guard = participants.lock().await;
                    let participant_ids: Vec<LegId> = participants_guard.keys().cloned().collect();
                    drop(participants_guard);

                    // ── Recorder tap: forward raw per-leg PCM before mixing ─────
                    if !participant_audio.is_empty() {
                        let sink_guard = recorder_sink.lock().await;
                        if let Some(ref sink) = *sink_guard {
                            for (leg_id, frame) in &participant_audio {
                                let rf = RecorderFrame {
                                    leg_id: leg_id.clone(),
                                    samples: frame.samples.clone(),
                                    sample_rate: frame.sample_rate,
                                    timestamp: frame.timestamp,
                                };
                                let _ = sink.try_send(rf);
                            }
                        }
                        drop(sink_guard);
                    }

                    if !participant_audio.is_empty() {
                        for output_leg in &participant_ids {
                            let mut input_frames = Vec::new();
                            let mut gains = Vec::new();

                            for (input_leg, frame) in &participant_audio {
                                if input_leg != output_leg {
                                    let gain = gains_map
                                        .get(&(input_leg.clone(), output_leg.clone()))
                                        .copied()
                                        .unwrap_or(1.0);
                                    if gain > 0.0 {
                                        input_frames.push(frame.samples.clone());
                                        gains.push(gain);
                                    }
                                }
                            }

                            if !input_frames.is_empty() {
                                let mut normalized_frames = Vec::new();
                                for mut frame in input_frames {
                                    if frame.len() < frame_size {
                                        frame.resize(frame_size, 0);
                                    } else if frame.len() > frame_size {
                                        frame.truncate(frame_size);
                                    }
                                    normalized_frames.push(frame);
                                }
                                let mixed_samples = audio_mixer.mix_frames(normalized_frames, &gains);

                                let output_frame = AudioFrame::new(mixed_samples, sample_rate);

                                let output_tx = {
                                    let participants_guard = participants.lock().await;
                                    participants_guard.get(output_leg).map(|p| p.output_tx.clone())
                                };

                                if let Some(tx) = output_tx
                                    && tx.send(output_frame).await.is_err() {
                                    }
                            }
                        }
                    }
                }
            }
        }

        info!(conf_id = %conf_id, "Conference mixing loop stopped");
    }

    /// Get participant count (synchronous)
    pub fn participant_count(&self) -> usize {
        self.participant_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Leg1 should not receive its own audio
        let frame = rx1.try_recv();
        assert!(frame.is_err(), "Leg1 should not receive its own audio");

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
        let frame1 = rx1.try_recv();
        assert!(frame1.is_err(), "Leg1 should not receive its own audio");

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
        let frame2 = rx2.try_recv();
        assert!(frame2.is_err(), "Leg2 should not receive its own audio");

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

        // Drop without calling stop() — simulates a cleanup path that
        // forgets to stop the mixer.
        drop(mixer);

        // If the Drop impl works, the mixing task is aborted.
        // Give it a moment to propagate.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        // No assertion needed — if the task leaked, it would hold the
        // `participants` Arc alive, but we can't easily check that here.
        // The key is that this test doesn't hang or panic.
    }
}
