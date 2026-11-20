//! Optional level statistics from PCM already available in the media path.
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct VolumeStatsConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeSummary {
    pub measurement: &'static str,
    pub direction: &'static str,
    pub window_ms: u64,
    pub observed_ms: u64,
    pub window_start_micros: u64,
    pub window_end_micros: u64,
    pub sample_count: u64,
    pub bucket_count: usize,
    pub rms: f64,
    pub attenuation_dbov: u8,
}

#[derive(Default)]
struct Energy {
    samples: u64,
    squares: u64,
}

pub(crate) struct Bucket {
    start_micros: u64,
    end_micros: u64,
    at: Instant,
    start: Instant,
    observed: Duration,
    energy: Energy,
}

#[derive(Default)]
pub(crate) struct VolumeState {
    buckets: Option<VecDeque<Bucket>>,
    // The last observed window survives live-window expiry and source replacement.
    pub(crate) last: Option<VolumeSummary>,
}

pub(crate) type VolumeSink = Arc<Mutex<VolumeState>>;

#[derive(Clone, Copy)]
enum PcmPoint {
    Ingress,
    Egress,
}

struct Coverage {
    start: Instant,
    start_micros: u64,
    end: Instant,
    end_micros: u64,
    observed: Duration,
}

pub(crate) struct VolumeSampler {
    interval: Duration,
    bucket_since: Instant,
    coverage: Option<Coverage>,
    covered_until: Option<Instant>,
    energy: Energy,
    sink: Option<VolumeSink>,
    pending: VolumeAggregate,
    log_interval: Duration,
    last_emit: Instant,
    identity: Option<(String, String)>,
    point: PcmPoint,
}

impl VolumeSampler {
    pub(crate) fn new(
        config: crate::quality_stats::QualityStatsConfig,
        sink: VolumeSink,
        identity: Option<(String, String)>,
    ) -> Self {
        *sink.lock() = VolumeState { buckets: Some(VecDeque::new()), last: None };
        Self::at_point(config, Some(sink), identity, PcmPoint::Egress)
    }

    pub(crate) fn ingress(
        config: crate::quality_stats::QualityStatsConfig,
        identity: (String, String),
    ) -> Self {
        Self::at_point(config, None, Some(identity), PcmPoint::Ingress)
    }

    fn at_point(
        config: crate::quality_stats::QualityStatsConfig,
        sink: Option<VolumeSink>,
        identity: Option<(String, String)>,
        point: PcmPoint,
    ) -> Self {
        let now = Instant::now();
        Self {
            interval: Duration::from_millis(config.sample_interval_ms),
            pending: VolumeAggregate::default(),
            log_interval: Duration::from_millis(config.log_interval_ms),
            last_emit: now,
            identity,
            bucket_since: now,
            coverage: None,
            covered_until: None,
            energy: Energy::default(),
            sink,
            point,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.finish();
        let now = Instant::now();
        self.bucket_since = now;
        self.last_emit = now;
        if let Some(sink) = &self.sink
            && let Some(buckets) = sink.lock().buckets.as_mut()
        {
            buckets.clear();
        }
    }

    pub(crate) fn finish(&mut self) {
        self.complete_bucket();
        self.publish();
    }

    fn complete_bucket(&mut self) {
        let Some(coverage) = self.coverage.take() else {
            return;
        };
        let bucket = Bucket {
            start_micros: coverage.start_micros,
            end_micros: coverage.end_micros,
            at: coverage.end,
            start: coverage.start,
            observed: coverage.observed,
            energy: std::mem::take(&mut self.energy),
        };
        self.pending.include(&bucket);
        if let Some(sink) = &self.sink
            && let Some(buckets) = sink.lock().buckets.as_mut()
        {
            // This deque serves live queries only. Every bucket has already
            // contributed to the complete pending log aggregate above.
            if buckets.len() == 241 {
                buckets.pop_front();
            }
            buckets.push_back(bucket);
        }
    }

    fn publish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let duration =
            Duration::from_micros(pending.end_micros.saturating_sub(pending.start_micros));
        if let Some(value) = pending.summary(duration, self.point) {
            if let Some(sink) = &self.sink {
                sink.lock().last = Some(value.clone());
            }
            self.emit(&value);
        }
    }

    fn emit(&self, value: &VolumeSummary) {
        if let Some((session_id, leg_id)) = &self.identity {
            match serde_json::to_string(value) {
                Ok(volume) => match self.point {
                    PcmPoint::Egress => tracing::info!(session_id = %session_id, leg_id = %leg_id,
                        egress_mode = "pcm_pacing", volume = %volume, "PCM egress volume window"),
                    PcmPoint::Ingress => tracing::info!(session_id = %session_id, leg_id = %leg_id,
                        volume = %volume, "PCM ingress volume window"),
                },
                Err(error) => tracing::warn!(session_id = %session_id, leg_id = %leg_id,
                    direction = value.direction, %error, "Failed to serialize PCM volume window"),
            }
        }
    }

    pub(crate) fn advance(&mut self, now: Instant) {
        let emit_due = now.duration_since(self.last_emit) >= self.log_interval;
        if emit_due || now.duration_since(self.bucket_since) >= self.interval {
            self.complete_bucket();
            self.bucket_since = now;
        }
        if emit_due {
            self.last_emit = now;
            self.publish();
        }
    }

    pub(crate) fn observe(&mut self, pcm: &[i16], sample_rate: u32) {
        if pcm.is_empty() {
            return;
        }
        let now = Instant::now();
        let end_micros = unix_micros();
        let duration = Duration::from_secs_f64(pcm.len() as f64 / f64::from(sample_rate));
        // Timestamp measured PCM from its processing time and frame duration.
        // An earlier observation already owns time through covered_until,
        // including at sample and publication boundaries.
        let start = self.covered_until.map_or(now - duration, |end| end.max(now - duration));
        let observed = now.saturating_duration_since(start);
        let start_micros = end_micros.saturating_sub(observed.as_micros() as u64);
        self.covered_until = Some(now);
        let coverage = self.coverage.get_or_insert(Coverage {
            start,
            start_micros,
            end: start,
            end_micros,
            observed: Duration::ZERO,
        });
        coverage.start = coverage.start.min(start);
        coverage.start_micros = coverage.start_micros.min(start_micros);
        coverage.observed += observed;
        coverage.end = now;
        coverage.end_micros = coverage.end_micros.max(end_micros);
        self.energy.samples += pcm.len() as u64;
        self.energy.squares += pcm
            .iter()
            .map(|&sample| {
                let value = i64::from(sample);
                (value * value) as u64
            })
            .sum::<u64>();
    }
}

fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or_default()
}

fn attenuation(mean_square: f64) -> u8 {
    if mean_square == 0.0 {
        return 127;
    }
    (-10.0 * (mean_square / (32768.0 * 32768.0)).log10())
        .round()
        .clamp(0.0, 126.0) as u8
}

pub(crate) fn summary(sink: &VolumeSink, window: Duration) -> Option<VolumeSummary> {
    let now = Instant::now();
    let cutoff = now.checked_sub(window).unwrap_or(now);
    let mut state = sink.lock();
    let buckets = state.buckets.as_mut()?;
    while buckets.front().is_some_and(|bucket| bucket.at <= cutoff) {
        buckets.pop_front();
    }
    summarize(buckets.iter().filter(|bucket| bucket.start >= cutoff && bucket.at <= now), window)
}

#[derive(Default)]
struct VolumeAggregate {
    energy: Energy,
    count: usize,
    observed: Duration,
    start_micros: u64,
    end_micros: u64,
}

impl VolumeAggregate {
    fn include(&mut self, bucket: &Bucket) {
        if self.count == 0 {
            self.start_micros = bucket.start_micros;
        }
        self.start_micros = self.start_micros.min(bucket.start_micros);
        self.end_micros = self.end_micros.max(bucket.end_micros);
        self.observed += bucket.observed;
        self.energy.samples += bucket.energy.samples;
        self.energy.squares += bucket.energy.squares;
        self.count += 1;
    }

    fn summary(self, window: Duration, point: PcmPoint) -> Option<VolumeSummary> {
        let total = self.energy;
        (total.samples > 0).then(|| {
            let mean_square = total.squares as f64 / total.samples as f64;
            let (measurement, direction) = match point {
                PcmPoint::Ingress => ("pbx_pcm_post_decode", "ingress"),
                PcmPoint::Egress => ("pbx_pcm_pre_encode", "egress"),
            };
            VolumeSummary {
                window_ms: window.as_micros().div_ceil(1_000).max(1) as u64,
                observed_ms: self.observed.min(window).as_micros().div_ceil(1_000) as u64,
                window_start_micros: self.start_micros,
                window_end_micros: self.end_micros,
                measurement,
                direction,
                sample_count: total.samples,
                bucket_count: self.count,
                rms: mean_square.sqrt(),
                attenuation_dbov: attenuation(mean_square),
            }
        })
    }
}

fn summarize<'a>(
    buckets: impl Iterator<Item = &'a Bucket>,
    window: Duration,
) -> Option<VolumeSummary> {
    let mut aggregate = VolumeAggregate::default();
    for bucket in buckets {
        aggregate.include(bucket);
    }
    aggregate.summary(window, PcmPoint::Egress)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    pub(crate) struct Logs(pub(crate) Arc<Mutex<Vec<serde_json::Value>>>);
    impl Subscriber for Logs {
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
            struct Fields(Option<serde_json::Value>);
            impl Visit for Fields {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "volume" {
                        self.0 = Some(serde_json::from_str(&format!("{value:?}")).unwrap());
                    }
                }
            }
            let mut fields = Fields(None);
            event.record(&mut fields);
            if let Some(volume) = fields.0 {
                self.0.lock().push(volume);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn observations_do_not_overlap_across_sample_buckets() {
        let mut sampler = VolumeSampler::ingress(
            crate::quality_stats::QualityStatsConfig {
                sample_interval_ms: 500,
                log_interval_ms: 2000,
                ..Default::default()
            },
            ("session".into(), "peer".into()),
        );
        let pcm = [100; 160];
        tokio::time::advance(Duration::from_millis(490)).await;
        sampler.observe(&pcm, 8_000);
        tokio::time::advance(Duration::from_millis(10)).await;
        sampler.advance(Instant::now());
        sampler.observe(&pcm, 8_000);
        tokio::time::advance(Duration::from_millis(400)).await;
        sampler.observe(&pcm, 8_000);
        sampler.complete_bucket();
        assert_eq!(sampler.pending.observed, Duration::from_millis(50));
        assert_eq!(sampler.pending.energy.samples, 480);
    }

    #[tokio::test(start_paused = true)]
    async fn volume_logs_process_all_pcm_once_including_delayed_and_empty_groups() {
        use tracing::instrument::WithSubscriber;

        let logs = Logs::default();
        let captured = logs.0.clone();
        async {
            let sink = VolumeSink::default();
            let mut sampler = VolumeSampler::new(
                crate::quality_stats::QualityStatsConfig {
                    enabled: true,
                    sample_interval_ms: 500,
                    window_ms: 2000,
                    log_interval_ms: 2000,
                },
                sink,
                Some(("volume-complete-session".into(), "caller".into())),
            );
            sampler.finish();
            assert!(
                captured.lock().is_empty(),
                "an empty group is not a zero-volume observation"
            );
            sampler.observe(&[100, -100], 8000);
            sampler.observe(&[300, -300], 8000);
            // The producer resumes after missing an entire two-second group.
            // PCM already consumed before that pause must still be published.
            tokio::time::advance(Duration::from_secs(3)).await;
            sampler.advance(Instant::now());
            sampler.observe(&[200, -200].repeat(80), 8000);
            sampler.finish();
            sampler.finish();
        }
        .with_subscriber(logs)
        .await;
        let values = captured.lock();
        assert_eq!(
            values.len(),
            2,
            "periodic and final observations are published exactly once"
        );
        assert_eq!(values[0]["sample_count"], 4);
        assert_eq!(values[0]["measurement"], "pbx_pcm_pre_encode");
        assert_eq!(values[0]["direction"], "egress");
        assert_eq!(values[0]["attenuation_dbov"], 43);
        assert_eq!(values[1]["attenuation_dbov"], 44);
        for key in ["peak", "clipped_count", "min_attenuation_dbov", "max_attenuation_dbov"] {
            assert!(values[0].get(key).is_none(), "unexpected statistic {key}");
        }
        assert!((values[0]["rms"].as_f64().unwrap() - 223.60679774997897).abs() < 1e-9);
        assert_eq!(values[1]["sample_count"], 160);
        assert_eq!(values[1]["observed_ms"], 20);
        assert_eq!(values[1]["rms"], 200.0);
        assert_eq!(
            values
                .iter()
                .map(|row| row["sample_count"].as_u64().unwrap())
                .sum::<u64>(),
            164
        );
    }
}
