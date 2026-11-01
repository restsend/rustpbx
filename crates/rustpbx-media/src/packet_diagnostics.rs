//! Leg-lifetime packet evidence, controlled by the existing quality sampler settings.
//! This is observation only: an idle stage is never a network or media-fault verdict.
use crate::leg::{Leg, LegInner};
use rustrtc::diagnostics::{DeferredSendObserver, DeferredSendSnapshot, PacketDiagnostics};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::instrument::WithSubscriber;

const STAGES: [&str; 8] = [
    "udp_read",
    "turn_unwrapped",
    "conn_received",
    "rtp_accepted",
    "pipeline_ingress",
    "pipeline_egress",
    "send_attempts",
    "send_completed",
];
const IDLE_EVIDENCE_AFTER: Duration = Duration::from_secs(5);
type BridgeLegs = parking_lot::Mutex<(Option<Leg>, Option<Leg>)>;

#[derive(Debug, Clone, Serialize)]
pub struct LegPacketSnapshot {
    pub observed_unix_ms: u64,
    /// A stopping snapshot is captured before existing media shutdown. Late
    /// deferred results are correlated INFO records, never a CDR backfill.
    pub snapshot_scope: &'static str,
    pub async_send_results_in_cdr_final: bool,
    pub deferred_sends_pending: u64,
    pub deferred_sends_drained_at_snapshot: bool,
    pub leg_id: String,
    pub accepted_rtp: u64,
    pub pipeline_ingress: u64,
    /// Pre-protect/submit observation; this does not establish send completion.
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
    pub stages: PacketDiagnostics,
}

impl LegPacketSnapshot {
    pub(crate) fn capture(leg: &LegInner) -> Self {
        let tap = leg.stats();
        let stages = leg.pc().packet_diagnostics();
        let deferred_sends_pending = stages
            .transports
            .iter()
            .map(|transport| transport.conn.send_deferred_pending)
            .sum();
        Self {
            observed_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            snapshot_scope: "live",
            async_send_results_in_cdr_final: false,
            deferred_sends_pending,
            deferred_sends_drained_at_snapshot: deferred_sends_pending == 0,
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
            stages,
        }
    }

    fn counts(&self) -> [u64; 8] {
        let mut counts = [0; 8];
        for ice in std::iter::once(&self.stages.ice).chain(self.stages.media_ice.iter()) {
            counts[0] += ice.udp_read.packets;
            counts[1] += ice.turn_channel_data.packets + ice.turn_data_indication.packets;
        }
        counts[3] = self.accepted_rtp;
        counts[4] = self.pipeline_ingress;
        counts[5] = self.pipeline_egress;
        for t in &self.stages.transports {
            counts[2] += t.conn.received.packets;
            counts[6] += t.conn.send_attempts.packets;
            counts[7] += t.conn.send_sync_success.packets + t.conn.send_async_completed.packets;
        }
        counts
    }

    fn errors(&self) -> u64 {
        let ice_errors: u64 = std::iter::once(&self.stages.ice)
            .chain(self.stages.media_ice.iter())
            .map(|i| {
                i.udp_read_errors.packets
                    + i.udp_read_exits.packets
                    + i.turn_unknown_channel.packets
                    + i.turn_malformed.packets
            })
            .sum();
        ice_errors
            + self
                .stages
                .transports
                .iter()
                .map(|t| {
                    t.conn.send_errors.packets
                        + t.conn.send_queue_closed.packets
                        + t.conn.dtls_no_receiver.packets
                        + t.conn.rtp_no_receiver.packets
                        + t.srtp_parse_errors.packets
                        + t.srtp_unprotect_errors.packets
                        + t.srtp_no_session.packets
                        + t.srtcp_unprotect_errors.packets
                        + t.srtcp_no_session.packets
                        + t.rtcp_parse_errors.packets
                        + t.rtp_parse_errors.packets
                        + t.listener_full.packets
                        + t.listener_closed.packets
                        + t.rtcp_listener_closed.packets
                        + t.no_listener.packets
                        + t.srtp_protect_errors.packets
                        + t.srtp_send_no_session.packets
                        + t.bridge_dropped.packets
                })
                .sum::<u64>()
    }

    fn route(&self) -> Vec<String> {
        let mut route: Vec<_> = std::iter::once(&self.stages.ice)
            .chain(self.stages.media_ice.iter())
            .filter_map(|i| i.selected_pair.as_ref())
            .map(|p| {
                format!(
                    "{}:{}>{}:{}:{}",
                    p.local_type, p.local_addr, p.remote_type, p.remote_addr, p.transport
                )
            })
            .collect();
        route.extend(self.stages.transports.iter().map(|t| {
            format!(
                "{}>{}:srtp={}",
                t.local_addr, t.remote_addr, t.srtp_session_ready
            )
        }));
        route
    }
}

#[derive(Default)]
struct LateSendLogState {
    last_emit: Option<Instant>,
    saw_error: bool,
    // At most one value per connection with suppressed outcomes in this interval.
    // Entries are removed on flush; no per-packet history or media owner is held.
    pending: BTreeMap<u64, (Instant, u64, DeferredSendSnapshot)>,
    flush_scheduled: bool,
}

struct LateSendLogger {
    session_id: String,
    root_session_id: String,
    leg_id: String,
    interval: Duration,
    started: Instant,
    subscriber: tracing::Dispatch,
    state: parking_lot::Mutex<LateSendLogState>,
}

impl LateSendLogger {
    fn emit(&self, observed: Instant, observed_unix_ms: u64, result: &DeferredSendSnapshot) {
        tracing::dispatcher::with_default(&self.subscriber, || {
            tracing::info!(target: "rustpbx_media::packet_diagnostics",
                session_id = %self.session_id, root_session_id = %self.root_session_id,
                leg_id = %self.leg_id, direction = "pbx_to_peer", connection_id = result.connection_id,
                event = if result.pending == 0 { "deferred_drained_at_snapshot" } else { "deferred_result" },
                snapshot_scope = "after_leg_stopping", async_send_results_in_cdr_final = false,
                observed_unix_ms, snapshot_age_ms = observed.elapsed().as_millis() as u64,
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                deferred_sends_drained_at_snapshot = result.pending == 0,
                snapshot = %serde_json::to_string(result).unwrap_or_default(),
                "media.packet_send_tail");
        });
    }

    fn observe(self: &Arc<Self>, result: DeferredSendSnapshot) {
        let now = Instant::now();
        let observed_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let runtime = tokio::runtime::Handle::try_current().ok();
        let mut state = self.state.lock();
        let first_error = result.failed && !state.saw_error;
        state.saw_error |= result.failed;
        if runtime.is_none()
            || state.last_emit.is_none()
            || first_error
            || state
                .last_emit
                .is_some_and(|last| now.duration_since(last) >= self.interval)
        {
            state.pending.remove(&result.connection_id);
            self.emit(now, observed_unix_ms, &result);
            state.last_emit = Some(now);
        } else {
            state
                .pending
                .insert(result.connection_id, (now, observed_unix_ms, result));
            if !state.flush_scheduled {
                state.flush_scheduled = true;
                let delay = self
                    .interval
                    .saturating_sub(now.duration_since(state.last_emit.unwrap()));
                let logger = self.clone();
                // A single on-demand flush retains only IDs, subscriber and
                // value snapshots. It never retains a leg, PC or transport.
                runtime.unwrap().spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut state = logger.state.lock();
                    state.flush_scheduled = false;
                    let pending = std::mem::take(&mut state.pending);
                    if !pending.is_empty() {
                        for (_, (observed, observed_unix_ms, result)) in pending {
                            logger.emit(observed, observed_unix_ms, &result);
                        }
                        state.last_emit = Some(Instant::now());
                    }
                });
            }
        }
    }
}

fn late_send_observer(
    session_id: String,
    root_session_id: String,
    leg_id: String,
    interval: Duration,
) -> Arc<DeferredSendObserver> {
    let logger = Arc::new(LateSendLogger {
        session_id,
        root_session_id,
        leg_id,
        interval,
        started: Instant::now(),
        subscriber: tracing::dispatcher::get_default(Clone::clone),
        state: parking_lot::Mutex::new(LateSendLogState::default()),
    });
    Arc::new(DeferredSendObserver::new(move |result| {
        logger.observe(result)
    }))
}

pub(crate) struct LegPacketDiagnostics {
    session_id: String,
    root_session_id: String,
    interval: Duration,
    started: Instant,
    last_emit: Instant,
    sampled: [u64; 8],
    emitted: Option<[u64; 8]>,
    growth: [Option<Instant>; 8],
    idle: [bool; 8],
    route: Vec<String>,
    last_route_event: Option<Instant>,
    route_changes: u64,
    errors: u64,
    error_burst: bool,
    last_error: Option<Instant>,
    transitions: u64,
    latest: Option<LegPacketSnapshot>,
    pub(crate) task: Option<tokio::task::JoinHandle<()>>,
    pub(crate) bridge: Weak<BridgeLegs>,
    stopped: bool,
    late_sends: Arc<DeferredSendObserver>,
}

impl LegPacketDiagnostics {
    pub(crate) fn new(
        session_id: String,
        root_session_id: String,
        leg_id: String,
        interval: Duration,
    ) -> Self {
        let now = Instant::now();
        let late_sends = late_send_observer(
            session_id.clone(),
            root_session_id.clone(),
            leg_id,
            interval,
        );
        Self {
            session_id,
            root_session_id,
            interval,
            started: now,
            last_emit: now,
            sampled: [0; 8],
            emitted: None,
            growth: [None; 8],
            idle: [false; 8],
            route: Vec::new(),
            last_route_event: None,
            route_changes: 0,
            errors: 0,
            error_burst: false,
            last_error: None,
            transitions: 0,
            latest: None,
            task: None,
            bridge: Weak::new(),
            stopped: false,
            late_sends,
        }
    }

    pub(crate) fn sample(&mut self, leg: &LegInner, final_sample: bool) {
        if self.stopped {
            return;
        }
        let now = Instant::now();
        if final_sample {
            self.late_sends.mark_stopping();
        }
        let mut snapshot = LegPacketSnapshot::capture(leg);
        if final_sample {
            snapshot.snapshot_scope = "leg_stopping";
        }
        let counts = snapshot.counts();
        let errors = snapshot.errors();
        let route = snapshot.route();
        let initial = self.emitted.is_none();
        // Hold/handshake/IVR silence is context, never a packet-failure conclusion.
        let observe_idle = !snapshot.gated
            && !snapshot.app_paused
            && snapshot
                .transceiver_directions
                .iter()
                .any(|d| d == "SendRecv" || d == "RecvOnly");
        let mut changes = Vec::new();
        for i in 0..STAGES.len() {
            if counts[i] < self.sampled[i] {
                self.growth[i] = None;
                self.idle[i] = false;
                changes.push(format!("{}:counter_reset", STAGES[i]));
            } else if counts[i] > self.sampled[i] {
                self.growth[i] = Some(now);
                if self.idle[i] {
                    changes.push(format!("{}:resumed", STAGES[i]));
                }
                self.idle[i] = false;
            } else if observe_idle
                && !final_sample
                && !self.idle[i]
                && self.growth[i].is_some_and(|t| now.duration_since(t) >= IDLE_EVIDENCE_AFTER)
            {
                self.idle[i] = true;
                changes.push(format!("{}:no_growth", STAGES[i]));
            }
            // A pause does not preserve a pending alarm countdown across resume.
            if !observe_idle {
                self.growth[i] = None;
                self.idle[i] = false;
            }
        }
        let new_errors = errors > self.errors;
        if new_errors {
            self.last_error = Some(now);
            if !self.error_burst {
                changes.push("stage_error:first_in_burst".into());
            }
            self.error_burst = true;
        } else if self
            .last_error
            .is_some_and(|t| now.duration_since(t) >= self.interval)
        {
            self.error_burst = false;
        }
        if !initial && route != self.route {
            self.route_changes += 1;
            if self
                .last_route_event
                .is_none_or(|t| now.duration_since(t) >= self.interval)
            {
                changes.push("selected_route_or_srtp_changed".into());
                self.last_route_event = Some(now);
            }
        }
        self.transitions += changes.len() as u64;
        self.sampled = counts;
        self.route = route;
        self.errors = errors;
        let transition = !changes.is_empty();
        if initial
            || final_sample
            || transition
            || now.duration_since(self.last_emit) >= self.interval
        {
            let event = if final_sample {
                "final"
            } else if initial {
                "initial"
            } else if transition {
                "transition"
            } else {
                "periodic"
            };
            let deltas: Vec<Option<u64>> = (0..STAGES.len())
                .map(|i| {
                    self.emitted
                        .and_then(|previous| counts[i].checked_sub(previous[i]))
                })
                .collect();
            // Only anomalous/route-change samples expand the peer leg; normal logs
            // remain one bounded snapshot per leg per configured interval.
            let peers: Vec<_> = if transition && !final_sample {
                self.bridge
                    .upgrade()
                    .map(|bridge| {
                        let (a, b) = bridge.lock().clone();
                        [a, b]
                            .into_iter()
                            .flatten()
                            .filter(|peer| peer.id() != leg.id())
                            .map(|peer| LegPacketSnapshot::capture(&peer))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            tracing::info!(target: "rustpbx_media::packet_diagnostics",
                session_id = %self.session_id, root_session_id = %self.root_session_id,
                leg_id = %leg.id(), direction = "peer_to_pbx_and_pbx_to_peer", event,
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                window_ms = now.duration_since(self.last_emit).as_millis() as u64,
                stage_order = ?STAGES, delta = ?deltas, changes = ?changes,
                transition_count = self.transitions, route_change_count = self.route_changes, error_observations = errors,
                interpretation = "stage_observation_only_dtx_or_hold_may_be_silent",
                snapshot = %serde_json::to_string(&snapshot).unwrap_or_default(),
                peer_snapshots = %serde_json::to_string(&peers).unwrap_or_default(),
                "media.packet_stages");
            self.last_emit = now;
            self.emitted = Some(counts);
        }
        self.latest = Some(snapshot);
        if final_sample {
            self.stopped = true;
        }
    }

    pub(crate) fn snapshot(&self, leg: &LegInner) -> Option<LegPacketSnapshot> {
        if self.stopped {
            self.latest.clone()
        } else {
            Some(LegPacketSnapshot::capture(leg))
        }
    }
}

impl LegInner {
    pub(crate) fn start_packet_diagnostics(leg: &Leg, cfg: &crate::leg::LegConfig) {
        let Some(session_id) = cfg.call_id.clone().filter(|id| !id.is_empty()) else {
            return;
        };
        if !cfg.quality_stats.enabled {
            return;
        }
        let mut diagnostics = LegPacketDiagnostics::new(
            session_id.clone(),
            cfg.root_session_id.clone().unwrap_or(session_id),
            leg.id().to_string(),
            Duration::from_millis(cfg.quality_stats.log_interval_ms.max(2000)),
        );
        leg.pc()
            .set_deferred_send_observer(diagnostics.late_sends.clone());
        diagnostics.sample(leg, false);
        let weak = Arc::downgrade(leg);
        let interval = Duration::from_millis(cfg.quality_stats.sample_interval_ms.max(250));
        diagnostics.task = Some(tokio::spawn(
            async move {
                let mut ticks = tokio::time::interval(interval);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ticks.tick().await;
                loop {
                    ticks.tick().await;
                    let Some(leg) = weak.upgrade() else {
                        break;
                    };
                    if let Some(state) = leg.packet_diagnostics.lock().as_mut() {
                        state.sample(&leg, false);
                    }
                }
            }
            .with_current_subscriber(),
        ));
        *leg.packet_diagnostics.lock() = Some(diagnostics);
    }

    pub(crate) fn set_packet_diagnostic_bridge(&self, bridge: Weak<BridgeLegs>) {
        if let Some(state) = self.packet_diagnostics.lock().as_mut() {
            state.bridge = bridge;
        }
    }

    pub fn packet_diagnostic_snapshot(&self) -> Option<LegPacketSnapshot> {
        self.packet_diagnostics
            .lock()
            .as_ref()
            .and_then(|state| state.snapshot(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct TailLogs(Arc<parking_lot::Mutex<Vec<BTreeMap<String, String>>>>);
    impl Subscriber for TailLogs {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            if event.metadata().level() != &tracing::Level::INFO {
                return;
            }
            struct Fields(BTreeMap<String, String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.insert(field.name().into(), format!("{value:?}"));
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.insert(field.name().into(), value.to_string());
                }
            }
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            if fields
                .0
                .get("message")
                .is_some_and(|v| v.contains("media.packet_send_tail"))
            {
                self.0.lock().push(fields.0);
            }
        }
    }

    // Actual TCP writes are blocked at the real write-half mutex. No send is
    // mocked: shutdown produces a real write error, release permits actual I/O.
    // This catches disappearing late outcomes, missing IDs, per-packet tail
    // floods, and an observer accidentally retaining the stopped media leg.
    #[tokio::test(flavor = "current_thread")]
    async fn packet_diagnostics_late_deferred_results_are_correlated_and_bounded() {
        use rustrtc::transports::ice::{IceSocketWrapper, conn::IceConn};
        use rustrtc::transports::rtp::RtpTransport;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let logs = TailLogs::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        for fail in [false, true] {
            let suffix = if fail { "error" } else { "success" };
            let mut cfg = crate::leg::LegConfig::rtp_pcmu();
            cfg.call_id = Some(format!("late-node-full-{suffix}"));
            cfg.root_session_id = Some(format!("late-root-full-{suffix}"));
            cfg.quality_stats.enabled = true;
            cfg.quality_stats.log_interval_ms = 2000;
            let leg = LegInner::new(format!("late-leg-{suffix}"), &cfg, None).unwrap();
            let observer = leg
                .packet_diagnostics
                .lock()
                .as_ref()
                .unwrap()
                .late_sends
                .clone();
            let weak = Arc::downgrade(&leg);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (server, peer) = listener.accept().await.unwrap();
            let (read, write) = server.into_split();
            let write = Arc::new(tokio::sync::Mutex::new(write));
            let blocked = write.lock().await;
            let wrapper = IceSocketWrapper::TcpStream(
                Arc::new(tokio::sync::Mutex::new(read)),
                write.clone(),
                peer,
            );
            let (_tx, rx) = tokio::sync::watch::channel(Some(wrapper));
            let conn = IceConn::new(rx, peer, None);
            conn.set_deferred_send_observer(observer);
            let transport = RtpTransport::new(conn.clone(), false);
            let packets = 16;
            let queued = 1;
            assert_eq!(conn.try_send(b"tail").unwrap(), 4);
            tokio::task::yield_now().await;
            assert_eq!(
                transport.packet_diagnostics().conn.send_deferred_pending,
                queued
            );
            assert!(
                logs.0
                    .lock()
                    .iter()
                    .all(|row| row["session_id"] != format!("late-node-full-{suffix}"))
            );
            // This must return without waiting for the intentionally blocked write.
            leg.stop();
            let final_snapshot = leg.packet_diagnostic_snapshot().unwrap();
            assert_eq!(final_snapshot.snapshot_scope, "leg_stopping");
            assert!(!final_snapshot.async_send_results_in_cdr_final);
            drop(leg);
            assert!(
                weak.upgrade().is_none(),
                "late observer must not retain Leg"
            );
            drop(blocked);
            let mut frame = [0; 6];
            tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut frame))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&frame, b"\0\x04tail");
            tokio::task::yield_now().await;
            if fail {
                let mut blocked = write.lock().await;
                blocked.shutdown().await.unwrap();
                for _ in 0..packets {
                    conn.try_send(b"tail").unwrap();
                }
                drop(blocked);
            } else {
                for _ in 1..packets {
                    conn.try_send(b"tail").unwrap();
                    tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut frame))
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(&frame, b"\0\x04tail");
                    tokio::task::yield_now().await;
                }
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                while transport.packet_diagnostics().conn.send_deferred_pending != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if fail {
                // A success just emitted: the first real failure must bypass
                // that interval, rather than waiting for the pending flush.
                assert!(logs.0.lock().iter().any(|row| row["session_id"] == format!("late-node-full-{suffix}")
                    && serde_json::from_str::<serde_json::Value>(&row["snapshot"]).unwrap()["failed"] == true));
            }
            let weak_conn = Arc::downgrade(&conn);
            drop(transport);
            drop(conn);
            assert!(
                weak_conn.upgrade().is_none(),
                "observer must not retain IceConn"
            );
            tokio::time::sleep(Duration::from_millis(2150)).await;
            let rows: Vec<_> = logs
                .0
                .lock()
                .iter()
                .filter(|row| row["session_id"] == format!("late-node-full-{suffix}"))
                .cloned()
                .collect();
            assert!(!rows.is_empty(), "late result vanished");
            assert!(
                rows.len() <= 3,
                "tail logging must merge a burst, got {}",
                rows.len()
            );
            for row in &rows {
                assert_eq!(row["root_session_id"], format!("late-root-full-{suffix}"));
                assert_eq!(row["leg_id"], format!("late-leg-{suffix}"));
                assert_eq!(row["direction"], "pbx_to_peer");
                assert_eq!(row["snapshot_scope"], "after_leg_stopping");
            }
            let last: serde_json::Value =
                serde_json::from_str(&rows.last().unwrap()["snapshot"]).unwrap();
            assert_eq!(last["pending"], 0);
            assert_eq!(last["completed"], if fail { 1 } else { packets });
            assert_eq!(last["errors"], if fail { packets } else { 0 });
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn packet_diagnostics_late_multiple_connections_keep_each_final_result() {
        use rustrtc::transports::ice::{IceSocketWrapper, conn::IceConn};
        use tokio::io::AsyncReadExt;
        let logs = TailLogs::default();
        let _subscriber = tracing::subscriber::set_default(logs.clone());
        let mut cfg = crate::leg::LegConfig::rtp_pcmu();
        cfg.call_id = Some("multiple-node-full".into());
        cfg.root_session_id = Some("multiple-root-full".into());
        cfg.quality_stats.enabled = true;
        cfg.quality_stats.log_interval_ms = 2000;
        let leg = LegInner::new("multiple-leg", &cfg, None).unwrap();
        let observer = leg
            .packet_diagnostics
            .lock()
            .as_ref()
            .unwrap()
            .late_sends
            .clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut connections = Vec::new();
        for _ in 0..2 {
            let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (server, peer) = listener.accept().await.unwrap();
            let (read, write) = server.into_split();
            let wrapper = IceSocketWrapper::TcpStream(
                Arc::new(tokio::sync::Mutex::new(read)),
                Arc::new(tokio::sync::Mutex::new(write)),
                peer,
            );
            let (tx, rx) = tokio::sync::watch::channel(Some(wrapper));
            let conn = IceConn::new(rx, peer, None);
            conn.set_deferred_send_observer(observer.clone());
            connections.push((client, conn, peer, tx));
        }
        leg.stop();
        drop(leg);
        for index in [0, 0, 1] {
            let (client, conn, _, _) = &mut connections[index];
            conn.try_send(b"tail").unwrap();
            let mut frame = [0; 6];
            tokio::time::timeout(Duration::from_secs(1), client.read_exact(&mut frame))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&frame, b"\0\x04tail");
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(2150)).await;
        let rows = logs.0.lock();
        let ids: std::collections::BTreeSet<u64> = rows
            .iter()
            .map(|row| {
                let value: serde_json::Value = serde_json::from_str(&row["snapshot"]).unwrap();
                let id = value["connection_id"].as_u64().unwrap();
                assert_ne!(id, 0);
                assert_eq!(row["connection_id"], id.to_string());
                id
            })
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "each real connection needs its own stable owner ID"
        );
        for (index, (_, _, peer, _)) in connections.iter().enumerate() {
            let expected = if index == 0 { 2 } else { 1 };
            assert!(
                rows.iter().any(|row| {
                    let value: serde_json::Value = serde_json::from_str(&row["snapshot"]).unwrap();
                    row["session_id"] == "multiple-node-full"
                        && row["root_session_id"] == "multiple-root-full"
                        && row["leg_id"] == "multiple-leg"
                        && value["remote_addr"] == peer.to_string()
                        && value["completed"] == expected
                        && value["pending"] == 0
                }),
                "connection {index} final result was overwritten: {rows:?}"
            );
        }
    }
}
