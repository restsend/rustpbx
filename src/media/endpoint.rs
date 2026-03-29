use crate::media::source::{BridgeInputAdapter, BridgeInputConfig, FileInput, IdleInput, PeerInputSource};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{MediaKind as TrackMediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use rustrtc::{MediaKind, PeerConnection, RtpSender};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

/// Outbound media for one peer.
///
/// Contract: this is a single-producer output. Source/provider switching keeps
/// one stable sender-facing output object while replacing the active input
/// behind it. `PeerOutput` itself is the `MediaStreamTrack` polled by the
/// sender on this leg.
pub struct PeerOutput {
    id: String,
    kind: TrackMediaKind,
    input_tx: watch::Sender<Arc<PeerInput>>,
    input_rx: Mutex<watch::Receiver<Arc<PeerInput>>>,
    mark_next: std::sync::atomic::AtomicBool,
}

impl PeerOutput {
    pub fn attach(track_id: &str, target_pc: &PeerConnection) -> Result<Arc<Self>> {
        let idle_input = Arc::new(PeerInput::idle());
        let (input_tx, input_rx) = watch::channel(idle_input);
        let output = Arc::new(Self {
            id: track_id.to_string(),
            kind: TrackMediaKind::Audio,
            input_tx,
            input_rx: Mutex::new(input_rx),
            mark_next: std::sync::atomic::AtomicBool::new(true),
        });

        let target_transceiver = target_pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == MediaKind::Audio)
            .ok_or_else(|| anyhow!("no audio transceiver on target pc"))?;

        let existing_sender = target_transceiver
            .sender()
            .ok_or_else(|| anyhow!("no sender on target audio transceiver"))?;

        let sender = RtpSender::builder(
            output.clone() as Arc<dyn MediaStreamTrack>,
            existing_sender.ssrc(),
        )
        .stream_id(existing_sender.stream_id().to_string())
        .params(existing_sender.params())
        .build();

        target_transceiver.set_sender(Some(sender));

        Ok(output)
    }

    pub fn set_input(&self, input: PeerInput) {
        self.mark_next
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.input_tx.send(Arc::new(input));
    }

    pub fn clear_input(&self) {
        self.set_input(PeerInput::idle());
    }
}

#[async_trait]
impl MediaStreamTrack for PeerOutput {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> TrackMediaKind {
        self.kind
    }

    fn state(&self) -> TrackState {
        TrackState::Live
    }

    async fn recv(&self) -> MediaResult<MediaSample> {
        let mut input_rx = self.input_rx.lock().await;
        loop {
            let input = input_rx.borrow_and_update().clone();
            tokio::select! {
                result = input.recv() => {
                    let mut sample = result?;
                    if let MediaSample::Audio(frame) = &mut sample {
                        if self.mark_next.swap(false, std::sync::atomic::Ordering::Relaxed) {
                            frame.marker = true;
                        }
                    }
                    return Ok(sample);
                },
                changed = input_rx.changed() => {
                    if changed.is_err() {
                        continue;
                    }
                    continue;
                },
            }
        }
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        Ok(())
    }
}

#[derive(Clone)]
enum PeerInputKind {
    Track(Arc<dyn MediaStreamTrack>),
    Source(Arc<Mutex<Box<dyn PeerInputSource>>>),
}

/// Inbound media for one peer.
///
/// Contract: this is a single-consumer input. The inner track may be owned
/// through `Arc`, but the media abstraction assumes one logical consumer for
/// each peer input at a time.
#[derive(Clone)]
pub struct PeerInput {
    kind: PeerInputKind,
}

impl PeerInput {
    pub fn from_track(track: Arc<dyn MediaStreamTrack>) -> Self {
        Self {
            kind: PeerInputKind::Track(track),
        }
    }

    pub fn from_source(source: Box<dyn PeerInputSource>) -> Self {
        Self {
            kind: PeerInputKind::Source(Arc::new(Mutex::new(source))),
        }
    }

    pub fn idle() -> Self {
        Self::from_source(Box::new(IdleInput::new()))
    }

    pub fn from_file(input: FileInput) -> Self {
        Self::from_source(Box::new(input))
    }

    pub async fn recv(&self) -> MediaResult<MediaSample> {
        match &self.kind {
            PeerInputKind::Track(track) => track.recv().await,
            PeerInputKind::Source(source) => {
                let mut source = source.lock().await;
                source.recv().await
            }
        }
    }

    pub fn adapted_for_output(&self, config: BridgeInputConfig) -> Self {
        match &self.kind {
            PeerInputKind::Track(track) => {
                Self::from_source(Box::new(BridgeInputAdapter::new(track.clone(), config)))
            }
            PeerInputKind::Source(_) => {
                if config.is_passthrough() {
                    self.clone()
                } else {
                    panic!("cannot apply directional adaptation to a non-track peer input");
                }
            }
        }
    }
}

/// Extract the receiver track from a PeerConnection's audio transceiver.
pub fn receiver_track_for_pc(pc: &PeerConnection) -> Result<Arc<dyn MediaStreamTrack>> {
    let transceiver = pc
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == MediaKind::Audio)
        .ok_or_else(|| anyhow!("no audio transceiver on pc"))?;

    let receiver = transceiver
        .receiver()
        .ok_or_else(|| anyhow!("no receiver on audio transceiver"))?;

    Ok(receiver.track())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rustrtc::media::error::MediaError;
    use rustrtc::media::frame::AudioFrame;
    use std::collections::VecDeque;
    use std::sync::Arc;

    struct FakeTrack {
        id: &'static str,
        samples: tokio::sync::Mutex<VecDeque<MediaSample>>,
    }

    #[async_trait]
    impl MediaStreamTrack for FakeTrack {
        fn id(&self) -> &str {
            self.id
        }

        fn kind(&self) -> TrackMediaKind {
            TrackMediaKind::Audio
        }

        fn state(&self) -> TrackState {
            TrackState::Live
        }

        async fn recv(&self) -> MediaResult<MediaSample> {
            self.samples
                .lock()
                .await
                .pop_front()
                .ok_or(MediaError::EndOfStream)
        }

        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
        }
    }

    fn frame(
        timestamp: u32,
        sequence_number: u16,
        payload: &[u8],
    ) -> AudioFrame {
        AudioFrame {
            rtp_timestamp: timestamp,
            clock_rate: 8000,
            data: Bytes::copy_from_slice(payload),
            sequence_number: Some(sequence_number),
            payload_type: Some(101),
            marker: false,
            raw_packet: None,
            source_addr: None,
        }
    }

    #[tokio::test]
    async fn peer_output_marks_first_packet_after_input_switch() {
        let first_input = PeerInput::from_track(Arc::new(FakeTrack {
            id: "track-a",
            samples: tokio::sync::Mutex::new(VecDeque::from([MediaSample::Audio(frame(
                1000,
                1,
                &[0u8; 4],
            ))])),
        }));
        let second_input = PeerInput::from_track(Arc::new(FakeTrack {
            id: "track-b",
            samples: tokio::sync::Mutex::new(VecDeque::from([MediaSample::Audio(frame(
                1160,
                2,
                &[0u8; 4],
            ))])),
        }));
        let idle_input = Arc::new(PeerInput::idle());
        let (input_tx, input_rx) = watch::channel(idle_input);
        let output = PeerOutput {
            id: "test-output".to_string(),
            kind: TrackMediaKind::Audio,
            input_tx,
            input_rx: Mutex::new(input_rx),
            mark_next: std::sync::atomic::AtomicBool::new(true),
        };

        output.set_input(first_input);
        let first = output.recv().await.unwrap();
        let MediaSample::Audio(first) = first else {
            panic!("expected audio sample");
        };
        assert!(first.marker);

        output.set_input(second_input);
        let second = output.recv().await.unwrap();
        let MediaSample::Audio(second) = second else {
            panic!("expected audio sample");
        };
        assert!(second.marker);
    }
}
