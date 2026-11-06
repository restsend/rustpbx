//! Application-owned snapshots from existing RTP observers and transport stats.
//! Lower-level socket/decrypt/drop outcomes are correlated transport logs, not
//! a second packet-counter API. No task, callback or history is installed here.
use crate::leg::{Leg, LegInner};
use rustrtc::stats::StatsProvider;
use serde::Serialize;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct LegPacketSnapshot {
    pub observed_unix_ms: u64,
    pub snapshot_scope: &'static str,
    pub leg_id: String,
    pub accepted_rtp: u64,
    pub pipeline_ingress: u64,
    /// Existing plaintext egress observation, before protect/socket submission.
    pub pipeline_egress: u64,
    pub recording_queue_drops: u64,
    pub advertised_addr: Option<String>,
    pub peer_advertised_addr: Option<String>,
    pub peer_state: String,
    pub ice_state: String,
    pub signaling_state: String,
    pub gated: bool,
    pub app_paused: bool,
    pub transceiver_directions: Vec<String>,
    pub egress_mode: crate::media_health::MediaEgressMode,
    /// Vendor stats retain their original meanings. In particular txPackets
    /// is not a completion total for the direct/deferred relay fast path.
    pub transport_stats: Vec<serde_json::Value>,
    pub unmeasured_boundaries: [&'static str; 3],
}
impl LegPacketSnapshot {
    pub(crate) fn capture(leg: &LegInner) -> Self {
        let tap = leg.stats();
        let mut transports = Vec::new();
        for transceiver in leg.pc().get_transceivers() {
            if let Some(transport) = transceiver.sender().and_then(|sender| sender.transport()) {
                if !transports
                    .iter()
                    .any(|existing| std::sync::Arc::ptr_eq(existing, &transport))
                {
                    transports.push(transport);
                }
            }
        }
        let transport_stats = transports
            .iter()
            .flat_map(|transport| match transport.ice_conn().collect() {
                Ok(entries) => entries
                    .into_iter()
                    .map(|entry| serde_json::json!(entry.values))
                    .collect(),
                Err(error) => {
                    vec![serde_json::json!({"unavailable": true, "error": error.to_string()})]
                }
            })
            .collect();
        Self {
            observed_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            snapshot_scope: "live",
            leg_id: leg.id().to_string(),
            accepted_rtp: leg.pc().received_rtp_packets(),
            pipeline_ingress: tap.ingress_packets,
            pipeline_egress: tap.egress_packets,
            recording_queue_drops: tap.recording_queue_drops,
            advertised_addr: leg.advertised_addr(),
            peer_advertised_addr: leg.peer_advertised_addr(),
            peer_state: format!("{:?}", *leg.pc().subscribe_peer_state().borrow()),
            ice_state: format!("{:?}", *leg.pc().subscribe_ice_connection_state().borrow()),
            signaling_state: format!("{:?}", leg.pc().signaling_state()),
            gated: leg.is_gated(),
            app_paused: leg
                .rtp_timeout_state()
                .app_paused
                .load(std::sync::atomic::Ordering::Relaxed),
            transceiver_directions: leg
                .pc()
                .get_transceivers()
                .into_iter()
                .map(|t| format!("{:?}", t.direction()))
                .collect(),
            egress_mode: if leg.egress_is_relay() {
                crate::media_health::MediaEgressMode::DirectRtpForwarding
            } else {
                crate::media_health::MediaEgressMode::PcmPacing
            },
            transport_stats,
            unmeasured_boundaries: [
                "raw_socket_reads",
                "socket_send_completion",
                "parse_decrypt_drop_totals",
            ],
        }
    }
}

pub(crate) struct LegPacketDiagnostics {
    session_id: String,
    root_session_id: String,
    started: Instant,
    latest: Option<LegPacketSnapshot>,
}
impl LegPacketDiagnostics {
    pub(crate) fn finish(&mut self, leg: &LegInner) {
        if self.latest.is_some() {
            return;
        }
        let mut snapshot = LegPacketSnapshot::capture(leg);
        snapshot.snapshot_scope = "leg_stopping";
        tracing::info!(target: "rustpbx_media::packet_diagnostics",
            session_id = %self.session_id, root_session_id = %self.root_session_id,
            leg_id = %leg.id(), direction = "peer_to_pbx_and_pbx_to_peer", event = "final",
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            lower_level_evidence = "media.transport and media.transport_drop logs",
            snapshot = %serde_json::to_string(&snapshot).unwrap_or_default(), "media.packet_stages");
        self.latest = Some(snapshot);
    }
}
impl LegInner {
    pub(crate) fn start_packet_diagnostics(leg: &Leg, cfg: &crate::leg::LegConfig) {
        let Some(session_id) = cfg.call_id.clone().filter(|id| !id.is_empty()) else {
            return;
        };
        let root_session_id = cfg
            .root_session_id
            .clone()
            .unwrap_or_else(|| session_id.clone());
        tracing::info!(target: "rustpbx_media::packet_diagnostics",
            %session_id, %root_session_id, leg_id = %leg.id(),
            direction = "peer_to_pbx_and_pbx_to_peer", event = "initial",
            transport = ?cfg.transport, "media.packet_stages");
        *leg.packet_diagnostics.lock() = Some(LegPacketDiagnostics {
            session_id,
            root_session_id,
            started: Instant::now(),
            latest: None,
        });
    }
    pub fn packet_diagnostic_snapshot(&self) -> Option<LegPacketSnapshot> {
        self.packet_diagnostics.lock().as_ref().map(|state| {
            state
                .latest
                .clone()
                .unwrap_or_else(|| LegPacketSnapshot::capture(self))
        })
    }
}
