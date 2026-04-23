//! Conference Media Bridge
//!
//! Bridges conference mixed audio output (output_rx) to a SipSession's media track.
//! This enables participants in a conference to hear the mixed audio from all other participants.
//!
//! Now supports full-duplex bridging:
//! - Forward loop: conference mixer output → SIP track (mixed audio to participant)
//! - Reverse loop: SIP track → conference mixer input (participant audio to mixer)

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, trace, warn};

use crate::call::domain::LegId;
use crate::call::runtime::ConferenceManager;
use crate::media::conference_mixer::AudioFrame;

/// Trait for sending media samples to a track.
pub trait AudioSender: Send + Sync {
    fn send(
        &self,
        sample: rustrtc::media::MediaSample,
    ) -> impl std::future::Future<Output = Result<(), mpsc::error::SendError<rustrtc::media::MediaSample>>> + Send;
}

impl AudioSender for tokio::sync::mpsc::Sender<rustrtc::media::MediaSample> {
    async fn send(
        &self,
        sample: rustrtc::media::MediaSample,
    ) -> Result<(), mpsc::error::SendError<rustrtc::media::MediaSample>> {
        self.send(sample).await
    }
}

/// Trait for receiving decoded PCM audio from a track.
pub trait AudioReceiver: Send + Sync {
    /// Receive the next PCM audio frame.
    /// Returns None when the receiver is closed.
    fn recv(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PcmAudioFrame>> + Send + '_>>;
}

/// Decoded PCM audio frame from a participant.
#[derive(Debug, Clone)]
pub struct PcmAudioFrame {
    /// Raw PCM samples (16-bit signed, mono)
    pub samples: Vec<i16>,
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Timestamp in samples
    pub timestamp: u64,
}

impl PcmAudioFrame {
    pub fn new(samples: Vec<i16>, sample_rate: u32) -> Self {
        Self {
            samples,
            sample_rate,
            timestamp: 0,
        }
    }
}

/// Bridges conference audio to a media track.
pub struct ConferenceMediaBridge {
    conference_manager: Arc<ConferenceManager>,
}

impl ConferenceMediaBridge {
    pub fn new(conference_manager: Arc<ConferenceManager>) -> Self {
        Self { conference_manager }
    }

    /// Start bridging conference mixed audio to a leg's media path (output only).
    ///
    /// This spawns a background task that continuously reads mixed audio from the conference
    /// and injects it into the leg's media track via the provided audio sender.
    pub async fn start_bridge<S>(
        &self,
        conf_id: &str,
        leg_id: &LegId,
        audio_sender: S,
    ) -> anyhow::Result<ConferenceBridgeHandle>
    where
        S: AudioSender + Send + Sync + 'static,
    {
        let _conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id);
        
        // Take the output_rx for this leg
        let output_rx = self.conference_manager
            .take_participant_output_rx(leg_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("No output_rx found for leg {} in conference {}", leg_id, conf_id))?;

        info!(
            conf_id = %conf_id,
            leg_id = %leg_id,
            "Starting conference media bridge (output only)"
        );
        crate::metrics::conference::created();

        // Spawn background task to forward mixed audio
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();
        
        let leg_id_clone = leg_id.clone();
        let conf_id_string = conf_id.to_string();
        let handle = tokio::spawn(async move {
            Self::forward_loop(
                output_rx,
                audio_sender,
                leg_id_clone,
                conf_id_string,
                cancel_token_clone,
            ).await;
        });

        Ok(ConferenceBridgeHandle {
            _tasks: vec![handle],
            cancel_token,
        })
    }

    /// Start full-duplex bridge for a leg.
    ///
    /// This creates both forward and reverse loops:
    /// - Forward: conference mixer output → SIP track (mixed audio to participant)
    /// - Reverse: SIP track → conference mixer input (participant audio to mixer)
    pub async fn start_bridge_full_duplex<S>(
        &self,
        conf_id: &str,
        leg_id: &LegId,
        audio_sender: S,
        audio_receiver: Box<dyn AudioReceiver>,
    ) -> anyhow::Result<ConferenceBridgeHandle>
    where
        S: AudioSender + Send + Sync + 'static,
    {
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id);
        
        // Add participant to conference and get channels
        let channels = self.conference_manager
            .add_participant(&conf_id_obj, leg_id.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to add participant to conference: {}", e))?;
        
        let input_tx = channels.input_tx;
        
        // Take the output_rx for this leg
        let output_rx = self.conference_manager
            .take_participant_output_rx(leg_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("No output_rx found for leg {} in conference {}", leg_id, conf_id))?;

        info!(
            conf_id = %conf_id,
            leg_id = %leg_id,
            "Starting full-duplex conference media bridge"
        );
        crate::metrics::conference::created();

        let cancel_token = tokio_util::sync::CancellationToken::new();
        
        // Spawn forward loop: conference → SIP
        let forward_cancel = cancel_token.child_token();
        let leg_id_forward = leg_id.clone();
        let conf_id_forward = conf_id.to_string();
        let forward_handle = tokio::spawn(async move {
            Self::forward_loop(
                output_rx,
                audio_sender,
                leg_id_forward,
                conf_id_forward,
                forward_cancel,
            ).await;
        });

        // Spawn reverse loop: SIP → conference
        let reverse_cancel = cancel_token.child_token();
        let leg_id_reverse = leg_id.clone();
        let conf_id_reverse = conf_id.to_string();
        let reverse_handle = tokio::spawn(async move {
            Self::reverse_loop(
                audio_receiver,
                input_tx,
                leg_id_reverse,
                conf_id_reverse,
                reverse_cancel,
            ).await;
        });

        Ok(ConferenceBridgeHandle {
            _tasks: vec![forward_handle, reverse_handle],
            cancel_token,
        })
    }

    /// Forward loop: read mixed audio from conference, encode to RTP, and send to media track.
    pub async fn forward_loop<S>(
        mut output_rx: mpsc::Receiver<AudioFrame>,
        audio_sender: S,
        leg_id: LegId,
        conf_id: String,
        cancel_token: tokio_util::sync::CancellationToken,
    ) where
        S: AudioSender + Send + Sync + 'static,
    {
        use audio_codec::create_encoder;
        use rustrtc::media::{AudioFrame as RtcAudioFrame, MediaSample};

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            "Conference media bridge forward loop started"
        );

        // Initialize PCMU encoder for 8kHz mono (default, can be overridden per-leg)
        let mut encoder = create_encoder(audio_codec::CodecType::PCMU);
        let mut rtp_timestamp: u32 = rand::random();
        let mut sequence_number: u16 = rand::random();
        let sample_rate = 8000u32;
        let interval_ms = 20u64;
        let samples_per_frame = (sample_rate * interval_ms as u32 / 1000) as usize;
        let rtp_ticks_per_frame = sample_rate * interval_ms as u32 / 1000;

        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    info!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Conference media bridge forward loop cancelled"
                    );
                    break;
                }
                Some(frame) = output_rx.recv() => {
                    // Resample to 8kHz if needed using linear interpolation
                    let pcm_samples = if frame.sample_rate == sample_rate {
                        frame.samples
                    } else {
                        resample_linear(
                            &frame.samples,
                            frame.sample_rate,
                            sample_rate,
                        )
                    };

                    // Process in chunks of samples_per_frame
                    for chunk in pcm_samples.chunks(samples_per_frame) {
                        let chunk_to_encode = if chunk.len() < samples_per_frame {
                            // Pad with silence if needed
                            let mut padded = vec![0i16; samples_per_frame];
                            padded[..chunk.len()].copy_from_slice(chunk);
                            padded
                        } else {
                            chunk.to_vec()
                        };

                        let encoded = encoder.encode(&chunk_to_encode);
                        let rtc_frame = RtcAudioFrame {
                            rtp_timestamp,
                            clock_rate: sample_rate,
                            data: encoded.into(),
                            sequence_number: Some(sequence_number),
                            payload_type: Some(0), // PCMU = 0
                            marker: false,
                            header_extension: None,
                            raw_packet: None,
                            source_addr: None,
                        };

                        let bytes_sent = chunk_to_encode.len() * 2; // i16 = 2 bytes
                        if let Err(e) = audio_sender.send(MediaSample::Audio(rtc_frame)).await {
                            warn!(
                                leg_id = %leg_id,
                                error = %e,
                                "Failed to send conference audio to media track"
                            );
                            return;
                        }
                        crate::metrics::conference::media_injected_bytes(&conf_id, bytes_sent as u64);

                        rtp_timestamp = rtp_timestamp.wrapping_add(rtp_ticks_per_frame);
                        sequence_number = sequence_number.wrapping_add(1);
                    }

                    trace!(
                        leg_id = %leg_id,
                        samples = pcm_samples.len(),
                        original_sample_rate = frame.sample_rate,
                        "Encoded and sent mixed audio frame from conference"
                    );
                }
                else => {
                    warn!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Conference output_rx closed"
                    );
                    break;
                }
            }
        }

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            "Conference media bridge forward loop ended"
        );
    }

    /// Reverse loop: read decoded PCM from audio receiver, and send to conference mixer input.
    pub async fn reverse_loop(
        mut audio_receiver: Box<dyn AudioReceiver>,
        input_tx: mpsc::Sender<AudioFrame>,
        leg_id: LegId,
        conf_id: String,
        cancel_token: tokio_util::sync::CancellationToken,
    ) {
        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            "Conference media bridge reverse loop started"
        );

        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    info!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Conference media bridge reverse loop cancelled"
                    );
                    break;
                }
                Some(pcm_frame) = audio_receiver.recv() => {
                    let sample_count = pcm_frame.samples.len();
                    let sample_rate = pcm_frame.sample_rate;
                    let audio_frame = AudioFrame::new(pcm_frame.samples, sample_rate);
                    
                    if let Err(e) = input_tx.send(audio_frame).await {
                        warn!(
                            leg_id = %leg_id,
                            conf_id = %conf_id,
                            error = %e,
                            "Failed to send audio to conference mixer input"
                        );
                        break;
                    }

                    trace!(
                        leg_id = %leg_id,
                        samples = sample_count,
                        sample_rate = sample_rate,
                        "Sent participant audio to conference mixer"
                    );
                }
                else => {
                    info!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Audio receiver closed, stopping reverse loop"
                    );
                    break;
                }
            }
        }

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            "Conference media bridge reverse loop ended"
        );
    }
}

/// Resample audio using linear interpolation.
fn resample_linear(samples: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    if src_rate == dst_rate {
        return samples.to_vec();
    }

    let ratio = src_rate as f32 / dst_rate as f32;
    let new_len = (samples.len() as f32 / ratio) as usize;
    let mut result = Vec::with_capacity(new_len);

    for i in 0..new_len {
        let src_idx = i as f32 * ratio;
        let src_idx_floor = src_idx.floor() as usize;
        let src_idx_ceil = (src_idx.ceil() as usize).min(samples.len().saturating_sub(1));
        let frac = src_idx - src_idx.floor();

        let sample = if src_idx_floor == src_idx_ceil {
            samples[src_idx_floor]
        } else {
            let s0 = samples[src_idx_floor] as f32;
            let s1 = samples[src_idx_ceil] as f32;
            (s0 + frac * (s1 - s0)) as i16
        };
        result.push(sample);
    }

    result
}

/// Handle to control a conference media bridge.
pub struct ConferenceBridgeHandle {
    _tasks: Vec<tokio::task::JoinHandle<()>>,
    pub cancel_token: tokio_util::sync::CancellationToken,
}

impl ConferenceBridgeHandle {
    /// Stop the bridge.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }
}

/// Per-session conference bridge state.
pub struct SessionConferenceBridge {
    pub bridge_handle: Option<ConferenceBridgeHandle>,
    pub conf_id: Option<String>,
}

impl SessionConferenceBridge {
    pub fn new() -> Self {
        Self {
            bridge_handle: None,
            conf_id: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.bridge_handle.is_some()
    }

    pub fn stop_bridge(&mut self) {
        if let Some(ref handle) = self.bridge_handle {
            handle.stop();
        }
        self.bridge_handle = None;
        self.conf_id = None;
    }
}

impl Default for SessionConferenceBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::domain::LegId;
    use crate::media::conference_mixer::AudioFrame;
    use rustrtc::media::MediaSample;

    /// Mock audio sender that records all sent samples.
    struct MockAudioSender {
        samples: std::sync::Arc<tokio::sync::Mutex<Vec<MediaSample>>>,
    }

    impl MockAudioSender {
        fn new() -> Self {
            Self {
                samples: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            }
        }

        async fn get_samples(&self) -> Vec<MediaSample> {
            self.samples.lock().await.clone()
        }
    }

    impl AudioSender for MockAudioSender {
        async fn send(
            &self,
            sample: rustrtc::media::MediaSample,
        ) -> Result<(), mpsc::error::SendError<rustrtc::media::MediaSample>> {
            self.samples.lock().await.push(sample);
            Ok(())
        }
    }

    /// Mock audio receiver that provides predefined PCM frames.
    struct MockAudioReceiver {
        frames: Vec<PcmAudioFrame>,
        index: usize,
    }

    impl MockAudioReceiver {
        fn new(frames: Vec<PcmAudioFrame>) -> Self {
            Self { frames, index: 0 }
        }
    }

    impl AudioReceiver for MockAudioReceiver {
        fn recv(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PcmAudioFrame>> + Send + '_>> {
            Box::pin(async move {
                if self.index < self.frames.len() {
                    let frame = self.frames[self.index].clone();
                    self.index += 1;
                    Some(frame)
                } else {
                    None
                }
            })
        }
    }

    #[tokio::test]
    async fn test_conference_bridge_creation() {
        let conf_mgr = Arc::new(ConferenceManager::new());
        let _bridge = ConferenceMediaBridge::new(conf_mgr);
    }

    #[tokio::test]
    async fn test_start_bridge_requires_output_rx() {
        let conf_mgr = Arc::new(ConferenceManager::new());
        let bridge = ConferenceMediaBridge::new(conf_mgr);
        let leg_id = LegId::new("test-leg");
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        let result = bridge.start_bridge("conf-1", &leg_id, tx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bridge_handle_stop() {
        let conf_mgr = Arc::new(ConferenceManager::new());
        let bridge = ConferenceMediaBridge::new(conf_mgr);
        let leg_id = LegId::new("test-leg");
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        if let Ok(handle) = bridge.start_bridge("conf-1", &leg_id, tx).await {
            handle.stop();
        }
    }

    #[tokio::test]
    async fn test_forward_loop_audio_encoding() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_clone = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Spawn forward loop
        let handle = tokio::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_clone,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
            )
            .await;
        });

        // Send an audio frame at 8kHz (matching the encoder)
        let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();
        tx.send(AudioFrame {
            sample_rate: 8000,
            samples: samples.clone(),
            timestamp: 0,
        })
        .await
        .unwrap();

        // Give it time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Cancel and wait
        cancel.cancel();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            handle,
        )
        .await;

        // Verify audio was encoded and sent
        let sent = sender.get_samples().await;
        assert!(
            !sent.is_empty(),
            "Expected audio samples to be sent after encoding"
        );

        // Verify the first sample is an Audio variant
        match &sent[0] {
            MediaSample::Audio(frame) => {
                assert_eq!(frame.clock_rate, 8000);
                assert_eq!(frame.payload_type, Some(0)); // PCMU
                assert!(!frame.data.is_empty());
            }
            _ => panic!("Expected Audio sample, got {:?}", sent[0]),
        }
    }

    #[tokio::test]
    async fn test_forward_loop_resampling() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_clone = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_clone,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
            )
            .await;
        });

        // Send audio at 16kHz (needs decimation to 8kHz)
        let samples: Vec<i16> = (0..320).map(|i| (i as i16 * 50) % 32767).collect();
        tx.send(AudioFrame {
            sample_rate: 16000,
            samples,
            timestamp: 0,
        })
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            handle,
        )
        .await;

        let sent = sender.get_samples().await;
        assert!(!sent.is_empty(), "Expected resampled audio to be sent");
    }

    #[tokio::test]
    async fn test_forward_loop_sequence_increment() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_clone = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_clone,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
            )
            .await;
        });

        // Send two frames to verify sequence numbers increment
        for _ in 0..2 {
            let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();
            tx.send(AudioFrame {
                sample_rate: 8000,
                samples,
                timestamp: 0,
            })
            .await
            .unwrap();
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            handle,
        )
        .await;

        let sent = sender.get_samples().await;
        assert!(sent.len() >= 2, "Expected at least 2 audio packets");

        // Verify sequence numbers increment
        let seq1 = match &sent[0] {
            MediaSample::Audio(f) => f.sequence_number.unwrap(),
            _ => panic!("Expected Audio"),
        };
        let seq2 = match &sent[1] {
            MediaSample::Audio(f) => f.sequence_number.unwrap(),
            _ => panic!("Expected Audio"),
        };
        assert_eq!(
            seq2,
            seq1.wrapping_add(1),
            "Sequence numbers should increment by 1"
        );
    }

    #[tokio::test]
    async fn test_reverse_loop_sends_to_mixer() {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(10);
        let pcm_frames = vec![
            PcmAudioFrame::new(vec![1000i16; 160], 8000),
            PcmAudioFrame::new(vec![2000i16; 160], 8000),
        ];
        let receiver = MockAudioReceiver::new(pcm_frames);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            ConferenceMediaBridge::reverse_loop(
                Box::new(receiver),
                input_tx,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
            )
            .await;
        });

        // Wait for frames to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            handle,
        )
        .await;

        // Verify frames were sent to mixer input
        let mut received_count = 0;
        while let Ok(frame) = input_rx.try_recv() {
            received_count += 1;
            assert_eq!(frame.samples.len(), 160);
            assert_eq!(frame.sample_rate, 8000);
        }
        assert_eq!(received_count, 2, "Expected 2 frames to be sent to mixer input");
    }

    #[tokio::test]
    async fn test_full_duplex_bridge() {
        let conf_mgr = Arc::new(ConferenceManager::new());
        let bridge = ConferenceMediaBridge::new(conf_mgr.clone());
        
        // Create conference first
        conf_mgr.create_conference("conf-1".into(), None).await.unwrap();
        
        let leg_id = LegId::new("test-leg");
        let sender = MockAudioSender::new();
        let receiver = MockAudioReceiver::new(vec![
            PcmAudioFrame::new(vec![1000i16; 160], 8000),
        ]);
        
        let handle = bridge.start_bridge_full_duplex("conf-1", &leg_id, sender, Box::new(receiver)).await;
        assert!(handle.is_ok(), "Full-duplex bridge should start successfully");
        
        let handle = handle.unwrap();
        handle.stop();
    }

    #[tokio::test]
    async fn test_session_conference_bridge_lifecycle() {
        let mut session_bridge = SessionConferenceBridge::new();
        assert!(!session_bridge.is_active());

        session_bridge.conf_id = Some("conf-1".to_string());
        // Note: we can't set bridge_handle without a real JoinHandle,
        // but we can test the stop logic
        session_bridge.stop_bridge();
        assert!(!session_bridge.is_active());
        assert!(session_bridge.conf_id.is_none());
    }

    #[tokio::test]
    async fn test_forward_loop_cancel_immediately() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<AudioFrame>(10);
        let sender = MockAudioSender::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
            )
            .await;
        });

        // Cancel immediately
        cancel.cancel();
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            handle,
        )
        .await;

        assert!(result.is_ok(), "Forward loop should exit cleanly on cancel");
    }
}
