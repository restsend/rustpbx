use crate::media::recorder::{Leg, Recorder};
use crate::media::source::{
    AudioMapping, DtmfMapping, FileOutputProvider, IdleOutputProvider, OutputProvider,
    PeerInputProvider, TranscodeSpec,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaKind as TrackMediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use rustrtc::{MediaKind, PeerConnection, RtpSender};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, watch};

struct RecordingOutputProvider {
    inner: Box<dyn OutputProvider>,
    binding: RecorderBinding,
}

#[async_trait]
impl OutputProvider for RecordingOutputProvider {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        let sample = self.inner.recv().await?;
        if let Ok(mut guard) = self.binding.recorder.lock() {
            if let Some(recorder) = guard.as_mut() {
                let _ = recorder.write_sample(
                    self.binding.leg,
                    &sample,
                    self.binding.dtmf_pt,
                    self.binding.dtmf_clock_rate,
                    self.binding.codec_hint,
                );
            }
        }
        Ok(sample)
    }
}

struct OutputRtpState {
    next_output_timestamp: u32,
    next_output_sequence: u16,
    last_input_timestamp: Option<u32>,
    last_input_sequence: Option<u16>,
    last_clock_rate: Option<u32>,
}

impl Default for OutputRtpState {
    fn default() -> Self {
        Self {
            next_output_timestamp: rand::random(),
            next_output_sequence: rand::random(),
            last_input_timestamp: None,
            last_input_sequence: None,
            last_clock_rate: None,
        }
    }
}

impl OutputRtpState {
    fn estimate_step(frame: &AudioFrame) -> u32 {
        (frame.clock_rate / 50).max(1)
    }

    fn rewrite(&mut self, frame: &mut AudioFrame) {
        let input_timestamp = frame.rtp_timestamp;
        let input_sequence = frame.sequence_number;
        let step = match (
            self.last_input_timestamp,
            self.last_input_sequence,
            self.last_clock_rate,
            input_sequence,
        ) {
            (Some(last_ts), Some(last_seq), Some(last_rate), Some(input_seq))
                if last_rate == frame.clock_rate && input_seq == last_seq.wrapping_add(1) =>
            {
                let delta = input_timestamp.wrapping_sub(last_ts);
                if delta > 0 && delta < frame.clock_rate.saturating_mul(5) {
                    delta
                } else {
                    Self::estimate_step(frame)
                }
            }
            _ => Self::estimate_step(frame),
        };

        frame.rtp_timestamp = self.next_output_timestamp;
        frame.sequence_number = Some(self.next_output_sequence);

        self.next_output_timestamp = self.next_output_timestamp.wrapping_add(step);
        self.next_output_sequence = self.next_output_sequence.wrapping_add(1);
        self.last_input_timestamp = Some(input_timestamp);
        self.last_input_sequence = input_sequence;
        self.last_clock_rate = Some(frame.clock_rate);
    }
}

pub struct PeerOutput {
    id: String,
    kind: TrackMediaKind,
    provider_tx: watch::Sender<Arc<Mutex<Box<dyn OutputProvider>>>>,
    provider_rx: Mutex<watch::Receiver<Arc<Mutex<Box<dyn OutputProvider>>>>>,
    rtp_state: StdMutex<OutputRtpState>,
}

impl PeerOutput {
    pub fn attach(track_id: &str, target_pc: &PeerConnection) -> Result<Arc<Self>> {
        let idle_provider: Arc<Mutex<Box<dyn OutputProvider>>> =
            Arc::new(Mutex::new(Box::new(IdleOutputProvider::new())));
        let (provider_tx, provider_rx) = watch::channel(idle_provider);
        let output = Arc::new(Self {
            id: track_id.to_string(),
            kind: TrackMediaKind::Audio,
            provider_tx,
            provider_rx: Mutex::new(provider_rx),
            rtp_state: StdMutex::new(OutputRtpState::default()),
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

    pub fn install_provider(&self, provider: Box<dyn OutputProvider>) {
        let _ = self.provider_tx.send(Arc::new(Mutex::new(provider)));
    }

    pub fn clear_provider(&self) {
        self.install_provider(Box::new(IdleOutputProvider::new()));
    }

    pub fn install_file_provider(&self, provider: FileOutputProvider) {
        self.install_provider(Box::new(provider));
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
        let mut provider_rx = self.provider_rx.lock().await;
        loop {
            let provider = provider_rx.borrow_and_update().clone();
            let mut provider = provider.lock().await;
            tokio::select! {
                result = provider.recv() => {
                    let mut sample = result?;
                    if let MediaSample::Audio(frame) = &mut sample {
                        if let Ok(mut rtp_state) = self.rtp_state.lock() {
                            rtp_state.rewrite(frame);
                        }
                    }
                    return Ok(sample);
                },
                changed = provider_rx.changed() => {
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
struct RecorderBinding {
    recorder: Arc<StdMutex<Option<Recorder>>>,
    leg: Leg,
    dtmf_pt: Option<u8>,
    dtmf_clock_rate: Option<u32>,
    codec_hint: Option<CodecType>,
}

pub struct PeerInput {
    track: Arc<dyn MediaStreamTrack>,
    recorder: Option<RecorderBinding>,
}

impl PeerInput {
    pub fn new(track: Arc<dyn MediaStreamTrack>) -> Self {
        Self {
            track,
            recorder: None,
        }
    }

    pub fn track(&self) -> Arc<dyn MediaStreamTrack> {
        self.track.clone()
    }

    pub fn set_recorder(
        &mut self,
        recorder: Arc<StdMutex<Option<Recorder>>>,
        leg: Leg,
        dtmf_pt: Option<u8>,
        dtmf_clock_rate: Option<u32>,
        codec_hint: Option<CodecType>,
    ) {
        self.recorder = Some(RecorderBinding {
            recorder,
            leg,
            dtmf_pt,
            dtmf_clock_rate,
            codec_hint,
        });
    }

    pub fn provider_for_output(
        &self,
        audio_mapping: Option<AudioMapping>,
        dtmf_mapping: Option<DtmfMapping>,
        transcode: Option<TranscodeSpec>,
    ) -> Box<dyn OutputProvider> {
        let provider: Box<dyn OutputProvider> = Box::new(PeerInputProvider::new(
            self.track.clone(),
            audio_mapping,
            dtmf_mapping,
            transcode,
        ));

        if let Some(binding) = &self.recorder {
            Box::new(RecordingOutputProvider {
                inner: provider,
                binding: binding.clone(),
            })
        } else {
            provider
        }
    }
}

pub struct MediaPeer {
    input: PeerInput,
    output: Arc<PeerOutput>,
}

impl MediaPeer {
    pub fn new(track: Arc<dyn MediaStreamTrack>, output: Arc<PeerOutput>) -> Self {
        Self {
            input: PeerInput::new(track),
            output,
        }
    }

    pub fn input(&self) -> &PeerInput {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut PeerInput {
        &mut self.input
    }

    pub fn output(&self) -> &Arc<PeerOutput> {
        &self.output
    }

    pub fn set_recorder(
        &mut self,
        recorder: Arc<StdMutex<Option<Recorder>>>,
        leg: Leg,
        dtmf_pt: Option<u8>,
        dtmf_clock_rate: Option<u32>,
        codec_hint: Option<CodecType>,
    ) {
        self.input
            .set_recorder(recorder, leg, dtmf_pt, dtmf_clock_rate, codec_hint);
    }
}
