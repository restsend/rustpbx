//! Bounded sliding statistics for already observed transport counters and RTCP reports.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, try_from = "RawConfig")]
pub struct QualityStatsConfig {
    pub enabled: bool,
    pub sample_interval_ms: u64,
    pub window_ms: u64,
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

#[derive(Debug, Serialize)]
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

struct Sample {
    at: Instant,
    elapsed: Duration,
    rates: [f64; 3],
    gap: Option<SampleGap>,
}

#[derive(Clone, Copy)]
enum SampleGap {
    Delayed,
    CounterReset,
}

struct Report {
    coalesced: u64,
    at: Instant,
    side: usize,
    values: [Option<f64>; 3],
}

pub(crate) struct QualityWindow {
    window: Duration,
    sample_interval: Duration,
    samples: VecDeque<Sample>,
    reports: VecDeque<Report>,
}

impl QualityWindow {
    pub(crate) fn new(window: Duration, sample_interval: Duration) -> Self {
        Self {
            window,
            sample_interval,
            samples: VecDeque::new(),
            reports: VecDeque::new(),
        }
    }
    pub(crate) fn observe(&mut self, at: Instant, elapsed: Duration, counters: [u64; 3]) {
        self.prune(at);
        if elapsed.is_zero() {
            return;
        }
        if self.samples.len() == 241 {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            at,
            elapsed,
            rates: counters.map(|count| count as f64 / elapsed.as_secs_f64()),
            gap: (elapsed > self.sample_interval * 2).then_some(SampleGap::Delayed),
        });
    }
    pub(crate) fn reset(&mut self, at: Instant) {
        self.samples.clear();
        self.reports.clear();
        self.samples.push_back(Sample {
            at,
            elapsed: Duration::ZERO,
            rates: [0.0; 3],
            gap: Some(SampleGap::CounterReset),
        });
    }
    pub(crate) fn report(
        &mut self,
        at: Instant,
        side: usize,
        values: [Option<f64>; 3],
        observations: u64,
    ) {
        self.prune(at);
        if self.reports.len() == 482 {
            self.reports.pop_front();
        }
        self.reports.push_back(Report {
            at,
            side,
            values,
            coalesced: observations.saturating_sub(1),
        });
    }
    fn prune(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        while self
            .samples
            .front()
            .is_some_and(|sample| sample.at <= cutoff)
        {
            self.samples.pop_front();
        }
        self.reports.retain(|report| report.at > cutoff);
    }
    pub(crate) fn summary(&mut self, now: Instant) -> Option<WindowSummary> {
        self.prune(now);
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let weighted: Vec<_> = self
            .samples
            .iter()
            .filter_map(|sample| {
                let start = sample.at.checked_sub(sample.elapsed).unwrap_or(sample.at);
                // Counter deltas do not reveal packet timing within a sampled interval.
                // Exclude a straddling interval instead of inventing proportional counts.
                (sample.gap.is_none() && start >= cutoff && sample.at <= now)
                    .then(|| (sample, sample.elapsed.as_secs_f64()))
            })
            .collect();
        let sampling_gaps = self
            .samples
            .iter()
            .filter(|sample| sample.gap.is_some())
            .count();
        let partial_samples = self
            .samples
            .iter()
            .filter(|sample| {
                sample.gap.is_none()
                    && sample
                        .at
                        .checked_sub(sample.elapsed)
                        .is_some_and(|start| start < cutoff)
            })
            .count();
        // Partial intervals cannot establish rates, but their coverage and
        // independently observed RTCP reports must not disappear.
        if weighted.is_empty()
            && sampling_gaps == 0
            && partial_samples == 0
            && self.reports.is_empty()
        {
            return None;
        }
        let counter_resets = self
            .samples
            .iter()
            .filter(|sample| matches!(sample.gap, Some(SampleGap::CounterReset)))
            .count();
        let rate = |index: usize| {
            range(
                weighted
                    .iter()
                    .map(|(sample, weight)| (sample.rates[index], *weight)),
            )
        };
        let report = |side| {
            let value = |index: usize| {
                range(
                    self.reports
                        .iter()
                        .filter(|report| report.side == side)
                        .filter_map(|report| report.values[index].map(|value| (value, 1.0))),
                )
            };
            ReportSummary {
                coalesced_reports: self
                    .reports
                    .iter()
                    .filter(|report| report.side == side)
                    .map(|report| report.coalesced)
                    .sum(),
                side: if side == 0 { "caller" } else { "callee" },
                direction: "egress",
                jitter_ms: value(0),
                rtt_ms: value(1),
                loss_percent: value(2),
            }
        };
        Some(WindowSummary {
            window_ms: self.window.as_millis() as u64,
            sampling_gaps,
            partial_samples,
            counter_resets,
            observed_ms: (weighted.iter().map(|(_, weight)| *weight).sum::<f64>() * 1000.0).round()
                as u64,
            rx_packets_per_second: rate(0),
            tx_packets_per_second: rate(1),
            recording_queue_drops_per_second: rate(2),
            reports: [report(0), report(1)],
        })
    }
}

fn range(values: impl Iterator<Item = (f64, f64)>) -> Option<Range> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut weighted_sum = 0.0;
    let mut total_weight = 0.0;
    let mut sample_count = 0;
    for (value, weight) in values {
        if !value.is_finite() || weight <= 0.0 {
            continue;
        }
        min = min.min(value);
        max = max.max(value);
        weighted_sum += value * weight;
        total_weight += weight;
        sample_count += 1;
    }
    (sample_count > 0).then(|| Range {
        min,
        max,
        avg: weighted_sum / total_weight,
        sample_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_window_excludes_old_samples_and_weights_elapsed_time() {
        let start = Instant::now();
        let mut window = QualityWindow::new(Duration::from_secs(2), Duration::from_millis(500));
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
            start + Duration::from_secs(1),
            0,
            [Some(0.0), None, Some(0.0)],
            2,
        );
        window.observe(
            start + Duration::from_secs(2),
            Duration::from_secs(1),
            [40, 80, 2],
        );
        let summary = window.summary(start + Duration::from_secs(2)).unwrap();
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().min, 20.0);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().max, 200.0);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().avg, 75.0);
        assert_eq!(summary.observed_ms, 2000);
        assert_eq!(
            summary.reports[0].jitter_ms.as_ref().unwrap().sample_count,
            1
        );
        assert!(summary.reports[0].rtt_ms.is_none());
        assert_eq!(summary.reports[0].coalesced_reports, 1);
        window.observe(
            start + Duration::from_secs(3),
            Duration::from_secs(1),
            [100, 0, 0],
        );
        let summary = window.summary(start + Duration::from_secs(3)).unwrap();
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().min, 40.0);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().max, 100.0);
        assert_eq!(summary.rx_packets_per_second.as_ref().unwrap().avg, 70.0);
        assert!(
            summary.reports[0].jitter_ms.is_none(),
            "old report is not a fresh sample"
        );
        let partial = window.summary(start + Duration::from_millis(3250)).unwrap();
        assert_eq!(partial.partial_samples, 1);
        assert_eq!(partial.observed_ms, 1000);
        assert_eq!(partial.rx_packets_per_second.as_ref().unwrap().avg, 100.0);
        assert!(window.summary(start + Duration::from_secs(6)).is_none());
        window.observe(
            start + Duration::from_secs(6),
            Duration::from_secs(3),
            [9000, 0, 0],
        );
        let gap = window.summary(start + Duration::from_secs(6)).unwrap();
        assert!(
            gap.rx_packets_per_second.is_none(),
            "a long sampler pause is not a rate spike"
        );
        assert_eq!(gap.sampling_gaps, 1);
        window.observe(
            start + Duration::from_millis(6500),
            Duration::from_millis(500),
            [0, 0, 0],
        );
        let observed_zero = window.summary(start + Duration::from_millis(6500)).unwrap();
        assert_eq!(
            observed_zero.rx_packets_per_second.unwrap().avg,
            0.0,
            "successfully sampled unchanged counters are observed zero, not missing data"
        );
        window.reset(start + Duration::from_secs(7));
        let reset = window.summary(start + Duration::from_secs(7)).unwrap();
        assert_eq!(reset.counter_resets, 1);
        assert!(reset.rx_packets_per_second.is_none());

        let results: Vec<_> = [(500, 501), (7500, 10_000)]
            .into_iter()
            .map(|(interval_ms, elapsed_ms)| {
                let interval = Duration::from_millis(interval_ms);
                let mut window = QualityWindow::new(interval, interval);
                let at = start + Duration::from_millis(elapsed_ms);
                window.observe(at, Duration::from_millis(elapsed_ms), [100, 50, 5]);
                let partial = window.summary(at);
                window.report(at, 0, [Some(4.0), Some(17.0), Some(1.0)], 2);
                let with_report = window.summary(at);
                assert!(window.summary(at + interval).is_none());
                (interval_ms, elapsed_ms, partial, with_report)
            })
            .collect();
        let missing: Vec<_> = results
            .iter()
            .filter(|(_, _, partial, report)| partial.is_none() || report.is_none())
            .map(|(interval, elapsed, _, _)| (*interval, *elapsed))
            .collect();
        assert!(
            missing.is_empty(),
            "straddling samples must remain visible: {missing:?}"
        );
        for (_, _, partial, with_report) in results {
            for summary in [partial.as_ref().unwrap(), with_report.as_ref().unwrap()] {
                assert_eq!(summary.partial_samples, 1);
                assert_eq!(summary.observed_ms, 0);
                assert_eq!(summary.sampling_gaps, 0);
                assert!(summary.rx_packets_per_second.is_none());
                assert!(summary.tx_packets_per_second.is_none());
                assert!(summary.recording_queue_drops_per_second.is_none());
            }
            let report = &with_report.unwrap().reports[0];
            assert_eq!(report.jitter_ms.as_ref().unwrap().avg, 4.0);
            assert_eq!(report.rtt_ms.as_ref().unwrap().avg, 17.0);
            assert_eq!(report.loss_percent.as_ref().unwrap().avg, 1.0);
            assert_eq!(report.coalesced_reports, 1);
        }
    }
}
