use crate::media::audio_egress_track::{
    AudioEgressTrack, AudioInputTap, RecordedAudioSample, RecorderTap,
};
use crate::media::negotiate::{CodecInfo, NegotiatedLegProfile};
use crate::media::recorder::{Leg as RecorderLeg, Recorder};
use crate::media::{RtcTrack, RtpTrackBuilder};
use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub type AudioEgressSlot = AsyncMutex<Option<Arc<AudioEgressTrack>>>;

const AUDIO_EGRESS_TRACK_ID: &str = "audio-egress";

#[derive(Debug, Clone)]
pub struct LocalAudioTrackOptions {
    pub track_id: String,
    pub is_webrtc: bool,
    pub cancel_token: CancellationToken,
    pub codec_info: Vec<CodecInfo>,
    pub enable_latching: bool,
    pub external_ip: Option<String>,
    pub bind_ip: Option<String>,
    pub rtp_range: (Option<u16>, Option<u16>),
    pub ice_servers: Option<Vec<rustrtc::IceServer>>,
}

pub fn build_local_audio_track(options: LocalAudioTrackOptions) -> RtcTrack {
    let mut track_builder = RtpTrackBuilder::new(options.track_id)
        .with_cancel_token(options.cancel_token)
        .with_enable_latching(options.enable_latching);

    if let Some(external_ip) = options.external_ip {
        track_builder = track_builder.with_external_ip(external_ip);
    }
    if let Some(bind_ip) = options.bind_ip {
        track_builder = track_builder.with_bind_ip(bind_ip);
    }

    if let (Some(start), Some(end)) = options.rtp_range {
        track_builder = track_builder.with_rtp_range(start, end);
    }

    if !options.codec_info.is_empty() {
        track_builder = track_builder.with_codec_info(options.codec_info);
    }

    if options.is_webrtc {
        track_builder = track_builder.with_mode(rustrtc::TransportMode::WebRtc);
        if let Some(ice_servers) = options.ice_servers {
            track_builder = track_builder.with_ice_servers(ice_servers);
        }
    }

    track_builder.build()
}

pub fn input_audio_track(
    pc: &rustrtc::PeerConnection,
) -> Option<Arc<dyn rustrtc::media::MediaStreamTrack>> {
    pc.get_transceivers()
        .into_iter()
        .find(|t| t.kind() == rustrtc::MediaKind::Audio)
        .and_then(|t| t.receiver())
        .map(|receiver| receiver.track() as Arc<dyn rustrtc::media::MediaStreamTrack>)
}

pub async fn ensure_audio_egress(
    slot: &AudioEgressSlot,
    target_pc: &rustrtc::PeerConnection,
    egress_profile: NegotiatedLegProfile,
    session_id: &str,
    direction: &str,
) -> Result<Arc<AudioEgressTrack>> {
    let mut guard = slot.lock().await;
    install_audio_egress_track(&mut guard, target_pc, egress_profile, session_id, direction)
}

pub async fn stage_peer_audio_route(
    source_pc: &rustrtc::PeerConnection,
    target_slot: &AudioEgressSlot,
    target_pc: &rustrtc::PeerConnection,
    ingress_profile: NegotiatedLegProfile,
    egress_profile: NegotiatedLegProfile,
    tap: AudioInputTap,
    session_id: &str,
    direction: &str,
) -> Result<()> {
    let source_track = input_audio_track(source_pc)
        .ok_or_else(|| anyhow::anyhow!("{}: source PC has no audio receiver", direction))?;
    let output = ensure_audio_egress(
        target_slot,
        target_pc,
        egress_profile.clone(),
        session_id,
        direction,
    )
    .await?;

    output.stage_peer_with_tap(source_track, ingress_profile, egress_profile, tap);
    debug!(
        session_id = %session_id,
        direction = %direction,
        "Staged peer source on audio egress track"
    );

    Ok(())
}

pub fn recorder_tap(
    recorder: Arc<RwLock<Option<Recorder>>>,
    leg: RecorderLeg,
    profile: NegotiatedLegProfile,
) -> RecorderTap {
    const RECORDER_CHANNEL_CAPACITY: usize = 256;
    let (tx, mut rx) = mpsc::channel::<RecordedAudioSample>(RECORDER_CHANNEL_CAPACITY);

    tokio::spawn(async move {
        while let Some(sample) = rx.recv().await {
            let mut guard = recorder.write();
            if let Some(rec) = guard.as_mut()
                && let Err(err) =
                    rec.write_sample(sample.leg, &sample.sample, None, None, sample.codec_hint)
            {
                tracing::warn!("recorder write_sample failed: {err}");
            }
        }
    });

    RecorderTap::new(tx, leg, profile)
}

fn install_audio_egress_track(
    slot: &mut Option<Arc<AudioEgressTrack>>,
    target_pc: &rustrtc::PeerConnection,
    egress_profile: NegotiatedLegProfile,
    session_id: &str,
    direction: &str,
) -> Result<Arc<AudioEgressTrack>> {
    let target_transceiver = target_pc
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == rustrtc::MediaKind::Audio)
        .ok_or_else(|| anyhow::anyhow!("{}: no audio transceiver on target PC", direction))?;

    let existing_sender = target_transceiver
        .sender()
        .ok_or_else(|| anyhow::anyhow!("{}: no sender on target audio transceiver", direction))?;

    let created_output = slot.is_none();
    let output = match slot {
        Some(track) => track.clone(),
        None => {
            let track = Arc::new(AudioEgressTrack::new(
                AUDIO_EGRESS_TRACK_ID.to_string(),
                egress_profile,
            ));
            *slot = Some(track.clone());
            track
        }
    };

    if created_output || existing_sender.track_id() != AUDIO_EGRESS_TRACK_ID {
        let sender = rustrtc::RtpSender::builder(
            output.clone() as Arc<dyn rustrtc::media::MediaStreamTrack>,
            existing_sender.ssrc(),
        )
        .stream_id(existing_sender.stream_id().to_string())
        .params(existing_sender.params())
        .build();

        target_transceiver.set_sender(Some(sender));

        debug!(
            session_id = %session_id,
            direction = %direction,
            track_id = %AUDIO_EGRESS_TRACK_ID,
            "Installed audio egress track on target sender"
        );
    }

    Ok(output)
}
