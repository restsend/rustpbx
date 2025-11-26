use super::track_codec::TrackCodec;
use crate::{
    config::IceServer,
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::{
        codecs::CodecType,
        negotiate::prefer_audio_codec,
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use std::{sync::Arc, time::SystemTime};
use tokio::time::sleep;
use tokio::{select, sync::Mutex, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use webrtc::{
    api::{
        APIBuilder,
        media_engine::{
            MIME_TYPE_G722, MIME_TYPE_PCMA, MIME_TYPE_PCMU, MIME_TYPE_TELEPHONE_EVENT, MediaEngine,
        },
        setting_engine::SettingEngine,
    },
    ice_transport::{ice_candidate_type::RTCIceCandidateType, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{
        RTCRtpTransceiver,
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
        rtp_receiver::RTCRtpReceiver,
    },
    track::{track_local::TrackLocal, track_remote::TrackRemote},
};
use webrtc::{
    peer_connection::RTCPeerConnection,
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
// Configuration for integrating a webrtc crate track with our WebrtcTrack
#[derive(Clone)]
pub struct WebrtcTrackConfig {
    pub track: Arc<TrackLocalStaticSample>,
    pub payload_type: u8,
}

pub struct WebrtcTrack {
    track_id: TrackId,
    track_config: TrackConfig,
    processor_chain: ProcessorChain,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
    local_track: Option<Arc<TrackLocalStaticSample>>,
    encoder: TrackCodec,
    pub prefered_codec: Option<CodecType>,
    ssrc: u32,
    pub peer_connection: Option<Arc<RTCPeerConnection>>,
    pub ice_servers: Option<Vec<IceServer>>,
    pub external_ip: Option<String>,
}

impl WebrtcTrack {
    pub fn create_audio_track(
        codec: CodecType,
        stream_id: Option<String>,
    ) -> Arc<TrackLocalStaticSample> {
        let stream_id = stream_id.unwrap_or("rustpbx-track".to_string());
        Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: codec.mime_type().to_string(),
                clock_rate: codec.clock_rate(),
                channels: codec.channels(),
                ..Default::default()
            },
            "audio".to_string(),
            stream_id,
        ))
    }
    pub fn get_media_engine(prefered_codec: Option<CodecType>) -> Result<MediaEngine> {
        let mut media_engine = MediaEngine::default();
        for codec in vec![
            #[cfg(feature = "opus")]
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "audio/opus".to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_G722.to_owned(),
                    clock_rate: 8000,
                    channels: 1,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 9,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_PCMU.to_owned(),
                    clock_rate: 8000,
                    channels: 1,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 0,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_PCMA.to_owned(),
                    clock_rate: 8000,
                    channels: 1,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 8,
                ..Default::default()
            },
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_TELEPHONE_EVENT.to_owned(),
                    clock_rate: 8000,
                    channels: 1,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 101,
                ..Default::default()
            },
        ] {
            if let Some(prefered_codec) = prefered_codec {
                if codec.capability.mime_type == prefered_codec.mime_type() {
                    media_engine.register_codec(codec, RTPCodecType::Audio)?;
                }
            } else {
                media_engine.register_codec(codec, RTPCodecType::Audio)?;
            }
        }
        Ok(media_engine)
    }

    pub fn new(
        cancel_token: CancellationToken,
        id: TrackId,
        track_config: TrackConfig,
        ice_servers: Option<Vec<IceServer>>,
    ) -> Self {
        let processor_chain = ProcessorChain::new(track_config.samplerate);
        Self {
            track_id: id,
            track_config,
            processor_chain,
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token,
            local_track: None,
            encoder: TrackCodec::new(),
            prefered_codec: None,
            ssrc: 0,
            peer_connection: None,
            ice_servers,
            external_ip: None,
        }
    }

    pub fn with_external_ip(mut self, external_ip: String) -> Self {
        self.external_ip = Some(external_ip);
        self
    }

    pub fn with_ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = ssrc;
        self
    }

    pub fn with_prefered_codec(mut self, codec: Option<CodecType>) -> Self {
        self.prefered_codec = codec;
        self
    }

    async fn create(&mut self) -> Result<()> {
        let media_engine = Self::get_media_engine(self.prefered_codec)?;
        let mut setting_engine = SettingEngine::default();

        if let Some(ref external_ip) = self.external_ip {
            setting_engine.set_nat_1to1_ips(vec![external_ip.clone()], RTCIceCandidateType::Srflx);
        }
        let api = APIBuilder::new()
            .with_setting_engine(setting_engine)
            .with_media_engine(media_engine)
            .build();

        let ice_servers = if let Some(ice_servers) = &self.ice_servers {
            ice_servers
                .iter()
                .map(|s| RTCIceServer {
                    urls: s.urls.clone(),
                    username: s.username.clone().unwrap_or_default(),
                    credential: s.credential.clone().unwrap_or_default(),
                    ..Default::default()
                })
                .collect()
        } else {
            vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_string()],
                ..Default::default()
            }]
        };
        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let cancel_token = self.cancel_token.clone();
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);
        self.peer_connection = Some(peer_connection.clone());
        let peer_connection_clone = peer_connection.clone();

        let cancel_token_clone = cancel_token.clone();
        let track_id = self.track_id.clone();
        peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                debug!(track_id, "peer connection state changed: {}", s);
                let cancel_token = cancel_token.clone();
                let peer_connection_clone = peer_connection_clone.clone();
                let track_id_clone = track_id.clone();
                Box::pin(async move {
                    match s {
                        RTCPeerConnectionState::Connected => {}
                        RTCPeerConnectionState::Disconnected
                        | RTCPeerConnectionState::Closed
                        | RTCPeerConnectionState::Failed => {
                            info!(
                                track_id = track_id_clone,
                                "peer connection is {}, try to close", s
                            );
                            cancel_token.cancel();
                            peer_connection_clone.close().await.ok();
                        }
                        _ => {}
                    }
                })
            },
        ));
        let packet_sender = self.packet_sender.clone();
        let track_id_clone = self.track_id.clone();
        let processor_chain = self.processor_chain.clone();
        peer_connection.on_track(Box::new(
            move |track: Arc<TrackRemote>,
                  _receiver: Arc<RTCRtpReceiver>,
                  _transceiver: Arc<RTCRtpTransceiver>| {
                let track_id_clone = track_id_clone.clone();
                let packet_sender_clone = packet_sender.clone();
                let processor_chain = processor_chain.clone();
                let track_samplerate = match track.codec().payload_type {
                    9 => 16000,   // G722
                    111 => 48000, // Opus
                    _ => 8000,    // PCMU, PCMA, TELEPHONE_EVENT
                };
                info!(
                    track_id=track_id_clone,
                    "on_track received: {} samplerate: {}",
                    track.codec().capability.mime_type,
                    track_samplerate,
                );
                let cancel_token_clone = cancel_token_clone.clone();
                Box::pin(async move {
                    loop {
                        select! {
                            _ = cancel_token_clone.cancelled() => {
                                info!(track_id=track_id_clone, "track cancelled");
                                break;
                            }
                            Ok((packet, _)) = track.read_rtp() => {
                                let packet_sender = packet_sender_clone.lock().await;
                            if let Some(sender) = packet_sender.as_ref() {
                                let mut frame = AudioFrame {
                                    track_id: track_id_clone.clone(),
                                    samples: crate::media::Samples::RTP {
                                        payload_type: packet.header.payload_type,
                                        payload: packet.payload.to_vec(),
                                        sequence_number: packet.header.sequence_number,
                                    },
                                    timestamp: crate::media::get_timestamp(),
                                    sample_rate: track_samplerate,
                                    ..Default::default()
                                };
                                if let Err(e) = processor_chain.process_frame(&mut frame) {
                                    warn!(track_id=track_id_clone,"Failed to process frame: {}", e);
                                    break;
                                }
                                match sender.send(frame) {
                                    Ok(_) => {}
                                    Err(e) => {
                                        warn!(track_id=track_id_clone,"Failed to send packet: {}", e);
                                        break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
            },
        ));

        #[cfg(feature = "opus")]
        let codec = self.prefered_codec.clone().unwrap_or(CodecType::Opus);

        #[cfg(not(feature = "opus"))]
        let codec = self.prefered_codec.clone().unwrap_or(CodecType::G722);

        let track = Self::create_audio_track(codec, Some(self.track_id.clone()));
        peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        self.local_track = Some(track.clone());
        self.track_config.codec = codec;

        Ok(())
    }

    pub async fn setup_with_offer(
        &mut self,
        offer: String,
        timeout: Option<Duration>,
    ) -> Result<RTCSessionDescription> {
        let remote_desc = RTCSessionDescription::offer(offer)?;
        if self.prefered_codec.is_none() {
            let codec = match prefer_audio_codec(&remote_desc.unmarshal()?) {
                Some(codec) => codec,
                None => {
                    return Err(anyhow::anyhow!("No codec found"));
                }
            };
            self.prefered_codec = Some(codec);
        }
        self.create().await?;

        let peer_connection = self
            .peer_connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Peer connection is not created"))?;

        peer_connection.set_remote_description(remote_desc).await?;

        let answer = peer_connection.create_answer(None).await?;
        let mut gather_complete = peer_connection.gathering_complete_promise().await;
        peer_connection.set_local_description(answer).await?;
        select! {
            _ = gather_complete.recv() => {
                info!(track_id = self.track_id,"ICE candidate received");
            }
            _ = sleep(timeout.unwrap_or(HANDSHAKE_TIMEOUT)) => {
                warn!(track_id = self.track_id,"wait candidate timeout");
            }
        }

        let answer = peer_connection
            .local_description()
            .await
            .ok_or(anyhow::anyhow!("Failed to get local description"))?;

        info!(
            track_id = self.track_id,
            codec = ?self.prefered_codec,
            "set remote description and create answer success"
        );
        Ok(answer)
    }

    pub async fn local_description(&mut self) -> Result<String> {
        if self.peer_connection.is_none() {
            self.create().await?;
            if let Some(peer_connection) = &self.peer_connection {
                let offer = peer_connection.create_offer(None).await?;
                peer_connection.set_local_description(offer).await?;
                peer_connection
                    .gathering_complete_promise()
                    .await
                    .recv()
                    .await;
            }
        }
        let peer_connection = self
            .peer_connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Peer connection is not created"))?;

        peer_connection
            .local_description()
            .await
            .ok_or(anyhow::anyhow!("Failed to get local description"))
            .map(|desc| desc.sdp)
    }
}

#[async_trait]
impl Track for WebrtcTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.track_config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }

    async fn handshake(&mut self, offer: String, timeout: Option<Duration>) -> Result<String> {
        self.setup_with_offer(offer, timeout)
            .await
            .map(|answer| answer.sdp)
    }

    async fn update_remote_description(&mut self, answer: &String) -> Result<()> {
        let peer_connection = self
            .peer_connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Peer connection is not created"))?;
        let remote_desc = RTCSessionDescription::answer(answer.clone())?;
        peer_connection.set_remote_description(remote_desc).await?;
        Ok(())
    }

    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Store the packet sender
        *self.packet_sender.lock().await = Some(packet_sender.clone());
        let token_clone = self.cancel_token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.track_id.clone();
        let start_time = crate::media::get_timestamp();
        let ssrc = self.ssrc;
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let _ = event_sender_clone.send(SessionEvent::TrackEnd {
                track_id,
                timestamp: crate::media::get_timestamp(),
                duration: crate::media::get_timestamp() - start_time,
                ssrc,
                play_id: None,
            });
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Cancel all processing
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        if self.local_track.is_none() {
            return Ok(());
        }
        let local_track = match self.local_track.as_ref() {
            Some(track) => track,
            None => {
                return Ok(()); // no local track, ignore
            }
        };

        let payload_type = self.track_config.codec.payload_type();
        let (_payload_type, payload) = self.encoder.encode(payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }

        let sample = webrtc::media::Sample {
            data: payload.into(),
            duration: Duration::from_millis(self.track_config.ptime.as_millis() as u64),
            timestamp: SystemTime::now(),
            packet_timestamp: packet.timestamp as u32,
            ..Default::default()
        };
        match local_track.write_sample(&sample).await {
            Ok(_) => {}
            Err(e) => {
                warn!("failed to send sample: {}", e);
                return Err(anyhow::anyhow!("Failed to send sample: {}", e));
            }
        }
        Ok(())
    }
}
