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
//! (ICE/socket level): a growing gap between them localises drops to the
//! receive pipeline, while `transport_rx == 0` with a wire-selected pair
//! localises them to the network path.

use serde::Serialize;
use std::time::Duration;

/// Default window after route activation with zero inbound RTP on a leg
/// before the leg is flagged stalled. Overridable via
/// `[media] stall_detect_secs`.
pub const DEFAULT_STALL_DETECT: Duration = Duration::from_secs(15);

/// Per-leg media health sample.
#[derive(Debug, Clone, Serialize)]
pub struct LegMediaHealth {
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
    /// Transport-level inbound RTP (socket truth).
    pub transport_rx_packets: u64,
    /// Transport-level outbound RTP incl. relayed packets (sender counter).
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
    /// Fast-path relay (true) vs transcoding (false).
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
