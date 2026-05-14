use crate::media::negotiate::NegotiatedLegProfile;
use crate::media::transcoder::{RtpTiming, Transcoder, rewrite_dtmf_duration};
use crate::media::{Track, recorder::Leg};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use parking_lot::Mutex;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{MediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioMapping {
    pub source_pt: u8,
    pub target_pt: u8,
    pub source_clock_rate: u32,
    pub target_clock_rate: u32,
}

/// Mapping for a single DTMF PT from source to target, or drop if target is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    recorder_tx: Option<mpsc::Sender<(Leg, MediaSample)>>,
    sipflow_tx: Option<mpsc::Sender<(Leg, MediaSample)>>,
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
        recorder_tx: Option<mpsc::Sender<(Leg, MediaSample)>>,
        sipflow_tx: Option<mpsc::Sender<(Leg, MediaSample)>>,
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

            let audio_mapping = self.audio_mapping.lock().clone();
            let dtmf_mapping = self.dtmf_mapping.lock().clone();
            let sample = self.inner.recv().await?;

            if let Some(tx) = &self.recorder_tx {
                let _ = tx.try_send((self.recorder_leg, sample.clone()));
            }

            // SipFlow RTP recording: non-blocking tee, drops if consumer falls behind.
            if let Some(tx) = &self.sipflow_tx {
                let _ = tx.try_send((self.recorder_leg, sample.clone()));
            }

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

                if let Some(mapping) = dtmf_mapping.as_ref().filter(|_| matched_dtmf) {
                    if let Some(target_pt) = mapping.target_pt {
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

                        return Ok(MediaSample::Audio(dtmf_frame));
                    }

                    return Ok(sample);
                }

                if let Some(audio_mapping) = audio_mapping.as_ref().filter(|_| matched_audio) {
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

                        return Ok(MediaSample::Audio(output));
                    }

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

                        return Ok(MediaSample::Audio(output));
                    }
                }
            }

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
    use bytes::Bytes;
    use rustrtc::media::frame::AudioFrame;

    /// Minimal track that yields exactly one pre-defined sample then blocks.
    struct OneShotTrack {
        sample: tokio::sync::Mutex<Option<MediaSample>>,
    }

    impl OneShotTrack {
        fn new(sample: MediaSample) -> Arc<Self> {
            Arc::new(Self {
                sample: tokio::sync::Mutex::new(Some(sample)),
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
                let mut guard = self.sample.lock().await;
                if let Some(s) = guard.take() {
                    return Ok(s);
                }
                // Block until the test polls again — simulate a quiet stream.
                drop(guard);
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
        let (tx, mut rx) = mpsc::channel::<(Leg, MediaSample)>(256);
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
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, MediaSample)>(256);
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
        let (leg, _sf_sample) = sf_rx.try_recv().expect("sample must be in sipflow channel");
        assert_eq!(leg, Leg::A);
    }

    /// Both recorder_tx AND sipflow_tx can be active simultaneously; each
    /// must receive its own copy of the sample.
    #[tokio::test]
    async fn both_recorder_and_sipflow_receive_sample() {
        let (rec_tx, mut rec_rx) = mpsc::channel::<(Leg, MediaSample)>(256);
        let (sf_tx, mut sf_rx) = mpsc::channel::<(Leg, MediaSample)>(256);
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
        let (sf_leg, _) = sf_rx.try_recv().expect("sipflow channel must have sample");
        assert_eq!(rec_leg, Leg::B);
        assert_eq!(sf_leg, Leg::B);
    }

    #[tokio::test]
    async fn sipflow_full_channel_does_not_block() {
        let (sf_tx, _sf_rx) = mpsc::channel::<(Leg, MediaSample)>(1);

        let _ = sf_tx.try_send((Leg::A, audio_sample(0)));

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
}
