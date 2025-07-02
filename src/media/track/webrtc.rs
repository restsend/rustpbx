use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        codecs::CodecType,
        negotiate::prefer_audio_codec,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    AudioFrame,
};
use anyhow::Result;
use async_trait::async_trait;
use std::{sync::Arc, time::SystemTime};
use tokio::time::sleep;
use tokio::{select, sync::Mutex, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::{
    api::{
        media_engine::{
            MediaEngine, MIME_TYPE_G722, MIME_TYPE_PCMA, MIME_TYPE_PCMU, MIME_TYPE_TELEPHONE_EVENT,
        },
        APIBuilder,
    },
    ice_transport::ice_server::RTCIceServer,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
        rtp_receiver::RTCRtpReceiver,
        RTCRtpTransceiver,
    },
    track::{track_local::TrackLocal, track_remote::TrackRemote},
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
                channels: 1,
                ..Default::default()
            },
            "audio".to_string(),
            stream_id,
        ))
    }
    pub fn get_media_engine() -> Result<MediaEngine> {
        let mut media_engine = MediaEngine::default();
        for codec in vec![
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
            media_engine.register_codec(codec, RTPCodecType::Audio)?;
        }
        Ok(media_engine)
    }

    pub async fn get_ice_servers() -> Vec<RTCIceServer> {
        let mut servers = vec![];
        if let Ok(server) = std::env::var("STUN_SERVER") {
            servers.push(RTCIceServer {
                urls: vec![server],
                ..Default::default()
            });
        }
        servers
    }

    pub fn new(cancel_token: CancellationToken, id: TrackId, track_config: TrackConfig) -> Self {
        let processor_chain = ProcessorChain::new(track_config.samplerate);
        Self {
            track_id: id,
            track_config,
            processor_chain,
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token,
            local_track: None,
            encoder: TrackCodec::new(),
        }
    }

    pub async fn setup_webrtc_track(
        &mut self,
        offer: String,
        timeout: Option<Duration>,
    ) -> Result<RTCSessionDescription> {
        let media_engine = Self::get_media_engine()?;
        let api = APIBuilder::new().with_media_engine(media_engine).build();

        let config = RTCConfiguration {
            ice_servers: Self::get_ice_servers().await,
            ..Default::default()
        };

        let cancel_token = self.cancel_token.clone();
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);
        let peer_connection_clone = peer_connection.clone();
        peer_connection.on_ice_candidate(Box::new(
            move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
                info!("ICE candidate received: {:?}", candidate);
                Box::pin(async move {})
            },
        ));
        let cancel_token_clone = cancel_token.clone();
        peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                info!("peer connection state changed: {}", s);
                let cancel_token = cancel_token.clone();
                let peer_connection_clone = peer_connection_clone.clone();
                Box::pin(async move {
                    match s {
                        RTCPeerConnectionState::Connected => {}
                        RTCPeerConnectionState::Closed
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Disconnected => {
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
                    9 => 16000, // G722
                    _ => 8000,  // PCMU, PCMA, TELEPHONE_EVENT
                };
                info!(
                    "on_track received: {} track_samplerate: {}",
                    track.codec().capability.mime_type,
                    track_samplerate,
                );
                let cancel_token_clone = cancel_token_clone.clone();
                Box::pin(async move {
                    loop {
                        select! {
                            _ = cancel_token_clone.cancelled() => {
                                info!("track cancelled");
                                break;
                            }
                            Ok((packet, _)) = track.read_rtp() => {
                                let packet_sender = packet_sender_clone.lock().await;
                            if let Some(sender) = packet_sender.as_ref() {
                                let frame = AudioFrame {
                                    track_id: track_id_clone.clone(),
                                    samples: crate::Samples::RTP {
                                        payload_type: packet.header.payload_type,
                                        payload: packet.payload.to_vec(),
                                        sequence_number: packet.header.sequence_number,
                                    },
                                    timestamp: crate::get_timestamp(),
                                    sample_rate: track_samplerate,
                                    ..Default::default()
                                };
                                if let Err(e) = processor_chain.process_frame(&frame) {
                                    error!("Failed to process frame: {}", e);
                                    break;
                                }
                                match sender.send(frame) {
                                    Ok(_) => {}
                                    Err(e) => {
                                        error!("Failed to send packet: {}", e);
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
        let remote_desc = RTCSessionDescription::offer(offer)?;
        let codec = match prefer_audio_codec(&remote_desc.unmarshal()?) {
            Some(codec) => codec,
            None => {
                return Err(anyhow::anyhow!("No codec found"));
            }
        };

        let track = Self::create_audio_track(codec, Some(self.track_id.clone()));
        peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        self.local_track = Some(track.clone());
        self.track_config.codec = codec;

        info!(
            "set remote description codec:{}\noffer:\n{}",
            codec.mime_type(),
            remote_desc.sdp
        );
        peer_connection.set_remote_description(remote_desc).await?;

        let answer = peer_connection.create_answer(None).await?;
        let mut gather_complete = peer_connection.gathering_complete_promise().await;
        peer_connection.set_local_description(answer).await?;
        select! {
            _ = gather_complete.recv() => {
                info!("ICE candidate received");
            }
            _ = sleep(timeout.unwrap_or(HANDSHAKE_TIMEOUT)) => {
                warn!("wait candidate timeout");
            }
        }

        let answer = peer_connection
            .local_description()
            .await
            .ok_or(anyhow::anyhow!("Failed to get local description"))?;

        info!("Final WebRTC answer from PeerConnection: {}", answer.sdp);
        Ok(answer)
    }
}

#[async_trait]
impl Track for WebrtcTrack {
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.track_config
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }

    async fn handshake(&mut self, offer: String, timeout: Option<Duration>) -> Result<String> {
        self.setup_webrtc_track(offer, timeout)
            .await
            .map(|answer| answer.sdp)
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
        let start_time = crate::get_timestamp();
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let _ = event_sender_clone.send(SessionEvent::TrackEnd {
                track_id,
                timestamp: crate::get_timestamp(),
                duration: crate::get_timestamp() - start_time,
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
        let payload = self.encoder.encode(payload_type, packet.clone());

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
                error!("failed to send sample: {}", e);
                return Err(anyhow::anyhow!("Failed to send sample: {}", e));
            }
        }
        Ok(())
    }
}
