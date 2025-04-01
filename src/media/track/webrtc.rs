use super::TrackPacketReceiver;
use crate::{
    event::{EventSender, SessionEvent},
    media::{
        codecs::{convert_s16_to_u8, g722::G722Decoder, Decoder},
        jitter::JitterBuffer,
        negotiate::prefer_audio_codec,
        processor::{Processor, ProcessorChain},
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    AudioFrame,
};
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use std::{
    fs::File,
    io::Write,
    sync::{Arc, Mutex},
};
use tokio::{select, time::Duration};
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
    receiver: Mutex<Option<TrackPacketReceiver>>,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
    webrtc_track: Option<WebrtcTrackConfig>,
    rtp_sender: Option<Arc<RTCRtpSender>>,
}

impl WebrtcTrack {
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
            jitter_buffer: Arc::new(Mutex::new(JitterBuffer::new(config.sample_rate))),
            receiver: Mutex::new(None),
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            webrtc_track: None,
            rtp_sender: None,
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(config.sample_rate);
        }

        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_webrtc_track(mut self, webrtc_track: WebrtcTrackConfig) -> Self {
        self.webrtc_track = Some(webrtc_track);
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(sample_rate);
        }

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
                        if let Some(tx) = candidate_tx.lock().unwrap().take() {
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
                    "Track received: {:?} track_samplerate:{}",
                    track, track_samplerate,
                );
                Box::pin(async move {
                    while let Ok((packet, _)) = track.read_rtp().await {
                        let packet_sender = packet_sender_clone.lock().unwrap();
                        if let Some(sender) = packet_sender.as_ref() {
                            let frame = AudioFrame {
                                track_id: track_id_clone.clone(),
                                samples: crate::Samples::RTP(
                                    packet.header.payload_type,
                                    packet.payload.to_vec(),
                                ),
                                timestamp: packet.header.timestamp,
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

        info!("set remote description {}", remote_desc.sdp);
        peer_connection.set_remote_description(remote_desc).await?;

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: codec.mime_type().to_string(),
                clock_rate: 8000,
                channels: 1,
                ..Default::default()
            },
            "audio".to_string(),
            self.stream_id.clone(),
        ));

        let rtp_sender = peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        self.rtp_sender = Some(rtp_sender);
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

        self.config = self.config.clone().with_sample_rate(codec.samplerate());

        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(codec.samplerate());
        }

        self.webrtc_track = Some(WebrtcTrackConfig {
            track: Arc::clone(&track),
            payload_type: codec.payload_type(),
        });
        info!("Peer connection established answer: {}", answer.sdp);
        Ok(answer)
    }

    async fn start_jitter_processing(&self, token: CancellationToken) -> Result<()> {
        let jitter_buffer = self.jitter_buffer.clone();
        let packet_sender = self.packet_sender.clone();
        let sample_rate = self.config.sample_rate;
        let track_id = self.track_id.clone();

        tokio::spawn(async move {
            let mut interval =
                IntervalStream::new(tokio::time::interval(Duration::from_millis(20)));
            loop {
                select! {
                    _ = token.cancelled() => {
                        debug!("Jitter processing cancelled");
                        break;
                    }
                    Some(_) = interval.next() => {
                        // Process any packets in the jitter buffer
                        let mut buffer = jitter_buffer.lock().unwrap();
                        // Pull frames from jitter buffer
                        let frames = buffer.pull_frames(20, sample_rate);
                        drop(buffer); // Release the lock before processing

                        // Process and forward each frame
                        for frame in frames {
                            // Forward the processed frame
                            if let Some(tx) = packet_sender.lock().unwrap().as_ref() {
                                tx.send(frame).ok();
                            }
                        }
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
        *self.packet_sender.lock().unwrap() = Some(packet_sender.clone());

        // Create a channel for receiving packets
        let (_receiver_sender, receiver) = mpsc::unbounded_channel();

        // Store the receiver in self
        *self.receiver.lock().unwrap() = Some(receiver);

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
        if let Some(_webrtc_track) = &self.webrtc_track {
            // Clone the packet
            let frame = packet.clone();

            // Process the frame with processor chain
            if let Err(e) = self.processor_chain.process_frame(&frame) {
                tracing::error!("Error processing frame: {}", e);
            }

            // Add the frame to the jitter buffer which will be processed by our jitter_processing task
            {
                let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
                jitter_buffer.push(frame);
            }

            // Just return success - the actual processor chain will run in the jitter buffer task
            return Ok(());
        }
        Ok(())
    }
}
