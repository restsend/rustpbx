//! Complete interval statistics for observed transport counters and RTCP reports.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, try_from = "RawConfig")]
pub struct QualityStatsConfig {
    pub enabled: bool,
    pub sample_interval_ms: u64,
    /// Lookback for live health/volume queries; unpublished log statistics never expire.
    pub window_ms: u64,
    /// Periodic log cadence. Each emission consumes all observations since the previous emission.
    pub log_interval_ms: u64,
}

#[derive(Deserialize)]
#[serde(default)]
struct RawConfig {
    enabled: bool,
    sample_interval_ms: u64,
    window_ms: u64,
    log_interval_ms: u64,
}

impl Default for QualityStatsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_interval_ms: 5_000,
            window_ms: 10_000,
            log_interval_ms: 10_000,
        }
    }
}

impl Default for RawConfig {
    fn default() -> Self {
        let config = QualityStatsConfig::default();
        Self {
            enabled: config.enabled,
            sample_interval_ms: config.sample_interval_ms,
            window_ms: config.window_ms,
            log_interval_ms: config.log_interval_ms,
        }
    }
}

impl TryFrom<RawConfig> for QualityStatsConfig {
    type Error = &'static str;
    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        if !(250..=10_000).contains(&raw.sample_interval_ms)
            || !(raw.sample_interval_ms..=60_000).contains(&raw.window_ms)
            || !(2_000..=60_000).contains(&raw.log_interval_ms)
        {
            return Err(
                "quality stats require sample 250..10000ms, sample<=window<=60000ms, log 2000..60000ms",
            );
        }
        Ok(Self {
            enabled: raw.enabled,
            sample_interval_ms: raw.sample_interval_ms,
            window_ms: raw.window_ms,
            log_interval_ms: raw.log_interval_ms,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Range {
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub sample_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ReportSummary {
    pub coalesced_reports: u64,
    pub side: &'static str,
    pub direction: &'static str,
    pub jitter_ms: Option<Range>,
    pub rtt_ms: Option<Range>,
    pub loss_percent: Option<Range>,
}

#[derive(Debug, Serialize)]
pub struct WindowSummary {
    pub window_ms: u64,
    pub observed_ms: u64,
    pub rx_packets_per_second: Option<Range>,
    pub tx_packets_per_second: Option<Range>,
    pub recording_queue_drops_per_second: Option<Range>,
    pub partial_samples: usize,
    pub sampling_gaps: usize,
    pub counter_resets: usize,
    pub reports: [ReportSummary; 2],
}

#[derive(Default)]
struct Accumulator {
    min: f64,
    max: f64,
    sum: f64,
    weight: f64,
    count: usize,
}

impl Accumulator {
    fn observe(&mut self, value: f64, weight: f64) {
        if !value.is_finite() || weight <= 0.0 {
            return;
        }
        if self.count == 0 {
            self.min = value;
            self.max = value;
        }
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value * weight;
        self.weight += weight;
        self.count += 1;
    }

    fn take(&mut self) -> Option<Range> {
        let value = std::mem::take(self);
        (value.count > 0).then(|| Range {
            min: value.min,
            max: value.max,
            avg: value.sum / value.weight,
            sample_count: value.count,
        })
    }
}

pub(crate) struct QualityWindow {
    sample_interval: Duration,
    start: Option<Instant>,
    observed: Duration,
    rates: [Accumulator; 3],
    reports: [[Accumulator; 3]; 2],
    coalesced: [u64; 2],
    sampling_gaps: usize,
    counter_resets: usize,
}

impl QualityWindow {
    pub(crate) fn new(sample_interval: Duration) -> Self {
        Self {
            sample_interval,
            start: None,
            observed: Duration::ZERO,
            rates: Default::default(),
            reports: Default::default(),
            coalesced: [0; 2],
            sampling_gaps: 0,
            counter_resets: 0,
        }
    }

    fn include(&mut self, at: Instant) {
        self.start = Some(self.start.map_or(at, |start| start.min(at)));
    }

    pub(crate) fn observe(&mut self, at: Instant, elapsed: Duration, counters: [u64; 3]) {
        if elapsed.is_zero() {
            return;
        }
        self.include(at.checked_sub(elapsed).unwrap_or(at));
        self.observed += elapsed;
        self.sampling_gaps += usize::from(elapsed > self.sample_interval * 2);
        // A delayed sample still measures the complete counter delta. Retain its
        // actual duration rather than discarding it or inventing a two-second rate.
        for (range, count) in self.rates.iter_mut().zip(counters) {
            range.observe(count as f64 / elapsed.as_secs_f64(), elapsed.as_secs_f64());
        }
    }

    pub(crate) fn reset(&mut self, at: Instant) {
        // A counter baseline reset must not erase observations awaiting publication.
        self.include(at);
        self.counter_resets += 1;
    }

    pub(crate) fn report(
        &mut self,
        at: Instant,
        side: usize,
        values: [Option<f64>; 3],
        observations: u64,
    ) {
        self.include(at);
        for (range, value) in self.reports[side].iter_mut().zip(values) {
            if let Some(value) = value {
                range.observe(value, 1.0);
            }
        }
        self.coalesced[side] += observations.saturating_sub(1);
    }

    pub(crate) fn summary(&mut self, now: Instant) -> Option<WindowSummary> {
        let start = self.start.take()?;
        let reports = [0, 1].map(|side| {
            let [jitter_ms, rtt_ms, loss_percent] =
                self.reports[side].each_mut().map(Accumulator::take);
            ReportSummary {
                side: if side == 0 { "caller" } else { "callee" },
                direction: "egress",
                jitter_ms,
                rtt_ms,
                loss_percent,
                coalesced_reports: std::mem::take(&mut self.coalesced[side]),
            }
        });
        let [
            rx_packets_per_second,
            tx_packets_per_second,
            recording_queue_drops_per_second,
        ] = self.rates.each_mut().map(Accumulator::take);
        Some(WindowSummary {
            window_ms: now.saturating_duration_since(start).as_millis().max(1) as u64,
            observed_ms: std::mem::take(&mut self.observed).as_millis() as u64,
            rx_packets_per_second,
            tx_packets_per_second,
            recording_queue_drops_per_second,
            reports,
            partial_samples: 0,
            sampling_gaps: std::mem::take(&mut self.sampling_gaps),
            counter_resets: std::mem::take(&mut self.counter_resets),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_windows_process_every_observation_once_despite_late_emission() {
        let start = Instant::now();
        let mut window = QualityWindow::new(Duration::from_millis(500));
        // Two counter samples and two RTCP reports in a single two-second group.
        window.observe(
            start + Duration::from_millis(500),
            Duration::from_millis(500),
            [100, 50, 5],
        );
        window.observe(
            start + Duration::from_secs(1),
            Duration::from_millis(500),
            [10, 20, 0],
        );
        window.report(
            start + Duration::from_millis(500),
            0,
            [Some(2.0), None, Some(0.0)],
            1,
        );
        window.report(
            start + Duration::from_secs(1),
            0,
            [Some(6.0), None, Some(10.0)],
            2,
        );
        // Logging was delayed past the nominal window: none of those observations may expire.
        let summary = window
            .summary(start + Duration::from_secs(5))
            .expect("unpublished data must survive");
        assert_eq!(summary.window_ms, 5000);
        assert_eq!(summary.observed_ms, 1000);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().min, 20.0);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().max, 200.0);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().avg, 110.0);
        assert_eq!(summary.tx_packets_per_second.as_ref().unwrap().avg, 70.0);
        assert_eq!(
            summary
                .recording_queue_drops_per_second
                .as_ref()
                .unwrap()
                .avg,
            5.0
        );
        assert_eq!(
            summary.reports[0].jitter_ms.as_ref().unwrap().sample_count,
            2
        );
        assert_eq!(summary.reports[0].jitter_ms.as_ref().unwrap().avg, 4.0);
        assert_eq!(summary.reports[0].coalesced_reports, 1);
        assert!(summary.reports[0].rtt_ms.is_none());
        assert!(
            window.summary(start + Duration::from_secs(7)).is_none(),
            "empty groups are not zero-valued data or a repeat"
        );
        // A late observation covers the missing groups. Preserve its actual elapsed time and all deltas.
        window.observe(
            start + Duration::from_secs(8),
            Duration::from_secs(7),
            [140, 70, 7],
        );
        let late = window.summary(start + Duration::from_secs(8)).unwrap();
        assert_eq!(late.window_ms, 7000);
        assert_eq!(late.observed_ms, 7000);
        assert_eq!(late.sampling_gaps, 1);
        assert_eq!(late.rx_packets_per_second.unwrap().avg, 20.0);
        assert!(
            late.reports[0].jitter_ms.is_none(),
            "reports from a published group must not repeat"
        );
        assert!(window.summary(start + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn diagnostic_windows_preserve_pending_data_across_resets_and_distinguish_zero_from_absence() {
        let start = Instant::now();
        let mut window = QualityWindow::new(Duration::from_millis(500));
        assert!(window.summary(start + Duration::from_secs(2)).is_none());
        window.observe(
            start + Duration::from_millis(2500),
            Duration::from_millis(500),
            [20, 10, 2],
        );
        window.report(
            start + Duration::from_millis(2500),
            0,
            [Some(4.0), None, None],
            1,
        );
        window.reset(start + Duration::from_secs(3));
        window.observe(
            start + Duration::from_millis(3500),
            Duration::from_millis(500),
            [0, 0, 0],
        );
        let summary = window.summary(start + Duration::from_secs(4)).unwrap();
        assert_eq!(summary.observed_ms, 1000);
        assert_eq!(summary.counter_resets, 1);
        assert_eq!(summary.rx_packets_per_second.unwrap().avg, 20.0);
        assert_eq!(summary.reports[0].jitter_ms.as_ref().unwrap().avg, 4.0);
        assert!(summary.reports[1].loss_percent.is_none());
        assert!(window.summary(start + Duration::from_secs(6)).is_none());
        window.observe(
            start + Duration::from_millis(6500),
            Duration::from_millis(500),
            [0, 0, 0],
        );
        let zero = window.summary(start + Duration::from_secs(7)).unwrap();
        assert_eq!(zero.rx_packets_per_second.unwrap().avg, 0.0);
        assert_eq!(zero.observed_ms, 500);
        assert_eq!(zero.counter_resets, 0);
    }
}
