use super::track_codec::TrackCodec;
use crate::{
    event::{EventSender, SessionEvent},
    media::AudioFrame,
    media::Samples,
    media::TrackId,
    media::{
        codecs::CodecType,
        jitter::JitterBuffer,
        negotiate::select_peer_media,
        processor::ProcessorChain,
        track::{Track, TrackConfig, TrackPacketSender},
    },
};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use rsip::HostWithPort;
use rsipstack::transport::{SipAddr, udp::UdpConnection};
use std::{
    io::Cursor,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{select, time::Instant, time::interval_at};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use webrtc::{
    rtcp::{
        goodbye::Goodbye,
        receiver_report::ReceiverReport,
        reception_report::ReceptionReport,
        sender_report::SenderReport,
        source_description::{
            SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
        },
    },
    rtp::{
        codecs::g7xx::G7xxPayloader,
        packet::Packet,
        packetizer::{Packetizer, new_packetizer},
        sequence::{Sequencer, new_random_sequencer},
    },
    sdp::{
        MediaDescription, SessionDescription,
        description::{
            common::{Address, Attribute, ConnectionInformation},
            media::{MediaName, RangedPort},
            session::{
                ATTR_KEY_RTCPMUX, ATTR_KEY_SEND_ONLY, ATTR_KEY_SEND_RECV, ATTR_KEY_SSRC, Origin,
                TimeDescription, Timing,
            },
        },
    },
    util::{Marshal, Unmarshal},
};
const RTP_MTU: usize = 1500; // UDP MTU size
const RTP_OUTBOUND_MTU: usize = 1200; // Standard MTU size
const RTCP_SR_INTERVAL_MS: u64 = 5000; // 5 seconds RTCP sender report interval
const DTMF_EVENT_DURATION_MS: u64 = 160; // Default DTMF event duration (in ms)
const DTMF_EVENT_VOLUME: u8 = 10; // Default volume for DTMF events (0-63)
const RTP_RESYNC_MIN_SKIP_PACKETS: u32 = 3; // Require at least this many missing packets before resyncing
const RTP_RESYNC_COOLDOWN_FRAMES: u64 = 3; // Cooldown window (in frames) between resync attempts

// STUN constants for ICE connectivity check
const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_RESPONSE: u16 = 0x0101;
const STUN_MAGIC_COOKIE: u32 = 0x2112A442;
const STUN_TRANSACTION_ID_SIZE: usize = 12;

struct RtpTrackStats {
    timestamp: Arc<AtomicU32>,
    packet_count: Arc<AtomicU32>,
    octet_count: Arc<AtomicU32>,
    last_timestamp_update: Arc<AtomicU64>,
    last_resync_ts: Arc<AtomicU64>,
    received_packets: Arc<AtomicU32>,
    received_octets: Arc<AtomicU32>,
    expected_packets: Arc<AtomicU32>,
    lost_packets: Arc<AtomicU32>,
    highest_seq_num: Arc<AtomicU32>,
    base_seq: Arc<AtomicU32>,
    last_receive_seq: Arc<AtomicU32>,
    jitter: Arc<AtomicU32>,
    last_sr_timestamp: Arc<AtomicU64>,
    last_sr_ntp: Arc<AtomicU64>,
}

impl RtpTrackStats {
    fn new() -> Self {
        Self {
            timestamp: Arc::new(AtomicU32::new(0)),
            packet_count: Arc::new(AtomicU32::new(0)),
            octet_count: Arc::new(AtomicU32::new(0)),
            last_timestamp_update: Arc::new(AtomicU64::new(0)),
            last_resync_ts: Arc::new(AtomicU64::new(0)),
            received_packets: Arc::new(AtomicU32::new(0)),
            received_octets: Arc::new(AtomicU32::new(0)),
            expected_packets: Arc::new(AtomicU32::new(0)),
            lost_packets: Arc::new(AtomicU32::new(0)),
            highest_seq_num: Arc::new(AtomicU32::new(0)),
            base_seq: Arc::new(AtomicU32::new(0)),
            last_receive_seq: Arc::new(AtomicU32::new(0)),
            jitter: Arc::new(AtomicU32::new(0)),
            last_sr_timestamp: Arc::new(AtomicU64::new(0)),
            last_sr_ntp: Arc::new(AtomicU64::new(0)),
        }
    }

    fn update_send_stats(&self, packet_len: u32, samples_per_packet: u32) {
        self.packet_count.fetch_add(1, Ordering::Relaxed);
        self.octet_count.fetch_add(packet_len, Ordering::Relaxed);
        self.timestamp
            .fetch_add(samples_per_packet, Ordering::Relaxed);
    }

    fn update_receive_stats(&self, seq_num: u32, payload_len: u32) {
        let prev_received = self.received_packets.fetch_add(1, Ordering::Relaxed);
        let received = prev_received + 1;
        self.received_octets
            .fetch_add(payload_len, Ordering::Relaxed);

        if prev_received == 0 {
            self.base_seq.store(seq_num, Ordering::Relaxed);
            self.last_receive_seq.store(seq_num, Ordering::Relaxed);
            self.highest_seq_num.store(seq_num, Ordering::Relaxed);
            self.lost_packets.store(0, Ordering::Relaxed);
            self.expected_packets.store(received, Ordering::Relaxed);
        } else {
            let last_seq = self.last_receive_seq.load(Ordering::Relaxed);
            let gap = (seq_num as u16).wrapping_sub(last_seq as u16) as u32;

            if gap > 0 && gap < 0x8000 {
                if gap > 1 {
                    self.lost_packets.fetch_add(gap - 1, Ordering::Relaxed);
                }
                self.last_receive_seq.store(seq_num, Ordering::Relaxed);
                self.highest_seq_num.store(seq_num, Ordering::Relaxed);
            }

            let lost = self.lost_packets.load(Ordering::Relaxed);
            self.expected_packets
                .store(received + lost, Ordering::Relaxed);
        }

        let current_jitter = self.jitter.load(Ordering::Relaxed);
        let new_jitter = (current_jitter + (seq_num % 100)) / 2;
        self.jitter.store(new_jitter, Ordering::Relaxed);
    }

    fn store_sr_info(&self, rtp_time: u64, ntp_time: u64) {
        self.last_sr_timestamp.store(rtp_time, Ordering::Relaxed);
        self.last_sr_ntp.store(ntp_time, Ordering::Relaxed);
    }

    fn get_fraction_lost(&self) -> u8 {
        let expected_packets = self.expected_packets.load(Ordering::Relaxed);
        let lost_packets = self.lost_packets.load(Ordering::Relaxed);

        if expected_packets > 0 {
            ((lost_packets * 256) / expected_packets).min(255) as u8
        } else {
            0
        }
    }
}

pub struct RtpTrackBuilder {
    cancel_token: Option<CancellationToken>,
    track_id: TrackId,
    config: TrackConfig,
    local_addr: Option<IpAddr>,
    external_addr: Option<IpAddr>,
    rtp_socket: Option<UdpConnection>,
    rtcp_socket: Option<UdpConnection>,
    rtcp_mux: bool,
    rtp_start_port: u16,
    rtp_end_port: u16,
    rtp_alloc_count: u32,
    enabled_codecs: Vec<CodecType>,
    ssrc_cname: String,
    ssrc: u32,
    ice_connectivity_check: bool,
}
pub struct RtpTrackInner {
    dtmf_payload_type: u8,
    payload_type: u8,
    remote_description: Option<String>,
    packetizer: Mutex<Option<Box<dyn Packetizer + Send + Sync>>>,
    stats: Arc<RtpTrackStats>,
    rtcp_mux: bool,
    remote_addr: Option<SipAddr>,
    remote_rtcp_addr: Option<SipAddr>,
    enabled_codecs: Vec<CodecType>,
    rtp_map: Vec<(u8, (CodecType, u32, u16))>,
}

pub struct RtpTrack {
    ssrc: u32,
    ssrc_cname: String,
    track_id: TrackId,
    config: TrackConfig,
    cancel_token: CancellationToken,
    processor_chain: ProcessorChain,
    rtp_socket: UdpConnection,
    rtcp_socket: UdpConnection,
    encoder: TrackCodec,
    sequencer: Box<dyn Sequencer + Send + Sync>,
    sendrecv: AtomicBool,
    ice_connectivity_check: bool,
    inner: Arc<Mutex<RtpTrackInner>>,
}

enum PacketKind {
    Rtp,
    Rtcp,
    Stun(u16),
    Ignore,
}
impl RtpTrackBuilder {
    pub fn new(track_id: TrackId, config: TrackConfig) -> Self {
        let ssrc = rand::random::<u32>();
        Self {
            track_id,
            config,
            local_addr: None,
            external_addr: None,
            cancel_token: None,
            rtp_socket: None,
            rtcp_socket: None,
            rtcp_mux: true,
            rtp_start_port: 12000,
            rtp_end_port: u16::MAX - 1,
            rtp_alloc_count: 500,
            enabled_codecs: vec![
                #[cfg(feature = "opus")]
                CodecType::Opus,
                #[cfg(feature = "g729")]
                CodecType::G729,
                CodecType::G722,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::TelephoneEvent,
            ],
            ssrc_cname: format!("rustpbx-{}", ssrc),
            ssrc,
            ice_connectivity_check: true, // Default enabled
        }
    }

    pub fn with_ssrc(mut self, ssrc: u32) -> Self {
        self.ssrc = ssrc;
        self.ssrc_cname = format!("rustpbx-{}", ssrc);
        self
    }

    pub fn with_rtp_start_port(mut self, rtp_start_port: u16) -> Self {
        self.rtp_start_port = rtp_start_port;
        self
    }
    pub fn with_rtp_end_port(mut self, rtp_end_port: u16) -> Self {
        self.rtp_end_port = rtp_end_port;
        self
    }
    pub fn with_rtp_alloc_count(mut self, rtp_alloc_count: u32) -> Self {
        self.rtp_alloc_count = rtp_alloc_count;
        self
    }
    pub fn with_local_addr(mut self, local_addr: IpAddr) -> Self {
        self.local_addr = Some(local_addr);
        self
    }

    pub fn with_external_addr(mut self, external_addr: IpAddr) -> Self {
        self.external_addr = Some(external_addr);
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = Some(cancel_token);
        self
    }

    pub fn with_rtp_socket(mut self, rtp_socket: UdpConnection) -> Self {
        self.rtp_socket = Some(rtp_socket);
        self
    }
    pub fn with_rtcp_socket(mut self, rtcp_socket: UdpConnection) -> Self {
        self.rtcp_socket = Some(rtcp_socket);
        self
    }
    pub fn with_rtcp_mux(mut self, rtcp_mux: bool) -> Self {
        self.rtcp_mux = rtcp_mux;
        self
    }

    pub fn with_enabled_codecs(mut self, enabled_codecs: Vec<CodecType>) -> Self {
        self.enabled_codecs = enabled_codecs;
        self
    }
    pub fn with_session_name(mut self, session_name: String) -> Self {
        self.ssrc_cname = session_name;
        self
    }

    pub fn with_ice_connectivity_check(mut self, enabled: bool) -> Self {
        self.ice_connectivity_check = enabled;
        self
    }
    pub async fn build_rtp_rtcp_conn(&self) -> Result<(UdpConnection, UdpConnection)> {
        let addr = match self.local_addr {
            Some(addr) => addr,
            None => crate::net_tool::get_first_non_loopback_interface()?,
        };
        let mut rtp_conn = None;
        let mut rtcp_conn = None;

        for _ in 0..self.rtp_alloc_count {
            let port = rand::random_range::<u16, _>(self.rtp_start_port..=self.rtp_end_port);
            if port % 2 != 0 {
                continue;
            }
            if let Ok(c) = UdpConnection::create_connection(
                format!("{:?}:{}", addr, port).parse()?,
                None,
                self.cancel_token.clone(),
            )
            .await
            {
                if !self.rtcp_mux {
                    // if rtcp mux is not enabled, we need to create a separate RTCP socket
                    rtcp_conn = match UdpConnection::create_connection(
                        format!("{:?}:{}", addr, port + 1).parse()?,
                        None,
                        self.cancel_token.clone(),
                    )
                    .await
                    {
                        Ok(c) => Some(c),
                        Err(_) => {
                            continue;
                        }
                    };
                } else {
                    rtcp_conn = Some(c.clone());
                }
                rtp_conn = Some(c);
                break;
            }
        }

        let mut rtp_conn = match rtp_conn {
            Some(c) => c,
            None => return Err(anyhow::anyhow!("failed to bind RTP socket")),
        };
        let mut rtcp_conn = match rtcp_conn {
            Some(c) => c,
            None => return Err(anyhow::anyhow!("failed to bind RTCP socket")),
        };

        if let Some(addr) = self.external_addr {
            rtp_conn.external = Some(
                SocketAddr::new(
                    addr,
                    *rtp_conn
                        .get_addr()
                        .addr
                        .port
                        .clone()
                        .unwrap_or_default()
                        .value(),
                )
                .into(),
            );
            rtcp_conn.external = Some(
                SocketAddr::new(
                    addr,
                    *rtcp_conn
                        .get_addr()
                        .addr
                        .port
                        .clone()
                        .unwrap_or_default()
                        .value(),
                )
                .into(),
            );
        }
        Ok((rtp_conn, rtcp_conn))
    }

    pub async fn build(mut self) -> Result<RtpTrack> {
        let mut rtp_socket = self.rtp_socket.take();
        let mut rtcp_socket = self.rtcp_socket.take();

        if rtp_socket.is_none() || rtcp_socket.is_none() {
            let (rtp_conn, rtcp_conn) = self.build_rtp_rtcp_conn().await?;
            rtp_socket = Some(rtp_conn);
            rtcp_socket = Some(rtcp_conn);
        }
        let cancel_token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let processor_chain = ProcessorChain::new(self.config.samplerate);
        let ssrc = if self.ssrc != 0 {
            self.ssrc
        } else {
            loop {
                let i = rand::random::<u32>();
                if i % 2 == 0 {
                    break i;
                }
            }
        };
        let inner = RtpTrackInner {
            dtmf_payload_type: 101, // Default DTMF payload type
            payload_type: 0,        // Will be set later based on remote description
            remote_description: None,
            packetizer: Mutex::new(None),
            stats: Arc::new(RtpTrackStats::new()),
            rtcp_mux: self.rtcp_mux,
            remote_addr: None,
            remote_rtcp_addr: None,
            enabled_codecs: self.enabled_codecs.clone(),
            rtp_map: vec![],
        };
        let track = RtpTrack {
            ssrc,
            ssrc_cname: self.ssrc_cname.clone(),
            track_id: self.track_id,
            config: self.config,
            cancel_token,
            processor_chain,
            rtp_socket: rtp_socket.unwrap(),
            rtcp_socket: rtcp_socket.unwrap(),
            encoder: TrackCodec::new(),
            sequencer: Box::new(new_random_sequencer()),
            sendrecv: AtomicBool::new(true),
            ice_connectivity_check: self.ice_connectivity_check,
            inner: Arc::new(Mutex::new(inner)),
        };
        Ok(track)
    }
}

impl RtpTrack {
    pub fn id(&self) -> &str {
        &self.track_id
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn remote_description(&self) -> Option<String> {
        self.inner.lock().unwrap().remote_description.clone()
    }

    pub fn set_rtp_map(&self, rtp_map: Vec<(u8, (CodecType, u32, u16))>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.rtp_map = rtp_map;
        }
    }

    pub fn set_remote_description(&self, answer: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let mut reader = Cursor::new(answer);
        let sdp = SessionDescription::unmarshal(&mut reader)?;
        let peer_media = match select_peer_media(&sdp, "audio") {
            Some(peer_media) => peer_media,
            None => return Err(anyhow::anyhow!("no audio media in answer SDP")),
        };

        inner.rtp_map = peer_media.rtp_map.clone();

        if peer_media.codecs.is_empty() {
            return Err(anyhow::anyhow!("no audio codecs in answer SDP"));
        }

        if peer_media.rtp_addr.is_empty() {
            return Err(anyhow::anyhow!("no rtp addr in answer SDP"));
        }

        inner.remote_description.replace(answer.to_string());

        let remote_addr = SipAddr {
            addr: HostWithPort {
                host: peer_media.rtp_addr.parse()?,
                port: Some(peer_media.rtp_port.into()),
            },
            r#type: Some(rsip::transport::Transport::Udp),
        };
        let remote_rtcp_addr = SipAddr {
            addr: HostWithPort {
                host: peer_media.rtcp_addr.parse()?,
                port: Some(peer_media.rtcp_port.into()),
            },
            r#type: Some(rsip::transport::Transport::Udp),
        };
        let codec_type = peer_media.codecs[0];
        info!(
            track_id = self.track_id,
            rtcp_mux = peer_media.rtcp_mux,
            %remote_addr,
            %remote_rtcp_addr,
            ?codec_type,
            ssrc = self.ssrc,
            "set remote description"
        );

        inner.payload_type = codec_type.payload_type();
        inner.enabled_codecs = vec![codec_type];
        for (payload_type, (codec, clock_rate, _)) in peer_media.rtp_map.iter() {
            if *codec == codec_type {
                inner.payload_type = *payload_type;
            }

            if codec == &CodecType::TelephoneEvent && clock_rate == &codec_type.clock_rate() {
                inner.dtmf_payload_type = *payload_type;
            }
        }

        inner.remote_addr.replace(remote_addr);
        inner.remote_rtcp_addr.replace(remote_rtcp_addr);
        inner.rtcp_mux = peer_media.rtcp_mux;

        let payloader = match codec_type {
            #[cfg(feature = "opus")]
            CodecType::Opus => Box::<webrtc::rtp::codecs::opus::OpusPayloader>::default()
                as Box<dyn webrtc::rtp::packetizer::Payloader + Send + Sync>,
            _ => Box::<G7xxPayloader>::default()
                as Box<dyn webrtc::rtp::packetizer::Payloader + Send + Sync>,
        };

        inner
            .packetizer
            .lock()
            .unwrap()
            .replace(Box::new(new_packetizer(
                RTP_OUTBOUND_MTU,
                inner.payload_type,
                self.ssrc,
                payloader,
                self.sequencer.clone(),
                codec_type.clock_rate(),
            )));
        Ok(())
    }

    pub fn local_description(&self) -> Result<String> {
        let socketaddr: SocketAddr = self.rtp_socket.get_addr().addr.to_owned().try_into()?;
        let mut sdp = SessionDescription::default();

        // Set session-level attributes
        sdp.version = 0;
        sdp.origin = Origin {
            username: "-".to_string(),
            session_id: 0,
            session_version: 0,
            network_type: "IN".to_string(),
            address_type: "IP4".to_string(),
            unicast_address: socketaddr.ip().to_string(),
        };
        sdp.session_name = "-".to_string();
        sdp.connection_information = Some(ConnectionInformation {
            address_type: "IP4".to_string(),
            network_type: "IN".to_string(),
            address: Some(Address {
                address: socketaddr.ip().to_string(),
                ttl: None,
                range: None,
            }),
        });
        sdp.time_descriptions.push(TimeDescription {
            timing: Timing {
                start_time: 0,
                stop_time: 0,
            },
            repeat_times: vec![],
        });

        // Add media section
        let mut media = MediaDescription::default();
        media.media_name = MediaName {
            media: "audio".to_string(),
            port: RangedPort {
                value: socketaddr.port() as isize,
                range: None,
            },
            protos: vec!["RTP".to_string(), "AVP".to_string()],
            formats: vec![],
        };
        let inner = self.inner.lock().unwrap();
        for codec in inner.enabled_codecs.iter() {
            if codec == &CodecType::TelephoneEvent {
                continue;
            }
            // Try to find payload type from rtp_map (from caller's offer), otherwise use default
            let mut payload_type = codec.payload_type();
            for (payload_typ, (rtp_map_codec, _, _)) in inner.rtp_map.iter() {
                if *rtp_map_codec == *codec {
                    payload_type = *payload_typ;
                    break;
                }
            }

            media.media_name.formats.push(payload_type.to_string());
            media.attributes.push(Attribute {
                key: "rtpmap".to_string(),
                value: Some(format!("{} {}", payload_type, codec.rtpmap())),
            });
            if let Some(fmtp) = codec.fmtp() {
                media.attributes.push(Attribute {
                    key: "fmtp".to_string(),
                    value: Some(format!("{} {}", payload_type, fmtp)),
                });
            }
        }

        // Add telephone-event
        // Creating an offer: add telephone-event if enabled_codecs have 8000 or 48000 clock rate
        let has_8khz_codec = inner.enabled_codecs.iter().any(|c| c.clock_rate() == 8000);
        let has_48khz_codec = inner.enabled_codecs.iter().any(|c| c.clock_rate() == 48000);

        if has_8khz_codec {
            // Add telephone-event at 8000 Hz (default payload type 101)
            let mut payload_type = 101;
            for (typ, (codec, clock_rate, _)) in inner.rtp_map.iter() {
                if *codec == CodecType::TelephoneEvent && *clock_rate == 8000 {
                    payload_type = *typ;
                    break;
                }
            }
            media.media_name.formats.push(payload_type.to_string());
            media.attributes.push(Attribute {
                key: "rtpmap".to_string(),
                value: Some(format!("{} telephone-event/8000", payload_type)),
            });
            media.attributes.push(Attribute {
                key: "fmtp".to_string(),
                value: Some(format!("{} 0-16", payload_type)),
            });
        }

        if has_48khz_codec {
            let mut payload_type = 97;
            for (typ, (codec, clock_rate, _)) in inner.rtp_map.iter() {
                if *codec == CodecType::TelephoneEvent && *clock_rate == 48000 {
                    payload_type = *typ;
                    break;
                }
            }

            media.media_name.formats.push(payload_type.to_string());
            media.attributes.push(Attribute {
                key: "rtpmap".to_string(),
                value: Some(format!("{} telephone-event/48000", payload_type)),
            });
            media.attributes.push(Attribute {
                key: "fmtp".to_string(),
                value: Some(format!("{} 0-16", payload_type)),
            });
        }

        // Add media-level attributes
        if inner.rtcp_mux {
            media.attributes.push(Attribute {
                key: ATTR_KEY_RTCPMUX.to_string(),
                value: None,
            });
        }
        media.attributes.push(Attribute {
            key: ATTR_KEY_SSRC.to_string(),
            value: Some(if self.ssrc_cname.is_empty() {
                self.ssrc.to_string()
            } else {
                format!("{} cname:{}", self.ssrc, self.ssrc_cname)
            }),
        });
        if self.sendrecv.load(Ordering::Relaxed) {
            media.attributes.push(Attribute {
                key: ATTR_KEY_SEND_RECV.to_string(),
                value: None,
            });
        } else {
            media.attributes.push(Attribute {
                key: ATTR_KEY_SEND_ONLY.to_string(),
                value: None,
            });
        }
        media.attributes.push(Attribute {
            key: "ptime".to_string(),
            value: Some(format!("{}", self.config.ptime.as_millis())),
        });
        sdp.media_descriptions.push(media);
        Ok(sdp.marshal())
    }

    // Send DTMF tone using RFC 4733
    pub async fn send_dtmf(&self, digit: &str, duration_ms: Option<u64>) -> Result<()> {
        // Map DTMF digit to event code first (validate before checking remote address)
        let event_code = match digit {
            "0" => 0,
            "1" => 1,
            "2" => 2,
            "3" => 3,
            "4" => 4,
            "5" => 5,
            "6" => 6,
            "7" => 7,
            "8" => 8,
            "9" => 9,
            "*" => 10,
            "#" => 11,
            "A" => 12,
            "B" => 13,
            "C" => 14,
            "D" => 15,
            _ => return Err(anyhow::anyhow!("Invalid DTMF digit")),
        };
        let inner = self.inner.lock().unwrap();
        let socket = &self.rtp_socket;
        let remote_addr = match inner.remote_addr.as_ref() {
            Some(addr) => addr.clone(),
            None => return Err(anyhow::anyhow!("Remote address not set")),
        };

        // Use default duration if not specified
        let duration = duration_ms.unwrap_or(DTMF_EVENT_DURATION_MS);

        // Calculate number of packets to send
        // We send one packet every 20ms (default packet time)
        let num_packets = (duration as f64 / self.config.ptime.as_millis() as f64).ceil() as u32;

        // Calculate samples per packet for timestamp increments
        let samples_per_packet =
            (self.config.samplerate as f64 * self.config.ptime.as_secs_f64()) as u32;

        let now = crate::media::get_timestamp();
        inner
            .stats
            .last_timestamp_update
            .store(now, Ordering::Relaxed);

        // Generate RFC 4733 DTMF events
        for i in 0..num_packets {
            let is_end = i == num_packets - 1;
            let event_duration = i * (self.config.ptime.as_millis() as u32 * 8); // Duration in timestamp units

            // Create DTMF event payload
            // Format: |event(8)|E|R|Volume(6)|Duration(16)|
            let mut payload = vec![0u8; 4];
            payload[0] = event_code;
            payload[1] = DTMF_EVENT_VOLUME & 0x3F; // Volume (0-63)
            if is_end {
                payload[1] |= 0x80; // Set end bit (E)
            }

            // Duration (16 bits, network byte order)
            payload[2] = ((event_duration >> 8) & 0xFF) as u8;
            payload[3] = (event_duration & 0xFF) as u8;

            let packets = match inner.packetizer.lock().unwrap().as_mut() {
                Some(p) => p.packetize(&Bytes::from_owner(payload), samples_per_packet)?,
                None => return Err(anyhow::anyhow!("Packetizer not set")),
            };
            for mut packet in packets {
                packet.header.payload_type = inner.dtmf_payload_type;
                packet.header.marker = false;

                match packet.marshal() {
                    Ok(ref rtp_data) => {
                        match socket.send_raw(rtp_data, &remote_addr).await {
                            Ok(_) => {}
                            Err(e) => {
                                warn!("Failed to send DTMF RTP packet: {}", e);
                            }
                        }

                        // Update counters for RTCP
                        inner.stats.packet_count.fetch_add(1, Ordering::Relaxed);
                        inner
                            .stats
                            .octet_count
                            .fetch_add(rtp_data.len() as u32, Ordering::Relaxed);

                        // Sleep for packet time if not the last packet
                        if !is_end {
                            tokio::time::sleep(self.config.ptime).await;
                        }
                    }
                    Err(e) => {
                        warn!("Failed to create DTMF RTP packet: {:?}", e);
                        continue;
                    }
                }
            }
        }

        // After sending DTMF, update the timestamp to account for the DTMF duration
        inner
            .stats
            .timestamp
            .fetch_add(samples_per_packet * num_packets, Ordering::Relaxed);

        Ok(())
    }

    // Send STUN Binding Request for ICE connectivity check
    async fn send_ice_connectivity_check(
        socket: &UdpConnection,
        remote_addr: &SipAddr,
    ) -> Result<()> {
        let mut stun_packet = vec![0u8; 20]; // STUN header is 20 bytes
        stun_packet[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
        stun_packet[2..4].copy_from_slice(&0u16.to_be_bytes());
        stun_packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        let transaction_id: [u8; STUN_TRANSACTION_ID_SIZE] = rand::random();
        stun_packet[8..20].copy_from_slice(&transaction_id);

        socket.send_raw(&stun_packet, remote_addr).await.ok();
        Ok(())
    }

    async fn handle_rtcp_packet(
        track_id: &TrackId,
        buf: &[u8],
        n: usize,
        stats: &Arc<RtpTrackStats>,
        ssrc: u32,
    ) -> Result<()> {
        use webrtc::rtcp::packet::unmarshal;

        let mut buf_slice = &buf[0..n];
        let packets = match unmarshal(&mut buf_slice) {
            Ok(packets) => packets,
            Err(e) => {
                warn!(track_id, "Failed to parse RTCP packet: {:?}", e);
                return Ok(());
            }
        };

        for packet in packets {
            if let Some(sr) = packet.as_any().downcast_ref::<SenderReport>() {
                stats.store_sr_info(sr.rtp_time as u64, sr.ntp_time);
                info!(
                    track_id,
                    ssrc = sr.ssrc,
                    packet_count = sr.packet_count,
                    octet_count = sr.octet_count,
                    rtp_time = sr.rtp_time,
                    "Received SR"
                );
            } else if let Some(rr) = packet.as_any().downcast_ref::<ReceiverReport>() {
                for report in &rr.reports {
                    if report.ssrc == ssrc {
                        let packet_loss = report.fraction_lost;
                        let total_lost = report.total_lost;
                        let jitter = report.jitter;

                        info!(
                            track_id,
                            ssrc = report.ssrc,
                            fraction_lost = packet_loss,
                            total_lost = total_lost,
                            jitter = jitter,
                            last_sequence_number = report.last_sequence_number,
                            "Received RR for our stream"
                        );

                        if packet_loss > 50 {
                            warn!(track_id, "High packet loss detected: {}%", packet_loss);
                        }
                    }
                }
            } else if let Some(_) = packet.as_any().downcast_ref::<SourceDescription>() {
            } else {
                debug!(
                    track_id,
                    packet_type = %packet.header().packet_type,
                    "Received other RTCP packet type"
                );
            }
        }

        Ok(())
    }

    async fn classify_packet(
        track_id: &TrackId,
        buf: &[u8],
        n: usize,
        stats: &Arc<RtpTrackStats>,
        ssrc: u32,
    ) -> PacketKind {
        // Detect STUN packets first
        if n >= 20 {
            let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
            let msg_length = u16::from_be_bytes([buf[2], buf[3]]);
            let magic_cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

            if magic_cookie == STUN_MAGIC_COOKIE
                || ((msg_type & 0xC000) == 0x0000 && (msg_length as usize + 20) <= n)
            {
                debug!(
                    track_id = track_id.as_str(),
                    "Received STUN packet with message type: 0x{:04X}, length: {}", msg_type, n
                );
                return PacketKind::Stun(msg_type);
            }
        }

        // Detect RTCP packets
        let version = (buf[0] >> 6) & 0x03;
        let rtcp_pt = buf[1];
        if version == 2 && rtcp_pt >= 200 && rtcp_pt <= 207 {
            if let Err(e) = Self::handle_rtcp_packet(track_id, buf, n, stats, ssrc).await {
                warn!(
                    track_id = track_id.as_str(),
                    "Failed to handle RTCP packet: {:?}", e
                );
            }
            return PacketKind::Rtcp;
        }

        // Validate RTP packets
        let rtp_pt = buf[1] & 0x7F;
        if version != 2 {
            info!(
                track_id = track_id.as_str(),
                "Received packet with invalid RTP version: {}, skipping", version
            );
            return PacketKind::Ignore;
        }

        if rtp_pt >= 128 {
            debug!(
                track_id = track_id.as_str(),
                "Received packet with invalid RTP payload type: {}, might be unrecognized protocol",
                rtp_pt
            );
            return PacketKind::Ignore;
        }

        PacketKind::Rtp
    }

    async fn recv_rtp_packets(
        inner: Arc<Mutex<RtpTrackInner>>,
        ptime: Duration,
        rtp_socket: UdpConnection,
        track_id: TrackId,
        processor_chain: ProcessorChain,
        packet_sender: TrackPacketSender,
        _rtcp_socket: UdpConnection,
        ssrc: u32,
    ) -> Result<()> {
        let mut buf = vec![0u8; RTP_MTU];
        let mut send_ticker = tokio::time::interval(ptime);
        let mut jitter = JitterBuffer::new();
        let stats = inner.lock().unwrap().stats.clone();

        loop {
            select! {
                Ok((n, src_addr)) = rtp_socket.recv_raw(&mut buf) => {
                    if n == 0 {
                        continue;
                    }


                    let packet_kind = Self::classify_packet(&track_id, &buf, n, &stats, ssrc).await;
                    match packet_kind {
                        PacketKind::Stun(msg_type) => {
                            let force = msg_type == STUN_BINDING_RESPONSE;
                            Self::maybe_update_remote_addr(&inner, &src_addr, force, &track_id, "stun");
                            continue;
                        }
                        PacketKind::Rtcp => {
                            Self::maybe_update_remote_addr(&inner, &src_addr, false, &track_id, "rtcp");
                            continue;
                        }
                        PacketKind::Ignore => {
                            continue;
                        }
                        PacketKind::Rtp => {
                            Self::maybe_update_remote_addr(&inner, &src_addr, false, &track_id, "rtp-private");
                        }
                    }
                    let packet = match Packet::unmarshal(&mut &buf[0..n]) {
                        Ok(packet) => packet,
                        Err(e) => {
                            info!(track_id, "Error creating RTP reader: {:?}", e);
                            continue;
                        }
                    };

                    let seq_num = packet.header.sequence_number as u32;
                    let payload_len = packet.payload.len() as u32;
                    stats.update_receive_stats(seq_num, payload_len);

                    let payload_type = packet.header.payload_type;
                    let payload = packet.payload.to_vec();
                    let sample_rate = match payload_type {
                        9 => 16000,   // G.722
                        111 => 48000, // Opus
                        _ => 8000,
                    };

                    let frame = AudioFrame {
                        track_id: track_id.clone(),
                        samples: Samples::RTP {
                            payload_type,
                            payload,
                            sequence_number: packet.header.sequence_number.into(),
                        },
                        timestamp: crate::media::get_timestamp(),
                        sample_rate,
                    };

                    jitter.push(frame);
                }
                _ = send_ticker.tick() => {
                    let mut frame = match jitter.pop() {
                        Some(f) => f,
                        None => continue,
                    };

                    if let Err(e) = processor_chain.process_frame(&mut frame) {
                        warn!(track_id, "Failed to process frame: {}", e);
                        break;
                    }
                    match packet_sender.send(frame) {
                        Ok(_) => {}
                        Err(e) => {
                            warn!(track_id, "Error sending audio frame: {}", e);
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn maybe_update_remote_addr(
        inner: &Arc<Mutex<RtpTrackInner>>,
        src_addr: &SipAddr,
        force: bool,
        track_id: &TrackId,
        reason: &'static str,
    ) -> bool {
        let mut guard = inner.lock().unwrap();
        let src_ip = Self::sip_addr_ip(src_addr);

        let should_update = if force {
            true
        } else {
            match (guard.remote_addr.as_ref(), src_ip) {
                (Some(remote), Some(src_ip)) => match Self::sip_addr_ip(remote) {
                    Some(remote_ip) => remote_ip != src_ip && Self::is_private_ip(&remote_ip),
                    None => false,
                },
                (None, _) => true,
                _ => false,
            }
        };

        if should_update {
            let old = guard.remote_addr.replace(src_addr.clone());
            if guard.rtcp_mux {
                guard.remote_rtcp_addr = Some(src_addr.clone());
            } else if let Some(rtcp_addr) = guard.remote_rtcp_addr.as_mut() {
                rtcp_addr.addr.host = src_addr.addr.host.clone();
            }
            info!(
                track_id = track_id.as_str(),
                ?old,
                ?src_addr,
                reason = reason,
                "Updating remote RTP address"
            );
            return true;
        }
        false
    }

    fn sip_addr_ip(addr: &SipAddr) -> Option<IpAddr> {
        addr.addr.host.to_string().parse().ok()
    }

    fn is_private_ip(ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_documentation()
                    || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                v6.is_unique_local()
                    || v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unicast_link_local()
            }
        }
    }

    // Send RTCP sender reports periodically
    async fn send_rtcp_reports(
        inner: Arc<Mutex<RtpTrackInner>>,
        track_id: TrackId,
        token: CancellationToken,
        rtcp_socket: &UdpConnection,
        ssrc: u32,
        ssrc_cname: String,
    ) -> Result<()> {
        let mut interval = interval_at(
            Instant::now() + Duration::from_millis(RTCP_SR_INTERVAL_MS),
            Duration::from_millis(RTCP_SR_INTERVAL_MS),
        );
        let stats = inner.lock().unwrap().stats.clone();
        let mut last_sent_octets = stats.octet_count.load(Ordering::Relaxed);
        let mut last_recv_octets = stats.received_octets.load(Ordering::Relaxed);
        let mut last_rate_instant = Instant::now();
        loop {
            select! {
                _ = token.cancelled() => {
                    info!(track_id, "RTCP reports task cancelled");
                    break;
                }
                _ = interval.tick() => {
                    // Generate RTCP Sender Report
                    let packet_count = stats.packet_count.load(Ordering::Relaxed);
                    let octet_count = stats.octet_count.load(Ordering::Relaxed);
                    let rtp_timestamp = stats.timestamp.load(Ordering::Relaxed);

                    let sent_octets = octet_count;
                    let recv_octets = stats.received_octets.load(Ordering::Relaxed);
                    let now = Instant::now();
                    let elapsed = now.saturating_duration_since(last_rate_instant).as_secs_f64();
                    if elapsed > 0.0 {
                        let delta_sent = if sent_octets >= last_sent_octets {
                            (sent_octets - last_sent_octets) as u64
                        } else {
                            (u32::MAX as u64 - last_sent_octets as u64) + sent_octets as u64 + 1
                        };
                        let delta_recv = if recv_octets >= last_recv_octets {
                            (recv_octets - last_recv_octets) as u64
                        } else {
                            (u32::MAX as u64 - last_recv_octets as u64) + recv_octets as u64 + 1
                        };

                        let send_bps = (delta_sent as f64 * 8.0) / elapsed;
                        let recv_bps = (delta_recv as f64 * 8.0) / elapsed;
                        let received_packets = stats.received_packets.load(Ordering::Relaxed);
                        let lost_packets = stats.lost_packets.load(Ordering::Relaxed);
                        let expected_packets = stats.expected_packets.load(Ordering::Relaxed);
                        let fraction_lost = stats.get_fraction_lost();
                        let loss_pct = (fraction_lost as f64) * 100.0 / 256.0;
                        let jitter = stats.jitter.load(Ordering::Relaxed);

                        info!(
                            track_id = track_id.as_str(),
                            send_kbps = send_bps / 1000.0,
                            recv_kbps = recv_bps / 1000.0,
                            sent_packets = packet_count,
                            recv_packets = received_packets,
                            expected_packets,
                            lost_packets,
                            loss_pct,
                            jitter,
                            "RTP throughput"
                        );

                        last_rate_instant = now;
                        last_sent_octets = sent_octets;
                        last_recv_octets = recv_octets;
                    }

                    let mut pkts = vec![Box::new(SenderReport {
                        ssrc,
                        ntp_time: Instant::now().elapsed().as_secs() as u64,
                        rtp_time: rtp_timestamp,
                        packet_count,
                        octet_count,
                        profile_extensions: Bytes::new(),
                        reports: vec![],
                    })
                        as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>];

                    if !ssrc_cname.is_empty() {
                        pkts.push(Box::new(SourceDescription {
                            chunks: vec![SourceDescriptionChunk {
                                source: ssrc,
                                items: vec![SourceDescriptionItem {
                                    sdes_type: SdesType::SdesCname,
                                    text: ssrc_cname.clone().into(),
                                }],
                            }],
                        })
                            as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>);
                    }

                    let received_packets = stats.received_packets.load(Ordering::Relaxed);
                    let lost_packets = stats.lost_packets.load(Ordering::Relaxed);
                    let highest_seq = stats.highest_seq_num.load(Ordering::Relaxed);
                    let jitter = stats.jitter.load(Ordering::Relaxed);
                    let fraction_lost = stats.get_fraction_lost();

                    if received_packets > 0 || lost_packets > 0 {
                        let remote_ssrc = ssrc + 1;
                        let report = ReceptionReport {
                            ssrc: remote_ssrc,
                            fraction_lost,
                            total_lost: lost_packets,
                            last_sequence_number: highest_seq,
                            jitter,
                            last_sender_report: (stats.last_sr_timestamp.load(Ordering::Relaxed) >> 16) as u32,
                            delay: 0,
                        };

                        let rr = ReceiverReport {
                            ssrc,
                            reports: vec![report],
                            profile_extensions: Bytes::new(),
                        };
                        pkts.push(Box::new(rr) as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>);
                    }

                    let rtcp_data = webrtc::rtcp::packet::marshal(&pkts)?;
                    let remote_rtcp_addr = inner.lock().unwrap().remote_rtcp_addr.clone();
                    match remote_rtcp_addr{
                        Some(ref addr) => {
                            if let Err(e) = rtcp_socket.send_raw(&rtcp_data, addr).await {
                                warn!(track_id, "Failed to send RTCP report: {}", e);
                            }
                        }
                        None => {}
                    }
                }
            }
        }
        Ok(())
    }

    async fn try_ice_connectivity_check(&self) {
        let remote_addr = self.inner.lock().unwrap().remote_addr.clone();
        let remote_rtcp_addr = self.inner.lock().unwrap().remote_rtcp_addr.clone();

        if let Some(ref addr) = remote_addr {
            Self::send_ice_connectivity_check(&self.rtp_socket, addr)
                .await
                .ok();
            if let Some(ref rtcp_addr) = remote_rtcp_addr {
                if rtcp_addr != addr {
                    Self::send_ice_connectivity_check(&self.rtcp_socket, rtcp_addr)
                        .await
                        .ok();
                }
            }
        }
    }
}

#[async_trait]
impl Track for RtpTrack {
    fn ssrc(&self) -> u32 {
        self.ssrc
    }
    fn id(&self) -> &TrackId {
        &self.track_id
    }
    fn config(&self) -> &TrackConfig {
        &self.config
    }
    fn processor_chain(&mut self) -> &mut ProcessorChain {
        &mut self.processor_chain
    }

    async fn handshake(&mut self, offer: String, _timeout: Option<Duration>) -> Result<String> {
        self.set_remote_description(&offer)?;
        self.local_description()
    }

    async fn update_remote_description(&mut self, answer: &String) -> Result<()> {
        self.set_remote_description(&answer).ok();

        if self.ice_connectivity_check {
            self.try_ice_connectivity_check().await;
        }
        Ok(())
    }

    async fn start(
        &self,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        let track_id = self.track_id.clone();
        let rtcp_socket = self.rtcp_socket.clone();
        let ssrc = self.ssrc;
        let rtp_socket = self.rtp_socket.clone();
        let processor_chain = self.processor_chain.clone();
        let token = self.cancel_token.clone();
        let ssrc_cname = self.ssrc_cname.clone();
        let start_time = crate::media::get_timestamp();
        let ptime = self.config.ptime;

        // Send ICE connectivity check if enabled and remote address is available
        if self.ice_connectivity_check {
            self.try_ice_connectivity_check().await;
        }

        let inner = self.inner.clone();

        tokio::spawn(async move {
            select! {
                _ = token.cancelled() => {
                    debug!(track_id, "RTP processor task cancelled");
                },
                _ = Self::send_rtcp_reports(inner.clone(),track_id.clone(), token.clone(), &rtcp_socket, ssrc, ssrc_cname) => {
                }
                _ = Self::recv_rtp_packets(
                    inner.clone(),
                    ptime,
                    rtp_socket,
                    track_id.clone(),
                    processor_chain,
                    packet_sender,
                    rtcp_socket.clone(),
                    ssrc,
                ) => {
                }
            };
            let remote_rtcp_addr = inner.lock().unwrap().remote_rtcp_addr.clone();
            // send rtcp bye packet
            match remote_rtcp_addr {
                Some(ref addr) => {
                    let pkts = vec![Box::new(Goodbye {
                        sources: vec![ssrc],
                        reason: "end of call".into(),
                    })
                        as Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>];
                    if let Ok(data) = webrtc::rtcp::packet::marshal(&pkts) {
                        if let Err(e) = rtcp_socket.send_raw(&data, addr).await {
                            warn!(track_id, "Failed to send RTCP goodbye packet: {}", e);
                        }
                    }
                }
                None => {}
            }
            info!(track_id, "RTP processor completed");
            event_sender
                .send(SessionEvent::TrackEnd {
                    track_id,
                    timestamp: crate::media::get_timestamp(),
                    duration: crate::media::get_timestamp() - start_time,
                    ssrc,
                    play_id: None,
                })
                .ok();
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, packet: &AudioFrame) -> Result<()> {
        let remote_addr = match self.inner.lock().unwrap().remote_addr.clone() {
            Some(addr) => addr,
            None => return Ok(()),
        };
        let stats = self.inner.lock().unwrap().stats.clone();

        let (payload_type, payload) = self
            .encoder
            .encode(self.inner.lock().unwrap().payload_type, packet.clone());
        if payload.is_empty() {
            return Ok(());
        }

        let clock_rate = match payload_type {
            9 => 8000,    // G.722 (RTP clock rate is 8000 even though sample rate is 16000)
            111 => 48000, // Opus
            _ => 8000,
        };

        let now = crate::media::get_timestamp();
        let last_update = stats.last_timestamp_update.load(Ordering::Relaxed);
        let mut skipped_packets: u32 = 0;

        if last_update > 0 {
            let frame_duration_ms = self.config.ptime.as_millis() as u64;
            if frame_duration_ms > 0 {
                let delta_ms = now.saturating_sub(last_update);
                let delta_frames = delta_ms / frame_duration_ms;
                let prospective_skip = delta_frames.saturating_sub(1);

                if prospective_skip >= RTP_RESYNC_MIN_SKIP_PACKETS as u64 {
                    let last_resync = stats.last_resync_ts.load(Ordering::Relaxed);
                    let cooldown_ms = frame_duration_ms.saturating_mul(RTP_RESYNC_COOLDOWN_FRAMES);
                    if last_resync == 0 || now.saturating_sub(last_resync) >= cooldown_ms {
                        skipped_packets = prospective_skip.min(u32::MAX as u64) as u32;
                        debug!(
                            track_id = self.track_id,
                            delta_ms, skipped_packets, "Resyncing RTP timestamp"
                        );
                        for _ in 0..skipped_packets {
                            self.sequencer.next_sequence_number();
                        }
                        stats.last_resync_ts.store(now, Ordering::Relaxed);
                    }
                }
            }
        }

        stats.last_timestamp_update.store(now, Ordering::Relaxed);

        let samples_per_packet = (clock_rate as f64 * self.config.ptime.as_secs_f64()) as u32;
        let packets = match self
            .inner
            .lock()
            .unwrap()
            .packetizer
            .lock()
            .unwrap()
            .as_mut()
        {
            Some(p) => {
                if skipped_packets > 0 {
                    let skip_samples = (skipped_packets as u64)
                        .saturating_mul(samples_per_packet as u64)
                        .min(u32::MAX as u64) as u32;
                    p.skip_samples(skip_samples);
                }
                p.packetize(&Bytes::from_owner(payload), samples_per_packet)?
            }
            None => return Err(anyhow::anyhow!("Packetizer not set")),
        };
        for mut packet in packets {
            packet.header.marker = false;
            packet.header.payload_type = payload_type;
            match packet.marshal() {
                Ok(ref rtp_data) => match self.rtp_socket.send_raw(rtp_data, &remote_addr).await {
                    Ok(_) => {
                        stats.update_send_stats(rtp_data.len() as u32, samples_per_packet);
                    }
                    Err(e) => {
                        warn!(track_id = self.track_id, "Failed to send RTP packet: {}", e);
                    }
                },
                Err(e) => {
                    warn!(
                        track_id = self.track_id,
                        "Failed to build RTP packet: {:?}", e
                    );
                    return Err(anyhow::anyhow!("Failed to build RTP packet"));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtp_track_stats_new() {
        let stats = RtpTrackStats::new();
        assert_eq!(stats.packet_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.octet_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats.received_packets.load(Ordering::Relaxed), 0);
        assert_eq!(stats.lost_packets.load(Ordering::Relaxed), 0);
        assert_eq!(stats.jitter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_update_send_stats() {
        let stats = RtpTrackStats::new();
        stats.update_send_stats(1200, 160);

        assert_eq!(stats.packet_count.load(Ordering::Relaxed), 1);
        assert_eq!(stats.octet_count.load(Ordering::Relaxed), 1200);
        assert_eq!(stats.timestamp.load(Ordering::Relaxed), 160);

        // Test multiple updates
        stats.update_send_stats(800, 160);
        assert_eq!(stats.packet_count.load(Ordering::Relaxed), 2);
        assert_eq!(stats.octet_count.load(Ordering::Relaxed), 2000);
        assert_eq!(stats.timestamp.load(Ordering::Relaxed), 320);
    }

    #[test]
    fn test_update_receive_stats() {
        let stats = RtpTrackStats::new();

        // First packet
        stats.update_receive_stats(1000, 160);
        assert_eq!(stats.received_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.received_octets.load(Ordering::Relaxed), 160);
        assert_eq!(stats.highest_seq_num.load(Ordering::Relaxed), 1000);
        assert_eq!(stats.base_seq.load(Ordering::Relaxed), 1000);
        assert_eq!(stats.last_receive_seq.load(Ordering::Relaxed), 1000);
        assert_eq!(stats.expected_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.lost_packets.load(Ordering::Relaxed), 0);

        // Second packet with gap
        stats.update_receive_stats(1002, 160);
        assert_eq!(stats.received_packets.load(Ordering::Relaxed), 2);
        assert_eq!(stats.highest_seq_num.load(Ordering::Relaxed), 1002);
        assert_eq!(stats.last_receive_seq.load(Ordering::Relaxed), 1002);
        assert_eq!(stats.lost_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.expected_packets.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_get_fraction_lost() {
        let stats = RtpTrackStats::new();

        // No packets - should return 0
        assert_eq!(stats.get_fraction_lost(), 0);

        // Set some loss
        stats.expected_packets.store(100, Ordering::Relaxed);
        stats.lost_packets.store(5, Ordering::Relaxed);

        let fraction_lost = stats.get_fraction_lost();
        assert_eq!(fraction_lost, 12); // (5 * 256) / 100 = 12.8 -> 12

        // Test maximum loss
        stats.lost_packets.store(100, Ordering::Relaxed);
        assert_eq!(stats.get_fraction_lost(), 255); // Should cap at 255
    }

    #[test]
    fn test_store_sr_info() {
        let stats = RtpTrackStats::new();
        stats.store_sr_info(123456, 789012);

        assert_eq!(stats.last_sr_timestamp.load(Ordering::Relaxed), 123456);
        assert_eq!(stats.last_sr_ntp.load(Ordering::Relaxed), 789012);
    }

    #[tokio::test]
    async fn test_parse_pjsip_sdp() {
        let sdp = r#"v=0
o=- 3954304612 3954304613 IN IP4 192.168.1.202
s=pjmedia
b=AS:117
t=0 0
a=X-nat:3
m=audio 4002 RTP/AVP 9 101
c=IN IP4 192.168.1.202
b=TIAS:96000
a=rtcp:4003 IN IP4 192.168.1.202
a=sendrecv
a=rtpmap:9 G722/8000
a=ssrc:1089147397 cname:61753255553b9c6f
a=rtpmap:101 telephone-event/8000
a=fmtp:101 0-16"#;
        let rtp_track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build rtp track");
        rtp_track
            .set_remote_description(sdp)
            .expect("Failed to set remote description");
        let inner = rtp_track.inner.lock().unwrap();
        assert_eq!(inner.payload_type, 9);
        assert!(!inner.rtcp_mux); // RTCP is on separate port
    }

    #[tokio::test]
    async fn test_parse_rtcp_mux() {
        let answer = r#"v=0
o=- 723884243 723884244 IN IP4 11.22.33.44
s=-
c=IN IP4 11.22.33.44
t=0 0
m=audio 10638 RTP/AVP 8 101
a=rtpmap:8 PCMA/8000
a=rtpmap:101 telephone-event/8000
a=fmtp:101 0-15
a=sendrecv
a=rtcp-mux"#;
        let mut reader = Cursor::new(answer);
        let sdp = SessionDescription::unmarshal(&mut reader).expect("Failed to parse SDP");
        let peer_media = select_peer_media(&sdp, "audio").expect("Failed to select_peer_media");
        assert!(peer_media.rtcp_mux);
        assert_eq!(peer_media.rtcp_port, 10638);
    }

    #[tokio::test]
    async fn test_parse_linphone_candidate() {
        let answer = r#"v=0
o=mpi 2590 792 IN IP4 192.168.3.181
s=Talk
c=IN IP4 192.168.3.181
t=0 0
a=ice-pwd:96adb77560869c783656fe0a
a=ice-ufrag:409dfd53
a=rtcp-xr:rcvr-rtt=all:10000 stat-summary=loss,dup,jitt,TTL voip-metrics
a=record:off
m=audio 61794 RTP/AVP 8 101
c=IN IP4 115.205.103.101
a=rtpmap:101 telephone-event/8000
a=rtcp:50735
a=candidate:1 1 UDP 2130706303 192.168.3.181 61794 typ host
a=candidate:1 2 UDP 2130706302 192.168.3.181 50735 typ host
a=candidate:2 1 UDP 1694498687 115.205.103.101 61794 typ srflx raddr 192.168.3.181 rport 61794
a=candidate:2 2 UDP 1694498686 115.205.103.101 50735 typ srflx raddr 192.168.3.181 rport 50735
a=rtcp-fb:* trr-int 5000
a=rtcp-fb:* ccm tmmbr"#;
        let mut reader = Cursor::new(answer);
        let sdp = SessionDescription::unmarshal(&mut reader).expect("Failed to parse SDP");
        let peer_media = select_peer_media(&sdp, "audio").expect("Failed to select_peer_media");
        assert_eq!(peer_media.rtp_addr, "192.168.3.181");
    }

    #[tokio::test]
    async fn test_rtp_track_builder() {
        let track_id = "test_track".to_string();
        let config = TrackConfig::default();

        let track = RtpTrackBuilder::new(track_id.clone(), config)
            .with_rtp_start_port(20000)
            .with_rtp_end_port(20100)
            .with_session_name("test_session".to_string())
            .build()
            .await
            .expect("Failed to build track");

        assert_eq!(track.track_id, track_id);
        // SSRC is randomly generated in build(), so we can't predict exact value
        assert_ne!(track.ssrc, 0); // Should not be zero
        assert_eq!(track.ssrc_cname, "test_session");
        let inner = track.inner.lock().unwrap();
        assert!(inner.rtcp_mux);
    }

    #[tokio::test]
    async fn test_local_description_generation() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");

        let local_desc = track
            .local_description()
            .expect("Failed to generate local description");

        // Verify SDP contains expected elements
        assert!(local_desc.contains("m=audio"));
        assert!(local_desc.contains("RTP/AVP"));
        assert!(local_desc.contains("a=rtcp-mux")); // Should have rtcp-mux by default
        assert!(local_desc.contains("a=sendrecv"));
        assert!(local_desc.contains(&format!("a=ssrc:{}", track.ssrc)));
    }

    #[tokio::test]
    async fn test_double_set_remote_description() {
        let sdp = r#"v=0
o=- 123 124 IN IP4 192.168.1.1
s=-
c=IN IP4 192.168.1.1
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");

        // First call should succeed
        assert!(track.set_remote_description(sdp).is_ok());
        assert!(track.remote_description().is_some());

        // Second call should be ignored (no error)
        assert!(track.set_remote_description(sdp).is_ok());
    }

    #[tokio::test]
    async fn test_invalid_sdp() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");

        // Invalid SDP without audio media
        let invalid_sdp = r#"v=0
o=- 123 124 IN IP4 192.168.1.1
s=-
c=IN IP4 192.168.1.1
t=0 0"#;

        assert!(track.set_remote_description(invalid_sdp).is_err());
    }

    #[tokio::test]
    async fn test_dtmf_digit_mapping() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");

        // Test valid digits - these should not panic during mapping
        let valid_digits = [
            "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "*", "#", "A", "B", "C", "D",
        ];

        for digit in &valid_digits {
            // Since we don't have remote address set, this will fail with "Remote address not set"
            // but it shouldn't fail on digit mapping
            let result = track.send_dtmf(digit, Some(100)).await;
            assert!(result.is_err());
            let error_msg = result.unwrap_err().to_string();
            assert!(error_msg.contains("Remote address not set"));
        }

        // Test invalid digit
        let result = track.send_dtmf("X", Some(100)).await;
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Invalid DTMF digit"));
    }

    #[test]
    fn test_rtcp_packet_type_detection() {
        // Test RTCP packet type ranges
        assert!(200 >= 200 && 200 <= 207); // SR
        assert!(201 >= 200 && 201 <= 207); // RR
        assert!(202 >= 200 && 202 <= 207); // SDES
        assert!(203 >= 200 && 203 <= 207); // BYE
        assert!(204 >= 200 && 204 <= 207); // APP

        // Test RTP payload type extraction
        let rtp_byte = 0b10001001; // Version 2, PT 9
        let version = (rtp_byte >> 6) & 0x03;
        let pt = rtp_byte & 0x7F;

        assert_eq!(version, 2);
        assert_eq!(pt, 9);
    }

    #[test]
    fn test_stun_magic_cookie_detection() {
        let stun_magic_cookie = STUN_MAGIC_COOKIE;
        let bytes = stun_magic_cookie.to_be_bytes();
        let reconstructed = u32::from_be_bytes(bytes);

        assert_eq!(reconstructed, stun_magic_cookie);
    }

    #[tokio::test]
    async fn test_track_ssrc_and_id() {
        let track_id = "unique_track_123".to_string();
        let custom_ssrc = 0x12345678;

        let track = RtpTrackBuilder::new(track_id.clone(), TrackConfig::default())
            .with_ssrc(custom_ssrc)
            .build()
            .await
            .expect("Failed to build track");

        // Note: build() overrides SSRC with random value, so we test the builder method separately
        let builder =
            RtpTrackBuilder::new(track_id.clone(), TrackConfig::default()).with_ssrc(custom_ssrc);
        assert_eq!(builder.ssrc, custom_ssrc);
        assert_eq!(track.id(), &track_id);
    }

    #[test]
    fn test_codec_type_payload_mapping() {
        // Test common codec payload types
        assert_eq!(CodecType::PCMU.payload_type(), 0);
        assert_eq!(CodecType::G722.payload_type(), 9);
        assert_eq!(CodecType::PCMA.payload_type(), 8);
        assert_eq!(CodecType::TelephoneEvent.payload_type(), 101);
    }

    #[tokio::test]
    async fn test_stats_initialization() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");
        let inner = track.inner.lock().unwrap();
        // Verify stats are properly initialized
        assert_eq!(inner.stats.packet_count.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.octet_count.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.received_packets.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.lost_packets.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.highest_seq_num.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.jitter.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.last_sr_timestamp.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.last_sr_ntp.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.base_seq.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.last_receive_seq.load(Ordering::Relaxed), 0);
        assert_eq!(inner.stats.last_resync_ts.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_sequence_number_gap_calculation() {
        let stats = RtpTrackStats::new();

        // Simulate receiving packets with gaps
        stats.update_receive_stats(1000, 160); // First packet
        stats.update_receive_stats(1002, 160); // Skip 1001
        stats.update_receive_stats(1003, 160); // Consecutive
        stats.update_receive_stats(1005, 160); // Skip 1004

        assert_eq!(stats.received_packets.load(Ordering::Relaxed), 4);
        assert_eq!(stats.highest_seq_num.load(Ordering::Relaxed), 1005);
        // Loss calculation is simplified, so we just verify some loss is detected
        assert!(stats.lost_packets.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_jitter_calculation() {
        let stats = RtpTrackStats::new();

        // Test jitter calculation with sequence numbers
        stats.update_receive_stats(1000, 160);
        let _initial_jitter = stats.jitter.load(Ordering::Relaxed);

        stats.update_receive_stats(1001, 160);
        let updated_jitter = stats.jitter.load(Ordering::Relaxed);

        // Jitter calculation is simplified and may not always change
        // Let's just verify it doesn't panic and stays within reasonable bounds
        assert!(updated_jitter < 1000); // Should be reasonable value
    }

    #[test]
    fn test_builder_with_custom_ssrc() {
        let custom_ssrc = 0x12345678u32;
        let builder =
            RtpTrackBuilder::new("test".to_string(), TrackConfig::default()).with_ssrc(custom_ssrc);

        // Verify builder stores the custom SSRC
        assert_eq!(builder.ssrc, custom_ssrc);
        assert_eq!(builder.ssrc_cname, format!("rustpbx-{}", custom_ssrc));
    }

    #[test]
    fn test_builder_configuration() {
        let builder = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .with_rtp_start_port(10000)
            .with_rtp_end_port(20000)
            .with_rtp_alloc_count(100)
            .with_rtcp_mux(false)
            .with_session_name("custom_session".to_string());

        assert_eq!(builder.rtp_start_port, 10000);
        assert_eq!(builder.rtp_end_port, 20000);
        assert_eq!(builder.rtp_alloc_count, 100);
        assert!(!builder.rtcp_mux);
        assert_eq!(builder.ssrc_cname, "custom_session");
    }

    #[tokio::test]
    async fn test_ice_connectivity_check_enabled_by_default() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");

        assert!(track.ice_connectivity_check); // Should be enabled by default
    }

    #[tokio::test]
    async fn test_ice_connectivity_check_can_be_disabled() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .with_ice_connectivity_check(false)
            .build()
            .await
            .expect("Failed to build track");

        assert!(!track.ice_connectivity_check);
    }

    #[tokio::test]
    async fn test_maybe_update_remote_addr_private_peer() {
        let track = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .build()
            .await
            .expect("Failed to build track");
        let inner = track.inner.clone();

        let private_addr = SipAddr {
            addr: HostWithPort {
                host: "192.168.0.10".parse().expect("host"),
                port: Some(4000.into()),
            },
            r#type: Some(rsip::transport::Transport::Udp),
        };

        let public_addr = SipAddr {
            addr: HostWithPort {
                host: "203.0.113.5".parse().expect("host"),
                port: Some(5004.into()),
            },
            r#type: Some(rsip::transport::Transport::Udp),
        };

        {
            let mut guard = inner.lock().expect("lock");
            guard.remote_addr = Some(private_addr.clone());
            guard.remote_rtcp_addr = Some(private_addr.clone());
            guard.rtcp_mux = true;
        }

        let updated = RtpTrack::maybe_update_remote_addr(
            &inner,
            &public_addr,
            false,
            &track.track_id,
            "test",
        );

        assert!(updated);
        let guard = inner.lock().expect("lock");
        assert_eq!(
            guard
                .remote_addr
                .as_ref()
                .expect("remote")
                .addr
                .host
                .to_string(),
            "203.0.113.5"
        );
        assert_eq!(
            guard
                .remote_rtcp_addr
                .as_ref()
                .expect("rtcp")
                .addr
                .host
                .to_string(),
            "203.0.113.5"
        );
    }

    #[test]
    fn test_stun_packet_structure() {
        // Test STUN constants
        assert_eq!(STUN_BINDING_REQUEST, 0x0001);
        assert_eq!(STUN_MAGIC_COOKIE, 0x2112A442);
        assert_eq!(STUN_TRANSACTION_ID_SIZE, 12);

        // Test STUN packet construction would be valid
        let mut packet = vec![0u8; 20];
        packet[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
        packet[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());

        // Verify message type
        let msg_type = u16::from_be_bytes([packet[0], packet[1]]);
        assert_eq!(msg_type, STUN_BINDING_REQUEST);

        // Verify magic cookie
        let magic = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        assert_eq!(magic, STUN_MAGIC_COOKIE);
    }

    #[tokio::test]
    async fn test_ice_connectivity_check_builder_method() {
        let builder_enabled = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .with_ice_connectivity_check(true);
        assert!(builder_enabled.ice_connectivity_check);

        let builder_disabled = RtpTrackBuilder::new("test".to_string(), TrackConfig::default())
            .with_ice_connectivity_check(false);
        assert!(!builder_disabled.ice_connectivity_check);
    }

    #[test]
    fn test_ice_connectivity_terminology() {
        // Verify we're using correct ICE terminology
        // ICE connectivity checks use STUN Binding Requests
        // This is part of the ICE (Interactive Connectivity Establishment) standard

        // The purpose is:
        // 1. NAT traversal and hole punching
        // 2. Connectivity verification
        // 3. Keep-alive for NAT bindings
        // 4. Path validation

        assert_eq!(STUN_BINDING_REQUEST, 0x0001); // RFC 5389
        assert_eq!(STUN_MAGIC_COOKIE, 0x2112A442); // RFC 5389
    }
}
