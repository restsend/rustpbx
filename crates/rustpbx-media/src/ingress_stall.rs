//! Per-leg inbound-RTP stall watcher.
//!
//! The bridge quality sampler (and the health monitor built on it) only
//! exists once two legs are bridged, and its `stalled` flag means "route
//! active and the leg NEVER received RTP" — an initial black-hole detector.
//! Neither covers the failure shape seen in production: a caller leg that
//! receives media through the IVR/queue phase and then goes silent
//! (upstream gateway stops sending, VAD suppression, path loss) before the
//! bridge ever forms — invisible until the far-end client's own watchdog
//! hangs the call ~70s later.
//!
//! This watcher is owned by the [`Leg`](crate::leg::Leg) itself, covers
//! every phase of the leg's life (pre-bridge included), and is strictly
//! observational: it logs and publishes an event; it never tears a call
//! down. Detection is edge-triggered: one WARN per silence episode, one
//! resume line, and a per-leg episode cap so a flapping path cannot spam.
//!
//! Conditions are disjoint from the bridge monitor's `proxy.media_stalled`
//! (which requires lifetime RX == 0): this watcher only arms after the leg
//! HAS received media, so the two can never double-report the same fact.

use crate::ingress_tap::IngressTap;
use crate::leg_stats::LegRtcpStats;
use rustrtc::PeerConnection;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Default silence window before the watcher reports (`ingress_stall_warn_secs`).
/// Chosen so a pre-bridge stall is reported long before any client-side
/// watchdog acts, while staying above normal VAD/comfort-noise gaps.
pub const DEFAULT_INGRESS_STALL_WARN_SECS: u64 = 10;
/// Upper bound on reported episodes per leg. A flapping path (receive 1s /
/// silent 10s forever) must not produce unbounded log volume; the CDR edge
/// is further limited to once per leg by the session-side forwarder.
const MAX_EPISODES: u32 = 10;
/// Poll cadence. Silence-window precision is ±1 tick, far below the
/// default threshold.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// One reported silence episode (or its recovery) on a leg that had media.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IngressStallEvent {
    pub leg_id: String,
    /// `stalled` = media stopped after flowing; `resumed` = packets seen again.
    pub kind: IngressStallKind,
    /// How long the accepted-RTP counter had been frozen when the edge fired.
    pub silent_secs: u64,
    /// Accepted (post-validation) inbound RTP over the leg's whole life.
    pub lifetime_accepted: u64,
    /// Tap-level ingress counter — equal to `lifetime_accepted` when the
    /// pipeline is healthy; a divergence would indicate internal drops.
    pub tap_ingress: u64,
    /// Most recent ingress packet evidence (from the tap's sample ring).
    pub last_seq: Option<u16>,
    pub last_peer: Option<String>,
    /// Age of the last RTCP report processed from the remote. Advancing
    /// while `silent_secs` grows means the peer is ALIVE and deliberately
    /// suppressing RTP (VAD/silence-suppression signature); frozen means
    /// the whole remote stream went quiet.
    pub rtcp_age_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg(target_os = "linux")]
    pub kernel_udp: Option<KernelUdpSnapshot>,
    /// 1-based episode counter for this leg.
    pub episode: u32,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IngressStallKind {
    Stalled,
    Resumed,
}

/// System-wide UDP stack counters (`/proc/net/net/snmp`), captured at the
/// stall edge. `RcvbufErrors` growing between episodes is the signature of
/// a socket nobody is reading (receive-loop stall) rather than an upstream
/// that stopped sending.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct KernelUdpSnapshot {
    pub in_datagrams: u64,
    pub rcvbuf_errors: u64,
}

/// The observable decision for one poll tick. Facts are attached by the
/// caller only on [`IngressWatchDecision::Stalled`] /
/// [`IngressWatchDecision::Resumed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IngressWatchDecision {
    Quiet,
    Stalled { silent_for: Duration },
    Resumed { silent_for: Duration },
}

/// Pure edge-detection state for one leg. Time and counters are injected,
/// so the whole matrix is testable without a runtime.
#[derive(Debug)]
pub(crate) struct IngressWatchState {
    warn_after: Duration,
    max_episodes: u32,
    /// True once an advance of the accepted counter was observed — the leg
    /// demonstrably received media while being watched.
    armed: bool,
    last_count: u64,
    last_advance_at: Option<Instant>,
    warned: bool,
    episodes: u32,
}

impl IngressWatchState {
    pub(crate) fn new(warn_after: Duration) -> Self {
        Self {
            warn_after,
            max_episodes: MAX_EPISODES,
            armed: false,
            last_count: 0,
            last_advance_at: None,
            warned: false,
            episodes: 0,
        }
    }

    pub(crate) fn episodes(&self) -> u32 {
        self.episodes
    }

    /// Feed one poll sample.
    ///
    /// `accepted` is the leg's cumulative accepted-RTP counter.
    /// `suppressed` mirrors the RTP-timeout monitor's pause conditions
    /// (hold / transfer-in-progress / app-driven leg) — periods where
    /// inbound silence is expected, so the baseline is re-synced and no
    /// edge can fire across them.
    pub(crate) fn observe(
        &mut self,
        now: Instant,
        accepted: u64,
        suppressed: bool,
    ) -> IngressWatchDecision {
        if suppressed {
            // Expected silence: follow the counter and forget any open
            // episode. Deliberately NOT emitting a Resumed edge — hold and
            // friends are routine, not diagnostics. An advance observed
            // inside the suppressed window still ARMS the watcher: the leg
            // demonstrably had media, so a post-unhold freeze is a real
            // stall even if every flow tick happened while held.
            if accepted != self.last_count {
                self.armed = true;
            }
            self.last_count = accepted;
            self.last_advance_at = Some(now);
            self.warned = false;
            return IngressWatchDecision::Quiet;
        }

        if accepted != self.last_count {
            let was_warned = self.warned;
            let silent_for = if was_warned {
                self.last_advance_at
                    .map(|at| now.duration_since(at))
                    .unwrap_or_default()
            } else {
                Duration::ZERO
            };
            self.last_count = accepted;
            self.last_advance_at = Some(now);
            if !self.armed {
                // First observed advance arms the watcher: from here on a
                // frozen counter is a real stall, not "no media yet".
                self.armed = true;
            }
            self.warned = false;
            if was_warned {
                return IngressWatchDecision::Resumed { silent_for };
            }
            return IngressWatchDecision::Quiet;
        }

        if !self.armed || self.warned || self.episodes >= self.max_episodes {
            return IngressWatchDecision::Quiet;
        }
        let Some(since) = self.last_advance_at else {
            return IngressWatchDecision::Quiet;
        };
        let silent_for = now.duration_since(since);
        if silent_for >= self.warn_after {
            self.warned = true;
            self.episodes += 1;
            return IngressWatchDecision::Stalled { silent_for };
        }
        IngressWatchDecision::Quiet
    }

}

/// Read the system-wide UDP counters on the stall edge (Linux only). These
/// are absolute netns-wide values — comparing them across episodes is what
/// separates "kernel dropped it" from "it never arrived".
#[cfg(target_os = "linux")]
fn read_kernel_udp() -> Option<KernelUdpSnapshot> {
    let raw = std::fs::read_to_string("/proc/net/snmp").ok()?;
    let mut lines = raw.lines().filter(|l| l.starts_with("Udp:"));
    let headers = lines.next()?;
    let values = lines.next()?;
    let names: Vec<&str> = headers["Udp:".len()..].split_whitespace().collect();
    let nums: Vec<u64> = values["Udp:".len()..]
        .split_whitespace()
        .filter_map(|v| v.parse().ok())
        .collect();
    let idx = |key: &str| names.iter().position(|n| *n == key);
    Some(KernelUdpSnapshot {
        in_datagrams: idx("InDatagrams").and_then(|i| nums.get(i).copied())?,
        rcvbuf_errors: idx("RcvbufErrors").and_then(|i| nums.get(i).copied())?,
    })
}

/// Spawn the per-leg watcher. Holds only narrow handles (PC, tap, RTCP
/// stats, timeout flags) — never the [`Leg`](crate::leg::Leg) itself, so
/// leg teardown is never delayed by this task. Exits when `cancel` fires
/// (leg `stop()`) or the event channel's last subscriber disappears makes
/// sends harmless anyway; the channel simply loses its receivers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_ingress_watch(
    leg_id: String,
    pc: PeerConnection,
    tap: std::sync::Arc<IngressTap>,
    rtcp: std::sync::Arc<LegRtcpStats>,
    event_tx: broadcast::Sender<IngressStallEvent>,
    warn_after: Duration,
    cancel: CancellationToken,
    rtp_timeout: std::sync::Arc<crate::leg::RtpTimeoutState>,
    quiet_expected: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = IngressWatchState::new(warn_after);
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let now = Instant::now();
                    // Expected-silence windows: the session flags them
                    // explicitly — `quiet_expected` while the leg is on
                    // hold (sendonly renegotiation → the peer legitimately
                    // stops sending), `app_paused` while an app drives the
                    // call or a transfer is in flight. NOTE: the RTP
                    // timeout's own `active` flag is deliberately NOT part
                    // of this check — an un-armed timeout is the NORMAL
                    // call state, and must never suppress stall detection.
                    let suppressed =
                        quiet_expected.load(std::sync::atomic::Ordering::Relaxed)
                            || rtp_timeout.app_paused.load(std::sync::atomic::Ordering::Relaxed);
                    let accepted = pc.received_rtp_packets();
                    debug!(
                        leg_id = %leg_id,
                        suppressed,
                        accepted,
                        quiet_expected = quiet_expected.load(std::sync::atomic::Ordering::Relaxed),
                        "ingress watch tick"
                    );
                    match state.observe(now, accepted, suppressed) {
                        IngressWatchDecision::Quiet => {}
                        IngressWatchDecision::Stalled { silent_for } => {
                            let tap_stats = tap.stats();
                            let rtcp_age_secs = rtcp.last_report_age_secs();
                            let (last_seq, last_peer) = tap_stats
                                .audio_packet_samples
                                .as_ref()
                                .and_then(|s| s.last_ingress())
                                .map(|p| (Some(p.sequence), Some(p.peer_addr.to_string())))
                                .unwrap_or((None, None));
                            #[cfg(target_os = "linux")]
                            let kernel_udp = read_kernel_udp();
                            let episode = state.episodes();
                            let event = IngressStallEvent {
                                leg_id: leg_id.clone(),
                                kind: IngressStallKind::Stalled,
                                silent_secs: silent_for.as_secs(),
                                lifetime_accepted: accepted,
                                tap_ingress: tap_stats.ingress_packets,
                                last_seq,
                                last_peer,
                                rtcp_age_secs,
                                #[cfg(target_os = "linux")]
                                kernel_udp,
                                episode,
                            };
                            warn!(
                                leg_id = %leg_id,
                                silent_secs = event.silent_secs,
                                lifetime_accepted = event.lifetime_accepted,
                                tap_ingress = event.tap_ingress,
                                last_seq = ?event.last_seq,
                                last_peer = ?event.last_peer,
                                rtcp_age_secs = ?event.rtcp_age_secs,
                                episode = event.episode,
                                "leg ingress stalled: no new inbound RTP after receiving media \
                                 (lifetime_accepted == tap_ingress means the pipeline consumed \
                                 every wire packet; advancing rtcp_age with frozen RTP = the \
                                 peer is alive and suppressing media)"
                            );
                            let _ = event_tx.send(event);
                        }
                        IngressWatchDecision::Resumed { silent_for } => {
                            info!(
                                leg_id = %leg_id,
                                silent_for_secs = silent_for.as_secs(),
                                "leg ingress resumed after stall"
                            );
                            let event = IngressStallEvent {
                                leg_id: leg_id.clone(),
                                kind: IngressStallKind::Resumed,
                                silent_secs: silent_for.as_secs(),
                                lifetime_accepted: accepted,
                                tap_ingress: tap.stats().ingress_packets,
                                last_seq: None,
                                last_peer: None,
                                rtcp_age_secs: rtcp.last_report_age_secs(),
                                #[cfg(target_os = "linux")]
                                kernel_udp: None,
                                episode: state.episodes(),
                            };
                            let _ = event_tx.send(event);
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn t(secs: u64) -> Instant {
        base() + Duration::from_secs(secs)
    }

    fn state() -> IngressWatchState {
        IngressWatchState::new(Duration::from_secs(10))
    }

    #[test]
    fn never_receiving_leg_stays_quiet_forever() {
        let mut s = state();
        // Transport comes up at t=1; no media ever arrives.
        for sec in 1..=200u64 {
            assert_eq!(
                s.observe(t(sec), 0, false),
                IngressWatchDecision::Quiet,
                "tick {sec}"
            );
        }
    }

    #[test]
    fn first_advance_arms_and_threshold_fires_once_per_episode() {
        let mut s = state();
        // Media flows t=1..5 (50pps → +50/tick), then stops.
        for sec in 1..=5u64 {
            assert_eq!(s.observe(t(sec), sec * 50, false), IngressWatchDecision::Quiet);
        }
        // Frozen from t=5 to t=14 (9s silent) — below the 10s threshold.
        for sec in 6..=14u64 {
            assert_eq!(s.observe(t(sec), 250, false), IngressWatchDecision::Quiet, "tick {sec}");
        }
        // t=15: exactly 10s frozen → the edge fires once.
        assert_eq!(
            s.observe(t(15), 250, false),
            IngressWatchDecision::Stalled { silent_for: Duration::from_secs(10) }
        );
        // Held silence afterwards stays silent (single edge per episode).
        for sec in 16..=30u64 {
            assert_eq!(s.observe(t(sec), 250, false), IngressWatchDecision::Quiet, "tick {sec}");
        }
        assert_eq!(s.episodes(), 1);
    }

    #[test]
    fn resume_after_stall_emits_one_resume_edge_and_re_arms() {
        let mut s = state();
        for sec in 1..=5u64 {
            let _ = s.observe(t(sec), sec * 50, false);
        }
        assert_eq!(
            s.observe(t(16), 250, false),
            IngressWatchDecision::Stalled { silent_for: Duration::from_secs(11) }
        );
        // Media returns at t=20 (11s of silence since t=5+... last advance t=5).
        assert_eq!(
            s.observe(t(20), 300, false),
            IngressWatchDecision::Resumed { silent_for: Duration::from_secs(15) }
        );
        // A second episode after another stall is counted and reported
        // (silence from t=20; the 10s window completes at t=30).
        for sec in 21..=29u64 {
            let _ = s.observe(t(sec), 300, false);
        }
        assert_eq!(
            s.observe(t(30), 300, false),
            IngressWatchDecision::Stalled { silent_for: Duration::from_secs(10) }
        );
        assert_eq!(s.episodes(), 2);
    }

    #[test]
    fn suppression_follows_baseline_and_never_fires_across_hold() {
        let mut s = state();
        for sec in 1..=5u64 {
            let _ = s.observe(t(sec), sec * 50, false);
        }
        // Hold: 60s of expected silence — baseline follows, no edge, and the
        // post-hold countdown restarts cleanly.
        for sec in 6..=65u64 {
            assert_eq!(s.observe(t(sec), 250, true), IngressWatchDecision::Quiet);
        }
        // Unhold at t=66, still no media: fresh 10s countdown from t=66.
        for sec in 66..=74u64 {
            assert_eq!(s.observe(t(sec), 250, false), IngressWatchDecision::Quiet, "tick {sec}");
        }
        assert_eq!(s.observe(t(75), 250, false), IngressWatchDecision::Stalled { silent_for: Duration::from_secs(10) });
    }

    #[test]
    fn suppression_mid_episode_cancels_the_pending_edge() {
        let mut s = state();
        for sec in 1..=5u64 {
            let _ = s.observe(t(sec), sec * 50, false);
        }
        // 8s frozen, then hold begins before the threshold.
        for sec in 6..=13u64 {
            let _ = s.observe(t(sec), 250, false);
        }
        for sec in 14..=40u64 {
            assert_eq!(s.observe(t(sec), 250, true), IngressWatchDecision::Quiet);
        }
        // Unhold: countdown restarts — no inherited edge from the pre-hold window.
        for sec in 41..=49u64 {
            assert_eq!(s.observe(t(sec), 250, false), IngressWatchDecision::Quiet, "tick {sec}");
        }
        assert_eq!(s.observe(t(50), 250, false), IngressWatchDecision::Stalled { silent_for: Duration::from_secs(10) });
        assert_eq!(s.episodes(), 1);
    }

    #[test]
    fn episode_cap_stops_reporting_but_state_keeps_counting() {
        let mut s = IngressWatchState::new(Duration::from_secs(1));
        // 11 stall/resume cycles; only the first 10 are reported.
        let mut reported = 0;
        let mut resumed = 0;
        let mut counter = 0u64;
        let mut at = t(1);
        for _ in 0..11 {
            // 1s of media (advances the counter; every episode after the
            // first also closes with a Resumed edge), then 1s+ of silence.
            counter += 50;
            match s.observe(at, counter, false) {
                IngressWatchDecision::Quiet => {}
                IngressWatchDecision::Resumed { .. } => resumed += 1,
                IngressWatchDecision::Stalled { .. } => panic!("media advance must not stall"),
            }
            at += Duration::from_secs(1);
            if s.observe(at, counter, false) != IngressWatchDecision::Quiet {
                reported += 1;
            }
            at += Duration::from_secs(2);
        }
        assert_eq!(reported, 10, "episode cap bounds reporting");
        assert_eq!(resumed, 10, "each cap-bounded stall closes with a resume");
        // The counter clamps at the cap (it counts REPORTED episodes).
        assert_eq!(s.episodes(), 10);
    }

    #[test]
    fn late_first_media_within_grace_still_arms() {
        let mut s = state();
        // Transport exists from t=0; first media only at t=50 (under the 60s
        // arm grace) — the advance arms normally.
        assert_eq!(s.observe(t(50), 10, false), IngressWatchDecision::Quiet);
        for sec in 51..=59u64 {
            let _ = s.observe(t(sec), 10, false);
        }
        assert_eq!(
            s.observe(t(60), 10, false),
            IngressWatchDecision::Stalled { silent_for: Duration::from_secs(10) }
        );
    }

    #[test]
    fn pause_before_first_media_never_arms() {
        let mut s = state();
        for sec in 1..=30u64 {
            assert_eq!(s.observe(t(sec), 0, true), IngressWatchDecision::Quiet);
        }
        // Media after unpause arms normally.
        assert_eq!(s.observe(t(31), 10, false), IngressWatchDecision::Quiet);
    }
}
