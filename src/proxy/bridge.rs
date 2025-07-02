use crate::{
    config::MediaProxyConfig,
    event::EventSender,
    media::{
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
            MediaBridgeType::WebRtcToRtp => {
                let track = Self::create_rtptrack(
                    self.cancel_token.clone(),
                    TrackId::new(),
                    TrackConfig::default(),
                    self.config.clone(),
                )
                .await?;
                let offer = track.local_description()?;
                self.callee_track = Some(Box::new(track));
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
                let _callee_answer = track.handshake(callee_answer, timeout).await?;
                match self.bridge_type {
                    MediaBridgeType::WebRtcToRtp => {
                        let track_config = track.config().clone();
                        let mut caller_track = WebrtcTrack::new(
                            self.cancel_token.clone(),
                            TrackId::new(),
                            track_config,
                        );
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
