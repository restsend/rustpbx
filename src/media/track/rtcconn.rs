use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};
use webrtc::api::{media_engine::MediaEngine, APIBuilder};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

use crate::media::processor::Processor;
use crate::media::stream::EventSender;
use crate::media::track::{
    Track, TrackConfig, TrackId, TrackPacket, TrackPacketSender, TrackPayload,
};

#[derive(Debug, Clone)]
pub enum RtcConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

impl From<RTCPeerConnectionState> for RtcConnectionState {
    fn from(state: RTCPeerConnectionState) -> Self {
        match state {
            RTCPeerConnectionState::New => RtcConnectionState::New,
            RTCPeerConnectionState::Connecting => RtcConnectionState::Connecting,
            RTCPeerConnectionState::Connected => RtcConnectionState::Connected,
            RTCPeerConnectionState::Disconnected => RtcConnectionState::Disconnected,
            RTCPeerConnectionState::Failed => RtcConnectionState::Failed,
            RTCPeerConnectionState::Closed => RtcConnectionState::Closed,
            RTCPeerConnectionState::Unspecified => RtcConnectionState::New,
        }
    }
}

/// Manages a WebRTC connection that can have local and remote tracks
pub struct RtcConnection {
    id: String,
    ice_servers: Vec<String>,
    peer_connection: Arc<Mutex<Option<webrtc::peer_connection::RTCPeerConnection>>>,
    local_tracks: Arc<Mutex<HashMap<TrackId, Arc<TrackLocalStaticRTP>>>>,
    remote_tracks: Arc<Mutex<HashMap<TrackId, Box<dyn Track>>>>,
    track_adaptors: Arc<Mutex<HashMap<TrackId, RemoteTrackAdaptor>>>,
    state: Arc<Mutex<RtcConnectionState>>,
    processors: Vec<Box<dyn Processor>>,
    cancel_token: CancellationToken,
}

impl RtcConnection {
    pub fn new(id: String) -> Self {
        Self {
            id,
            ice_servers: vec![],
            peer_connection: Arc::new(Mutex::new(None)),
            local_tracks: Arc::new(Mutex::new(HashMap::new())),
            remote_tracks: Arc::new(Mutex::new(HashMap::new())),
            track_adaptors: Arc::new(Mutex::new(HashMap::new())),
            state: Arc::new(Mutex::new(RtcConnectionState::New)),
            processors: Vec::new(),
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn with_ice_servers(mut self, ice_servers: Vec<String>) -> Self {
        self.ice_servers = ice_servers;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    /// Initialize WebRTC peer connection with media engine
    pub async fn initialize(&self) -> Result<()> {
        let mut m = MediaEngine::default();

        // Register default codecs
        m.register_default_codecs()?;

        // Create a registry to add our interceptors
        let registry = Registry::new();

        // Use the registry to construct our API
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        // Create ICE servers configuration
        let mut ice_servers = vec![];
        for server in &self.ice_servers {
            ice_servers.push(RTCIceServer {
                urls: vec![server.clone()],
                ..Default::default()
            });
        }

        // Create peer connection configuration
        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        // Create a new RTCPeerConnection
        let peer_connection = api.new_peer_connection(config).await?;

        // Set callback for when peer connection state changes
        let pc_state = self.state.clone();
        let pc_id = self.id.clone();
        peer_connection.on_peer_connection_state_change(Box::new(move |s| {
            let state: RtcConnectionState = s.into();
            info!("[{}] Peer connection state changed: {:?}", pc_id, state);

            let mut current_state = pc_state.lock().unwrap();
            *current_state = state;

            Box::pin(async {})
        }));

        // Set up callback for handling remote tracks
        let track_adaptors = self.track_adaptors.clone();
        let remote_tracks = self.remote_tracks.clone();
        let pc_id = self.id.clone();

        peer_connection.on_track(Box::new(move |track, _, _| {
            let track_id = track.id().to_string();
            let track_adaptors_clone = track_adaptors.clone();
            let remote_tracks_clone = remote_tracks.clone();
            let pc_id = pc_id.clone();

            debug!("[{}] Got remote track: {}", pc_id, track_id);

            Box::pin(async move {
                // Create an adaptor for this remote track
                let adaptor = RemoteTrackAdaptor::new(track_id.clone(), track.clone());

                // Store the adaptor
                {
                    let mut adaptors = track_adaptors_clone.lock().unwrap();
                    adaptors.insert(track_id.clone(), adaptor.clone());
                }

                // Also create a full Track implementation from this adaptor
                let track_impl = RtcTrack::from_remote_track(track_id.clone(), adaptor);

                // Store the Track implementation
                {
                    let mut tracks = remote_tracks_clone.lock().unwrap();
                    tracks.insert(track_id.clone(), Box::new(track_impl));
                }
            })
        }));

        // Store the peer connection
        {
            let mut pc = self.peer_connection.lock().unwrap();
            *pc = Some(peer_connection);
        }

        Ok(())
    }

    /// Create an offer to start a WebRTC session
    pub async fn create_offer(&self) -> Result<RTCSessionDescription> {
        let pc_guard = self.peer_connection.lock().unwrap();
        let pc = match &*pc_guard {
            Some(pc) => pc.clone(),
            None => return Err(anyhow!("Peer connection not initialized")),
        };

        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;

        Ok(offer)
    }

    /// Set the remote description to establish a WebRTC session
    pub async fn set_remote_description(&self, sdp: RTCSessionDescription) -> Result<()> {
        let pc_guard = self.peer_connection.lock().unwrap();
        let pc = match &*pc_guard {
            Some(pc) => pc.clone(),
            None => return Err(anyhow!("Peer connection not initialized")),
        };

        pc.set_remote_description(sdp).await?;
        Ok(())
    }

    /// Create an answer to an offer
    pub async fn create_answer(&self) -> Result<RTCSessionDescription> {
        let pc_guard = self.peer_connection.lock().unwrap();
        let pc = match &*pc_guard {
            Some(pc) => pc.clone(),
            None => return Err(anyhow!("Peer connection not initialized")),
        };

        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer.clone()).await?;

        Ok(answer)
    }

    /// Add a local track to the connection
    pub async fn add_local_track(
        &self,
        track_id: TrackId,
        codec: &str,
    ) -> Result<Arc<TrackLocalStaticRTP>> {
        let pc_guard = self.peer_connection.lock().unwrap();
        let pc = match &*pc_guard {
            Some(pc) => pc.clone(),
            None => return Err(anyhow!("Peer connection not initialized")),
        };

        // Create a new RTP track
        let track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: codec.to_string(),
                ..Default::default()
            },
            format!("track-{}", track_id),
            track_id.clone(),
        ));

        // Add the track to the peer connection
        pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Save track in local tracks map
        {
            let mut tracks = self.local_tracks.lock().unwrap();
            tracks.insert(track_id, Arc::clone(&track));
        }

        Ok(track)
    }

    /// Write RTP packet to a local track
    pub async fn write_rtp_to_track(&self, track_id: &TrackId, payload: &[u8]) -> Result<()> {
        let track = {
            let tracks = self.local_tracks.lock().unwrap();
            match tracks.get(track_id) {
                Some(track) => track.clone(),
                None => return Err(anyhow!("Track not found: {}", track_id)),
            }
        };

        track.write(&payload).await?;
        Ok(())
    }

    /// Get a remote track by ID, returns a cloned Track implementation
    pub fn get_remote_track(&self, track_id: &TrackId) -> Option<Box<dyn Track>> {
        let tracks = self.remote_tracks.lock().unwrap();
        tracks.get(track_id).map(|t| {
            // Create a clone of the track
            let track_clone: Box<dyn Track> = Box::new(RtcTrack::from_remote_track(
                track_id.clone(),
                self.track_adaptors
                    .lock()
                    .unwrap()
                    .get(track_id)
                    .unwrap()
                    .clone(),
            ));
            track_clone
        })
    }

    /// Get list of all remote track IDs
    pub fn get_remote_track_ids(&self) -> Vec<TrackId> {
        let tracks = self.remote_tracks.lock().unwrap();
        tracks.keys().cloned().collect()
    }

    /// Close the RTC connection
    pub async fn close(&self) -> Result<()> {
        let pc = {
            let mut pc_guard = self.peer_connection.lock().unwrap();
            pc_guard.take()
        };

        if let Some(pc) = pc {
            pc.close().await?;
        }

        // Cancel all tracks
        self.cancel_token.cancel();

        Ok(())
    }
}

/// Adaptor for remote tracks to bridge between webrtc-rs and our Track trait
#[derive(Debug, Clone)]
pub struct RemoteTrackAdaptor {
    track_id: TrackId,
    track: Arc<TrackRemote>,
    sender: Arc<Mutex<Option<TrackPacketSender>>>,
}

impl RemoteTrackAdaptor {
    pub fn new(track_id: TrackId, track: Arc<TrackRemote>) -> Self {
        Self {
            track_id,
            track,
            sender: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_sender(&self, sender: TrackPacketSender) {
        let mut s = self.sender.lock().unwrap();
        *s = Some(sender);
    }

    pub async fn start_reading(&self) -> Result<()> {
        let track = self.track.clone();
        let track_id = self.track_id.clone();
        let sender = self.sender.clone();

        // Create a task to read from the track
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];

            loop {
                // Read RTP packet from the track
                // TrackRemote.read returns a tuple (rtp::packet::Packet, Attributes)
                let result = track.read(&mut buf).await;
                match result {
                    Ok((packet, _)) => {
                        // Use payload_type and sequence_number from the packet
                        let payload_type = packet.header.payload_type;

                        // Get timestamp from packet
                        let timestamp = packet.header.timestamp as u64;

                        // Get the payload data and convert it to Vec<u8>
                        let payload_data = packet.payload.to_vec();

                        // Create a packet with RTP payload
                        let track_packet = TrackPacket {
                            track_id: track_id.clone(),
                            timestamp,
                            payload: TrackPayload::RTP(payload_type, payload_data),
                        };

                        // Send packet if we have a sender
                        let sender_opt = {
                            let s = sender.lock().unwrap();
                            s.clone()
                        };

                        if let Some(s) = sender_opt {
                            if let Err(e) = s.send(track_packet) {
                                error!("Error sending packet: {}", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading from track: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }
}

/// Track implementation that wraps a webrtc-rs remote track
pub struct RtcTrack {
    id: TrackId,
    adaptor: RemoteTrackAdaptor,
    config: TrackConfig,
    processors: Vec<Box<dyn Processor>>,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<TrackPacket>>>>,
    cancel_token: CancellationToken,
}

impl RtcTrack {
    pub fn from_remote_track(id: TrackId, adaptor: RemoteTrackAdaptor) -> Self {
        Self {
            id,
            adaptor,
            config: TrackConfig::default(),
            processors: Vec::new(),
            receiver: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }
}

#[async_trait]
impl Track for RtcTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
    }

    fn processors(&self) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .map(|p| p.as_ref() as &dyn Processor)
            .collect()
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Set the packet sender in the adaptor
        self.adaptor.set_sender(packet_sender.clone());

        // Start reading from the track
        self.adaptor.start_reading().await?;

        // Signal that the track is ready
        let _ = event_sender.send(crate::media::stream::MediaStreamEvent::TrackStart(
            self.id.clone(),
        ));

        // Clone token for the task
        let token_clone = token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.id.clone();

        // Start a task to watch for cancellation
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let _ = event_sender_clone
                .send(crate::media::stream::MediaStreamEvent::TrackStop(track_id));
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Cancel all processing
        self.cancel_token.cancel();

        Ok(())
    }

    async fn send_packet(&self, packet: &TrackPacket) -> Result<()> {
        // Process packet with processors
        // Note: This method is not typically called for remote tracks
        // but we'll implement it for completeness

        Ok(())
    }

    async fn recv_packet(&self) -> Option<TrackPacket> {
        let mut receiver_opt: Option<mpsc::UnboundedReceiver<TrackPacket>> = None;

        // Take ownership of the receiver
        {
            let mut receiver_guard = self.receiver.lock().unwrap();
            if let Some(receiver) = receiver_guard.take() {
                receiver_opt = Some(receiver);
            }
        }

        // Receive a packet
        if let Some(mut receiver) = receiver_opt {
            let packet_opt = receiver.recv().await;

            // Put the receiver back
            let mut receiver_guard = self.receiver.lock().unwrap();
            *receiver_guard = Some(receiver);

            return packet_opt;
        }

        None
    }
}

/// Adapter that connects a regular Track to a WebRTC track
pub struct LocalTrackAdapter {
    rtc_connection: Arc<RtcConnection>,
    track_id: TrackId,
    webrtc_track: Arc<TrackLocalStaticRTP>,
    cancel_token: CancellationToken,
}

impl LocalTrackAdapter {
    pub async fn new(
        rtc_connection: Arc<RtcConnection>,
        track_id: TrackId,
        codec: &str,
    ) -> Result<Self> {
        // Add a local track to the connection
        let webrtc_track = rtc_connection
            .add_local_track(track_id.clone(), codec)
            .await?;

        Ok(Self {
            rtc_connection,
            track_id,
            webrtc_track,
            cancel_token: CancellationToken::new(),
        })
    }

    /// Start forwarding packets from a Track to the WebRTC track
    pub async fn start_forwarding(
        &self,
        mut track_receiver: mpsc::UnboundedReceiver<TrackPacket>,
    ) -> Result<()> {
        let rtc_connection = self.rtc_connection.clone();
        let track_id = self.track_id.clone();
        let token = self.cancel_token.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        break;
                    }
                    packet_opt = track_receiver.recv() => {
                        if let Some(packet) = packet_opt {
                            if let TrackPayload::RTP(_, rtp_payload) = &packet.payload {
                                // Write RTP packet to WebRTC track
                                if let Err(e) = rtc_connection.write_rtp_to_track(&track_id, rtp_payload).await {
                                    error!("Error writing to WebRTC track: {}", e);
                                }
                            }
                        } else {
                            // Channel closed
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }

    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

// A factory to create RtcConnections
pub struct RtcConnectionFactory {
    ice_servers: Vec<String>,
}

impl RtcConnectionFactory {
    pub fn new(ice_servers: Vec<String>) -> Self {
        Self { ice_servers }
    }

    /// Create a new RtcConnection
    pub fn create_connection(&self, id: String) -> RtcConnection {
        RtcConnection::new(id).with_ice_servers(self.ice_servers.clone())
    }
}
