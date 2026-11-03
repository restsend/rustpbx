//! Per-call media health snapshots for call-trace diagnostics.
//!
//! [`MediaBridge`] samples both legs every tick (5 s) and publishes a
//! [`MediaHealthSnapshot`] on a `watch` channel. The SIP session turns the
//! stream into:
//!
//! - CDR `metadata["trace"]` events (periodic + anomaly-triggered), so a
//!   call's media state is visible on the console without reading logs;
//! - an RWI `call_error` event (`proxy.media_stalled`) when a connected
//!   route keeps receiving zero inbound RTP — the signature of a
//!   firewall / mis-advertised-address black hole.
//!
//! The snapshot intentionally separates *pipeline* counters (IngressTap:
//! packets that reached the forwarding path) from *transport* counters
//! (accepted RTP after parsing/decryption). A gap can describe the receive
//! pipeline, but zero accepted RTP cannot establish whether UDP reached the
//! application.

use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

/// Selected per-leg egress path, independent of negotiated codec equality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaEgressMode {
    DirectRtpForwarding,
    PcmPacing,
}

/// Default window after route activation with zero inbound RTP on a leg
/// before the leg is flagged stalled. Overridable via
/// `[media] stall_detect_secs`.
pub const DEFAULT_STALL_DETECT: Duration = Duration::from_secs(15);

/// Per-leg media health sample.
#[derive(Debug, Clone, Serialize)]
pub struct LegMediaHealth {
    pub leg_id: String,
    pub egress_mode: MediaEgressMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_egress: Option<crate::volume_stats::VolumeSummary>,
    /// `"caller"` (A) or `"callee"` (B).
    pub side: &'static str,
    /// `"rtp"`, `"webrtc"`, … (rustrtc transport mode).
    pub transport_mode: String,
    /// Negotiated audio codec name, when known.
    pub codec: Option<String>,
    /// Negotiated audio payload type.
    pub payload_type: Option<u8>,
    /// Audio `addr:port` this leg advertised in its local SDP — where the
    /// peer is expected to send media.
    pub advertised_addr: Option<String>,
    /// Audio `addr:port` the peer advertised in its SDP — where this leg
    /// sends media.
    pub peer_advertised_addr: Option<String>,
    /// Wire-level remote address currently in use (ICE selected pair /
    /// RTP latch target). `None` = no path selected yet.
    pub remote_addr: Option<String>,
    /// Observed source differs from the peer's advertised address
    /// (symmetric-RTP latch engaged on a plain-RTP leg). `None` when either
    /// side of the comparison is unknown.
    pub latched: Option<bool>,
    /// IngressTap: packets that reached the forwarding pipeline.
    pub ingress_packets: u64,
    /// IngressTap: packets produced toward the peer.
    pub egress_packets: u64,
    /// Inbound RTP accepted after parsing/decryption; not raw UDP or NIC reads.
    pub transport_rx_packets: u64,
    /// Outbound RTP sender accounting, including relay submissions; not peer receipt.
    pub transport_tx_packets: u64,
    /// Deltas over the last sample window (5 s).
    pub ingress_delta: u64,
    pub egress_delta: u64,
    pub rx_delta: u64,
    pub tx_delta: u64,
    /// `rx_delta` packets that never became ingress — receive-pipeline drops.
    pub rx_gap_delta: u64,
    /// RTCP-derived quality (0.0 when not reported yet).
    pub jitter_ms: f64,
    pub rtt_ms: f64,
    pub fraction_lost_pct: f64,
    /// Milliseconds since the last inbound RTP on this leg; `None` = never.
    pub ms_since_last_rx: Option<u64>,
    /// Route age (seconds since both legs accepted) at which the first
    /// inbound RTP packet was observed; `None` = never received.
    pub first_rx_after_s: Option<f64>,
    /// Zero inbound RTP for longer than the stall window while the route is
    /// active — the call-trace / RWI `media_stalled` trigger.
    pub stalled: bool,
}

/// Whole-bridge health snapshot published every sampler tick.
#[derive(Debug, Clone, Serialize)]
pub struct MediaHealthSnapshot {
    /// Seconds since the media route was activated (both legs accepted).
    pub route_age_secs: f64,
    /// Both selected legs use fast-path relay; false alone does not classify each leg.
    pub relay_mode: bool,
    /// A (caller) then B (callee).
    pub legs: Vec<LegMediaHealth>,
}

impl MediaHealthSnapshot {
    /// Legs flagged stalled.
    pub fn stalled_sides(&self) -> Vec<&'static str> {
        self.legs
            .iter()
            .filter(|l| l.stalled)
            .map(|l| l.side)
            .collect()
    }
}

/// Events surfaced by [`MediaBridge::spawn_health_monitor`]. The monitor
/// owns the watch channel, sampler-tick bounding and once-per-leg latching;
/// the consumer maps events onto its own diagnostics channels.
#[derive(Debug, Clone)]
pub enum HealthEvent {
    /// Periodic bounded health snapshot for call-trace metadata (emitted
    /// every `trace_interval_secs`, capped per call by the monitor).
    Trace {
        route_age_secs: f64,
        snapshot: Arc<MediaHealthSnapshot>,
    },
    /// Zero inbound RTP on a connected leg — the firewall /
    /// mis-advertised-address black-hole signature. Fires once per leg
    /// per call.
    Stalled {
        side: &'static str,
        snapshot: Arc<MediaHealthSnapshot>,
    },
}
