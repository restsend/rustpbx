use super::TrackPacketReceiver;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        codecs::{self, resample::resample_mono, CodecType},
        jitter::JitterBuffer,
        negotiate::prefer_audio_codec,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    AudioFrame, Samples,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use std::{fs::File, io::Write, sync::Arc, time::SystemTime};
use tokio::{select, sync::Mutex, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    time::sleep,
};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
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
        rtp_sender::RTCRtpSender,
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
    stream_id: String,
    config: TrackConfig,
    processor_chain: ProcessorChain,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
    local_track: Option<Arc<TrackLocalStaticSample>>,
}

impl WebrtcTrack {
    pub fn create_audio_track(
        codec: CodecType,
        stream_id: Option<String>,
    ) -> Arc<TrackLocalStaticSample> {
        let id = stream_id.unwrap_or("rustpbx-track".to_string());
        Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: codec.mime_type().to_string(),
                clock_rate: codec.clock_rate(),
                channels: 1,
                ..Default::default()
            },
            "audio".to_string(),
            id,
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
        let servers = vec![RTCIceServer {
            ..Default::default()
        }];
        servers
    }

    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(16000);
        Self {
            track_id: id,
            stream_id: "rustpbx-stream".to_string(),
            config: config.clone(),
            processor_chain: ProcessorChain::new(),
            jitter_buffer: Arc::new(Mutex::new(JitterBuffer::new())),
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            local_track: None,
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();
        self.jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new()));
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);
        self.jitter_buffer = Arc::new(Mutex::new(JitterBuffer::new()));
        self
    }
    pub fn with_stream_id(mut self, stream_id: String) -> Self {
        self.stream_id = stream_id;
        self
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

        let (candidate_tx, candidate_rx) = oneshot::channel();
        let candidate_tx = Arc::new(Mutex::new(Some(candidate_tx)));

        let peer_connection = api.new_peer_connection(config).await?;
        peer_connection.on_ice_candidate(Box::new(
            move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
                info!("ICE candidate received: {:?}", candidate);
                let candidate_tx = candidate_tx.clone();
                Box::pin(async move {
                    if candidate.is_none() {
                        if let Some(tx) = candidate_tx.lock().await.take() {
                            tx.send(()).ok();
                        }
                    }
                })
            },
        ));
        peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                info!("Peer connection state changed: {}", s);
                let cancel_token = cancel_token.clone();
                Box::pin(async move {
                    match s {
                        RTCPeerConnectionState::Connected => {}
                        RTCPeerConnectionState::Closed
                        | RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Disconnected => {
                            cancel_token.cancel();
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
                    "Track received: {} track_samplerate:{}",
                    track.codec().capability.mime_type,
                    track_samplerate,
                );

                Box::pin(async move {
                    while let Ok((packet, _)) = track.read_rtp().await {
                        let packet_sender = packet_sender_clone.lock().await;
                        if let Some(sender) = packet_sender.as_ref() {
                            let frame = AudioFrame {
                                track_id: track_id_clone.clone(),
                                samples: crate::Samples::RTP(
                                    packet.header.payload_type,
                                    packet.payload.to_vec(),
                                ),
                                timestamp: packet.header.timestamp as u64,
                                sample_rate: track_samplerate,
                                ..Default::default()
                            };
                            if let Err(e) = processor_chain.process_frame(&frame) {
                                error!("Failed to process frame: {}", e);
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

        info!(
            "set remote description codec:{} {}",
            codec.mime_type(),
            remote_desc.sdp
        );
        peer_connection.set_remote_description(remote_desc).await?;

        let track = Self::create_audio_track(codec, Some(self.stream_id.clone()));
        peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        self.local_track = Some(track.clone());
        self.config.codec = codec;

        let answer = peer_connection.create_answer(None).await?;
        peer_connection
            .set_local_description(answer.clone())
            .await?;

        select! {
            _ = candidate_rx => {
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
        info!("Peer connection established answer: {}", answer.sdp);
        Ok(answer)
    }

    async fn start_jitter_processing(&self, token: CancellationToken) -> Result<()> {
        let jitter_buffer = self.jitter_buffer.clone();
        let sample_rate = self.config.sample_rate;
        let ptime = self.config.ptime;
        let codec_type = self.config.codec;
        let mut encoder = codecs::create_encoder(codec_type);
        let rtp_sender = self
            .local_track
            .clone()
            .ok_or(anyhow::anyhow!("RTP sender not found"))?;
        tokio::spawn(async move {
            let mut interval = IntervalStream::new(tokio::time::interval(ptime));
            let ptime_ms = ptime.as_millis() as u32;
            let mut packet_timestamp = 0;

            info!(
                "webrtctrack: Starting jitter processing ptime_ms:{} codec_type:{:?} sample_rate:{}",
                ptime_ms, codec_type, sample_rate
            );
            loop {
                select! {
                    _ = token.cancelled() => {
                        debug!("Jitter processing cancelled");
                        break;
                    }
                    Some(_) = interval.next() => {
                        let frame = jitter_buffer.lock().await.pop();
                        match frame {
                            Some(frame) => {
                                let payload = match frame.samples {
                                    Samples::PCM(mut samples) => {
                                        if frame.sample_rate != sample_rate {
                                            samples = resample_mono(&samples, frame.sample_rate, sample_rate);
                                        }
                                        let payload = encoder.encode(&samples);
                                        packet_timestamp += payload.len() as u32;
                                        payload
                                    }
                                    Samples::RTP(_, payload) => {
                                        payload
                                    }
                                    Samples::Empty => {
                                        continue;
                                    }
                                };
                                let sample = webrtc::media::Sample {
                                    data: payload.into(),
                                    duration: Duration::from_millis(ptime_ms as u64),
                                    timestamp: SystemTime::now(),
                                    packet_timestamp,
                                    ..Default::default()
                                };

                                match rtp_sender.write_sample(&sample).await {
                                    Ok(_) => {}
                                    Err(e) => {
                                        error!("webrtctrack: Failed to send sample: {}", e);
                                        break;
                                    }
                                }
                            }
                            None => {
                                 continue;
                            }
                        };
                    }
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Track for WebrtcTrack {
    fn id(&self) -> &TrackId {
        &self.track_id
    }

    fn insert_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.insert_processor(processor);
    }

    fn append_processor(&mut self, processor: Box<dyn Processor>) {
        self.processor_chain.append_processor(processor);
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Store the packet sender
        *self.packet_sender.lock().await = Some(packet_sender.clone());

        let _ = event_sender.send(SessionEvent::TrackStart {
            track_id: self.track_id.clone(),
            timestamp: crate::get_timestamp(),
        });

        // Start jitter buffer processing
        self.start_jitter_processing(token.clone()).await?;

        // Clone token for the task
        let token_clone = token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.track_id.clone();

        // Start a task to watch for cancellation
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let _ = event_sender_clone.send(SessionEvent::TrackEnd {
                track_id,
                timestamp: crate::get_timestamp(),
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
        let frame = packet.clone();
        if let Err(e) = self.processor_chain.process_frame(&frame) {
            error!("Error processing frame: {}", e);
        }
        let mut jitter = self.jitter_buffer.lock().await;
        jitter.push(frame);

        Ok(())
    }
}
