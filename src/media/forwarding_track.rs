use crate::media::engine::command::SharedMediaSample;
use crate::media::negotiate::NegotiatedLegProfile;
use crate::media::transcoder::{RtpTiming, Transcoder, rewrite_dtmf_duration};
use crate::media::{ReceiveTimestampClock, Track, recorder::Leg};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use parking_lot::Mutex;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{MediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::trace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioMapping {
    pub source_pt: u8,
    pub target_pt: u8,
    pub source_clock_rate: u32,
    pub target_clock_rate: u32,
}

/// Mapping for a single DTMF PT from source to target, or drop if target is absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtmfMapping {
    pub source_pt: u8,
    pub target_pt: Option<u8>,
    pub source_clock_rate: u32,
    pub target_clock_rate: Option<u32>,
}

/// A wrapper track that sits between a source PC's receiver and a target PC's sender.
///
/// RtpSender's built-in loop calls `recv()` on this track, so all forwarding logic
/// (recording, transcoding, DTMF handling) happens inline with zero additional tasks.
pub struct ForwardingTrack {
    track_id: String,
    inner: Arc<dyn MediaStreamTrack>,
    update_ingress_profile: Mutex<Option<NegotiatedLegProfile>>,
    update_egress_profile: Mutex<Option<NegotiatedLegProfile>>,
    current_ingress_profile: Mutex<Option<NegotiatedLegProfile>>,
    current_egress_profile: Mutex<Option<NegotiatedLegProfile>>,
    transcoder: Mutex<Option<Transcoder>>,
    audio_mapping: Mutex<Option<AudioMapping>>,
    audio_timing: Mutex<Option<RtpTiming>>,
    dtmf_timing: Mutex<Option<RtpTiming>>,
    recorder_tx: Option<mpsc::Sender<(Leg, SharedMediaSample)>>,
    sipflow_tx: Option<mpsc::Sender<(Leg, SharedMediaSample, u64)>>,
    /// Optional egress capture for sipflow: captures the post-transcode/remap
    /// sample (what the target side hears) instead of the ingress. Used for the
    /// callee→caller direction to capture caller's egress as Leg::B.
    egress_sipflow_tx: Option<mpsc::Sender<(Leg, SharedMediaSample, u64)>>,
    /// Leg tag for egress sipflow capture.
    egress_leg: Option<Leg>,
    receive_clock: ReceiveTimestampClock,
    recorder_leg: Leg,
    dtmf_mapping: Mutex<Option<DtmfMapping>>,
}

pub struct ForwardingTrackHandle {
    track_id: String,
    forwarding: Arc<ForwardingTrack>,
}

impl ForwardingTrack {
    pub const DEFAULT_SIPFLOW_CHANNEL_CAPACITY: usize = 256;

    pub fn new(
        track_id: String,
        inner: Arc<dyn MediaStreamTrack>,
        recorder_tx: Option<mpsc::Sender<(Leg, SharedMediaSample)>>,
        sipflow_tx: Option<mpsc::Sender<(Leg, SharedMediaSample, u64)>>,
        recorder_leg: Leg,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
    ) -> Self {
        Self::with_egress(
            track_id,
            inner,
            recorder_tx,
            sipflow_tx,
            None,
            None,
            recorder_leg,
            ingress_profile,
            egress_profile,
        )
    }

    /// Extended constructor with egress sipflow channel.
    ///
    /// When `egress_sipflow_tx` is `Some`, the post-transcode/remap sample
    /// (output of `recv()`) is also teed to that channel with the given
    /// `egress_leg` tag. This is used for the callee→caller direction to
    /// capture the caller's egress (what caller hears) as Leg::B, instead
    /// of the callee's raw ingress.
    pub fn with_egress(
        track_id: String,
        inner: Arc<dyn MediaStreamTrack>,
        recorder_tx: Option<mpsc::Sender<(Leg, SharedMediaSample)>>,
        sipflow_tx: Option<mpsc::Sender<(Leg, SharedMediaSample, u64)>>,
        egress_sipflow_tx: Option<mpsc::Sender<(Leg, SharedMediaSample, u64)>>,
        egress_leg: Option<Leg>,
        recorder_leg: Leg,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
    ) -> Self {
        Self {
            track_id,
            inner,
            update_ingress_profile: Mutex::new(Some(ingress_profile)),
            update_egress_profile: Mutex::new(Some(egress_profile)),
            current_ingress_profile: Mutex::new(None),
            current_egress_profile: Mutex::new(None),
            transcoder: Mutex::new(None),
            audio_mapping: Mutex::new(None),
            audio_timing: Mutex::new(None),
            dtmf_timing: Mutex::new(None),
            recorder_tx,
            sipflow_tx,
            egress_sipflow_tx,
            egress_leg,
            receive_clock: ReceiveTimestampClock::new(),
            recorder_leg,
            dtmf_mapping: Mutex::new(None),
        }
    }

    pub fn stage_ingress_profile(&self, profile: NegotiatedLegProfile) {
        *self.update_ingress_profile.lock() = Some(profile);
    }

    pub fn stage_egress_profile(&self, profile: NegotiatedLegProfile) {
        *self.update_egress_profile.lock() = Some(profile);
    }

    pub fn ingress_profile(&self) -> Option<NegotiatedLegProfile> {
        self.update_ingress_profile
            .lock()
            .clone()
            .or_else(|| self.current_ingress_profile.lock().clone())
    }

    fn rebuild_runtime_if_needed(&self) {
        let (ingress_update, egress_update) = {
            let mut ingress_update = self.update_ingress_profile.lock();
            let mut egress_update = self.update_egress_profile.lock();
            if ingress_update.is_none() && egress_update.is_none() {
                return;
            }
            (ingress_update.take(), egress_update.take())
        };

        let (ingress, egress) = {
            let mut current_ingress = self.current_ingress_profile.lock();
            let mut current_egress = self.current_egress_profile.lock();

            if let Some(profile) = ingress_update {
                *current_ingress = Some(profile);
            }
            if let Some(profile) = egress_update {
                *current_egress = Some(profile);
            }

            match (current_ingress.clone(), current_egress.clone()) {
                (Some(ingress), Some(egress)) => (ingress, egress),
                _ => return,
            }
        };

        let audio_mapping = match (&ingress.audio, &egress.audio) {
            (Some(source_audio), Some(target_audio)) => Some(AudioMapping {
                source_pt: source_audio.payload_type,
                target_pt: target_audio.payload_type,
                source_clock_rate: source_audio.clock_rate,
                target_clock_rate: target_audio.clock_rate,
            }),
            _ => None,
        };

        let dtmf_mapping = ingress.dtmf.as_ref().map(|source_dtmf| DtmfMapping {
            source_pt: source_dtmf.payload_type,
            target_pt: egress.dtmf.as_ref().map(|codec| codec.payload_type),
            source_clock_rate: source_dtmf.clock_rate,
            target_clock_rate: egress.dtmf.as_ref().map(|codec| codec.clock_rate),
        });

        let transcoder = match (&ingress.audio, &egress.audio) {
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

        let audio_timing = audio_mapping.as_ref().and_then(|mapping| {
            if mapping.source_clock_rate != mapping.target_clock_rate
                || mapping.source_pt != mapping.target_pt
            {
                Some(RtpTiming::default())
            } else {
                None
            }
        });

        let dtmf_timing = dtmf_mapping.as_ref().and_then(|mapping| {
            mapping.target_clock_rate.and_then(|target_clock_rate| {
                if mapping.source_clock_rate != target_clock_rate {
                    Some(RtpTiming::default())
                } else {
                    None
                }
            })
        });

        *self.transcoder.lock() = transcoder;
        *self.audio_mapping.lock() = audio_mapping;
        *self.audio_timing.lock() = audio_timing;
        *self.dtmf_mapping.lock() = dtmf_mapping;
        *self.dtmf_timing.lock() = dtmf_timing;
    }

    /// Tee the sample to recorder and/or SipFlow capture channels (non-blocking).
    #[inline]
    fn tee_sample_to_capture_channels(&self, sample: &MediaSample) {
        // Recording / sipflow capture is audio-only by design: video samples
        // are forwarded as-is but never stored. Filter here so video never
        // wastes the recorder / sipflow channel capacity.
        if !matches!(sample, MediaSample::Audio(_)) {
            return;
        }

        let needs_recorder = self.recorder_tx.is_some();
        let needs_sipflow = self.sipflow_tx.is_some();
        let shared = if needs_recorder || needs_sipflow {
            Some(Arc::new(sample.clone()))
        } else {
            None
        };
        let received_at_micros = self.receive_clock.now_micros();

        if let (Some(tx), Some(shared)) = (&self.recorder_tx, &shared) {
            if let Err(e) = tx.try_send((self.recorder_leg, Arc::clone(shared))) {
                trace!(track_id = %self.track_id, "ForwardingTrack recorder channel full: {e}");
            }
        }
        if let (Some(tx), Some(shared)) = (&self.sipflow_tx, &shared) {
            if let Err(e) = tx.try_send((self.recorder_leg, Arc::clone(shared), received_at_micros))
            {
                trace!(track_id = %self.track_id, "ForwardingTrack sipflow channel full: {e}");
            }
        }
    }

    /// Tee the post-transcode/remap sample (egress) to sipflow capture channels.
    /// This captures what the target side hears (caller's egress) rather than the
    /// source side's ingress.
    #[inline]
    fn tee_egress_to_capture_channels(&self, sample: &MediaSample) {
        if !matches!(sample, MediaSample::Audio(_)) {
            return;
        }
        if let (Some(tx), Some(leg)) = (&self.egress_sipflow_tx, self.egress_leg) {
            let received_at_micros = self.receive_clock.now_micros();
            if let Err(e) = tx.try_send((leg, Arc::new(sample.clone()), received_at_micros)) {
                trace!(track_id = %self.track_id, "ForwardingTrack egress sipflow channel full: {e}");
            }
        }
    }

    /// Try to map a DTMF telephone-event frame to the egress payload type.
    #[inline]
    fn try_map_dtmf(
        &self,
        frame: &rustrtc::media::frame::AudioFrame,
        dtmf_mapping: &Option<DtmfMapping>,
        matched_dtmf: bool,
    ) -> Option<MediaSample> {
        let mapping = dtmf_mapping.as_ref().filter(|_| matched_dtmf)?;
        let target_pt = mapping.target_pt?;

        let mut dtmf_frame = frame.clone();
        dtmf_frame.payload_type = Some(target_pt);

        if let Some(target_clock_rate) = mapping.target_clock_rate
            && mapping.source_clock_rate != target_clock_rate
        {
            dtmf_frame.data = rewrite_dtmf_duration(
                &dtmf_frame.data,
                mapping.source_clock_rate,
                target_clock_rate,
            );
            let mut guard = self.dtmf_timing.lock();
            if let Some(timing) = guard.as_mut() {
                timing.rewrite(
                    &mut dtmf_frame,
                    mapping.source_clock_rate,
                    target_clock_rate,
                    target_pt,
                );
            }
        }
        Some(MediaSample::Audio(dtmf_frame))
    }

    /// Try to transcode or remap an audio frame to the egress codec/PT.
    #[inline]
    fn try_transcode_audio(
        &self,
        frame: &rustrtc::media::frame::AudioFrame,
        audio_mapping: &Option<AudioMapping>,
        matched_audio: bool,
    ) -> Option<MediaSample> {
        let audio_mapping = audio_mapping.as_ref().filter(|_| matched_audio)?;

        let mut guard = self.transcoder.lock();
        if let Some(transcoder) = guard.as_mut() {
            let mut output = transcoder.transcode(frame);
            let mut timing_guard = self.audio_timing.lock();
            if let Some(timing) = timing_guard.as_mut() {
                timing.rewrite(
                    &mut output,
                    audio_mapping.source_clock_rate,
                    audio_mapping.target_clock_rate,
                    audio_mapping.target_pt,
                );
            }
            return Some(MediaSample::Audio(output));
        }
        drop(guard);

        if frame.payload_type != Some(audio_mapping.target_pt)
            || audio_mapping.source_clock_rate != audio_mapping.target_clock_rate
        {
            let mut output = frame.clone();
            let mut timing_guard = self.audio_timing.lock();
            if let Some(timing) = timing_guard.as_mut() {
                timing.rewrite(
                    &mut output,
                    audio_mapping.source_clock_rate,
                    audio_mapping.target_clock_rate,
                    audio_mapping.target_pt,
                );
            } else {
                output.payload_type = Some(audio_mapping.target_pt);
                output.clock_rate = audio_mapping.target_clock_rate;
            }
            return Some(MediaSample::Audio(output));
        }

        None
    }
}

#[async_trait]
impl Track for ForwardingTrackHandle {
    fn id(&self) -> &str {
        &self.track_id
    }

    async fn handshake(&self, _remote_offer: String) -> Result<String> {
        Err(anyhow!("ForwardingTrackHandle does not support handshake"))
    }

    async fn local_description(&self) -> Result<String> {
        Err(anyhow!(
            "ForwardingTrackHandle does not expose a local description"
        ))
    }

    async fn set_remote_description(&self, _remote: &str) -> Result<()> {
        Err(anyhow!(
            "ForwardingTrackHandle does not support remote description updates"
        ))
    }

    async fn stop(&self) {}

    async fn get_peer_connection(&self) -> Option<rustrtc::PeerConnection> {
        None
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl ForwardingTrackHandle {
    pub fn new(track_id: String, forwarding: Arc<ForwardingTrack>) -> Self {
        Self {
            track_id,
            forwarding,
        }
    }

    pub fn forwarding(&self) -> Arc<ForwardingTrack> {
        self.forwarding.clone()
    }
}

#[async_trait]
impl MediaStreamTrack for ForwardingTrack {
    fn id(&self) -> &str {
        &self.track_id
    }

    fn kind(&self) -> MediaKind {
        self.inner.kind()
    }

    fn state(&self) -> TrackState {
        self.inner.state()
    }

    async fn recv(&self) -> MediaResult<MediaSample> {
        loop {
            self.rebuild_runtime_if_needed();

            let audio_mapping = *self.audio_mapping.lock();
            let dtmf_mapping = *self.dtmf_mapping.lock();
            let sample = self.inner.recv().await?;

            self.tee_sample_to_capture_channels(&sample);

            if let MediaSample::Audio(ref frame) = sample {
                let matched_dtmf = dtmf_mapping
                    .as_ref()
                    .is_some_and(|mapping| frame.payload_type == Some(mapping.source_pt));
                let matched_audio = audio_mapping
                    .as_ref()
                    .is_some_and(|mapping| frame.payload_type == Some(mapping.source_pt));

                if (audio_mapping.is_some() || dtmf_mapping.is_some())
                    && !matched_audio
                    && !matched_dtmf
                {
                    continue;
                }

                if let Some(result) = self.try_map_dtmf(frame, &dtmf_mapping, matched_dtmf) {
                    // Capture the post-remap egress (caller's output)
                    self.tee_egress_to_capture_channels(&result);
                    return Ok(result);
                }

                if let Some(result) = self.try_transcode_audio(frame, &audio_mapping, matched_audio)
                {
                    // Capture the post-transcode egress (caller's output)
                    self.tee_egress_to_capture_channels(&result);
                    return Ok(result);
                }
            }

            // No remap/transcode needed: the ingress is returned as the egress.
            // Capture this as the egress for callee→caller direction.
            self.tee_egress_to_capture_channels(&sample);
            return Ok(sample);
        }
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        self.inner.request_key_frame().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::create_decoder;
    use bytes::Bytes;
    use rustrtc::media::frame::AudioFrame;

    /// Minimal track that yields exactly one pre-defined sample then blocks.
    struct OneShotTrack {
        sample: parking_lot::Mutex<Option<MediaSample>>,
    }

    impl OneShotTrack {
        fn new(sample: MediaSample) -> Arc<Self> {
            Arc::new(Self {
                sample: parking_lot::Mutex::new(Some(sample)),
            })
        }
    }

    #[async_trait::async_trait]
    impl MediaStreamTrack for OneShotTrack {
        fn id(&self) -> &str {
            "one-shot"
        }
        fn kind(&self) -> MediaKind {
            MediaKind::Audio
        }
        fn state(&self) -> TrackState {
            TrackState::Live
        }
        async fn recv(&self) -> MediaResult<MediaSample> {
            loop {
                let s = self.sample.lock().take();
                if let Some(s) = s {
                    return Ok(s);
                }
                // Block until the test polls again — simulate a quiet stream.
                tokio::time::sleep(std::time::Duration::from_secs(999)).await;
            }
        }
        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
        }
    }

    fn audio_sample(pt: u8) -> MediaSample {
        let frame = AudioFrame {
            payload_type: Some(pt),
            clock_rate: 8000,
            data: Bytes::from_static(&[0u8; 160]),
            ..Default::default()
        };
        MediaSample::Audio(frame)
    }

    /// Issue #171: when a recorder_tx is wired the sample must be forwarded to
    /// the channel without blocking recv(), and must NOT be dropped.
    #[tokio::test]
    async fn sample_forwarded_to_recorder_channel() {
        let (tx, mut rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample.clone());

        let ft = ForwardingTrack::new(
            "test".to_string(),
            track,
            Some(tx),
            None, // no sipflow channel
            Leg::A,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        // Drive recv() once; because there is no audio_mapping the sample is
        // returned immediately AND should also have been sent to the channel.
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        // The forwarded sample is returned to the caller unchanged.
        assert!(matches!(result, MediaSample::Audio(_)));

        // The channel must have received the sample too (non-blocking assertion).
        let (leg, _chan_sample) = rx.try_recv().expect("sample must be in recorder channel");
        assert_eq!(leg, Leg::A);
    }

    /// Without a recorder_tx, recv() must still work without panicking.
    #[tokio::test]
    async fn no_recorder_tx_is_noop() {
        let sample = audio_sample(0);
        let track = OneShotTrack::new(sample);

        let ft = ForwardingTrack::new(
            "test-no-rec".to_string(),
            track,
            None, // no recorder channel
            None, // no sipflow channel
            Leg::B,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        assert!(matches!(result, MediaSample::Audio(_)));
    }

    /// Verify sipflow_tx receives a copy of each forwarded sample without
    /// blocking the hot path and without interfering with the recorder_tx.
    #[tokio::test]
    async fn sipflow_tx_receives_sample() {
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);
        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample.clone());

        let ft = ForwardingTrack::new(
            "test-sipflow".to_string(),
            track,
            None, // no recorder channel
            Some(sf_tx),
            Leg::A,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        // Hot path returns the sample unchanged.
        assert!(matches!(result, MediaSample::Audio(_)));

        // sipflow channel must also have received the sample.
        let (leg, _sf_sample, received_at_micros) =
            sf_rx.try_recv().expect("sample must be in sipflow channel");
        assert_eq!(leg, Leg::A);
        assert!(received_at_micros > 0);
    }

    /// Both recorder_tx AND sipflow_tx can be active simultaneously; each
    /// must receive its own copy of the sample.
    #[tokio::test]
    async fn both_recorder_and_sipflow_receive_sample() {
        let (rec_tx, mut rec_rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);
        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample.clone());

        let ft = ForwardingTrack::new(
            "test-both".to_string(),
            track,
            Some(rec_tx),
            Some(sf_tx),
            Leg::B,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        assert!(matches!(result, MediaSample::Audio(_)));

        let (rec_leg, _) = rec_rx
            .try_recv()
            .expect("recorder channel must have sample");
        let (sf_leg, _, received_at_micros) =
            sf_rx.try_recv().expect("sipflow channel must have sample");
        assert_eq!(rec_leg, Leg::B);
        assert_eq!(sf_leg, Leg::B);
        assert!(received_at_micros > 0);
    }

    #[tokio::test]
    async fn sipflow_full_channel_does_not_block() {
        let (sf_tx, _sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(1);

        let _ = sf_tx.try_send((Leg::A, Arc::new(audio_sample(0)), 1));

        let track = OneShotTrack::new(audio_sample(0));
        let ft = ForwardingTrack::new(
            "test-full".to_string(),
            track,
            None,
            Some(sf_tx),
            Leg::A,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv must not block when sipflow channel is full");

        assert!(result.is_ok());
    }

    fn make_profile_with_dtmf(
        audio_codec: audio_codec::CodecType,
        audio_pt: u8,
        dtmf_pt: Option<u8>,
    ) -> NegotiatedLegProfile {
        use crate::media::negotiate::NegotiatedCodec;
        NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                codec: audio_codec,
                payload_type: audio_pt,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: dtmf_pt.map(|pt| NegotiatedCodec {
                codec: audio_codec::CodecType::TelephoneEvent,
                payload_type: pt,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::Rtp,
        }
    }

    #[tokio::test]
    async fn dtmf_frame_bypasses_active_transcoder() {
        use audio_codec::CodecType;

        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, Some(101));
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, Some(101));

        // digit 5, volume 10, duration 160 ticks — a valid RFC 2833 packet.
        let dtmf_data = Bytes::from_static(&[0x05, 0x0A, 0x00, 0xA0]);
        let sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(101),
            clock_rate: 8000,
            data: dtmf_data.clone(),
            ..Default::default()
        });

        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-dtmf-bypass".to_string(),
            track,
            None,
            None,
            Leg::A,
            ingress,
            egress,
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("DTMF frame was unexpectedly dropped (recv timed out)")
            .expect("recv error");

        let MediaSample::Audio(frame) = result else {
            panic!("expected audio sample");
        };
        assert_eq!(
            frame.payload_type,
            Some(101),
            "telephone-event PT must not be changed"
        );
        assert_eq!(
            frame.data, dtmf_data,
            "telephone-event payload must not be modified by the transcoder"
        );
    }

    #[tokio::test]
    async fn dtmf_frame_passed_through_when_egress_has_no_dtmf_capability() {
        use audio_codec::CodecType;

        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, Some(101));
        // Egress has no DTMF → DtmfMapping::target_pt will be None.
        // The frame should be passed through as-is (not dropped) so that the far-end
        // trunk still receives RFC 2833 digits even when it omitted telephone-event from
        // its answer SDP (common behaviour for G729 wholesale trunks).
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, None);

        let sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(101),
            clock_rate: 8000,
            data: Bytes::from_static(&[0x05, 0x0A, 0x00, 0xA0]),
            ..Default::default()
        });

        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-dtmf-passthrough".to_string(),
            track,
            None,
            None,
            Leg::A,
            ingress,
            egress,
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), ft.recv())
            .await
            .expect("recv timed out — telephone-event must be passed through, not dropped")
            .expect("recv returned error");

        let MediaSample::Audio(frame) = result else {
            panic!("expected audio sample");
        };
        // PT must be unchanged (source PT 101) since no target PT mapping exists.
        assert_eq!(
            frame.payload_type,
            Some(101),
            "telephone-event PT should be preserved when egress has no DTMF capability"
        );
    }

    #[tokio::test]
    async fn audio_frame_transcoded_to_egress_pt() {
        use audio_codec::CodecType;

        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, Some(101));
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, Some(101));

        // 160 bytes of PCMU-encoded silence (0xFF = µ-law silence).
        let audio_sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(0), // PCMU
            clock_rate: 8000,
            data: Bytes::from(vec![0xFFu8; 160]),
            ..Default::default()
        });

        let track = OneShotTrack::new(audio_sample);
        let ft = ForwardingTrack::new(
            "test-audio-transcode".to_string(),
            track,
            None,
            None,
            Leg::A,
            ingress,
            egress,
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        let MediaSample::Audio(frame) = result else {
            panic!("expected audio sample");
        };
        assert_eq!(
            frame.payload_type,
            Some(8),
            "audio must be re-labeled with PCMA PT after PCMU→PCMA transcoding"
        );
    }

    /// When the ingress leg uses one dynamic PT for telephone-event and the
    /// egress leg negotiated a *different* dynamic PT (e.g. 101 vs 96), the
    /// ForwardingTrack must rewrite the PT in the forwarded frame.
    #[tokio::test]
    async fn dtmf_pt_remapped_to_egress_pt_when_pts_differ() {
        use audio_codec::CodecType;

        // Ingress: PCMU PT=0, telephone-event PT=101
        // Egress : PCMA PT=8, telephone-event PT=96 (different dynamic PT)
        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, Some(101));
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, Some(96));

        let dtmf_data = Bytes::from_static(&[0x05, 0x0A, 0x00, 0xA0]);
        let sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(101), // ingress PT
            clock_rate: 8000,
            data: dtmf_data.clone(),
            ..Default::default()
        });

        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-dtmf-remap".to_string(),
            track,
            None,
            None,
            Leg::A,
            ingress,
            egress,
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        let MediaSample::Audio(frame) = result else {
            panic!("expected audio sample");
        };
        assert_eq!(
            frame.payload_type,
            Some(96),
            "telephone-event PT must be remapped from ingress PT=101 to egress PT=96"
        );
        assert_eq!(
            frame.data, dtmf_data,
            "telephone-event payload must not be modified during PT remapping"
        );
    }

    // ============== End-to-end recording tests ============================

    /// Goertzel magnitude of `freq` (Hz) in `samples` at `sample_rate`.
    fn goertzel(samples: &[i16], freq: f64, sample_rate: u32) -> f64 {
        let n = samples.len() as f64;
        if n == 0.0 {
            return 0.0;
        }
        let k = (freq * n / sample_rate as f64).round();
        let omega = 2.0 * std::f64::consts::PI * k / n;
        let coeff = 2.0 * omega.cos();
        let (mut s_prev, mut s_prev2) = (0.0f64, 0.0f64);
        for &x in samples {
            let s = x as f64 + coeff * s_prev - s_prev2;
            s_prev2 = s_prev;
            s_prev = s;
        }
        (s_prev * s_prev + s_prev2 * s_prev2 - coeff * s_prev * s_prev2).sqrt()
    }

    fn assert_dtmf(pcm: &[i16], row: f64, col: f64, label: &str) {
        let sr = 8000u32;
        let tone = {
            let start = pcm.iter().position(|s| s.abs() > 500).unwrap_or(0);
            let end = pcm
                .iter()
                .rposition(|s| s.abs() > 500)
                .map(|i| i + 1)
                .unwrap_or(pcm.len());
            &pcm[start..end]
        };
        assert!(
            tone.len() >= 400,
            "{label}: tone too short ({} samples)",
            tone.len()
        );
        let mag_row = goertzel(tone, row, sr);
        let mag_col = goertzel(tone, col, sr);
        let mag_noise = goertzel(tone, 1000.0, sr);
        let wrong_row = if (row - 697.0).abs() < 1.0 {
            770.0
        } else {
            697.0
        };
        let wrong_col = if (col - 1209.0).abs() < 1.0 {
            1336.0
        } else {
            1209.0
        };
        let mag_wr = goertzel(tone, wrong_row, sr);
        let mag_wc = goertzel(tone, wrong_col, sr);
        println!(
            "{label}: row({row})={mag_row:.0} col({col})={mag_col:.0} noise={mag_noise:.0} wrow={mag_wr:.0} wcol={mag_wc:.0} len={}",
            tone.len()
        );
        let min_ratio = 4.0;
        assert!(
            mag_row > mag_noise * min_ratio,
            "{label}: row {row} vs noise"
        );
        assert!(
            mag_col > mag_noise * min_ratio,
            "{label}: col {col} vs noise"
        );
        assert!(
            mag_row > mag_wr * min_ratio,
            "{label}: row {row} vs wrong row {wrong_row}"
        );
        assert!(
            mag_col > mag_wc * min_ratio,
            "{label}: col {col} vs wrong col {wrong_col}"
        );
    }

    // ── C1: audio written to file recorder ───────────────────────────────

    #[tokio::test]
    async fn forwarding_track_writes_audio_to_recorder() {
        use crate::media::recorder::Recorder;
        use audio_codec::CodecType;
        use std::sync::{Arc, Mutex};

        let temp_path = std::env::temp_dir().join("test_ft_recorder_audio.wav");
        let path_str = temp_path.to_str().unwrap();
        let record = Arc::new(Mutex::new(
            Recorder::new(path_str, CodecType::PCMU).unwrap(),
        ));

        // ForwardingTrack tees at recv, BEFORE transcoding. The recorder must
        // decode the ingress codec (PCMU). Set the ingress profile accordingly.
        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, None);
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, None);
        record
            .lock()
            .unwrap()
            .set_leg_profile(Leg::A, ingress.clone());

        let (rec_tx, rec_rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(0), // PCMU
            clock_rate: 8000,
            data: Bytes::from(vec![0xFFu8; 160]), // µ-law silence (audible: near-zero)
            ..Default::default()
        });

        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-ft-recorder".into(),
            track,
            Some(rec_tx),
            None,
            Leg::A,
            ingress,
            egress,
        );

        let rec_clone = record.clone();
        let drain = tokio::spawn(async move {
            let mut rx = rec_rx;
            while let Some((leg, s)) = rx.recv().await {
                let _ = rec_clone
                    .lock()
                    .unwrap()
                    .write_sample(leg, &s, None, None, None);
            }
            rec_clone.lock().unwrap().finalize().ok();
        });

        let result = ft.recv().await.expect("recv");
        // egress should have been transcoded PCMU→PCMA (PT 8)
        if let MediaSample::Audio(f) = &result {
            assert_eq!(
                f.payload_type,
                Some(8),
                "egress PT should be PCMA after transcode"
            );
        }
        // Drop tx so drain task exits and finalizes.
        drop(ft);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // drain task holds the rx — we already dropped the original ft which
        // won't drop the channel tx. Let's just finalize manually.
        record.lock().unwrap().finalize().ok();
        drain.abort();

        let wav = std::fs::read(path_str).unwrap();
        assert!(wav.len() > 44, "WAV file must contain audio data");
        let _ = std::fs::remove_file(&temp_path);
    }

    // ── C2: DTMF telephone-event rendered as audible tone ─────────────────

    #[tokio::test]
    async fn forwarding_track_dtmf_rendered_as_tone() {
        use crate::media::recorder::Recorder;
        use audio_codec::CodecType;
        use std::sync::{Arc, Mutex};

        let temp_path = std::env::temp_dir().join("test_ft_dtmf_tone.wav");
        let path_str = temp_path.to_str().unwrap();
        let record = Arc::new(Mutex::new(
            Recorder::new(path_str, CodecType::PCMU).unwrap(),
        ));

        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, Some(101));
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, Some(96));
        record
            .lock()
            .unwrap()
            .set_leg_profile(Leg::A, ingress.clone());

        let (rec_tx, mut rec_rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        // digit '5' (code 5), end bit, duration 1600 ticks @ 8 kHz = 200 ms
        let dtmf_payload = Bytes::from_static(&[5u8, 0x80, 0x06, 0x40]);
        let sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(101),
            clock_rate: 8000,
            data: dtmf_payload,
            ..Default::default()
        });

        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-ft-dtmf".into(),
            track,
            Some(rec_tx),
            None,
            Leg::A,
            ingress,
            egress,
        );

        let result = ft.recv().await.expect("recv");
        // DTMF is passed through with the egress PT.
        if let MediaSample::Audio(f) = &result {
            assert_eq!(
                f.payload_type,
                Some(96),
                "DTMF egress PT should be remapped"
            );
        }

        let mut guard = record.lock().unwrap();
        if let Ok((leg, sample)) = rec_rx.try_recv() {
            let _ = guard.write_sample(leg, &sample, Some(101), None, None);
        }
        guard.finalize().ok();
        drop(guard);

        let wav = std::fs::read(path_str).unwrap();
        assert!(
            wav.len() > 1000,
            "WAV must contain DTMF tone (length {})",
            wav.len()
        );

        // Decode PCMU left channel → PCM → Goertzel
        let pcm = {
            let data = &wav[44..];
            let left: Vec<u8> = data.iter().step_by(2).copied().collect();
            audio_codec::create_decoder(CodecType::PCMU).decode(&left)
        };
        assert_dtmf(&pcm, 770.0, 1336.0, "FT DTMF digit 5 / leg A");
        let _ = std::fs::remove_file(&temp_path);
    }

    // ── C3: both legs stereo via two ForwardingTracks ─────────────────────

    #[tokio::test]
    async fn forwarding_track_both_legs_stereo_isolation() {
        use crate::media::recorder::Recorder;
        use audio_codec::CodecType;
        use std::sync::{Arc, Mutex};

        let temp_path = std::env::temp_dir().join("test_ft_stereo.wav");
        let path_str = temp_path.to_str().unwrap();
        let record = Arc::new(Mutex::new(
            Recorder::new(path_str, CodecType::PCMU).unwrap(),
        ));
        record
            .lock()
            .unwrap()
            .set_leg_profile(Leg::A, NegotiatedLegProfile::default());
        record
            .lock()
            .unwrap()
            .set_leg_profile(Leg::B, NegotiatedLegProfile::default());

        let (tx_a, mut rx_a) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let track_a = OneShotTrack::new(audio_sample(0));
        let ft_a = ForwardingTrack::new(
            "test-ft-a".into(),
            track_a,
            Some(tx_a),
            None,
            Leg::A,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        let (tx_b, mut rx_b) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let track_b = OneShotTrack::new(audio_sample(0));
        let ft_b = ForwardingTrack::new(
            "test-ft-b".into(),
            track_b,
            Some(tx_b),
            None,
            Leg::B,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        ft_a.recv().await.unwrap();
        ft_b.recv().await.unwrap();

        let mut guard = record.lock().unwrap();
        if let Ok((leg, s)) = rx_a.try_recv() {
            let _ = guard.write_sample(leg, &s, None, None, None);
        }
        if let Ok((leg, s)) = rx_b.try_recv() {
            let _ = guard.write_sample(leg, &s, None, None, None);
        }
        guard.finalize().ok();
        drop(guard);

        let wav = std::fs::read(path_str).unwrap();
        // 44-byte header + stereo PCMU data (2 bytes per tick, one per leg)
        assert!(
            wav.len() >= 44 + 320,
            "WAV must contain stereo data (len={})",
            wav.len()
        );

        // Both channels should have audible samples; the key invariant is
        // that even-index bytes (leg A / left) and odd-index bytes (leg B /
        // right) are correctly interleaved by the recorder.
        let data = &wav[44..];
        let left: Vec<u8> = data.iter().step_by(2).copied().collect();
        let right: Vec<u8> = data.iter().skip(1).step_by(2).copied().collect();
        let left_pcm = create_decoder(CodecType::PCMU).decode(&left);
        let right_pcm = create_decoder(CodecType::PCMU).decode(&right);

        // Both legs received the same PCMU payload; both channels should be
        // similarly audible (non-silent).  audio_sample(0) uses [0u8;160]
        // which in µ-law decodes to a large-magnitude value.
        assert!(
            left_pcm.iter().any(|s| s.abs() > 1000) && right_pcm.iter().any(|s| s.abs() > 1000),
            "both stereo channels must be audible (left max={}, right max={})",
            left_pcm.iter().map(|s| s.abs()).max().unwrap_or(0),
            right_pcm.iter().map(|s| s.abs()).max().unwrap_or(0),
        );
        let _ = std::fs::remove_file(&temp_path);
    }

    // ── C4: audio-only guard skips video ─────────────────────────────────

    #[tokio::test]
    async fn forwarding_track_audio_only_guard_skips_video() {
        use rustrtc::media::frame::VideoFrame;

        let (rec_tx, mut rec_rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);

        let video = MediaSample::Video(VideoFrame::default());
        let track = OneShotTrack::new(video);
        let ft = ForwardingTrack::new(
            "test-ft-video".into(),
            track,
            Some(rec_tx),
            Some(sf_tx),
            Leg::A,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        ft.recv().await.unwrap();
        // After the audio-only guard (B2), neither channel should receive video.
        assert!(
            rec_rx.try_recv().is_err(),
            "recorder channel must NOT receive video"
        );
        assert!(
            sf_rx.try_recv().is_err(),
            "sipflow channel must NOT receive video"
        );
    }

    // ── C5: WebRTC / Opus transcoding — recorder gets pre-transcode ──────

    #[tokio::test]
    async fn forwarding_track_opus_transcoding_recorder_gets_pre_transcode() {
        use crate::media::recorder::Recorder;
        use audio_codec::{CodecType, create_encoder};
        use std::sync::{Arc, Mutex};

        let temp_path = std::env::temp_dir().join("test_ft_opus.wav");
        let path_str = temp_path.to_str().unwrap();
        let record = Arc::new(Mutex::new(
            Recorder::new(path_str, CodecType::PCMU).unwrap(),
        ));

        // Ingress = Opus (typical WebRTC), egress = PCMU
        let ingress = make_profile_with_dtmf(CodecType::Opus, 111, Some(101));
        let egress = make_profile_with_dtmf(CodecType::PCMU, 0, Some(101));
        // Recorder set to Opus leg profile so it can decode Opus → PCMU internally
        record
            .lock()
            .unwrap()
            .set_leg_profile(Leg::A, ingress.clone());

        // Encode a short sine-wave into Opus
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 / 20.0).sin() * 5000.0) as i16)
            .collect();
        let mut encoder = create_encoder(CodecType::Opus);
        let opus_data = encoder.encode(&pcm);
        assert!(!opus_data.is_empty());

        let sample = MediaSample::Audio(AudioFrame {
            payload_type: Some(111), // Opus
            clock_rate: 48000,
            data: Bytes::from(opus_data),
            ..Default::default()
        });

        let (rec_tx, mut rec_rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-ft-opus".into(),
            track,
            Some(rec_tx),
            None,
            Leg::A,
            ingress,
            egress,
        );

        ft.recv().await.unwrap();

        let mut guard = record.lock().unwrap();
        if let Ok((leg, s)) = rec_rx.try_recv() {
            // The teed sample should be the Opus ingress (pre-transcode).
            // write_sample internally transcodes Opus → PCMU → records.
            let _ = guard.write_sample(leg, &s, None, None, None);
        }
        guard.finalize().ok();
        drop(guard);

        let wav = std::fs::read(path_str).unwrap();
        assert!(
            wav.len() > 100,
            "Opus → PCMU WAV must contain audio (len={})",
            wav.len()
        );
        let _ = std::fs::remove_file(&temp_path);
    }

    // ── C6: sipflow channel receives audio ───────────────────────────────

    #[tokio::test]
    async fn forwarding_track_sipflow_captures_audio_and_not_video() {
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);
        let audio = audio_sample(0);
        let track = OneShotTrack::new(audio);
        let ft = ForwardingTrack::new(
            "test-ft-sipflow".into(),
            track,
            None,
            Some(sf_tx),
            Leg::B,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        ft.recv().await.unwrap();

        let (leg, sample, received_at_micros) = sf_rx
            .try_recv()
            .expect("sipflow channel must receive audio");
        assert_eq!(leg, Leg::B);
        assert!(received_at_micros > 0);
        assert!(matches!(*sample, MediaSample::Audio(_)), "must be audio");
    }

    // ── C7: transcoded egress PT confirmed in recorder channel ────────────

    #[tokio::test]
    async fn forwarding_track_recorder_channel_sees_ingress_pt() {
        use audio_codec::CodecType;

        let (rec_tx, mut rec_rx) = mpsc::channel::<(Leg, SharedMediaSample)>(256);
        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, None);
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, None);

        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample);
        let ft = ForwardingTrack::new(
            "test-ft-c7".into(),
            track,
            Some(rec_tx),
            None,
            Leg::A,
            ingress,
            egress,
        );

        // The forwarding track returns the TRANSCODED (egress) sample.
        let result = ft.recv().await.unwrap();
        if let MediaSample::Audio(f) = &result {
            assert_eq!(f.payload_type, Some(8), "egress output PT should be PCMA");
        }

        // The recorder channel should receive the INGRESS (pre-transcode) sample.
        let (_, channel_sample) = rec_rx
            .try_recv()
            .expect("recorder channel must have sample");
        if let MediaSample::Audio(f) = &*channel_sample {
            assert_eq!(
                f.payload_type,
                Some(0),
                "recorder channel PT should be ingress PCMU"
            );
        }
    }

    // ── C8: egress sipflow captures post-transcode sample as Leg::B ────

    #[tokio::test]
    async fn egress_sipflow_captures_post_transcode_as_leg_b() {
        use audio_codec::CodecType;

        let (egress_sf_tx, mut egress_sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);
        let (_, mut ingress_sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);

        // PCMU ingress → PCMA egress (transcoding needed)
        let ingress = make_profile_with_dtmf(CodecType::PCMU, 0, None);
        let egress = make_profile_with_dtmf(CodecType::PCMA, 8, None);

        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample);

        // callee→caller direction: ingress sipflow = None, egress sipflow = Some
        let ft = ForwardingTrack::with_egress(
            "test-ft-c8".into(),
            track,
            None, // no recorder channel
            None, // no ingress sipflow (callee ingress should NOT be captured)
            Some(egress_sf_tx),
            Some(Leg::B), // egress tagged as Leg::B (caller's output)
            Leg::B,
            ingress,
            egress,
        );

        let result = ft.recv().await.unwrap();
        // The returned sample is the transcoded egress.
        if let MediaSample::Audio(f) = &result {
            assert_eq!(f.payload_type, Some(8), "egress output PT should be PCMA");
        }

        // Ingress sipflow channel should NOT receive anything (set to None).
        assert!(
            ingress_sf_rx.try_recv().is_err(),
            "ingress sipflow must be empty (None passed)"
        );

        // Egress sipflow channel must receive the post-transcode sample as Leg::B.
        let (leg, sf_sample, received_at_micros) = egress_sf_rx
            .try_recv()
            .expect("egress sipflow channel must have sample");
        assert_eq!(leg, Leg::B, "egress sipflow must be tagged Leg::B");
        assert!(received_at_micros > 0);
        if let MediaSample::Audio(f) = &*sf_sample {
            assert_eq!(
                f.payload_type,
                Some(8),
                "egress sipflow sample PT should be PCMA (post-transcode)"
            );
        }
    }

    #[tokio::test]
    async fn egress_sipflow_unset_does_not_capture() {
        // Without egress_sipflow_tx, no egress capture should happen.
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);
        let sample = audio_sample(0);
        let track = OneShotTrack::new(sample);

        // Using new() (not with_egress) → no egress capture configured
        let ft = ForwardingTrack::new(
            "test-ft-c8b".into(),
            track,
            None,
            Some(sf_tx),
            Leg::A,
            NegotiatedLegProfile::default(),
            NegotiatedLegProfile::default(),
        );

        ft.recv().await.unwrap();

        // Ingress sipflow should have captured the sample as Leg::A.
        let (leg, _, _) = sf_rx
            .try_recv()
            .expect("ingress sipflow must receive sample");
        assert_eq!(leg, Leg::A);

        // There should be only one sample in the sipflow channel (no duplicate egress).
        assert!(
            sf_rx.try_recv().is_err(),
            "sipflow must not have a second sample from egress"
        );
    }

    #[tokio::test]
    async fn egress_sipflow_no_transcode_captures_unchanged_sample() {
        // When no transcoding is needed, the egress is the same as the ingress.
        let (egress_sf_tx, mut egress_sf_rx) = mpsc::channel::<(Leg, SharedMediaSample, u64)>(256);

        // Same codec → no transcoding needed
        let profile = make_profile_with_dtmf(audio_codec::CodecType::PCMU, 0, None);

        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample);

        let ft = ForwardingTrack::with_egress(
            "test-ft-c8c".into(),
            track,
            None,
            None,
            Some(egress_sf_tx),
            Some(Leg::B),
            Leg::B,
            profile.clone(),
            profile,
        );

        ft.recv().await.unwrap();

        let (leg, _, _) = egress_sf_rx
            .try_recv()
            .expect("egress sipflow must receive sample even without transcoding");
        assert_eq!(leg, Leg::B);
    }
}
