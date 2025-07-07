use crate::{
    config::MediaProxyConfig,
    event::EventSender,
    media::{
        negotiate::prefer_audio_codec,
        recorder::RecorderOption,
        stream::{MediaStream, MediaStreamBuilder},
        track::{
            rtp::{RtpTrack, RtpTrackBuilder},
            webrtc::WebrtcTrack,
            Track, TrackConfig,
        },
    },
    TrackId,
};
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

#[derive(Debug, Copy, Clone)]
pub enum MediaBridgeType {
    WebRtcToRtp,
    RtpToWebRtc,
    Rtp2Rtp,
    Webrtc2Webrtc,
}
pub struct MediaBridgeBuilder {
    pub bridge_type: MediaBridgeType,
    pub config: Arc<MediaProxyConfig>,
    pub cancel_token: CancellationToken,
    pub caller_track: Option<Box<dyn Track>>,
    pub callee_track: Option<Box<dyn Track>>,
    pub recorder_config: Option<RecorderOption>,
}

impl MediaBridgeBuilder {
    async fn create_rtptrack(
        token: CancellationToken,
        track_id: TrackId,
        track_config: TrackConfig,
        mediaproxy_config: Arc<MediaProxyConfig>,
    ) -> Result<RtpTrack> {
        let mut rtp_track = RtpTrackBuilder::new(track_id, track_config).with_cancel_token(token);

        if let Some(rtp_start_port) = mediaproxy_config.rtp_start_port {
            rtp_track = rtp_track.with_rtp_start_port(rtp_start_port);
        }
        if let Some(rtp_end_port) = mediaproxy_config.rtp_end_port {
            rtp_track = rtp_track.with_rtp_end_port(rtp_end_port);
        }
        if let Some(ref external_ip) = mediaproxy_config.external_ip {
            rtp_track = rtp_track.with_external_addr(external_ip.parse()?);
        }
        rtp_track.build().await
    }

    pub async fn new(
        bridge_type: MediaBridgeType,
        cancel_token: CancellationToken,
        config: Arc<MediaProxyConfig>,
    ) -> Result<Self> {
        Ok(Self {
            bridge_type,
            config,
            cancel_token,
            caller_track: None,
            callee_track: None,
            recorder_config: None,
        })
    }

    pub fn with_recorder_config(mut self, recorder_config: RecorderOption) -> Self {
        self.recorder_config = Some(recorder_config);
        self
    }
    /// get local description for callee track
    pub async fn local_description(&mut self) -> Result<String> {
        if self.callee_track.is_some() {
            return Err(anyhow::anyhow!("Callee track already set"));
        }

        match self.bridge_type {
            MediaBridgeType::WebRtcToRtp | MediaBridgeType::Webrtc2Webrtc => {
                let callee_track = Self::create_rtptrack(
                    self.cancel_token.clone(),
                    format!("rtp-{}", rand::random::<u64>()),
                    TrackConfig::default(),
                    self.config.clone(),
                )
                .await?;
                let offer = callee_track.local_description()?;
                self.callee_track = Some(Box::new(callee_track));
                Ok(offer)
            }
            MediaBridgeType::Rtp2Rtp | MediaBridgeType::RtpToWebRtc => {
                let callee_track = Self::create_rtptrack(
                    self.cancel_token.clone(),
                    format!("rtp-{}", rand::random::<u64>()),
                    TrackConfig::default(),
                    self.config.clone(),
                )
                .await?;
                let offer = callee_track.local_description()?;
                self.callee_track = Some(Box::new(callee_track));
                Ok(offer)
            }
            _ => todo!(),
        }
    }

    /// handshake for caller and callee tracks
    pub async fn handshake(
        &mut self,
        caller_offer: String,
        callee_answer: String,
        timeout: Option<Duration>,
    ) -> Result<String> {
        match self.callee_track {
            Some(ref mut track) => {
                let prefered_codec;
                match track.handshake(callee_answer, timeout).await {
                    Ok(answer) => {
                        debug!(id = track.id(), "Callee track handshake: {:?}", answer);
                        let remote_desc = RTCSessionDescription::answer(answer)?;
                        match prefer_audio_codec(&remote_desc.unmarshal()?) {
                            Some(codec) => prefered_codec = Some(codec),
                            None => {
                                return Err(anyhow::anyhow!("No codec found"));
                            }
                        };
                    }
                    Err(e) => {
                        error!(
                            id = track.id(),
                            caller_offer, "Failed to handshake callee track: {:?}", e
                        );
                        return Err(e);
                    }
                };
                match self.bridge_type {
                    MediaBridgeType::WebRtcToRtp | MediaBridgeType::Webrtc2Webrtc => {
                        let track_config = track.config().clone();
                        let mut caller_track = WebrtcTrack::new(
                            self.cancel_token.clone(),
                            format!("webrtc-{}", track.id().to_string()),
                            track_config,
                        );
                        caller_track.prefered_codec = prefered_codec;
                        let answer = caller_track.handshake(caller_offer, timeout).await?;
                        self.caller_track = Some(Box::new(caller_track));
                        Ok(answer)
                    }
                    MediaBridgeType::Rtp2Rtp | MediaBridgeType::RtpToWebRtc => {
                        let track_config = track.config().clone();
                        let mut caller_track = Self::create_rtptrack(
                            self.cancel_token.clone(),
                            format!("rtp-{}", track.id().to_string()),
                            track_config,
                            self.config.clone(),
                        )
                        .await?;
                        let answer = caller_track.handshake(caller_offer, timeout).await?;
                        self.caller_track = Some(Box::new(caller_track));
                        Ok(answer)
                    }
                    _ => todo!(),
                }
            }
            None => Err(anyhow::anyhow!("Callee track not set")),
        }
    }

    pub async fn build(self, event_sender: EventSender) -> MediaStream {
        let mut builder =
            MediaStreamBuilder::new(event_sender).with_cancel_token(self.cancel_token);
        if let Some(recorder_config) = self.recorder_config {
            builder = builder.with_recorder_config(recorder_config);
        }
        let stream = builder.build();
        if let Some(caller_track) = self.caller_track {
            stream.update_track(caller_track).await;
        }
        if let Some(callee_track) = self.callee_track {
            stream.update_track(callee_track).await;
        }
        stream
    }
}
