use crate::media::audio_source::{AudioSource, FileAudioSource, ResamplingAudioSource};
use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
use crate::media::recorder::Leg as RecorderLeg;
use crate::media::transcoder::{Transcoder, rewrite_dtmf_duration};
use anyhow::Result;
use audio_codec::{CodecType, Encoder, create_encoder};
use parking_lot::Mutex;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};

const FRAME_MS: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackCompletion {
    pub interrupted: bool,
}

#[derive(Clone)]
pub struct RecorderTap {
    tx: mpsc::Sender<RecordedAudioSample>,
    leg: RecorderLeg,
    audio_payload_type: Option<u8>,
    codec_hint: Option<CodecType>,
}

pub struct RecordedAudioSample {
    pub leg: RecorderLeg,
    pub sample: MediaSample,
    pub codec_hint: Option<CodecType>,
}

impl RecorderTap {
    pub fn new(
        tx: mpsc::Sender<RecordedAudioSample>,
        leg: RecorderLeg,
        profile: NegotiatedLegProfile,
    ) -> Self {
        let audio = profile.audio;
        Self {
            tx,
            leg,
            audio_payload_type: audio.as_ref().map(|codec| codec.payload_type),
            codec_hint: audio.as_ref().map(|codec| codec.codec),
        }
    }

    fn record(&self, sample: MediaSample) {
        let MediaSample::Audio(frame) = &sample else {
            return;
        };

        if let Some(payload_type) = self.audio_payload_type
            && frame.payload_type != Some(payload_type) {
                return;
            }

        let _ = self.tx.try_send(RecordedAudioSample {
            leg: self.leg,
            sample,
            codec_hint: self.codec_hint,
        });
    }
}

type DtmfObserver = Arc<dyn Fn(char) + Send + Sync>;

#[derive(Clone, Default)]
pub struct AudioInputTap {
    recorder: Option<RecorderTap>,
    dtmf_observer: Option<DtmfObserver>,
}

impl AudioInputTap {
    pub fn with_recorder(mut self, recorder: RecorderTap) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn with_dtmf_observer<F>(mut self, observer: F) -> Self
    where
        F: Fn(char) + Send + Sync + 'static,
    {
        self.dtmf_observer = Some(Arc::new(observer));
        self
    }

    pub fn with_dtmf_observer_arc(mut self, observer: Arc<dyn Fn(char) + Send + Sync>) -> Self {
        self.dtmf_observer = Some(observer);
        self
    }
}

struct LocalInputTap {
    track: Arc<dyn MediaStreamTrack>,
    dtmf_payload_type: Option<u8>,
    recorder: Option<RecorderTap>,
    dtmf_observer: Option<DtmfObserver>,
    dtmf_detector: RtpDtmfDetector,
}

impl LocalInputTap {
    fn new(
        track: Arc<dyn MediaStreamTrack>,
        ingress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
    ) -> Self {
        Self {
            track,
            dtmf_payload_type: ingress_profile.dtmf.as_ref().map(|codec| codec.payload_type),
            recorder: tap.recorder,
            dtmf_observer: tap.dtmf_observer,
            dtmf_detector: RtpDtmfDetector::default(),
        }
    }

    async fn recv_once(&mut self) -> MediaResult<()> {
        let sample = self.track.recv().await?;
        let MediaSample::Audio(frame) = sample else {
            return Ok(());
        };

        if let Some(recorder) = &self.recorder {
            recorder.record(MediaSample::Audio(frame.clone()));
        }

        if self.dtmf_payload_type == frame.payload_type {
            let digit = self
                .dtmf_detector
                .observe(&frame.data, frame.rtp_timestamp);
            if let (Some(observer), Some(digit)) = (&self.dtmf_observer, digit) {
                observer(digit);
            }
        }

        Ok(())
    }
}

struct PlaybackCompletionTx {
    tx: Option<oneshot::Sender<PlaybackCompletion>>,
}

impl PlaybackCompletionTx {
    fn new(tx: Option<oneshot::Sender<PlaybackCompletion>>) -> Self {
        Self { tx }
    }

    fn complete(&mut self, interrupted: bool) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(PlaybackCompletion { interrupted });
        }
    }
}

#[derive(Clone, Debug)]
struct AudioMapping {
    source_pt: u8,
    target_pt: u8,
    source_clock_rate: u32,
    target_clock_rate: u32,
}

#[derive(Clone, Debug)]
struct DtmfMapping {
    source_pt: u8,
    target_pt: Option<u8>,
    source_clock_rate: u32,
    target_clock_rate: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RtpDtmfEventKey {
    digit_code: u8,
    rtp_timestamp: u32,
}

#[derive(Debug, Default)]
pub struct RtpDtmfDetector {
    last_event: Option<RtpDtmfEventKey>,
}

impl RtpDtmfDetector {
    pub fn observe(&mut self, payload: &[u8], rtp_timestamp: u32) -> Option<char> {
        if payload.len() < 4 {
            return None;
        }

        let digit_code = payload[0];
        let digit = match digit_code {
            0..=9 => (b'0' + digit_code) as char,
            10 => '*',
            11 => '#',
            12..=15 => (b'A' + (digit_code - 12)) as char,
            _ => return None,
        };

        let event = RtpDtmfEventKey {
            digit_code,
            rtp_timestamp,
        };

        if self.last_event == Some(event) {
            return None;
        }

        self.last_event = Some(event);
        Some(digit)
    }
}

struct EgressClock {
    next_timestamp: u32,
    next_sequence: u16,
    marker_next: bool,
}

impl Default for EgressClock {
    fn default() -> Self {
        Self {
            next_timestamp: rand::random(),
            next_sequence: rand::random(),
            marker_next: true,
        }
    }
}

impl EgressClock {
    fn mark_source_switch(&mut self) {
        self.marker_next = true;
    }

    fn stamp(&mut self, mut frame: AudioFrame, duration_ticks: u32) -> AudioFrame {
        frame.rtp_timestamp = self.next_timestamp;
        frame.sequence_number = Some(self.next_sequence);
        frame.marker = self.marker_next;
        self.marker_next = false;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.next_timestamp = self.next_timestamp.wrapping_add(duration_ticks);
        frame
    }
}

struct TimedFrame {
    frame: AudioFrame,
    duration_ticks: u32,
}

struct SilenceSource {
    codec: NegotiatedCodec,
    encoder: Box<dyn Encoder>,
    interval: tokio::time::Interval,
    samples_per_frame: usize,
    duration_ticks: u32,
}

impl SilenceSource {
    fn new(egress_profile: NegotiatedLegProfile) -> Self {
        let codec = selected_audio_codec(&egress_profile);
        let mut interval = tokio::time::interval(Duration::from_millis(FRAME_MS as u64));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            encoder: create_encoder(codec.codec),
            samples_per_frame: samples_per_frame(codec.codec),
            duration_ticks: duration_ticks(codec.clock_rate),
            codec,
            interval,
        }
    }

    async fn next_frame(&mut self) -> TimedFrame {
        self.interval.tick().await;
        let pcm = vec![0i16; self.samples_per_frame];
        let encoded = self.encoder.encode(&pcm);
        TimedFrame {
            frame: AudioFrame {
                clock_rate: self.codec.clock_rate,
                data: encoded.into(),
                payload_type: Some(self.codec.payload_type),
                ..Default::default()
            },
            duration_ticks: self.duration_ticks,
        }
    }
}

struct FileSource {
    egress_profile: NegotiatedLegProfile,
    codec: NegotiatedCodec,
    source: Box<dyn AudioSource>,
    encoder: Box<dyn Encoder>,
    interval: tokio::time::Interval,
    samples_per_frame: usize,
    duration_ticks: u32,
    completion: PlaybackCompletionTx,
}

impl FileSource {
    fn new(
        file_path: String,
        loop_playback: bool,
        egress_profile: NegotiatedLegProfile,
        completion_tx: Option<oneshot::Sender<PlaybackCompletion>>,
    ) -> Result<Self> {
        let codec = selected_audio_codec(&egress_profile);
        let file_source = FileAudioSource::new(file_path, loop_playback)?;
        let source = ResamplingAudioSource::new(Box::new(file_source), codec.codec.samplerate());
        let mut interval = tokio::time::interval(Duration::from_millis(FRAME_MS as u64));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let samples_per_frame = samples_per_frame(codec.codec);
        let duration_ticks = duration_ticks(codec.clock_rate);
        let encoder = create_encoder(codec.codec);

        Ok(Self {
            egress_profile,
            codec,
            source: Box::new(source),
            encoder,
            interval,
            samples_per_frame,
            duration_ticks,
            completion: PlaybackCompletionTx::new(completion_tx),
        })
    }

    async fn next_frame(&mut self) -> SourceRead {
        self.interval.tick().await;

        let mut pcm = vec![0i16; self.samples_per_frame];
        let read = self.source.read_samples(&mut pcm);
        if read == 0 {
            self.completion.complete(false);
            return SourceRead::Finished(self.egress_profile.clone());
        }

        let encoded = self.encoder.encode(&pcm[..read]);
        SourceRead::Frame(TimedFrame {
            frame: AudioFrame {
                clock_rate: self.codec.clock_rate,
                data: encoded.into(),
                payload_type: Some(self.codec.payload_type),
                ..Default::default()
            },
            duration_ticks: self.duration_ticks,
        })
    }

    fn interrupt(&mut self) {
        self.completion.complete(true);
    }
}

struct PeerSource {
    track: Arc<dyn MediaStreamTrack>,
    audio_mapping: Option<AudioMapping>,
    dtmf_mapping: Option<DtmfMapping>,
    transcoder: Option<Transcoder>,
    last_source_timestamp: Option<u32>,
    fallback_duration_ticks: u32,
    recorder: Option<RecorderTap>,
    dtmf_observer: Option<DtmfObserver>,
    dtmf_detector: RtpDtmfDetector,
}

impl PeerSource {
    fn new(
        track: Arc<dyn MediaStreamTrack>,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
    ) -> Self {
        let audio_mapping = match (&ingress_profile.audio, &egress_profile.audio) {
            (Some(source_audio), Some(target_audio)) => Some(AudioMapping {
                source_pt: source_audio.payload_type,
                target_pt: target_audio.payload_type,
                source_clock_rate: source_audio.clock_rate,
                target_clock_rate: target_audio.clock_rate,
            }),
            _ => None,
        };

        let dtmf_mapping = ingress_profile.dtmf.as_ref().map(|source_dtmf| DtmfMapping {
            source_pt: source_dtmf.payload_type,
            target_pt: egress_profile
                .dtmf
                .as_ref()
                .map(|codec| codec.payload_type),
            source_clock_rate: source_dtmf.clock_rate,
            target_clock_rate: egress_profile.dtmf.as_ref().map(|codec| codec.clock_rate),
        });

        let transcoder = match (&ingress_profile.audio, &egress_profile.audio) {
            (Some(source_audio), Some(target_audio))
                if source_audio.codec != target_audio.codec =>
            {
                Some(Transcoder::new(
                    source_audio.codec,
                    target_audio.codec,
                    target_audio.payload_type,
                ))
            }
            _ => None,
        };

        let target_clock_rate = egress_profile
            .audio
            .as_ref()
            .map(|audio| audio.clock_rate)
            .unwrap_or_else(|| CodecType::PCMU.clock_rate());

        Self {
            track,
            audio_mapping,
            dtmf_mapping,
            transcoder,
            last_source_timestamp: None,
            fallback_duration_ticks: duration_ticks(target_clock_rate),
            recorder: tap.recorder,
            dtmf_observer: tap.dtmf_observer,
            dtmf_detector: RtpDtmfDetector::default(),
        }
    }

    async fn next_frame(&mut self) -> MediaResult<SourceRead> {
        loop {
            let sample = self.track.recv().await?;
            let MediaSample::Audio(frame) = sample else {
                continue;
            };
            if let Some(recorder) = &self.recorder {
                recorder.record(MediaSample::Audio(frame.clone()));
            }

            let matched_dtmf = self
                .dtmf_mapping
                .as_ref()
                .is_some_and(|mapping| frame.payload_type == Some(mapping.source_pt));
            let matched_audio = self
                .audio_mapping
                .as_ref()
                .is_some_and(|mapping| frame.payload_type == Some(mapping.source_pt));

            if (self.audio_mapping.is_some() || self.dtmf_mapping.is_some())
                && !matched_audio
                && !matched_dtmf
            {
                continue;
            }

            if matched_dtmf {
                let Some(mapping) = self.dtmf_mapping.as_ref().cloned() else {
                    continue;
                };
                self.observe_dtmf(&frame);
                let Some(target_pt) = mapping.target_pt else {
                    continue;
                };

                let target_clock_rate = mapping
                    .target_clock_rate
                    .unwrap_or(mapping.source_clock_rate);
                let duration_ticks = self.frame_duration_ticks(
                    frame.rtp_timestamp,
                    mapping.source_clock_rate,
                    target_clock_rate,
                );
                let mut output = frame.clone();
                output.payload_type = Some(target_pt);
                output.clock_rate = target_clock_rate;
                if mapping.source_clock_rate != target_clock_rate {
                    output.data = rewrite_dtmf_duration(
                        &output.data,
                        mapping.source_clock_rate,
                        target_clock_rate,
                    );
                }
                return Ok(SourceRead::Frame(TimedFrame {
                    frame: output,
                    duration_ticks,
                }));
            }

            let Some(mapping) = self.audio_mapping.as_ref().cloned() else {
                return Ok(SourceRead::Frame(TimedFrame {
                    frame,
                    duration_ticks: self.fallback_duration_ticks,
                }));
            };

            if !matched_audio {
                continue;
            }

            let duration_ticks = self.frame_duration_ticks(
                frame.rtp_timestamp,
                mapping.source_clock_rate,
                mapping.target_clock_rate,
            );

            let output = if let Some(transcoder) = self.transcoder.as_mut() {
                transcoder.transcode(&frame)
            } else if frame.payload_type != Some(mapping.target_pt)
                || mapping.source_clock_rate != mapping.target_clock_rate
            {
                let mut output = frame.clone();
                output.payload_type = Some(mapping.target_pt);
                output.clock_rate = mapping.target_clock_rate;
                output
            } else {
                frame
            };

            return Ok(SourceRead::Frame(TimedFrame {
                frame: output,
                duration_ticks,
            }));
        }
    }

    fn observe_dtmf(&mut self, frame: &AudioFrame) {
        let digit = self
            .dtmf_detector
            .observe(&frame.data, frame.rtp_timestamp);
        if let (Some(observer), Some(digit)) = (&self.dtmf_observer, digit) {
            observer(digit);
        }
    }

    fn frame_duration_ticks(
        &mut self,
        source_timestamp: u32,
        source_clock_rate: u32,
        target_clock_rate: u32,
    ) -> u32 {
        let Some(last_timestamp) = self.last_source_timestamp.replace(source_timestamp) else {
            return duration_ticks(target_clock_rate);
        };

        let delta = source_timestamp.wrapping_sub(last_timestamp);
        if delta >= 0x8000_0000 {
            return duration_ticks(target_clock_rate);
        }

        (delta as u64 * target_clock_rate as u64 / source_clock_rate as u64) as u32
    }
}

enum ActiveSource {
    Silence(SilenceSource),
    File(FileSource),
    Peer(PeerSource),
}

impl ActiveSource {
    fn silence(egress_profile: NegotiatedLegProfile) -> Self {
        Self::Silence(SilenceSource::new(egress_profile))
    }

    fn interrupt(&mut self) {
        if let ActiveSource::File(source) = self {
            source.interrupt();
        }
    }

    async fn next_frame(&mut self) -> MediaResult<SourceRead> {
        match self {
            ActiveSource::Silence(source) => Ok(SourceRead::Frame(source.next_frame().await)),
            ActiveSource::File(source) => Ok(source.next_frame().await),
            ActiveSource::Peer(source) => source.next_frame().await,
        }
    }
}

enum SourceRead {
    Frame(TimedFrame),
    Finished(NegotiatedLegProfile),
}

pub struct AudioEgressTrack {
    track_id: String,
    current_source: tokio::sync::Mutex<ActiveSource>,
    pending_source: Mutex<Option<ActiveSource>>,
    source_changed: Notify,
    input_tap: tokio::sync::Mutex<Option<LocalInputTap>>,
    pending_input_tap: Mutex<Option<Option<LocalInputTap>>>,
    input_tap_changed: Notify,
    clock: Mutex<EgressClock>,
}

impl AudioEgressTrack {
    pub fn new(track_id: String, egress_profile: NegotiatedLegProfile) -> Self {
        Self {
            track_id,
            current_source: tokio::sync::Mutex::new(ActiveSource::silence(egress_profile)),
            pending_source: Mutex::new(None),
            source_changed: Notify::new(),
            input_tap: tokio::sync::Mutex::new(None),
            pending_input_tap: Mutex::new(None),
            input_tap_changed: Notify::new(),
            clock: Mutex::new(EgressClock::default()),
        }
    }

    pub fn stage_silence(&self, egress_profile: NegotiatedLegProfile) {
        self.set_pending_source(ActiveSource::silence(egress_profile));
    }

    pub fn stage_file(
        &self,
        file_path: String,
        loop_playback: bool,
        egress_profile: NegotiatedLegProfile,
        completion_tx: Option<oneshot::Sender<PlaybackCompletion>>,
    ) -> Result<()> {
        let source = FileSource::new(file_path, loop_playback, egress_profile, completion_tx)?;
        self.set_pending_source(ActiveSource::File(source));
        Ok(())
    }

    pub fn stage_peer(
        &self,
        track: Arc<dyn MediaStreamTrack>,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
    ) {
        self.stage_peer_with_tap(track, ingress_profile, egress_profile, AudioInputTap::default());
    }

    pub fn stage_peer_with_tap(
        &self,
        track: Arc<dyn MediaStreamTrack>,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
    ) {
        self.set_pending_source(ActiveSource::Peer(PeerSource::new(
            track,
            ingress_profile,
            egress_profile,
            tap,
        )));
    }

    pub fn stage_input_tap(
        &self,
        track: Arc<dyn MediaStreamTrack>,
        ingress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
    ) {
        *self.pending_input_tap.lock() = Some(Some(LocalInputTap::new(
            track,
            ingress_profile,
            tap,
        )));
        self.input_tap_changed.notify_one();
    }

    pub fn clear_input_tap(&self) {
        *self.pending_input_tap.lock() = Some(None);
        self.input_tap_changed.notify_one();
    }

    fn set_pending_source(&self, source: ActiveSource) {
        if let Some(mut pending) = self.pending_source.lock().replace(source) {
            pending.interrupt();
        }
        self.source_changed.notify_one();
    }

    fn take_pending_source(&self) -> Option<ActiveSource> {
        self.pending_source.lock().take()
    }

    fn take_pending_input_tap(&self) -> Option<Option<LocalInputTap>> {
        self.pending_input_tap.lock().take()
    }

    async fn apply_pending_source(&self, source: ActiveSource) {
        let mut current = self.current_source.lock().await;
        current.interrupt();
        *current = source;
        self.clock.lock().mark_source_switch();
    }

    async fn apply_pending_input_tap(&self, input_tap: Option<LocalInputTap>) {
        *self.input_tap.lock().await = input_tap;
    }

    async fn recv_input_tap(&self) -> MediaResult<()> {
        let mut guard = self.input_tap.lock().await;
        match guard.as_mut() {
            Some(input_tap) => input_tap.recv_once().await,
            None => std::future::pending().await,
        }
    }
}

#[async_trait::async_trait]
impl MediaStreamTrack for AudioEgressTrack {
    fn id(&self) -> &str {
        &self.track_id
    }

    fn kind(&self) -> MediaKind {
        MediaKind::Audio
    }

    fn state(&self) -> TrackState {
        TrackState::Live
    }

    async fn recv(&self) -> MediaResult<MediaSample> {
        loop {
            if let Some(source) = self.take_pending_source() {
                self.apply_pending_source(source).await;
                continue;
            }
            if let Some(input_tap) = self.take_pending_input_tap() {
                self.apply_pending_input_tap(input_tap).await;
                continue;
            }

            let mut current = self.current_source.lock().await;
            let source_changed = self.source_changed.notified();
            let input_tap_changed = self.input_tap_changed.notified();
            tokio::pin!(source_changed);
            tokio::pin!(input_tap_changed);

            let read = tokio::select! {
                biased;
                _ = &mut source_changed => {
                    drop(current);
                    continue;
                }
                _ = &mut input_tap_changed => {
                    drop(current);
                    continue;
                }
                read = current.next_frame() => read,
                input = self.recv_input_tap() => {
                    drop(current);
                    if input.is_err() {
                        self.clear_input_tap();
                    }
                    continue;
                }
            };
            drop(current);

            match read? {
                SourceRead::Frame(timed) => {
                    let frame = self.clock.lock().stamp(timed.frame, timed.duration_ticks);
                    return Ok(MediaSample::Audio(frame));
                }
                SourceRead::Finished(egress_profile) => {
                    self.apply_pending_source(ActiveSource::silence(egress_profile))
                        .await;
                    continue;
                }
            }
        }
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        Ok(())
    }
}

fn selected_audio_codec(profile: &NegotiatedLegProfile) -> NegotiatedCodec {
    profile.audio.clone().unwrap_or_else(default_audio_codec)
}

fn default_audio_codec() -> NegotiatedCodec {
    NegotiatedCodec {
        codec: CodecType::PCMU,
        payload_type: CodecType::PCMU.payload_type(),
        clock_rate: CodecType::PCMU.clock_rate(),
        channels: CodecType::PCMU.channels(),
    }
}

fn samples_per_frame(codec: CodecType) -> usize {
    (codec.samplerate() * FRAME_MS / 1000) as usize
}

fn duration_ticks(clock_rate: u32) -> u32 {
    clock_rate * FRAME_MS / 1000
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rustrtc::media::error::MediaError;
    use rustrtc::media::track::TrackState;
    use std::collections::VecDeque;

    struct BlockingTrack {
        notify: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl MediaStreamTrack for BlockingTrack {
        fn id(&self) -> &str {
            "blocking"
        }

        fn kind(&self) -> MediaKind {
            MediaKind::Audio
        }

        fn state(&self) -> TrackState {
            TrackState::Live
        }

        async fn recv(&self) -> MediaResult<MediaSample> {
            self.notify.notified().await;
            Err(MediaError::Closed)
        }

        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
        }
    }

    struct QueuedTrack {
        samples: tokio::sync::Mutex<VecDeque<MediaSample>>,
    }

    #[async_trait::async_trait]
    impl MediaStreamTrack for QueuedTrack {
        fn id(&self) -> &str {
            "queued"
        }

        fn kind(&self) -> MediaKind {
            MediaKind::Audio
        }

        fn state(&self) -> TrackState {
            TrackState::Live
        }

        async fn recv(&self) -> MediaResult<MediaSample> {
            loop {
                let mut guard = self.samples.lock().await;
                if let Some(sample) = guard.pop_front() {
                    return Ok(sample);
                }
                drop(guard);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }

        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
        }
    }

    fn pcmu_profile() -> NegotiatedLegProfile {
        NegotiatedLegProfile {
            audio: Some(default_audio_codec()),
            dtmf: None,
        }
    }

    fn pcmu_dtmf_profile() -> NegotiatedLegProfile {
        NegotiatedLegProfile {
            audio: Some(default_audio_codec()),
            dtmf: Some(NegotiatedCodec {
                codec: CodecType::TelephoneEvent,
                payload_type: 101,
                clock_rate: 8000,
                channels: 1,
            }),
        }
    }

    fn audio_sample(ts: u32) -> MediaSample {
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: ts,
            clock_rate: 8000,
            data: Bytes::from(vec![0u8; 160]),
            payload_type: Some(0),
            sequence_number: Some(1),
            ..Default::default()
        })
    }

    fn dtmf_sample(digit: u8, ts: u32) -> MediaSample {
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: ts,
            clock_rate: 8000,
            data: Bytes::from(vec![digit, 0x00, 0x00, 0xa0]),
            payload_type: Some(101),
            sequence_number: Some(1),
            ..Default::default()
        })
    }

    fn write_wav(path: &std::path::Path, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..samples {
            writer.write_sample::<i16>(0).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[tokio::test]
    async fn silence_source_emits_paced_frames() {
        let track = AudioEgressTrack::new("silence".to_string(), pcmu_profile());
        let sample = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("silence should emit")
            .expect("silence recv should succeed");

        let MediaSample::Audio(frame) = sample else {
            panic!("expected audio");
        };
        assert_eq!(frame.payload_type, Some(0));
        assert_eq!(frame.clock_rate, 8000);
        assert!(!frame.data.is_empty());
    }

    #[tokio::test]
    async fn file_source_emits_completion() {
        let temp = tempfile::NamedTempFile::with_suffix(".wav").unwrap();
        write_wav(temp.path(), 160);

        let track = AudioEgressTrack::new("file".to_string(), pcmu_profile());
        let (tx, rx) = oneshot::channel();
        track
            .stage_file(
                temp.path().to_string_lossy().to_string(),
                false,
                pcmu_profile(),
                Some(tx),
            )
            .unwrap();

        let _ = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("file should emit first frame")
            .expect("file recv should succeed");
        let _ = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("file should finish and fall back to silence")
            .expect("file finish recv should succeed");

        let completion = tokio::time::timeout(Duration::from_millis(500), rx)
            .await
            .expect("completion should arrive")
            .expect("completion sender should stay alive");
        assert!(!completion.interrupted);
    }

    #[tokio::test]
    async fn source_switch_interrupts_file() {
        let temp = tempfile::NamedTempFile::with_suffix(".wav").unwrap();
        write_wav(temp.path(), 8000);

        let track = AudioEgressTrack::new("switch".to_string(), pcmu_profile());
        let (tx, rx) = oneshot::channel();
        track
            .stage_file(
                temp.path().to_string_lossy().to_string(),
                true,
                pcmu_profile(),
                Some(tx),
            )
            .unwrap();

        let _ = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("file should emit")
            .expect("file recv should succeed");

        let blocking = Arc::new(BlockingTrack {
            notify: Arc::new(Notify::new()),
        });
        track.stage_peer(blocking, pcmu_profile(), pcmu_profile());
        let _ = tokio::time::timeout(Duration::from_millis(100), track.recv()).await;

        let completion = tokio::time::timeout(Duration::from_millis(500), rx)
            .await
            .expect("interruption should arrive")
            .expect("completion sender should stay alive");
        assert!(completion.interrupted);
    }

    #[tokio::test]
    async fn peer_source_is_restamped_continuously() {
        let source = Arc::new(QueuedTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([
                audio_sample(1234),
                audio_sample(1394),
            ])),
        });
        let track = AudioEgressTrack::new("peer".to_string(), pcmu_profile());
        track.stage_peer(source, pcmu_profile(), pcmu_profile());

        let first = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("peer frame should arrive")
            .expect("peer recv should succeed");
        let second = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("second peer frame should arrive")
            .expect("second peer recv should succeed");

        let MediaSample::Audio(first_frame) = first else {
            panic!("expected audio");
        };
        let MediaSample::Audio(second_frame) = second else {
            panic!("expected audio");
        };
        assert_ne!(first_frame.rtp_timestamp, 1234);
        assert_eq!(
            second_frame.rtp_timestamp,
            first_frame.rtp_timestamp.wrapping_add(160)
        );
        assert_eq!(
            second_frame.sequence_number,
            Some(first_frame.sequence_number.unwrap().wrapping_add(1))
        );
        assert_eq!(first_frame.payload_type, Some(0));
        assert_eq!(second_frame.payload_type, Some(0));
    }

    #[tokio::test]
    async fn peer_source_taps_recorder_from_single_pull_path() {
        let source = Arc::new(QueuedTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([audio_sample(1234)])),
        });
        let (tx, mut rx) = mpsc::channel(4);
        let tap = AudioInputTap::default().with_recorder(RecorderTap::new(
            tx,
            RecorderLeg::A,
            pcmu_profile(),
        ));
        let track = AudioEgressTrack::new("recorded-peer".to_string(), pcmu_profile());
        track.stage_peer_with_tap(source, pcmu_profile(), pcmu_profile(), tap);

        let output = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("peer frame should arrive")
            .expect("peer recv should succeed");
        let recorded = rx.try_recv().expect("recorder tap should receive sample");

        assert_eq!(recorded.leg, RecorderLeg::A);
        assert_eq!(recorded.codec_hint, Some(CodecType::PCMU));
        let MediaSample::Audio(output_frame) = output else {
            panic!("expected audio");
        };
        let MediaSample::Audio(recorded_frame) = recorded.sample else {
            panic!("expected recorded audio");
        };
        assert_ne!(output_frame.rtp_timestamp, 1234);
        assert_eq!(recorded_frame.rtp_timestamp, 1234);
    }

    #[tokio::test]
    async fn recorder_tap_skips_non_audio_payloads_from_profile() {
        let source = Arc::new(QueuedTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([dtmf_sample(1, 4321)])),
        });
        let (tx, mut rx) = mpsc::channel(4);
        let tap = AudioInputTap::default().with_recorder(RecorderTap::new(
            tx,
            RecorderLeg::A,
            pcmu_dtmf_profile(),
        ));
        let track = AudioEgressTrack::new("recorded-dtmf".to_string(), pcmu_dtmf_profile());
        track.stage_peer_with_tap(source, pcmu_dtmf_profile(), pcmu_dtmf_profile(), tap);

        let _ = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("dtmf frame should arrive")
            .expect("dtmf recv should succeed");

        assert!(rx.try_recv().is_err(), "DTMF should not be recorded as audio");
    }

    #[tokio::test]
    async fn peer_source_observes_dtmf_from_single_pull_path() {
        let source = Arc::new(QueuedTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([dtmf_sample(1, 4321)])),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tap = AudioInputTap::default().with_dtmf_observer(move |digit| {
            let _ = tx.send(digit);
        });
        let track = AudioEgressTrack::new("dtmf-peer".to_string(), pcmu_dtmf_profile());
        track.stage_peer_with_tap(source, pcmu_dtmf_profile(), pcmu_dtmf_profile(), tap);

        let _ = tokio::time::timeout(Duration::from_millis(100), track.recv())
            .await
            .expect("dtmf frame should arrive")
            .expect("dtmf recv should succeed");

        assert_eq!(rx.try_recv().expect("DTMF observer should receive digit"), '1');
    }

    #[tokio::test]
    async fn local_input_tap_observes_dtmf_from_egress_pull_path() {
        let source = Arc::new(QueuedTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([dtmf_sample(1, 4321)])),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tap = AudioInputTap::default().with_dtmf_observer(move |digit| {
            let _ = tx.send(digit);
        });
        let track = AudioEgressTrack::new("local-input".to_string(), pcmu_dtmf_profile());
        track.stage_input_tap(source, pcmu_dtmf_profile(), tap);

        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(100), track.recv())
                .await
                .expect("egress recv should complete")
                .expect("egress recv should succeed");
            if let Ok(digit) = rx.try_recv() {
                assert_eq!(digit, '1');
                return;
            }
        }

        panic!("DTMF observer should receive digit");
    }
}
