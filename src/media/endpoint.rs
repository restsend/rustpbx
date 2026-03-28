use crate::media::source::{
    AudioMapping, DtmfMapping, FileInput, IdleInput, MappedTrackInput, PeerInputSource,
    TranscodeSpec,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaKind as TrackMediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use rustrtc::{MediaKind, PeerConnection, RtpSender};
use std::sync::Arc;
use tokio::sync::{Mutex, watch};

struct OutputRtpState {
    next_output_timestamp: u32,
    next_output_sequence: u16,
}

impl Default for OutputRtpState {
    fn default() -> Self {
        Self {
            next_output_timestamp: rand::random(),
            next_output_sequence: rand::random(),
        }
    }
}

impl OutputRtpState {
    fn rewrite(&mut self, frame: &mut AudioFrame) {
        let step = (frame.clock_rate / 50).max(1);

        frame.rtp_timestamp = self.next_output_timestamp;
        frame.sequence_number = Some(self.next_output_sequence);

        self.next_output_timestamp = self.next_output_timestamp.wrapping_add(step);
        self.next_output_sequence = self.next_output_sequence.wrapping_add(1);
    }
}

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
    rtp_state: std::sync::Mutex<OutputRtpState>,
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
            rtp_state: std::sync::Mutex::new(OutputRtpState::default()),
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
                        if let Ok(mut rtp_state) = self.rtp_state.lock() {
                            rtp_state.rewrite(frame);
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

    pub fn adapted_for_output(
        &self,
        audio_mapping: Option<AudioMapping>,
        dtmf_mapping: Option<DtmfMapping>,
        transcode: Option<TranscodeSpec>,
    ) -> Self {
        match &self.kind {
            PeerInputKind::Track(track) => Self::from_source(Box::new(MappedTrackInput::new(
                track.clone(),
                audio_mapping,
                dtmf_mapping,
                transcode,
            ))),
            PeerInputKind::Source(_) => {
                if audio_mapping.is_none() && dtmf_mapping.is_none() && transcode.is_none() {
                    self.clone()
                } else {
                    panic!("cannot apply directional mapping to a non-track peer input");
                }
            }
        }
    }
}

/// Extract the receiver track from a PeerConnection's audio transceiver.
pub fn receiver_track_for_pc(
    pc: &PeerConnection,
) -> Result<Arc<dyn MediaStreamTrack>> {
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
