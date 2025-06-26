use crate::media::stream::MediaStream;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rsipstack::dialog::DialogId;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct SessionParty {
    pub aor: rsip::Uri,                          // Address of Record
    pub media_capabilities: Option<Vec<String>>, // Supported media types
    pub last_sdp: Option<String>,                // Last SDP exchange
}

impl SessionParty {
    pub fn new(aor: rsip::Uri) -> Self {
        Self {
            aor,
            media_capabilities: None,
            last_sdp: None,
        }
    }

    pub fn new_with_capabilities(aor: rsip::Uri, capabilities: Vec<String>) -> Self {
        Self {
            aor,
            media_capabilities: Some(capabilities),
            last_sdp: None,
        }
    }

    pub fn get_user(&self) -> String {
        self.aor.user().unwrap_or_default().to_string()
    }

    pub fn get_realm(&self) -> String {
        self.aor.host().to_string()
    }

    pub fn update_sdp(&mut self, sdp: String) {
        self.last_sdp = Some(sdp);
    }

    pub fn supports_webrtc(&self) -> bool {
        if let Some(ref capabilities) = self.media_capabilities {
            capabilities.contains(&"webrtc".to_string())
        } else {
            false
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaBridgeType {
    None,        // No conversion needed
    WebRtcToSip, // WebRTC to SIP
    SipToWebRtc, // SIP to WebRTC
    Transcode,   // Codec conversion
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionType {
    SipToSip,
    WebRtcToSip,
    SipToWebRtc,
    WebRtcToWebRtc,
}

#[derive(Clone, Debug)]
pub struct MediaStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u32,
    pub packets_received: u32,
    pub codec_used: Option<String>,
    pub last_packet_time: Option<Instant>,
}

impl Default for MediaStats {
    fn default() -> Self {
        Self {
            bytes_sent: 0,
            bytes_received: 0,
            packets_sent: 0,
            packets_received: 0,
            codec_used: None,
            last_packet_time: None,
        }
    }
}

#[derive(Clone)]
pub struct Session {
    pub dialog_id: DialogId,
    pub last_activity: Instant,
    pub caller: SessionParty,
    pub callees: Vec<SessionParty>,
    pub media_bridge_type: MediaBridgeType,
    pub established_at: Option<Instant>,
    pub media_stats: MediaStats,
    pub start_time: DateTime<Utc>,
    pub ring_time: Option<DateTime<Utc>>,
    pub answer_time: Option<DateTime<Utc>>,
    pub status_code: u16,
}

impl Session {
    pub fn new(dialog_id: DialogId, caller: SessionParty, callees: Vec<SessionParty>) -> Self {
        Self {
            dialog_id,
            last_activity: Instant::now(),
            caller,
            callees,
            media_bridge_type: MediaBridgeType::None,
            established_at: None,
            media_stats: MediaStats::default(),
            start_time: Utc::now(),
            ring_time: None,
            answer_time: None,
            status_code: 100, // Default to Trying
        }
    }

    pub fn set_established(&mut self) {
        self.established_at = Some(Instant::now());
        self.answer_time = Some(Utc::now());
    }

    pub fn set_ringing(&mut self) {
        self.ring_time = Some(Utc::now());
    }

    pub fn set_status_code(&mut self, code: u16) {
        self.status_code = code;
    }

    pub fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    pub fn set_media_bridge_type(&mut self, bridge_type: MediaBridgeType) {
        self.media_bridge_type = bridge_type;
    }

    pub fn update_media_stats(
        &mut self,
        bytes_sent: u64,
        bytes_received: u64,
        packets_sent: u32,
        packets_received: u32,
    ) {
        self.media_stats.bytes_sent += bytes_sent;
        self.media_stats.bytes_received += bytes_received;
        self.media_stats.packets_sent += packets_sent;
        self.media_stats.packets_received += packets_received;
        self.media_stats.last_packet_time = Some(Instant::now());
    }

    pub fn set_codec(&mut self, codec: String) {
        self.media_stats.codec_used = Some(codec);
    }

    pub fn duration(&self) -> Option<std::time::Duration> {
        self.established_at.map(|established| established.elapsed())
    }
}

// Enhanced Session structure with media stream support
#[derive(Clone)]
pub struct MediaSession {
    pub session: Session,
    pub media_stream: Option<Arc<MediaStream>>,
    pub session_type: SessionType,
    pub webrtc_sdp: Option<String>,
    pub sip_sdp: Option<String>,
}

impl MediaSession {
    pub fn new(
        session: Session,
        session_type: SessionType,
        webrtc_sdp: Option<String>,
        sip_sdp: Option<String>,
    ) -> Self {
        Self {
            session,
            media_stream: None,
            session_type,
            webrtc_sdp,
            sip_sdp,
        }
    }

    pub fn with_media_stream(mut self, media_stream: Option<Arc<MediaStream>>) -> Self {
        self.media_stream = media_stream;
        self
    }

    pub async fn cleanup_media_stream(&self) -> Result<()> {
        if let Some(ref stream) = self.media_stream {
            stream.stop(
                Some("session_cleanup".to_string()),
                Some("system".to_string()),
            );
            stream.cleanup().await?;
        }
        Ok(())
    }
}
